/* eslint-disable */
/**
 * tauri-bridge.js — drop-in replacement for the Electron preload.js.
 * Plain classic script (no imports, no bundler). Recreates window.clipboardAPI 1:1.
 *
 * Limits/validation come from src/limits.js (window.sharedLimits) - the one
 * and only JS copy. Rust parity is guarded by a test in src-tauri/src/limits.rs.
 */
(function () {
  'use strict';

  /* ---------------- limits: single source of truth ---------------- */
  /* src/limits.js is loaded as a classic script before this file and exposes
   * window.sharedLimits. There is no inline copy any more, so the renderer,
   * this bridge and shared/limits.js can never drift apart. */
  var L = window.sharedLimits;
  if (!L) throw new Error('tauri-bridge.js: limits.js must be loaded before tauri-bridge.js');
  var MAX_ITEMS = L.MAX_ITEMS;
  var MAX_STORED_ITEMS = L.MAX_STORED_ITEMS;
  var MAX_ITEM_BYTES = L.MAX_ITEM_BYTES;
  var MAX_HEAVY_ITEM_BYTES = L.MAX_HEAVY_ITEM_BYTES;
  var MAX_HEAVY_ITEMS = L.MAX_HEAVY_ITEMS;
  var applyRetention = L.applyRetention;
  var MAX_TABS = L.MAX_TABS;
  var MAX_TOTAL_BYTES = L.MAX_TOTAL_BYTES;
  var getByteSize = L.getByteSize;
  var normalizeEntry = L.normalizeEntry;
  // Shape + count check only: the renderer already rejected oversized items for
  // UX, and Rust re-validates byte sizes as the real trust boundary.
  var validateShape = L.validateShape;

  /* ---------------- whitelisted commands ---------------- */
  var CHANNELS = Object.freeze({
    GET_DATA: 'get_data',
    SAVE_DATA: 'save_data',
    COPY_TEXT: 'copy_text',
    HIDE_WINDOW: 'hide_window',
    PASTE_TO_ACTIVE: 'paste_to_active',
    READ_CLIPBOARD: 'read_clipboard',
    EXPORT_FILE: 'export_file',
    GET_SETTINGS: 'get_settings',
    SET_SETTINGS: 'set_settings',
    RENDERER_READY: 'renderer_ready',
    FLUSH_COMPLETE: 'flush_complete',
  });
  var ALLOWED = Object.freeze(Object.keys(CHANNELS).map(function (k) { return CHANNELS[k]; }));
  var EVENTS = Object.freeze([
    'shortcut-failed',
    // The backend asks the renderer to write out its debounced save
    // before the window hides ('flush-now') or the app quits ('app-will-quit').
    'flush-now',
    'app-will-quit',
    // The backend stored a clipboard row itself (window hidden / not ready);
    // the renderer reloads the store instead of losing the row.
    'history-updated',
    // A setting was changed outside the dialog (tray toggle, startup repair).
    'settings-changed',
    // Backend-side message that deserves a toast (e.g. autostart failed).
    'notice',
  ]);

  /* `listen()` resolves asynchronously, so calling renderer_ready right
   * after on(...) told the backend "I am listening" while the listeners were
   * still being attached — the first shortcut-failed event
   * went into the void. Every pending registration is tracked here and
   * rendererReady() waits for all of them. */
  var pendingListens = [];

  var NOOP = function () {};

  function invoke(channel, payload) {
    if (ALLOWED.indexOf(channel) === -1) {
      return Promise.reject(new Error('Blocked IPC channel: ' + String(channel)));
    }
    var c = window.__TAURI__ && window.__TAURI__.core;
    if (!c) return Promise.reject(new Error('Tauri IPC unavailable'));
    return c.invoke(channel, payload === undefined ? {} : payload);
  }

  function on(event, handler) {
    if (EVENTS.indexOf(event) === -1 || typeof handler !== 'function') return NOOP;
    var api = window.__TAURI__ && window.__TAURI__.event;
    if (!api) return NOOP;
    var un = null;
    var disposed = false;
    pendingListens.push(
      api.listen(event, function (e) { handler(e && e.payload); }).then(function (fn) {
        if (disposed) fn();
        else un = fn;
      })
    );
    return function () {
      disposed = true;
      if (un) un();
    };
  }

  function sendText(channel, name, text) {
    if (typeof text !== 'string') return Promise.reject(new TypeError(name + ': string expected'));
    if (getByteSize(text) > MAX_ITEM_BYTES) return Promise.reject(new RangeError(name + ': text too large'));
    return invoke(channel, { text: text });
  }

  // Only a real boolean is forwarded; anything else stays "not provided".
  function optBool(v) { return v === true ? true : v === false ? false : undefined; }
  function optStr(v) { return typeof v === 'string' ? v : undefined; }

  window.clipboardAPI = {
    getData: function () { return invoke(CHANNELS.GET_DATA); },
    getLimits: function () {
      return Object.freeze({
        maxItemBytes: MAX_ITEM_BYTES,
        maxItems: MAX_ITEMS,
        maxStoredItems: MAX_STORED_ITEMS,
        maxHeavyItemBytes: MAX_HEAVY_ITEM_BYTES,
        maxHeavyItems: MAX_HEAVY_ITEMS,
        maxTabs: MAX_TABS,
        maxTotalBytes: MAX_TOTAL_BYTES,
      });
    },
    normalizeEntry: function (entry) { return normalizeEntry(entry); },
    // Rolling-window retention (pinned kept, oldest unpinned evicted).
    applyRetention: function (list) { return applyRetention(list); },
    isHeavy: function (text) { return L.isHeavy(typeof text === 'string' ? text : ''); },
    saveData: function (data) {
      try {
        return invoke(CHANNELS.SAVE_DATA, { data: validateShape(data, 'saveData') });
      } catch (err) {
        return Promise.reject(err);
      }
    },
    readClipboard: function () { return invoke(CHANNELS.READ_CLIPBOARD); },
    hideWindow: function () { return invoke(CHANNELS.HIDE_WINDOW); },
    pasteToActive: function (text) { return sendText(CHANNELS.PASTE_TO_ACTIVE, 'pasteToActive', text); },
    copyText: function (text) { return sendText(CHANNELS.COPY_TEXT, 'copyText', text); },
    exportFile: function (content, suggestedName) {
      if (typeof content !== 'string') return Promise.reject(new TypeError('exportFile: string expected'));
      return invoke(CHANNELS.EXPORT_FILE, {
        payload: { content: content, suggestedName: optStr(suggestedName) },
      });
    },
    /** Resolves only after every event listener is really attached. */
    rendererReady: function () {
      return Promise.all(pendingListens.slice())
        .catch(function () { /* a failed listen must not block startup */ })
        .then(function () { return invoke(CHANNELS.RENDERER_READY); });
    },
    /** Acknowledges that all pending writes reached the disk. */
    flushComplete: function () { return invoke(CHANNELS.FLUSH_COMPLETE); },
    getSettings: function () { return invoke(CHANNELS.GET_SETTINGS); },
    setSettings: function (partial) {
      var p = partial || {};
      return invoke(CHANNELS.SET_SETTINGS, {
        partial: {
          shortcut: optStr(p.shortcut),
          autoStart: optBool(p.autoStart),
          autoCapture: optBool(p.autoCapture),
        },
      });
    },
    onShortcutFailed: function (handler) { return on('shortcut-failed', handler); },
    onFlushNow: function (handler) { return on('flush-now', handler); },
    onAppWillQuit: function (handler) { return on('app-will-quit', handler); },
    onHistoryUpdated: function (handler) { return on('history-updated', handler); },
    onSettingsChanged: function (handler) { return on('settings-changed', handler); },
    onNotice: function (handler) { return on('notice', handler); },
  };
})();

/* Disable the webview's native right-click menu (Back / Refresh / Inspect ...).
 * The app's own custom context menu in renderer.js keeps working. */
(function () {
  'use strict';
  document.addEventListener('contextmenu', function (e) {
    var el = e.target;
    var editable = el && (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA' || el.isContentEditable === true);
    if (!editable) e.preventDefault();
  }, { capture: true });
})();
