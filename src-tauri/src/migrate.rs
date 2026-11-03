use std::fs;
use std::path::PathBuf;

use tauri::AppHandle;

use crate::limits::sanitize_data;
use crate::store::config_dir;

const FILES: [&str; 3] = ["clipboard-data.json", "settings.json", "window-state.json"];

/// The Electron build stored settings under slightly different key names.
/// Copying the file verbatim meant a key mismatch silently reset the shortcut
/// and the auto-start choice, so the keys are mapped explicitly here.
fn normalize_legacy_settings(path: &std::path::Path) {
    let Ok(raw) = fs::read_to_string(path) else { return };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else { return };
    let Some(obj) = value.as_object() else { return };

    let pick_str = |keys: &[&str]| -> Option<String> {
        keys.iter()
            .find_map(|k| obj.get(*k).and_then(|v| v.as_str()).map(|s| s.to_string()))
    };
    let pick_bool = |keys: &[&str]| -> Option<bool> {
        keys.iter().find_map(|k| obj.get(*k).and_then(|v| v.as_bool()))
    };

    let mut settings = crate::store::Settings::default();
    if let Some(s) = pick_str(&["shortcut", "hotkey", "globalShortcut", "accelerator"]) {
        settings.shortcut = s;
    }
    if let Some(b) = pick_bool(&["autoStart", "auto_start", "startOnBoot", "launchAtLogin"]) {
        settings.auto_start = b;
    }
    if let Some(b) = pick_bool(&["autoCapture", "auto_capture", "captureClipboard", "watchClipboard"]) {
        settings.auto_capture = b;
    }
    let settings = crate::store::normalize_settings(settings);
    if let Ok(json) = serde_json::to_string_pretty(&settings) {
        let _ = fs::write(path, json);
    }
}

fn legacy_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    #[cfg(target_os = "windows")]
    {
        if let Ok(appdata) = std::env::var("APPDATA") {
            let base = PathBuf::from(appdata);
            dirs.push(base.join("mehedi-clipboard"));
        }
    }
    dirs
}

/// Copy (never move) the old Electron userData files on first run only.
pub fn migrate_if_needed(app: &AppHandle) {
    let target = config_dir(app);
    // v3.2: the history cap dropped from 1000 to 100 unpinned rows. Stores
    // written by an older build are trimmed once, in place, on first start.
    trim_to_new_cap(&target);

    let flag = target.join("migrated.flag");
    if flag.exists() || target.join("clipboard-data.json").exists() {
        return;
    }

    for dir in legacy_dirs() {
        if !dir.join("clipboard-data.json").exists() {
            continue;
        }
        for name in FILES {
            let src = dir.join(name);
            if src.exists() {
                let _ = fs::copy(&src, target.join(name));
            }
        }
        normalize_legacy_settings(&target.join("settings.json"));

        // Validate the copied data leniently so a corrupt file cannot break startup.
        let copied = target.join("clipboard-data.json");
        if let Ok(raw) = fs::read_to_string(&copied) {
            let value: serde_json::Value =
                serde_json::from_str(&raw).unwrap_or(serde_json::Value::Null);
            let clean = sanitize_data(&value);
            if let Ok(json) = serde_json::to_string(&clean) {
                let _ = fs::write(&copied, json);
            }
        }
        break;
    }

    let _ = fs::write(flag, "1");
}

/// One-time, in-place trim of an existing store to the v3.2 rolling window.
/// `sanitize_data` already applies retention (pinned kept, oldest unpinned
/// dropped), so this just rewrites the file when something was removed.
fn trim_to_new_cap(target: &std::path::Path) {
    let flag = target.join("retention-v3.2.flag");
    if flag.exists() {
        return;
    }
    let data_path = target.join("clipboard-data.json");
    if let Ok(raw) = fs::read_to_string(&data_path) {
        let value: serde_json::Value =
            serde_json::from_str(&raw).unwrap_or(serde_json::Value::Null);
        let clean = sanitize_data(&value);
        if let Ok(json) = serde_json::to_string(&clean) {
            if json != raw {
                let _ = fs::write(&data_path, json);
            }
        }
    }
    let _ = fs::create_dir_all(target);
    let _ = fs::write(flag, "1");
}
