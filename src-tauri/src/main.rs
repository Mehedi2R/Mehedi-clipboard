#![cfg_attr(all(not(debug_assertions), target_os = "windows"), windows_subsystem = "windows")]

mod capture;
mod commands;
mod limits;
mod migrate;
mod paste;
mod store;

use std::collections::hash_map::DefaultHasher;
use std::collections::VecDeque;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, LogicalPosition, Manager, WindowEvent};
// The clipboard itself is only touched by capture.rs and commands.rs now.
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut};

pub use capture::{capture_current, start_capture_thread, start_capture_watchdog};
use store::{load_settings, WindowState, DEFAULT_SHORTCUT};

/// How many recently seen clipboard hashes are remembered.
/// A single "last hash" re-imported A after an A -> B -> A round trip and made
/// apps that re-set the same clipboard content churn a 4 MB read every tick.
const RECENT_HASHES: usize = 16;

/// Geometry is written at most once per this interval.
const GEOM_FLUSH_MS: u64 = 500;

/// Windows parks a minimizing window at (-32000, -32000). Anything at or
/// below this bound is a transient/minimized rectangle, never a real position.
const MIN_WINDOW_COORD: i32 = -8;

/// How long the app waits for the renderer to flush pending writes on quit.
const QUIT_FLUSH_TIMEOUT_MS: u64 = 1500;

pub struct AppState {
    /// Only 64-bit hashes of the last few captured texts are
    /// kept. Storing the text itself meant holding up to 4 MB in memory forever.
    pub last_captured: Mutex<VecDeque<u64>>,
    pub active_shortcut: Mutex<String>,
    pub auto_capture: AtomicBool,
    pub capture_running: AtomicBool,
    pub quitting: Arc<AtomicBool>,
    /// Set by the `renderer_ready` command instead of guessing with
    /// a fixed sleep.
    pub renderer_ready: AtomicBool,
    /// Event payload waiting for the renderer to come up (currently only
    /// `shortcut-failed`).
    pub pending_shortcut_failed: Mutex<Option<serde_json::Value>>,
    /// Startup warning (currently only a failed autostart registration) parked
    /// until the renderer can actually show a toast.
    pub pending_notice: Mutex<Option<serde_json::Value>>,
    /// Debounced window geometry. The event handler only stores the
    /// rectangle; a background thread performs at most one disk write per
    /// GEOM_FLUSH_MS.
    pub pending_window_state: Mutex<Option<WindowState>>,
    /// The renderer signals "my debounced save is on disk" through
    /// `flush_complete`, which flips the flag and notifies the condvar.
    pub flush_ack: Arc<(Mutex<bool>, Condvar)>,
    /// The geometry writer parks here instead of polling every 500 ms.
    pub geometry_signal: Arc<(Mutex<bool>, Condvar)>,
    /// Unix-ms stamp written by every capture tick. The watchdog restarts the
    /// watcher when this goes stale, so auto-capture cannot silently stop.
    pub capture_heartbeat: AtomicU64,
    /// Texts whose disk write failed (locked/full disk). Retried on later ticks
    /// instead of being dropped.
    pub pending_captures: Mutex<VecDeque<String>>,
    pub watchdog_running: AtomicBool,
}

