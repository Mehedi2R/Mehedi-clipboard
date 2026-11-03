//! Auto-capture: the clipboard watcher that must never lose a copy.
//!
//! The file is organised top-down: constants, small helpers (time, clipboard,
//! renderer), the write path, the poll tick, and finally the two threads
//! (watcher + watchdog).
//!
//! Design rules — each one exists because of a way the old loop could drop a row
//! ---------------------------------------------------------------------------
//! 1. **Rust owns the pipeline.** Every row is written by
//!    `store::capture_text_outcome`, never by the renderer, so a busy webview,
//!    a closing window or a JS error can no longer swallow a capture. The
//!    renderer is only *told* ("history-updated") to reload.
//! 2. **The sequence number is committed only after a successful read.** Windows
//!    bumps the clipboard sequence while the copying app still owns the
//!    clipboard, so `read_text()` fails for a few milliseconds. Retry with
//!    backoff, and move the cursor only once the content was really read (or is
//!    provably not text).
//! 3. **Failed disk writes are queued, not dropped** (`pending_captures`).
//! 4. **The watcher is supervised**: every tick stamps a heartbeat, and the
//!    watchdog restarts the watcher if that stamp goes stale.
//! 5. **Panics can't kill the loop**: the tick body runs in `catch_unwind`, so
//!    one bad tick costs one tick, not the feature.
//! 6. **Startup captures what is already on the clipboard.**

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_clipboard_manager::ClipboardExt;

use crate::limits;
use crate::store::{self, CaptureOutcome};
use crate::{lock_or_recover, remember_hash, text_hash, AppState};

// ---------------------------------------------------------------- tuning knobs

/// Clipboard poll interval. A tick is a single sequence-number syscall on
/// Windows, so it can be short and still cost almost nothing.
const POLL_MS: u64 = 300;

/// A fresh sequence number can arrive while the copying app still holds the
/// clipboard open: how hard we try before giving up on this tick.
const READ_ATTEMPTS: u32 = 12;
const READ_BACKOFF_MS: u64 = 25;

/// How many failed-write texts may wait for a retry. Small on purpose: this is
/// a disk-trouble buffer, not a second history.
const MAX_PENDING: usize = 16;

/// Watchdog cadence and the staleness that counts as "the watcher is gone".
const WATCHDOG_TICK_MS: u64 = 2_000;
const HEARTBEAT_STALE_MS: u64 = 6_000;

/// A live renderer may hold unsaved edits in its 200 ms debounce; it is asked to
/// flush before the backend writes, so a capture can't clobber user edits.
const RENDERER_FLUSH_WAIT_MS: u64 = 350;

// -------------------------------------------------------------- small helpers

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Windows clipboard sequence number; `None` elsewhere (the poller then reads
/// the clipboard on every tick and relies on the hash check for deduping).
fn clipboard_sequence() -> Option<u32> {
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::System::DataExchange::GetClipboardSequenceNumber;
        return Some(unsafe { GetClipboardSequenceNumber() });
    }
    #[cfg(not(target_os = "windows"))]
    {
        None
    }
}

/// What one clipboard read attempt concluded.
enum ClipboardRead {
    /// Text was read successfully.
    Text(String),
    /// The clipboard holds no text (image, file list, or it was cleared).
    NotText,
    /// Still unreadable after every attempt — the caller must NOT advance its
    /// cursor, so the same change is retried on the next tick.
    Busy,
}

/// `true` when the error means "there is no text here", which is a final answer
/// rather than the busy-clipboard race.
fn is_no_text_error(msg: &str) -> bool {
    msg.contains("not available")
        || msg.contains("no text")
        || msg.contains("unsupported")
        || msg.contains("empty")
}

