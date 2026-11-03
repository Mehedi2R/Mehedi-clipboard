use std::time::Duration;

/// Upper bound for waiting until Windows has moved focus away from our own
/// window. The old code slept a fixed 60 ms and then fired blindly: on a loaded
/// machine the keystroke landed in the (still focused) clipboard window and the
/// paste was silently lost.
#[cfg(target_os = "windows")]
const FOCUS_WAIT_MS: u64 = 400;
#[cfg(target_os = "windows")]
const FOCUS_POLL_MS: u64 = 10;
/// Small settle time after focus really changed, before injecting.
#[cfg(target_os = "windows")]
const FOCUS_SETTLE_MS: u64 = 25;
#[cfg(target_os = "windows")]
const FALLBACK_SETTLE_MS: u64 = 160;
/// How long we wait for physically held modifiers to be released.
#[cfg(target_os = "windows")]
const MODIFIER_WAIT_MS: u64 = 250;

/// Send a system-wide Ctrl+V keystroke to whatever window now has focus.
///
/// `self_hwnd` is the app's own window handle; the injection waits until the
/// foreground window is *not* us any more, so the keystroke can never be eaten
/// by the clipboard window itself.
///
/// Fast path: Win32 `SendInput` — in-process, no child process, ~0 ms of
/// start-up cost (the old PowerShell helper cost roughly 200-400 ms per paste).
/// If `SendInput` is refused (UIPI / an elevated foreground window), the old
/// PowerShell helper is used as a fallback.
///
/// Returns `Err(reason)` when the keystroke could not be delivered, so the UI can
/// tell the user that the text is still sitting on the clipboard instead of
/// silently pretending the paste worked.
pub fn send_ctrl_v(self_hwnd: Option<isize>) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        if !wait_for_foreground_change(self_hwnd) {
            return Err(
                "focus did not return to the previous window — the text is on the clipboard".into(),
            );
        }
        std::thread::sleep(Duration::from_millis(FOCUS_SETTLE_MS));

        // The global shortcut (Ctrl+Shift+V) is often still physically
        // held when the user clicks a row. SendInput adds Ctrl+V *on top of*
        // the live modifier state, so the target app would receive
        // Ctrl+Shift+V. Release whatever is still down first.
        let forced = release_stuck_modifiers();

        let result = match send_input_ctrl_v() {
            Ok(()) => Ok(()),
            Err(fast_err) => {
                // SendInput was blocked - give the slow helper one chance.
                std::thread::sleep(Duration::from_millis(FALLBACK_SETTLE_MS));
                powershell_ctrl_v().map_err(|slow_err| format!("{fast_err}; {slow_err}"))
            }
        };

        // Put back the down-state of modifiers the user is still
        // physically holding, so their eventual real key-up is not a duplicate
        // (games / RDP sessions mis-handle the stray second key-up).
        restore_forced_modifiers(&forced);
        result
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = (self_hwnd, Duration::from_millis(0));
        Err("auto-paste is only supported on Windows".to_string())
    }
}

/* --------------------------- focus + modifiers --------------------------- */

/// Poll until the foreground window differs from our own.
/// Returns `false` when the app still owns focus after FOCUS_WAIT_MS.
#[cfg(target_os = "windows")]
fn wait_for_foreground_change(self_hwnd: Option<isize>) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

    let Some(own) = self_hwnd else {
        // Unknown handle: fall back to a short fixed settle time.
        std::thread::sleep(Duration::from_millis(80));
        return true;
    };

    let mut waited = 0;
    while waited < FOCUS_WAIT_MS {
        let fg = unsafe { GetForegroundWindow() }.0 as isize;
        if fg != own && fg != 0 {
            return true;
        }
        std::thread::sleep(Duration::from_millis(FOCUS_POLL_MS));
        waited += FOCUS_POLL_MS;
    }
    false
}