/// Lock a mutex without ever panicking.
///
/// A panic while a guard is held poisons the mutex; with `.lock().unwrap()`
/// every later command (including `save_data`) would panic too and the user
/// would silently lose data. The protected values here are all recoverable,
/// so we take the inner value back instead of propagating the poison.
pub fn lock_or_recover<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Stable 64-bit hash of a clipboard entry.
pub fn text_hash(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// Remember a hash, returning `true` when it had already been seen.
pub fn remember_hash(state: &AppState, hash: u64) -> bool {
    let mut seen = lock_or_recover(&state.last_captured);
    if seen.contains(&hash) {
        return true;
    }
    if seen.len() >= RECENT_HASHES {
        seen.pop_front();
    }
    seen.push_back(hash);
    false
}

const WIN_W: f64 = 380.0;
const WIN_H: f64 = 450.0;

/* --------------------------- window helpers --------------------------- */

pub fn hide_main_window(app: &AppHandle) -> bool {
    let Some(win) = app.get_webview_window("main") else { return false };
    // The window is about to go away from the user's point of view, so
    // ask the renderer to write out anything still sitting in its debounce.
    let _ = app.emit("flush-now", ());
    queue_window_state(app);
    flush_window_state(app);
    // Windows: minimize() before hide() returns focus to the previously active
    // app. This only works because the window is `minimizable: true`.
    #[cfg(target_os = "windows")]
    let _ = win.minimize();
    win.hide().is_ok()
}

/// Show + focus the main window (used after a failed auto-paste, and by the tray).
pub fn show_main_window(app: &AppHandle) -> bool {
    let Some(win) = app.get_webview_window("main") else { return false };
    let _ = win.unminimize();
    let shown = win.show().is_ok();
    let _ = win.set_focus();
    shown
}

/// Record the current geometry without touching the disk.
/// Minimized / off-screen rectangles are never queued, so the saved
/// position can no longer be poisoned by the minimize animation.
fn queue_window_state(app: &AppHandle) {
    let Some(win) = app.get_webview_window("main") else { return };
    if win.is_minimized().unwrap_or(false) || !win.is_visible().unwrap_or(true) {
        return;
    }
    let scale = win.scale_factor().unwrap_or(1.0);
    if let Ok(pos) = win.outer_position() {
        let pos = pos.to_logical::<f64>(scale);
        let (x, y) = (pos.x as i32, pos.y as i32);
        // Reject the (-32000, -32000) style rectangle Windows reports while a
        // window is being minimized or is otherwise parked off-screen.
        if x < MIN_WINDOW_COORD || y < MIN_WINDOW_COORD {
            return;
        }
        let state = app.state::<AppState>();
        *lock_or_recover(&state.pending_window_state) = Some(WindowState { x, y });
        // Wake the writer thread; it sleeps indefinitely otherwise.
        let signal = state.geometry_signal.clone();
        let (lock, cvar) = &*signal;
        *lock_or_recover(lock) = true;
        cvar.notify_all();
    }
}

/// Write the queued geometry, if any. Cheap no-op when nothing changed.
fn flush_window_state(app: &AppHandle) {
    let state = app.state::<AppState>();
    let pending = lock_or_recover(&state.pending_window_state).take();
    if let Some(win_state) = pending {
        store::save_window_state(app, &win_state);
    }
}

/// One writer thread instead of a disk write per Moved/Resized event.
/// It parks on a condvar and only wakes when geometry actually changed.
fn start_geometry_writer(app: &AppHandle) {
    let handle = app.clone();
    std::thread::spawn(move || loop {
        let signal = handle.state::<AppState>().geometry_signal.clone();
        let (lock, cvar) = &*signal;
        {
            let mut pending = lock_or_recover(lock);
            while !*pending {
                let (guard, _timeout) = cvar
                    .wait_timeout(pending, Duration::from_secs(5))
                    .unwrap_or_else(|e| e.into_inner());
                pending = guard;
                if handle.state::<AppState>().quitting.load(Ordering::SeqCst) {
                    return;
                }
            }
            *pending = false;
        }
        // On shutdown, skip the coalescing delay and write immediately.
        if handle.state::<AppState>().quitting.load(Ordering::SeqCst) {
            flush_window_state(&handle);
            return;
        }
        // Coalesce a burst of Moved events into a single disk write.
        std::thread::sleep(Duration::from_millis(GEOM_FLUSH_MS));
        flush_window_state(&handle);
    });
}

fn restore_window_state(app: &AppHandle) {
    let Some(win) = app.get_webview_window("main") else { return };
    let Some(state) = store::load_window_state(app) else { return };

    // The window is `resizable: false`, so its size is always the design size.
    // Only the position is persisted; saving/restoring a size was dead code.
    let (saved_w, saved_h) = (WIN_W, WIN_H);

    // Only restore when the saved rectangle intersects a connected monitor.
    let monitors = win.available_monitors().unwrap_or_default();
    let visible = monitors.iter().any(|m| {
        let scale = m.scale_factor();
        let mp = m.position().to_logical::<f64>(scale);
        let ms = m.size().to_logical::<f64>(scale);
        let (l, t, r, b) = (mp.x, mp.y, mp.x + ms.width, mp.y + ms.height);
        let (wl, wt, wr, wb) = (
            state.x as f64,
            state.y as f64,
            state.x as f64 + saved_w,
            state.y as f64 + saved_h,
        );
        wl < r && wr > l && wt < b && wb > t
    });

    if visible {
        let _ = win.set_position(LogicalPosition::new(state.x as f64, state.y as f64));
    }
}

fn toggle_window(app: &AppHandle) {
    let Some(win) = app.get_webview_window("main") else { return };
    let visible = win.is_visible().unwrap_or(false);
    let focused = win.is_focused().unwrap_or(false);
    if visible && focused {
        hide_main_window(app);
    } else {
        show_main_window(app);
    }
}

/* --------------------------- quit --------------------------- */

/// Graceful shutdown.
///
/// The renderer batches saves with a 200 ms debounce, so it is told to flush
/// first; the wait for its `flush_complete` acknowledgement happens off the UI
/// thread (IPC must keep running) and is capped by QUIT_FLUSH_TIMEOUT_MS so the
/// app still quits when the webview is already gone.
pub fn quit_app(app: &AppHandle) {
    let state = app.state::<AppState>();
    if state.quitting.swap(true, Ordering::SeqCst) {
        return; // a shutdown is already in progress
    }
    let ack = state.flush_ack.clone();
    *lock_or_recover(&ack.0) = false;

    let _ = app.emit("app-will-quit", ());

    // Wake the geometry writer immediately; otherwise it can sit on its
    // condvar for up to 5 s and outlive the app's exit.
    {
        let signal = state.geometry_signal.clone();
        let (lock, cvar) = &*signal;
        *lock_or_recover(lock) = true;
        cvar.notify_all();
    }

    let handle = app.clone();
    std::thread::spawn(move || {
        let (lock, cv) = &*ack;
        let mut done = lock_or_recover(lock);
        let deadline = Duration::from_millis(QUIT_FLUSH_TIMEOUT_MS);
        if !*done {
            let (guard, _timeout) = cv
                .wait_timeout(done, deadline)
                .unwrap_or_else(|p| p.into_inner());
            done = guard;
        }
        drop(done);

        queue_window_state(&handle);
        flush_window_state(&handle);
        let _ = handle.global_shortcut().unregister_all();
        handle.exit(0);
    });
}

/* --------------------------- shortcut helpers --------------------------- */

/// Convert Electron-style accelerators to a Tauri shortcut.
fn parse_accelerator(accel: &str) -> Option<Shortcut> {
    let mut mods = Modifiers::empty();
    let mut key: Option<Code> = None;

    for part in accel.split('+') {
        let p = part.trim();
        match p.to_ascii_lowercase().as_str() {
            "commandorcontrol" | "cmdorctrl" | "ctrl" | "control" | "command" | "cmd" => {
                mods |= Modifiers::CONTROL
            }
            "shift" => mods |= Modifiers::SHIFT,
            "alt" | "option" => mods |= Modifiers::ALT,
            "super" | "meta" | "win" => mods |= Modifiers::SUPER,
            _ => key = code_from_key(p),
        }
    }

    key.map(|k| Shortcut::new(Some(mods), k))
}

fn code_from_key(key: &str) -> Option<Code> {
    let upper = key.to_ascii_uppercase();
    let code = match upper.as_str() {
        "A" => Code::KeyA, "B" => Code::KeyB, "C" => Code::KeyC, "D" => Code::KeyD,
        "E" => Code::KeyE, "F" => Code::KeyF, "G" => Code::KeyG, "H" => Code::KeyH,
        "I" => Code::KeyI, "J" => Code::KeyJ, "K" => Code::KeyK, "L" => Code::KeyL,
        "M" => Code::KeyM, "N" => Code::KeyN, "O" => Code::KeyO, "P" => Code::KeyP,
        "Q" => Code::KeyQ, "R" => Code::KeyR, "S" => Code::KeyS, "T" => Code::KeyT,
        "U" => Code::KeyU, "V" => Code::KeyV, "W" => Code::KeyW, "X" => Code::KeyX,
        "Y" => Code::KeyY, "Z" => Code::KeyZ,
        "0" => Code::Digit0, "1" => Code::Digit1, "2" => Code::Digit2, "3" => Code::Digit3,
        "4" => Code::Digit4, "5" => Code::Digit5, "6" => Code::Digit6, "7" => Code::Digit7,
        "8" => Code::Digit8, "9" => Code::Digit9,
        "F1" => Code::F1, "F2" => Code::F2, "F3" => Code::F3, "F4" => Code::F4,
        "F5" => Code::F5, "F6" => Code::F6, "F7" => Code::F7, "F8" => Code::F8,
        "F9" => Code::F9, "F10" => Code::F10, "F11" => Code::F11, "F12" => Code::F12,
        "SPACE" => Code::Space, "TAB" => Code::Tab, "ESC" | "ESCAPE" => Code::Escape,
        "INSERT" => Code::Insert, "DELETE" => Code::Delete, "HOME" => Code::Home,
        "END" => Code::End, "PAGEUP" => Code::PageUp, "PAGEDOWN" => Code::PageDown,
        _ => return None,
    };
    Some(code)
}

/// Register `accel` without touching any other registration.
pub fn register_shortcut(app: &AppHandle, accel: &str) -> Result<(), String> {
    let shortcut = parse_accelerator(accel).ok_or_else(|| format!("invalid accelerator: {accel}"))?;
    app.global_shortcut()
        .register(shortcut)
        .map_err(|e| e.to_string())
}

/// Register the new accelerator **first** and only release the old one
/// when that succeeded. The previous order (`unregister_all()` then `register()`)
/// left the app with no working shortcut whenever the new one was taken.
pub fn switch_shortcut(app: &AppHandle, new_accel: &str, old_accel: Option<&str>) -> Result<(), String> {
    let shortcut =
        parse_accelerator(new_accel).ok_or_else(|| format!("invalid accelerator: {new_accel}"))?;
    let gs = app.global_shortcut();

    // Same accelerator re-applied: it is already registered, leave it alone.
    if old_accel.map(|o| o == new_accel).unwrap_or(false) {
        return Ok(());
    }

    gs.register(shortcut).map_err(|e| e.to_string())?;

    if let Some(old) = old_accel {
        if old != new_accel && !old.is_empty() {
            if let Some(previous) = parse_accelerator(old) {
                let _ = gs.unregister(previous);
            }
        }
    }
    Ok(())
}

/* --------------------------- autostart --------------------------- */

/// Registers the autostart plugin on Windows and is a no-op elsewhere, so the
/// builder chain stays free of #[cfg] blocks.
trait AutostartPlugin: Sized {
    fn plugin_windows_autostart(self) -> Self;
}

impl<R: tauri::Runtime> AutostartPlugin for tauri::Builder<R> {
    #[cfg(windows)]
    fn plugin_windows_autostart(self) -> Self {
        self.plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            // The registry entry launches the app with this flag, which is how
            // `started_hidden()` knows to keep the window in the tray on boot.
            Some(vec![AUTOSTART_ARG]),
        ))
    }

    #[cfg(not(windows))]
    fn plugin_windows_autostart(self) -> Self {
        self
    }
}