/// Read clipboard text, retrying while the previous owner still has it locked.
fn read_clipboard_text(app: &AppHandle) -> ClipboardRead {
    let mut last_err = None;
    for attempt in 0..READ_ATTEMPTS {
        match app.clipboard().read_text() {
            Ok(text) => return ClipboardRead::Text(text),
            Err(e) => {
                let msg = e.to_string().to_ascii_lowercase();
                if is_no_text_error(&msg) {
                    return ClipboardRead::NotText;
                }
                last_err = Some(msg);
                std::thread::sleep(Duration::from_millis(
                    READ_BACKOFF_MS * (attempt as u64 / 4 + 1),
                ));
            }
        }
    }
    if let Some(msg) = last_err {
        eprintln!("clipboard read failed after {READ_ATTEMPTS} attempts: {msg}");
    }
    ClipboardRead::Busy
}

/// Text we would never store: empty, or bigger than one history item may be.
fn is_capturable(text: &str) -> bool {
    !text.is_empty() && text.len() <= limits::MAX_ITEM_BYTES
}

/// Ask a live renderer to write out its debounced save and wait briefly for the
/// acknowledgement, so the backend's own write cannot overwrite pending edits.
fn flush_renderer(app: &AppHandle) {
    let state = app.state::<AppState>();
    if !state.renderer_ready.load(Ordering::SeqCst) {
        return;
    }
    let ack = state.flush_ack.clone();
    *lock_or_recover(&ack.0) = false;
    let _ = app.emit("flush-now", ());

    let (lock, cv) = &*ack;
    let done = lock_or_recover(lock);
    if !*done {
        let _ = cv
            .wait_timeout(done, Duration::from_millis(RENDERER_FLUSH_WAIT_MS))
            .unwrap_or_else(|p| p.into_inner());
    }
}

// ----------------------------------------------------------------- write path

/// Hand one text to the store and notify the UI when a row appeared.
/// Returns `false` only when the write itself failed (worth a retry).
fn write_through(app: &AppHandle, text: String) -> bool {
    match store::capture_text_outcome(app, text) {
        CaptureOutcome::Stored => {
            let _ = app.emit("history-updated", ());
            true
        }
        CaptureOutcome::Duplicate | CaptureOutcome::Rejected => true,
        CaptureOutcome::SaveFailed => false,
    }
}

/// Park a text whose write failed, dropping the oldest if the buffer is full.
fn park_for_retry(app: &AppHandle, text: String) {
    let state = app.state::<AppState>();
    let mut queue = lock_or_recover(&state.pending_captures);
    if queue.len() >= MAX_PENDING {
        queue.pop_front();
    }
    queue.push_back(text);
}

/// Store one captured text: flush the renderer first, then write, then queue a
/// retry (and tell the user) if the disk refused it.
fn commit(app: &AppHandle, text: String) {
    flush_renderer(app);
    if write_through(app, text.clone()) {
        return;
    }
    park_for_retry(app, text);
    let _ = app.emit(
        "notice",
        serde_json::json!({
            "message": "Couldn't save the copied item yet — retrying",
            "error": true
        }),
    );
}

/// Retry everything parked by a failed write, oldest first, so history order
/// survives a disk hiccup. Stops at the first text that still can't be written.
fn drain_pending(app: &AppHandle) {
    let state = app.state::<AppState>();
    while let Some(text) = lock_or_recover(&state.pending_captures).pop_front() {
        if !write_through(app, text.clone()) {
            lock_or_recover(&state.pending_captures).push_front(text);
            return;
        }
    }
}

/// Capture whatever is on the clipboard right now (startup, or auto-capture
/// switched on), so content copied while the app was not watching is kept too.
pub fn capture_current(app: &AppHandle) {
    let ClipboardRead::Text(text) = read_clipboard_text(app) else { return };
    if !is_capturable(&text) {
        return;
    }
    if remember_hash(&app.state::<AppState>(), text_hash(&text)) {
        return; // already in history
    }
    commit(app, text);
}

// ------------------------------------------------------------------ poll tick

