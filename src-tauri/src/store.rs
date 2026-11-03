use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

use crate::limits::{
    apply_retention, sanitize_data, validate_data, Data, Entry, MAX_ITEM_BYTES,
    MAX_NAME_LENGTH,
};

pub const DEFAULT_SHORTCUT: &str = "CommandOrControl+Shift+V";

/// Backups are taken on a time-based cadence (not per save, not per process
/// counter) so the cadence survives restarts and does not double disk writes.
const BACKUP_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(10 * 60);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub shortcut: String,
    pub auto_start: bool,
    pub auto_capture: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            shortcut: DEFAULT_SHORTCUT.to_string(),
            auto_start: false,
            auto_capture: true,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowState {
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsPayload {
    pub shortcut: String,
    pub auto_start: bool,
    pub auto_capture: bool,
    pub active_shortcut: String,
    pub default_shortcut: String,
    pub auto_start_supported: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PartialSettings {
    pub shortcut: Option<String>,
    pub auto_start: Option<bool>,
    pub auto_capture: Option<bool>,
}

pub fn config_dir(app: &AppHandle) -> PathBuf {
    let dir = app
        .path()
        .app_config_dir()
        .unwrap_or_else(|_| PathBuf::from("."));
    let _ = fs::create_dir_all(&dir);
    dir
}

/// Single place that builds a path inside the app config directory.
fn config_path(app: &AppHandle, name: &str) -> PathBuf {
    config_dir(app).join(name)
}

pub fn data_file(app: &AppHandle) -> PathBuf {
    config_path(app, "clipboard-data.json")
}

pub fn settings_file(app: &AppHandle) -> PathBuf {
    config_path(app, "settings.json")
}

pub fn window_state_file(app: &AppHandle) -> PathBuf {
    config_path(app, "window-state.json")
}

/// Write `bytes` to `path` through a .tmp file + fsync + rename, so a crash or
/// power loss can never leave a half-written (unparsable) file behind.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let tmp = tmp_path(path);
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// The `.tmp` suffix is appended, not substituted: `with_extension("tmp")`
/// would give `settings.json` and `settings.bak` the same temp path.
pub fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    name.push_str(".tmp");
    path.with_file_name(name)
}

/// Remove orphaned .tmp files left behind by a crash mid-write.
pub fn sweep_temp_files(app: &AppHandle) {
    let dir = config_dir(app);
    let Ok(entries) = fs::read_dir(&dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("tmp") {
            let _ = fs::remove_file(&path);
        }
    }
}

fn read_json(path: &Path) -> Option<serde_json::Value> {
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Read the store, falling back to the .bak copy, then to empty data.
pub fn load_data(app: &AppHandle) -> Data {
    let main = data_file(app);
    if let Some(value) = read_json(&main) {
        return sanitize_data(&value);
    }
    let bak = main.with_extension("json.bak");
    if let Some(value) = read_json(&bak) {
        // Keep the unreadable original for recovery: the next save would
        // otherwise overwrite it.
        if main.exists() {
            let _ = fs::rename(&main, main.with_extension(format!("corrupt-{}.json", stamp())));
            eprintln!("clipboard-data.json was unreadable; recovered from the .bak copy");
        }
        return sanitize_data(&value);
    }
    // Nothing readable. Returning empty data here means the very next debounced
    // save would overwrite a file that may still be recoverable, so move it
    // aside first.
    if main.exists() {
        let _ = fs::rename(&main, main.with_extension(format!("corrupt-{}.json", stamp())));
    }
    Data::default()
}

/// Unix seconds, used to name recovered/corrupt files.
fn stamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn io_error_code(err: &std::io::Error) -> &'static str {
    match err.kind() {
        ErrorKind::PermissionDenied => "permission",
        ErrorKind::ReadOnlyFilesystem => "read-only",
        ErrorKind::StorageFull => "disk-full",
        ErrorKind::ResourceBusy => "busy",
        _ => {
            let msg = err.to_string().to_lowercase();
            if msg.contains("no space") {
                "disk-full"
            } else if msg.contains("denied") {
                "permission"
            } else if msg.contains("read-only") {
                "read-only"
            } else if msg.contains("being used") || msg.contains("busy") {
                "busy"
            } else {
                "unknown"
            }
        }
    }
}

/// `write_atomic` with the (code, message) error shape the commands return.
fn write_atomic_mapped(path: &Path, bytes: &[u8]) -> Result<(), (String, String)> {
    write_atomic(path, bytes).map_err(|e| (io_error_code(&e).to_string(), e.to_string()))
}

/// Validate, back up the current file when the backup is stale, then write
/// atomically (tmp + fsync + rename).
pub fn save_data(app: &AppHandle, data: Data) -> Result<(), (String, String)> {
    let mut clean = validate_data(data, "saveData").map_err(|m| ("invalid".to_string(), m))?;
    // Trust boundary: even if the renderer forgot, never persist more than the
    // rolling window (100 unpinned rows, 25 of them heavy). Pinned rows stay.
    clean.items = apply_retention(clean.items);
    let main = data_file(app);
    let bak = main.with_extension("json.bak");

    let json = serde_json::to_string(&clean)
        .map_err(|e| ("invalid".to_string(), e.to_string()))?;

    // Back up whenever no backup exists yet, or the existing one is stale.
    if main.exists() && backup_is_stale(&bak) {
        let _ = fs::copy(&main, &bak);
    }

    write_atomic_mapped(&main, json.as_bytes())
}

/// True when there is no backup yet, or the one on disk is older than
/// BACKUP_MAX_AGE. Any metadata error falls back to "take a backup".
fn backup_is_stale(bak: &Path) -> bool {
    let Ok(meta) = fs::metadata(bak) else { return true };
    let Ok(modified) = meta.modified() else { return true };
    modified.elapsed().map(|age| age >= BACKUP_MAX_AGE).unwrap_or(true)
}

pub fn normalize_settings(mut s: Settings) -> Settings {
    let trimmed = s.shortcut.trim().to_string();
    s.shortcut = if trimmed.is_empty() || trimmed.chars().count() > 60 {
        DEFAULT_SHORTCUT.to_string()
    } else {
        trimmed
    };
    s
}

pub fn load_settings(app: &AppHandle) -> Settings {
    match read_json(&settings_file(app))
        .and_then(|v| serde_json::from_value::<Settings>(v).ok())
    {
        Some(s) => normalize_settings(s),
        None => Settings::default(),
    }
}

pub fn persist_settings(app: &AppHandle, settings: &Settings) -> bool {
    match serde_json::to_string_pretty(settings) {
        // Atomic: a torn settings.json would silently reset the shortcut and
        // the autostart flag on the next launch.
        Ok(json) => write_atomic(&settings_file(app), json.as_bytes()).is_ok(),
        Err(_) => false,
    }
}

pub fn load_window_state(app: &AppHandle) -> Option<WindowState> {
    read_json(&window_state_file(app)).and_then(|v| serde_json::from_value(v).ok())
}

pub fn save_window_state(app: &AppHandle, state: &WindowState) {
    if let Ok(json) = serde_json::to_string(state) {
        let _ = write_atomic(&window_state_file(app), json.as_bytes());
    }
}

/// Replace characters Windows forbids in file names, cap the length, guarantee
/// a `.json` extension and refuse reserved device names.
pub fn sanitize_file_name(name: Option<&str>) -> String {
    const FALLBACK: &str = "clipboard-backup-file.json";
    const RESERVED: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
        "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];

    let raw = name.map(|s| s.trim()).unwrap_or("");
    if raw.is_empty() {
        return FALLBACK.to_string();
    }

    let cleaned: String = raw
        .chars()
        .map(|c| match c {
            '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if (c as u32) < 0x20 => '_',
            other => other,
        })
        .take(MAX_NAME_LENGTH.min(120))
        .collect();

    // Windows silently drops trailing dots and spaces, which can turn a name
    // into an empty or unexpected one.
    let cleaned = cleaned.trim_end_matches([' ', '.']).trim().to_string();
    if cleaned.is_empty() {
        return FALLBACK.to_string();
    }

    // Always hand the dialog a .json name; the export payload is always JSON.
    let with_ext = if cleaned.to_ascii_lowercase().ends_with(".json") {
        cleaned
    } else {
        format!("{cleaned}.json")
    };

    let stem = with_ext.split('.').next().unwrap_or("").to_ascii_uppercase();
    if RESERVED.contains(&stem.as_str()) {
        return format!("_{with_ext}");
    }
    with_ext
}

/* --------------------------- background capture --------------------------- */

/// Monotonic counter so two captures inside the same millisecond still get
/// different ids.
static UID_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Unique id for a row created by the backend. The renderer mints ids of its
/// own; the `c` prefix keeps the two namespaces from ever colliding.
fn new_uid() -> String {
    let n = UID_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("c{millis}-{n}")
}

/// What happened to a captured clipboard text.
///
/// The capture loop must be able to tell "nothing to do" apart from "the disk
/// write failed", because only the second case may be retried — retrying a
/// duplicate would churn the disk forever.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureOutcome {
    /// The store on disk changed (new row, or an existing row promoted).
    Stored,
    /// Already the newest row / a pinned row: nothing to write.
    Duplicate,
    /// Empty or larger than MAX_ITEM_BYTES: never stored, never retried.
    Rejected,
    /// The pipeline ran but the disk write failed — safe to retry.
    SaveFailed,
}