/// Argument Windows passes when the app is launched by the Run key.
pub const AUTOSTART_ARG: &str = "--autostart";

/// True when this process was started by Windows at logon. Such a launch must
/// stay in the tray instead of popping the window in the user's face.
pub fn started_hidden() -> bool {
    std::env::args().any(|a| a == AUTOSTART_ARG)
}

/// Write the Run-key entry. Errors are returned instead of swallowed: a failed
/// registration used to leave the toggle "on" while nothing was registered.
#[cfg(windows)]
pub fn apply_autostart(app: &AppHandle, enabled: bool) -> Result<(), String> {
    use tauri_plugin_autostart::ManagerExt;
    let manager = app.autolaunch();
    let result = if enabled { manager.enable() } else { manager.disable() };
    result.map_err(|e| e.to_string())
}

/// Real state of the Run-key entry, not the value in settings.json.
/// `None` when it cannot be determined.
#[cfg(windows)]
pub fn autostart_enabled(app: &AppHandle) -> Option<bool> {
    use tauri_plugin_autostart::ManagerExt;
    app.autolaunch().is_enabled().ok()
}

/// The autostart plugin ships only on Windows, the single platform the setting
/// is reachable from.
#[cfg(not(windows))]
pub fn apply_autostart(_app: &AppHandle, _enabled: bool) -> Result<(), String> {
    Ok(())
}

