# Mehedi Clipboard — Changelog

Newest release first. This file replaces the old `CHANGES.md`, `FIXES.md`,
`BUGFIXES-v3.1.0.md` and `BUGFIXES-v3.1.1.md` (all merged here).
Current status board: `AUDIT-FIXES.md`. Current audit: `CODE-AUDIT-v3.1.2.md`.

---

## v3.3.2 — auto-capture that never misses a copy

New file: `src-tauri/src/capture.rs` (the watcher moved out of `main.rs`).

1. **Busy clipboard no longer loses the copy** — Windows bumps the clipboard
   sequence number while the copying app still owns the clipboard, so
   `read_text()` fails for a few ms. The read is now retried (12 attempts with
   backoff) and the sequence cursor advances **only after a successful read**.
2. **Rust always owns the pipeline** — every captured row is written by
   `store::capture_text_outcome`; the renderer is only told `history-updated`.
   A busy/closing/erroring webview can no longer swallow a capture.
3. **Failed disk writes are queued, not dropped** — `pending_captures` holds up
   to 16 texts and every later tick (and the watchdog) retries them; the user
   gets a "retrying" notice instead of silent loss.
4. **Watchdog supervision** — each tick stamps `capture_heartbeat`; a
   supervisor thread restarts the watcher when the stamp goes stale or the flag
   is clear, so capture can never stop silently for the rest of a session.
5. **Panic-proof ticks** — the tick body runs in `catch_unwind`; one bad tick
   costs one tick, not the feature.
6. **Nothing lost at the edges** — the clipboard is captured once at startup and
   whenever auto-capture is switched back on (tray or Settings).
7. **No clobbering user edits** — before writing, the backend asks a live
   renderer to flush its 200 ms debounce and waits up to 350 ms for the ack.
8. Poll interval 500 ms -> 300 ms (still one syscall per tick).

---

## v3.3.1 — single installation, always upgrade in place

1. **No more duplicate installs** (`src-tauri/nsis/hooks.nsh`, `tauri.conf.json`)
   The NSIS installer now runs a pre-install hook that closes a running
   `Mehedi Clipboard.exe`, finds any previous installation (per-user *and*
   machine-wide registry entries) and silently uninstalls it with `/P` before
   copying the new files. Result: reinstalling the same version replaces it,
   a newer version upgrades over the old one, and "Apps & features" never
   shows two entries.

2. **User data survives the upgrade**
   The old uninstaller is called with `/P` (preserve), so clipboard history,
   pins and settings stay intact across versions.

3. **Uninstall cleans up autostart**
   Removing the app now also deletes its `Run` registry value.

---

## v3.3.0 — reliable Windows autostart + tray-silent capture

### Autostart (bug fix)

1. **Tray toggle "Start with Windows"** (`src-tauri/src/main.rs`)
   The tray menu now has a checkbox for autostart (and one for auto capture),
   so the setting no longer lives only inside the Settings dialog.

2. **Failures are reported, never swallowed** (`main.rs`, `commands.rs`)
   `apply_autostart` returns a `Result`. A refused Run-key write shows a toast
   ("Windows refused the autostart change"), the toggle snaps back, and the
   rejected preference is not persisted. A failure during startup is parked and
   delivered as soon as the renderer is ready.

3. **The registry is the source of truth** (`commands.rs`)
   `get_settings` reports `autolaunch().is_enabled()` instead of the value in
   `settings.json`, and startup rewrites `settings.json` when the two disagree.

4. **Logon launches start hidden** (`main.rs`)
   The Run key registers the app with `--autostart`; that launch keeps the
   window hidden in the tray instead of opening it on every boot.

### Silent tray operation

5. **Capture pipeline moved into Rust** (`src-tauri/src/store.rs`)
   New `store::capture_text()` performs dedupe (re-copied text is promoted to
   the top, pinned rows untouched), retention (100 unpinned / 25 heavy) and the
   atomic disk write without the webview. While the app sits silently in the
   tray — hidden, throttled, or before the UI is ready — nothing is lost.

6. **Renderer stays in sync** (`main.rs`, `tauri-bridge.js`, `renderer.js`)
   A visible, ready renderer still handles captures itself; otherwise Rust
   stores the row and emits `history-updated`, and the renderer flushes its
   pending save and reloads the store. New `settings-changed` and `notice`
   events keep an open Settings dialog and user-facing warnings accurate.