/// One poll tick. `false` means the watcher should stop (quitting, or
/// auto-capture switched off).
fn tick(app: &AppHandle, last_seq: &mut Option<u32>) -> bool {
    let state = app.state::<AppState>();
    if state.quitting.load(Ordering::SeqCst) || !state.auto_capture.load(Ordering::SeqCst) {
        return false;
    }
    state.capture_heartbeat.store(now_ms(), Ordering::SeqCst);

    // A previous write failed: clearing that backlog comes first.
    drain_pending(app);

    // Cheap change detection (Windows only); elsewhere `seq` is None and every
    // tick reads the clipboard.
    let seq = clipboard_sequence();
    if let (Some(prev), Some(now)) = (*last_seq, seq) {
        if prev == now {
            return true;
        }
    }

    // The cursor is advanced only when the clipboard was really readable, so a
    // busy clipboard retries next tick instead of losing the copy.
    let text = match read_clipboard_text(app) {
        ClipboardRead::Text(text) => {
            *last_seq = seq;
            text
        }
        ClipboardRead::NotText => {
            *last_seq = seq;
            return true;
        }
        ClipboardRead::Busy => return true,
    };

    if !is_capturable(&text) {
        return true;
    }
    if remember_hash(&state, text_hash(&text)) {
        return true; // same content we already have
    }

    commit(app, text);
    true
}

// -------------------------------------------------------------------- threads

/// Start the watcher. Safe to call repeatedly: only one watcher ever runs.
pub fn start_capture_thread(app: &AppHandle) {
    let state = app.state::<AppState>();
    if state.capture_running.swap(true, Ordering::SeqCst) {
        return;
    }
    state.capture_heartbeat.store(now_ms(), Ordering::SeqCst);

    let handle = app.clone();
    std::thread::spawn(move || {
        let mut last_seq = clipboard_sequence();
        loop {
            std::thread::sleep(Duration::from_millis(POLL_MS));

            // A panic inside one tick must cost that tick only: without this the
            // thread would die and every later copy would be missing for the
            // rest of the session.
            let keep_going = catch_unwind(AssertUnwindSafe(|| tick(&handle, &mut last_seq)))
                .unwrap_or_else(|_| {
                    eprintln!("clipboard capture tick panicked — continuing");
                    true
                });
            if keep_going {
                continue;
            }

            // Clear the flag before leaving, then re-check: if auto-capture was
            // switched back on in that window, the caller's `swap(true)`
            // returned `true` and nobody would be watching. Restart here.
            let state = handle.state::<AppState>();
            state.capture_running.store(false, Ordering::SeqCst);
            if !state.quitting.load(Ordering::SeqCst) && state.auto_capture.load(Ordering::SeqCst) {
                start_capture_thread(&handle);
            }
            break;
        }
    });
}

/// Does the watcher need to be (re)started right now?
fn watcher_needs_restart(state: &AppState) -> bool {
    if !state.capture_running.load(Ordering::SeqCst) {
        return true;
    }
    let beat = state.capture_heartbeat.load(Ordering::SeqCst);
    beat == 0 || now_ms().saturating_sub(beat) > HEARTBEAT_STALE_MS
}

/// Supervisor: restarts the watcher whenever it is gone or its heartbeat goes
/// stale while auto-capture is on. This is the last line of defence — without
/// it, a thread that dies for any reason leaves `capture_running = true` and the
/// app stops recording copies until the next restart.
pub fn start_capture_watchdog(app: &AppHandle) {
    let state = app.state::<AppState>();
    if state.watchdog_running.swap(true, Ordering::SeqCst) {
        return;
    }
    let handle = app.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(WATCHDOG_TICK_MS));
        let state = handle.state::<AppState>();

        if state.quitting.load(Ordering::SeqCst) {
            state.watchdog_running.store(false, Ordering::SeqCst);
            break;
        }
        if !state.auto_capture.load(Ordering::SeqCst) {
            continue;
        }

        if watcher_needs_restart(&state) {
            if state.capture_running.load(Ordering::SeqCst) {
                eprintln!("clipboard capture stalled — restarting the watcher");
                state.capture_running.store(false, Ordering::SeqCst);
            }
            start_capture_thread(&handle);
        }

        // Retry parked writes even if the watcher itself is wedged.
        drain_pending(&handle);
    });
}