#[cfg(not(windows))]
pub fn autostart_enabled(_app: &AppHandle) -> Option<bool> {
    None
}

/// Flip autostart, persist it and keep the tray checkbox in sync.
/// Used by both the tray toggle and the settings dialog.
pub fn set_autostart(app: &AppHandle, enabled: bool) -> Result<(), String> {
    let outcome = apply_autostart(app, enabled);
    let effective = if outcome.is_ok() { enabled } else { autostart_enabled(app).unwrap_or(false) };
    let mut settings = store::load_settings(app);
    settings.auto_start = effective;
    store::persist_settings(app, &settings);
    sync_tray_autostart(effective);
    outcome
}

/* --------------------------- app --------------------------- */

/// The tray checkboxes are created once and then updated from wherever the
/// setting changes (tray click, settings dialog, startup repair).
static TRAY_AUTOSTART: OnceLock<CheckMenuItem<tauri::Wry>> = OnceLock::new();
static TRAY_AUTOCAPTURE: OnceLock<CheckMenuItem<tauri::Wry>> = OnceLock::new();

pub fn sync_tray_autostart(enabled: bool) {
    if let Some(item) = TRAY_AUTOSTART.get() {
        let _ = item.set_checked(enabled);
    }
}

pub fn sync_tray_autocapture(enabled: bool) {
    if let Some(item) = TRAY_AUTOCAPTURE.get() {
        let _ = item.set_checked(enabled);
    }
}