---

## v3.2.0 — retention rewrite: 100-item cap, 4 MB ceiling, heavy-item budget, virtualization

### Retention & storage limits

1. **Total unpinned item cap — 100** (`src/limits.js`, `src-tauri/src/limits.rs`)
   Previous `MAX_ITEMS = 1000` allowed the store to grow far beyond what the UI
   could render smoothly. The cap is now **100 unpinned items**. Pinned items
   do **not** count toward this limit and are never auto-deleted.

2. **FIFO eviction for unpinned items** (`src-tauri/src/store.rs`)
   When a new unpinned item is inserted and the unpinned count exceeds 100,
   the **oldest unpinned item at the bottom of the list is removed** — one in,
   one out. The count stays fixed at 100. Duplicate detection is preserved:
   if the incoming content already exists, no new row is created; the existing
   item is moved to the top instead.

3. **Per-item hard ceiling — 4 MB** (`src/limits.js`, `src-tauri/src/limits.rs`)
   `MAX_ITEM_BYTES` remains 4 MB and is now enforced as a hard rejection rule.
   Any clipboard payload larger than 4 MB is **not saved**; the UI shows a
   notice/toast explaining that the item was skipped.

4. **Heavy-item budget — 25 items over 256 KB** (`src/limits.js`, `src-tauri/src/limits.rs`)
   Items between 256 KB and 4 MB are classified as "heavy". Only **25 heavy
   items** may exist at once. When a new heavy item arrives and the budget is
   full, the oldest heavy item (unpinned first, then pinned if necessary) is
   evicted before the new one is accepted. This prevents memory pressure from
   large text/image payloads while keeping a useful working set.

### Performance (Windows Clipboard-style)

5. **Preview + lazy loading** (`src/renderer.js`, `src-tauri/src/commands.rs`)
   The list no longer renders full multi-megabyte payloads. Each item stores a
   small preview (truncated text / thumbnail metadata). The UI shows only this
   preview. The complete payload is fetched from the backend on demand via the
   new `get_item_full(id)` command when the user copies, pastes, edits, or
   expands an item — matching the Windows 10/11 Clipboard "fetch on action"
   behavior.

6. **Virtualized list** (`src/renderer.js`)
   Only the items visible in the viewport plus a small buffer are rendered.
   Nodes are recycled on scroll, so even with 100 items in memory the DOM stays
   small (~6–15 rows). This eliminates lag/hang when heavy items are present.

### Migration

7. **One-time trim for existing stores** (`src-tauri/src/migrate.rs`)
   Users upgrading from earlier versions may have more than 100 unpinned items.
   On first run, the migration automatically trims unpinned items from the
   bottom (oldest first) until only 100 remain. Pinned items are untouched.
   No user prompt is shown; the trim is logged and a backup `.bak` is written
   before mutation.

### Verification
- JS-side limit parity test updated for `MAX_ITEMS = 100`, `MAX_HEAVY_ITEMS = 25`,
  `MAX_HEAVY_ITEM_BYTES = 256 KB`, and the 4 MB rejection path.
- Rust not compiled in this environment — run `cargo check` / `cargo test`
  locally.

---

## v3.1.2 — resilience, memory ceiling, data integrity

### P0

1. **Bridge guard — no more blank window** (`src/renderer.js`)
   `bridgeGuard()` runs before anything else. If `window.clipboardAPI`,
   `rendererDom`, `rendererUtils` or the limits module failed to load, the app
   renders a visible recovery panel (what is missing + Reload) instead of dying
   at module scope. Global `error` / `unhandledrejection` handlers are attached
   at the same point, so a later failure is reported instead of silent.

2. **Total store size cap — 64 MB** (`src/limits.js`, `src-tauri/src/limits.rs`)
   Per-item 4 MB × 1000 items allowed a ~4 GB "valid" store that would OOM on
   save. New `MAX_TOTAL_BYTES` is enforced in `checkLimits()` (JS) and
   `validate_data()` (Rust); `sanitize_data()` stops accepting items once the
   budget is exhausted during import/load. Parity test covers the new constant.

3. **`save_data` no longer blocks an IPC worker** (`src-tauri/src/commands.rs`)
   The largest write in the app was still synchronous. It now runs on
   `tauri::async_runtime::spawn_blocking`, so a big save cannot freeze the UI or
   starve the quit-flush handshake (this completes the work started for
   `paste_to_active` / `export` in v3.1.1).