/// Store a captured clipboard text without involving the renderer.
///
/// This is what keeps the history complete while the app sits silently in the
/// tray: the webview may be hidden, throttled or not even ready yet, so the
/// capture -> dedupe -> retention -> disk pipeline runs entirely in Rust.
pub fn capture_text_outcome(app: &AppHandle, text: String) -> CaptureOutcome {
    if text.is_empty() || text.len() > MAX_ITEM_BYTES {
        return CaptureOutcome::Rejected;
    }
    let mut data = load_data(app);

    // Length first: a full comparison against 100 rows of up to 4 MB each would
    // run on every clipboard change.
    let len = text.len();
    let existing = data
        .items
        .iter()
        .position(|e| e.text.len() == len && e.text == text);

    match existing {
        // Pinned rows keep their place; re-copying one changes nothing.
        Some(pos) if data.items[pos].pinned => return CaptureOutcome::Duplicate,
        // Same text is stored once: re-copying promotes the row to the top.
        Some(0) => return CaptureOutcome::Duplicate,
        Some(pos) => {
            let entry = data.items.remove(pos);
            data.items.insert(0, entry);
        }
        None => {
            data.items.insert(0, Entry { text, pinned: false, uid: Some(new_uid()) });
        }
    }

    data.items = apply_retention(std::mem::take(&mut data.items));
    if save_data(app, data).is_ok() {
        CaptureOutcome::Stored
    } else {
        CaptureOutcome::SaveFailed
    }
}

