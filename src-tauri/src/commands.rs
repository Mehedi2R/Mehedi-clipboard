use std::sync::atomic::Ordering;

use serde::Serialize;
use tauri::{AppHandle, Manager, State};
use tauri_plugin_clipboard_manager::ClipboardExt;
use tauri_plugin_dialog::DialogExt;

use crate::limits::{get_byte_size, truncate_by_bytes, Data, MAX_ITEM_BYTES};
use crate::paste::send_ctrl_v;
use crate::store::{
    self, load_data, load_settings, persist_settings, sanitize_file_name, PartialSettings,
    SettingsPayload, DEFAULT_SHORTCUT,
};
use crate::{lock_or_recover, remember_hash, text_hash, AppState};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveResult {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportResult {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Result of an auto-paste attempt. `copied` stays true when the text reached the
/// clipboard even though the Ctrl+V keystroke could not be delivered.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PasteResult {
    pub ok: bool,
    pub copied: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetSettingsResult {
    pub ok: bool,
    pub shortcut_ok: bool,
    /// False when Windows refused the Run-key change; the UI says so instead of
    /// showing a toggle that does nothing.
    pub auto_start_ok: bool,
    pub persisted: bool,
    pub settings: SettingsPayload,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportPayload {
    pub content: String,
    pub suggested_name: Option<String>,
}

#[tauri::command]
pub fn get_data(app: AppHandle) -> Data {
    load_data(&app)
}

/// The store write (validate + serialize + backup copy + fsync + rename) is the
/// heaviest disk operation in the app and used to run straight on the IPC
/// worker pool, freezing every other command — including the quit flush —
/// behind it. It runs on a blocking worker now.
#[tauri::command]
pub async fn save_data(app: AppHandle, data: Data) -> SaveResult {
    let outcome = tauri::async_runtime::spawn_blocking(move || store::save_data(&app, data)).await;
    match outcome {
        Ok(Ok(())) => SaveResult { ok: true, code: None, message: None },
        Ok(Err((code, message))) => {
            SaveResult { ok: false, code: Some(code), message: Some(message) }
        }
        Err(join_err) => SaveResult {
            ok: false,
            code: Some("unknown".into()),
            message: Some(format!("save worker failed: {join_err}")),
        },
    }
}

/// Single place that validates the size, writes to the clipboard and records the
/// hash so the capture poller does not re-import what we just wrote.
/// `text` is moved into the clipboard (no second 4 MB allocation).
fn write_and_track(app: &AppHandle, state: &AppState, text: String) -> Result<(), String> {
    if get_byte_size(&text) > MAX_ITEM_BYTES {
        return Err("text is larger than the 4 MB limit".into());
    }
    let hash = text_hash(&text);
    app.clipboard()
        .write_text(text)
        .map_err(|_| "clipboard write failed".to_string())?;
    // Remember the hash (short ring buffer) so the capture poller does not
    // re-import what we just wrote.
    remember_hash(state, hash);
    Ok(())
}

#[tauri::command]
pub fn copy_text(app: AppHandle, state: State<'_, AppState>, text: String) -> PasteResult {
    match write_and_track(&app, &state, text) {
        Ok(()) => PasteResult { ok: true, copied: true, message: None },
        Err(message) => PasteResult { ok: false, copied: false, message: Some(message) },
    }
}

#[tauri::command]
pub fn read_clipboard(app: AppHandle) -> String {
    match app.clipboard().read_text() {
        Ok(text) => truncate_by_bytes(&text, MAX_ITEM_BYTES),
        Err(_) => String::new(),
    }
}

#[tauri::command]
pub fn hide_window(app: AppHandle) -> bool {
    crate::hide_main_window(&app)
}

#[tauri::command]
pub async fn paste_to_active(app: AppHandle, text: String) -> PasteResult {
    // The clipboard write is cheap and stays on the caller; everything that can
    // block (focus polling, modifier wait, PowerShell fallback — up to ~5.7 s)
    // runs on a blocking worker so the IPC pool stays free for the quit flush.
    {
        let state = app.state::<AppState>();
        if let Err(message) = write_and_track(&app, &state, text) {
            return PasteResult { ok: false, copied: false, message: Some(message) };
        }
    }
    // Remember our own window handle so the keystroke is only injected
    // once Windows has actually moved focus somewhere else.
    let self_hwnd = own_window_handle(&app);
    crate::hide_main_window(&app);

    let outcome = tauri::async_runtime::spawn_blocking(move || send_ctrl_v(self_hwnd)).await;

    match outcome {
        Ok(Ok(())) => PasteResult { ok: true, copied: true, message: None },
        Ok(Err(reason)) => {
            // Bring the window back so the renderer's warning is actually visible.
            crate::show_main_window(&app);
            PasteResult { ok: false, copied: true, message: Some(reason) }
        }
        Err(join_err) => {
            crate::show_main_window(&app);
            PasteResult {
                ok: false,
                copied: true,
                message: Some(format!("paste worker failed: {join_err}")),
            }
        }
    }
}

/// The main window's native handle, when it can be determined (Windows only).
fn own_window_handle(app: &AppHandle) -> Option<isize> {
    #[cfg(target_os = "windows")]
    {
        return app
            .get_webview_window("main")
            .and_then(|w| w.hwnd().ok())
            .map(|h| h.0 as isize);
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = app;
        None
    }
}

#[tauri::command]
pub async fn export_file(app: AppHandle, payload: ExportPayload) -> ExportResult {
    let name = sanitize_file_name(payload.suggested_name.as_deref());
    let downloads = app.path().download_dir().ok();

    let mut builder = app.dialog().file().set_file_name(&name).add_filter("JSON", &["json"]);
    if let Some(dir) = downloads {
        builder = builder.set_directory(dir);
    }

    // `blocking_save_file()` parked a runtime worker for as long as the
    // dialog was open, so other IPC commands queued behind it and the UI looked
    // frozen. The async dialog hands the result back through a channel instead.
    let (tx, rx) = std::sync::mpsc::channel();
    builder.save_file(move |picked| {
        let _ = tx.send(picked);
    });
    let chosen = tauri::async_runtime::spawn_blocking(move || rx.recv().ok().flatten())
        .await
        .ok()
        .flatten();
    let Some(path) = chosen else {
        return ExportResult {
            ok: false,
            file_path: None,
            code: Some("canceled".into()),
            message: None,
        };
    };

    let path_buf = match path.into_path() {
        Ok(p) => p,
        Err(e) => {
            return ExportResult {
                ok: false,
                file_path: None,
                code: Some("unknown".into()),
                message: Some(e.to_string()),
            }
        }
    };

    // Writing a multi-megabyte export must not block the async runtime either.
    let content = payload.content;
    let write_target = path_buf.clone();
    let written = tauri::async_runtime::spawn_blocking(move || {
        std::fs::write(&write_target, content.as_bytes())
    })
    .await
    .unwrap_or_else(|e| Err(std::io::Error::other(e.to_string())));

    match written {
        Ok(()) => ExportResult {
            ok: true,
            file_path: Some(path_buf.to_string_lossy().to_string()),
            code: None,
            message: None,
        },
        Err(e) => ExportResult {
            ok: false,
            file_path: None,
            code: Some("unknown".into()),
            message: Some(e.to_string()),
        },
    }
}

pub fn settings_payload(app: &AppHandle, state: &AppState) -> SettingsPayload {
    let settings = load_settings(app);
    let active = lock_or_recover(&state.active_shortcut).clone();
    // Report what the Windows Run key really holds; settings.json is only the
    // fallback when the registry cannot be read.
    let auto_start = crate::autostart_enabled(app).unwrap_or(settings.auto_start);
    SettingsPayload {
        shortcut: settings.shortcut.clone(),
        auto_start,
        auto_capture: settings.auto_capture,
        active_shortcut: active,
        default_shortcut: DEFAULT_SHORTCUT.to_string(),
        auto_start_supported: cfg!(target_os = "windows"),
    }
}

#[tauri::command]
pub fn get_settings(app: AppHandle, state: State<'_, AppState>) -> SettingsPayload {
    settings_payload(&app, &state)
}

#[tauri::command]
pub fn set_settings(
    app: AppHandle,
    state: State<'_, AppState>,
    partial: PartialSettings,
) -> SetSettingsResult {
    let current = load_settings(&app);
    let mut next = current.clone();
    if let Some(s) = partial.shortcut {
        next.shortcut = s;
    }
    if let Some(v) = partial.auto_start {
        next.auto_start = v;
    }
    if let Some(v) = partial.auto_capture {
        next.auto_capture = v;
    }
    next = store::normalize_settings(next);

    let mut shortcut_ok = true;
    if next.shortcut != current.shortcut
        || *lock_or_recover(&state.active_shortcut) != next.shortcut
    {
        // The previous accelerator stays registered until the new one is
        // accepted, so a taken shortcut can never leave the app hotkey-less.
        let previous = lock_or_recover(&state.active_shortcut).clone();
        let old = if previous.is_empty() { None } else { Some(previous.as_str()) };
        match crate::switch_shortcut(&app, &next.shortcut, old) {
            Ok(()) => {
                *lock_or_recover(&state.active_shortcut) = next.shortcut.clone();
            }
            Err(_) => {
                shortcut_ok = false;
                if previous.is_empty() {
                    // Nothing was registered before either: try the default once.
                    if crate::register_shortcut(&app, DEFAULT_SHORTCUT).is_ok() {
                        *lock_or_recover(&state.active_shortcut) = DEFAULT_SHORTCUT.to_string();
                        next.shortcut = DEFAULT_SHORTCUT.to_string();
                    } else {
                        // Be honest: no shortcut is active. The UI reads
                        // `activeShortcut` and says so instead of showing a
                        // hotkey that does nothing.
                        *lock_or_recover(&state.active_shortcut) = String::new();
                        next.shortcut = previous.clone();
                    }
                } else {
                    // The old accelerator is still registered and still works.
                    next.shortcut = previous.clone();
                }
            }
        }
    }

    let mut auto_start_ok = true;
    if let Err(e) = crate::apply_autostart(&app, next.auto_start) {
        eprintln!("autostart change failed: {e}");
        auto_start_ok = false;
        // Never persist a preference Windows rejected.
        next.auto_start = crate::autostart_enabled(&app).unwrap_or(current.auto_start);
    }
    crate::sync_tray_autostart(next.auto_start);

    state.auto_capture.store(next.auto_capture, Ordering::SeqCst);
    if next.auto_capture && !current.auto_capture {
        crate::start_capture_thread(&app);
        // Switching capture on should also keep whatever is on the clipboard
        // right now, instead of waiting for the next copy.
        crate::capture_current(&app);
    } else if next.auto_capture {
        crate::start_capture_thread(&app);
    }
    crate::sync_tray_autocapture(next.auto_capture);

    let persisted = persist_settings(&app, &next);

    SetSettingsResult {
        ok: true,
        shortcut_ok,
        auto_start_ok,
        persisted,
        settings: settings_payload(&app, &state),
    }
}

/// The renderer calls this once its event listeners are attached.
/// Any startup event that would otherwise have been emitted into the void
/// (currently `shortcut-failed`) is delivered here instead of after a blind
/// 600 ms sleep on a dedicated thread.
#[tauri::command]
pub fn renderer_ready(app: AppHandle, state: State<'_, AppState>) -> bool {
    state.renderer_ready.store(true, Ordering::SeqCst);
    use tauri::Emitter;
    let pending = lock_or_recover(&state.pending_shortcut_failed).take();
    if let Some(payload) = pending {
        let _ = app.emit("shortcut-failed", payload);
    }
    // Warnings raised before the UI existed (e.g. a failed autostart
    // registration) are delivered now instead of being lost.
    let notice = lock_or_recover(&state.pending_notice).take();
    if let Some(payload) = notice {
        let _ = app.emit("notice", payload);
    }
    true
}

/// The renderer calls this after it has written every pending change to
/// disk, which releases the shutdown thread waiting in `quit_app`.
#[tauri::command]
pub fn flush_complete(state: State<'_, AppState>) -> bool {
    let ack = state.flush_ack.clone();
    let (lock, cv) = &*ack;
    *lock_or_recover(lock) = true;
    cv.notify_all();
    true
}