4. **Duplicate `uid` rejected** (`limits.js`, `limits.rs`)
   Duplicate ids were accepted on save/import, so one delete removed two rows.
   Saves with duplicate uids are now rejected; load/import strips duplicates
   keeping the first occurrence.

### P1

5. **Clean, immediate quit** (`src-tauri/src/main.rs`)
   `quit_app` calls `cvar.notify_all()`, and the geometry writer skips its 500 ms
   coalescing sleep while quitting — final flush is immediate and the thread
   exits instead of holding shutdown for up to 5 s.

6. **Dead window-size code removed** (`src-tauri/src/main.rs`, `store.rs`)
   The window is `resizable: false`, so persisting/restoring `width`/`height`
   was dead weight (and the v3.1.1 "P2-A" change that restored size is reverted
   as unreachable). `WindowState` keeps position only.

### Verification
- All JS passes `node --check`; limit + dedupe logic exercised with Node.
- Rust not compiled in this environment — run `cargo check` / `cargo test`
  locally once (`limits_js_parity` now also checks `MAX_TOTAL_BYTES`).

---

## v3.1.1 — Fix pass 2 + P1 sweep

- **`paste_to_active` off the IPC pool** — command is async; focus polling,
  modifier wait, SendInput and the PowerShell fallback run on
  `spawn_blocking`. A worker panic reports `copied: true, ok: false`.
- **Window position no longer corrupted by minimize** — geometry is skipped
  while minimized/hidden, and any rectangle with `x < -8 || y < -8` (Windows
  reports `-32000` mid-minimize) is discarded.
- **Parity test path fixed** — `include_str!(concat!(env!("CARGO_MANIFEST_DIR"), …))`.
- **Geometry writer stops spinning** — condvar-based `AppState.geometry_signal`,
  `wait_timeout(5 s)`, burst coalescing.
- **Time-based backups** — `SAVE_COUNTER` removed; `.bak` refreshed when older
  than 10 minutes.
- **Capabilities narrowed** — `core:default` replaced with the explicit
  permissions in use (later reduced further to 3 in the P1 sweep).
- **Modifier restore** — `restore_forced_modifiers()` re-asserts only keys the
  user is still physically holding, fixing stray key-ups in games/RDP.
- **P1 sweep:** comment-noise cleanup; one lifecycle block (`blur` /
  `visibilitychange` / `beforeunload` sharing `onAppHidden()`); single
  `write_atomic_mapped()`; single `readRowH()`; settings-row layout;
  capabilities 24 → 3; slimmer Cargo profile + Windows-only autostart;
  malformed-tab guard; `copy_text` returns `PasteResult` so the toast shows the
  real reason.
- **Limits** — MAX_ITEMS 5000 → 1000, MAX_TABS 100 → 10, 4 MB per item.
- **Large payloads** — `maxlength` removed from textareas, `previewOf()` slices
  240 chars before regex, `itemBytes()` cached in a WeakMap.
- **Scrolling** — wheel gain/step/lookahead/speed/ease retuned.

---

## v3.1.0 — first audit fix pass

- **CSP added** (`tauri.conf.json`) — strict policy; `'unsafe-inline'` for
  styles only, no inline scripts exist.
- **Startup panic removed** — missing window icon returns an error; a failed
  tray only logs.
- **`tauri-plugin-fs` removed** — backend uses `std::fs`; broad fs permissions
  dropped.
- **Atomic writes** for settings and window state (tmp → `sync_all` → rename).
- **PowerShell fallback can't hang** — `spawn()` + 5 s deadline, child killed.
- **Virtual-list row height** unified behind the `--row-h` CSS variable.
- **Import shape guard** — non-array `parsed.items` falls back to line parsing.
- **Undo timer cleared on unload.**
- **DRY** — `write_and_track()` in `commands.rs` (text moved, not cloned),
  `config_path()` in `store.rs`.

## [3.2.1]

### Changed
- Same text is now stored only once. Copying an identical text again no longer
  creates a new entry — the existing unpinned row is promoted to the top of the
  unpinned section (pinned rows keep their position and are never reordered).
- Manual "add text" follows the same rule: duplicates reuse the existing row.

### Removed
- Unique Item ID guard (`duplicate item id` internal error). De-duplication is
  now content-based, so a per-copy unique id check is no longer needed.