/// Tell the renderer that a setting changed outside the dialog, so an open
/// Settings modal never shows a stale checkbox.
pub fn emit_settings_changed(app: &AppHandle) {
    let state = app.state::<AppState>();
    let payload = commands::settings_payload(app, &state);
    let _ = app.emit("settings-changed", payload);
}

fn build_tray(app: &AppHandle) -> tauri::Result<()> {
    let settings = load_settings(app);
    // The registry is the truth for autostart; settings.json can be stale after
    // an uninstall/reinstall or a manual registry edit.
    let autostart_on = autostart_enabled(app).unwrap_or(settings.auto_start);

    let open = MenuItem::with_id(app, "open", "Open Mehedi Clipboard", true, None::<&str>)?;
    let autostart = CheckMenuItem::with_id(
        app,
        "autostart",
        "Start with Windows",
        cfg!(windows),
        autostart_on,
        None::<&str>,
    )?;
    let autocapture = CheckMenuItem::with_id(
        app,
        "autocapture",
        "Auto capture clipboard",
        true,
        settings.auto_capture,
        None::<&str>,
    )?;
    // Two distinct separator items: the same menu item cannot appear twice.
    let sep_top = PredefinedMenuItem::separator(app)?;
    let sep_bottom = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[&open, &sep_top, &autostart, &autocapture, &sep_bottom, &quit],
    )?;

    let _ = TRAY_AUTOSTART.set(autostart);
    let _ = TRAY_AUTOCAPTURE.set(autocapture);

    let icon = app
        .default_window_icon()
        .cloned()
        .ok_or_else(|| tauri::Error::AssetNotFound("default window icon".into()))?;

    TrayIconBuilder::with_id("main-tray")
        .icon(icon)
        .tooltip("Mehedi Clipboard")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "open" => {
                show_main_window(app);
            }
            // Toggle autostart straight from the tray. The checkbox is only
            // left "on" when the Run key really was written.
            "autostart" => {
                let next = !autostart_enabled(app).unwrap_or(load_settings(app).auto_start);
                match set_autostart(app, next) {
                    Ok(()) => notify(app, if next {
                        "Mehedi Clipboard will start with Windows"
                    } else {
                        "Autostart turned off"
                    }, false),
                    Err(e) => {
                        eprintln!("autostart change failed: {e}");
                        notify(app, "Couldn't change the Windows autostart setting", true);
                    }
                }
                emit_settings_changed(app);
            }
            "autocapture" => {
                let mut settings = load_settings(app);
                settings.auto_capture = !settings.auto_capture;
                let enabled = settings.auto_capture;
                store::persist_settings(app, &settings);
                app.state::<AppState>().auto_capture.store(enabled, Ordering::SeqCst);
                if enabled {
                    start_capture_thread(app);
                    capture_current(app);
                }
                sync_tray_autocapture(enabled);
                emit_settings_changed(app);
            }
            // Graceful, flush-aware shutdown instead of a bare exit(0).
            "quit" => quit_app(app),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                toggle_window(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

/// Show a message in the renderer, or park it until the renderer is ready.
/// Silent failures were exactly why a broken autostart went unnoticed.
pub fn notify(app: &AppHandle, message: &str, is_error: bool) {
    let payload = serde_json::json!({ "message": message, "error": is_error });
    let state = app.state::<AppState>();
    if state.renderer_ready.load(Ordering::SeqCst) {
        let _ = app.emit("notice", payload);
    } else {
        *lock_or_recover(&state.pending_notice) = Some(payload);
    }
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            // Launching the app again must always bring it up, never hide it.
            show_main_window(app);
        }))
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new()
            .with_handler(|app, _shortcut, event| {
                if event.state() == tauri_plugin_global_shortcut::ShortcutState::Pressed {
                    toggle_window(app);
                }
            })
            .build())
        .plugin_windows_autostart()
        .manage(AppState {
            last_captured: Mutex::new(VecDeque::with_capacity(RECENT_HASHES)),
            active_shortcut: Mutex::new(String::new()),
            auto_capture: AtomicBool::new(true),
            capture_running: AtomicBool::new(false),
            quitting: Arc::new(AtomicBool::new(false)),
            renderer_ready: AtomicBool::new(false),
            pending_shortcut_failed: Mutex::new(None),
            pending_notice: Mutex::new(None),
            pending_window_state: Mutex::new(None),
            flush_ack: Arc::new((Mutex::new(false), Condvar::new())),
            geometry_signal: Arc::new((Mutex::new(false), Condvar::new())),
            capture_heartbeat: AtomicU64::new(0),
            pending_captures: Mutex::new(VecDeque::new()),
            watchdog_running: AtomicBool::new(false),
        })
        .setup(|app| {
            let handle = app.handle().clone();
            migrate::migrate_if_needed(&handle);
            // Clear stale .tmp files left behind by a crash mid-write.
            store::sweep_temp_files(&handle);

            let settings = load_settings(&handle);
            app.state::<AppState>()
                .auto_capture
                .store(settings.auto_capture, Ordering::SeqCst);

            restore_window_state(&handle);
            start_geometry_writer(&handle);
            // A missing/locked tray icon must not stop the app from starting:
            // the window and the global shortcut still work without a tray.
            if let Err(e) = build_tray(&handle) {
                eprintln!("tray icon unavailable: {e}");
            }

            // Global shortcut with fallback to the default accelerator.
            let gs = handle.global_shortcut();
            let _ = gs.unregister_all();
            let mut ok = register_shortcut(&handle, &settings.shortcut).is_ok();
            let mut active = settings.shortcut.clone();
            if !ok {
                ok = register_shortcut(&handle, DEFAULT_SHORTCUT).is_ok();
                active = if ok { DEFAULT_SHORTCUT.to_string() } else { String::new() };
            }
            *lock_or_recover(&app.state::<AppState>().active_shortcut) = active.clone();

            // Re-apply the saved autostart preference and surface a failure
            // instead of swallowing it, so "it never starts with Windows" is
            // visible the first time it happens.
            if let Err(e) = apply_autostart(&handle, settings.auto_start) {
                eprintln!("autostart could not be applied: {e}");
                if settings.auto_start {
                    let payload = serde_json::json!({
                        "message": "Couldn't register Mehedi Clipboard to start with Windows",
                        "error": true
                    });
                    *lock_or_recover(&app.state::<AppState>().pending_notice) = Some(payload);
                }
            }
            // Keep settings.json aligned with what the registry actually holds.
            if let Some(real) = autostart_enabled(&handle) {
                if real != settings.auto_start {
                    let mut fixed = settings.clone();
                    fixed.auto_start = real;
                    store::persist_settings(&handle, &fixed);
                }
                sync_tray_autostart(real);
            }

            // The capture thread must run even when the window never opens:
            // a silent tray session still records everything that is copied.
            if settings.auto_capture {
                start_capture_thread(&handle);
                // Anything copied before this session started (or between two
                // sessions) is picked up once, so nothing is missing.
                let seed = handle.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(400));
                    capture_current(&seed);
                });
            }
            // The supervisor runs for the whole session: it restarts the
            // watcher if it ever dies or stalls, and retries parked writes.
            start_capture_watchdog(&handle);

            // Launched by Windows at logon: stay in the tray.
            if let Some(win) = app.get_webview_window("main") {
                if started_hidden() {
                    let _ = win.hide();
                } else {
                    let _ = win.show();
                    let _ = win.set_focus();
                }
            }

            if !ok {
                // No throw-away thread and no fixed 600 ms guess. The
                // payload is parked until the renderer calls `renderer_ready`.
                let payload = serde_json::json!({ "shortcut": settings.shortcut, "active": active });
                *lock_or_recover(&app.state::<AppState>().pending_shortcut_failed) = Some(payload);
            }

            Ok(())
        })
        .on_window_event(|window, event| match event {
            // Close = hide. The app only quits from the tray.
            WindowEvent::CloseRequested { api, .. } => {
                api.prevent_close();
                hide_main_window(window.app_handle());
            }
            // Only remember the rectangle here; the writer thread persists
            // it at most twice per second instead of on every drag tick.
            WindowEvent::Moved(_) | WindowEvent::Resized(_) => {
                queue_window_state(window.app_handle());
            }
            // Losing focus is the last reliable moment before the user
            // switches away, so ask the renderer to flush its debounce.
            WindowEvent::Focused(false) => {
                let _ = window.app_handle().emit("flush-now", ());
            }
            _ => {}
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_data,
            commands::save_data,
            commands::copy_text,
            commands::read_clipboard,
            commands::hide_window,
            commands::paste_to_active,
            commands::export_file,
            commands::get_settings,
            commands::set_settings,
            commands::renderer_ready,
            commands::flush_complete,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Mehedi Clipboard");
}

fn main() {
    run();
}