/// Release Shift / Alt / Win / Ctrl if they are still physically down.
/// Returns the keys that were force-released so they can be restored.
///
/// Waits briefly for a natural release first — most users let go within a few
/// dozen milliseconds — and only then forces key-up events so the injected
/// Ctrl+V is not turned into Ctrl+Shift+V or Ctrl+Alt+V by the target app.
#[cfg(target_os = "windows")]
fn release_stuck_modifiers() -> Vec<windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, VIRTUAL_KEY, VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_RCONTROL,
        VK_RMENU, VK_RSHIFT, VK_RWIN,
    };

    const MODIFIERS: [VIRTUAL_KEY; 8] = [
        VK_LSHIFT, VK_RSHIFT, VK_LMENU, VK_RMENU, VK_LWIN, VK_RWIN, VK_LCONTROL, VK_RCONTROL,
    ];

    let down = |vk: VIRTUAL_KEY| unsafe { (GetAsyncKeyState(vk.0 as i32) as u16 & 0x8000) != 0 };

    // Give the user a moment to let go on their own.
    let mut waited = 0;
    while waited < MODIFIER_WAIT_MS && MODIFIERS.iter().any(|vk| down(*vk)) {
        std::thread::sleep(Duration::from_millis(FOCUS_POLL_MS));
        waited += FOCUS_POLL_MS;
    }

    // Anything still held gets an explicit key-up so the state is clean.
    let stuck: Vec<VIRTUAL_KEY> = MODIFIERS.iter().copied().filter(|vk| down(*vk)).collect();
    if stuck.is_empty() {
        return stuck;
    }
    send_keys(&stuck.iter().map(|vk| (*vk, true)).collect::<Vec<_>>());
    stuck
}

/// Re-assert the down-state of the modifiers we force-released, but only
/// for the ones the user is *still* physically holding.
#[cfg(target_os = "windows")]
fn restore_forced_modifiers(
    forced: &[windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY],
) {
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VIRTUAL_KEY};
    if forced.is_empty() {
        return;
    }
    let down = |vk: VIRTUAL_KEY| unsafe { (GetAsyncKeyState(vk.0 as i32) as u16 & 0x8000) != 0 };
    let still: Vec<(VIRTUAL_KEY, bool)> =
        forced.iter().copied().filter(|vk| down(*vk)).map(|vk| (vk, false)).collect();
    if !still.is_empty() {
        send_keys(&still);
    }
}

/* ------------------------------ fast path ------------------------------ */

#[cfg(target_os = "windows")]
fn make_input(
    vk: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY,
    up: bool,
) -> windows::Win32::UI::Input::KeyboardAndMouse::INPUT {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
    };
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// Send a batch of (key, is_key_up) events. Returns how many were accepted.
#[cfg(target_os = "windows")]
fn send_keys(keys: &[(windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY, bool)]) -> usize {
    use windows::Win32::UI::Input::KeyboardAndMouse::{SendInput, INPUT};
    let inputs: Vec<INPUT> = keys.iter().map(|(vk, up)| make_input(*vk, *up)).collect();
    unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) as usize }
}

#[cfg(target_os = "windows")]
fn send_input_ctrl_v() -> Result<(), String> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{VK_CONTROL, VK_V};

    let sequence = [
        (VK_CONTROL, false),
        (VK_V, false),
        (VK_V, true),
        (VK_CONTROL, true),
    ];

    let sent = send_keys(&sequence);
    if sent == sequence.len() {
        Ok(())
    } else {
        Err(format!(
            "SendInput delivered {sent}/{} keystrokes (blocked by the foreground window?)",
            sequence.len()
        ))
    }
}

/* ---------------------------- fallback path ---------------------------- */

#[cfg(target_os = "windows")]
fn powershell_ctrl_v() -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    use std::process::Command;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    // try/catch + explicit exit code: a throwing SendKeys must not report success
    const SCRIPT: &str = "$ErrorActionPreference='Stop'; \
try { Add-Type -AssemblyName System.Windows.Forms; \
[System.Windows.Forms.SendKeys]::SendWait('^v'); exit 0 } \
catch { [Console]::Error.Write($_.Exception.Message); exit 1 }";

    // The fallback must never hang the UI thread: powershell.exe is spawned and
    // killed if it has not finished within FALLBACK_TIMEOUT.
    const FALLBACK_TIMEOUT: Duration = Duration::from_secs(5);

    let mut child = match Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-WindowStyle",
            "Hidden",
            "-Command",
            SCRIPT,
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return Err(format!("powershell.exe could not be started: {e}")),
    };

    let deadline = std::time::Instant::now() + FALLBACK_TIMEOUT;
    let output = loop {
        match child.try_wait() {
            Ok(Some(_)) => break child.wait_with_output(),
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("keystroke helper timed out".to_string());
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => return Err(format!("keystroke helper failed: {e}")),
        }
    };

    match output {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => {
            let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
            Err(if err.is_empty() {
                format!("keystroke helper exited with {}", out.status)
            } else {
                err
            })
        }
        Err(e) => Err(format!("powershell.exe could not be started: {e}")),
    }
}
