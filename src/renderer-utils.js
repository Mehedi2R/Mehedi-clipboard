/* Pure helpers shared by renderer.js (no app state, no DOM writes).
 * Loaded as a classic script before renderer.js and exposed on window.rendererUtils. */
(function () {
  'use strict';

  // Byte size has exactly one implementation, in src/limits.js
  // (window.sharedLimits), the same file src-tauri/src/limits.rs is checked
  // against. Never add a second copy: it costs a TextEncoder per hot-loop call.
  const shared = window.sharedLimits;
  if (!shared) throw new Error('renderer-utils.js: limits.js must be loaded first');
  const getByteSize = shared.getByteSize;
  const fmt = (n) => Number(n).toLocaleString('en-US');

  // Shows a clear message when a save fails because a limit was exceeded
  class LimitError extends Error {
    constructor(message) {
      super(message);
      this.name = 'LimitError';
      this.isLimitError = true;
    }
  }

  // Save-failure code from the main process -> human readable reason
  class SaveError extends Error {
    constructor(code, detail) {
      super(SaveError.describe(code, detail));
      this.name = 'SaveError';
      this.code = code;
      this.isSaveError = true;
    }
    static describe(code, detail) {
      const extra = detail ? ` (${detail})` : '';
      switch (code) {
        case 'disk-full': return "Couldn't save: the disk is full.";
        case 'permission': return "Couldn't save: permission denied.";
        case 'read-only': return "Couldn't save: the data folder is read-only.";
        case 'busy': return "Couldn't save: the data file is in use.";
        case 'invalid': return `Couldn't save: the data was rejected${extra}.`;
        case 'canceled': return 'Canceled.';
        default: return `Couldn't save changes${extra}.`;
      }
    }
  }

  // deep copy helpers - so a rollback truly restores the previous state
  const cloneItems = (list) => list.map((i) => ({ ...i }));
  const cloneTabs = (list) => list.map((t) => ({ ...t, ids: [...t.ids] }));

  // Unique id per row - duplicate text is never a problem
  let uidSeq = 0;
  const makeUid = () =>
    `it-${Date.now().toString(36)}-${(uidSeq += 1)}-${Math.random().toString(36).slice(2, 7)}`;

  const makeTabId = () => `tab-${Date.now()}-${Math.random().toString(36).slice(2, 7)}`;

  /* A row only ever shows one line, so never run a regex over a multi-megabyte
   * string: slice a short head first. This is what kept 4 MB pastes from
   * freezing the list while rows were being built. */
  const PREVIEW_CHARS = 240;
  const previewOf = (text) =>
    typeof text === 'string' ? text.slice(0, PREVIEW_CHARS).replace(/\r?\n/g, ' ⏎ ') : '';

  /* getByteSize encodes the whole string, which is expensive on 4 MB items and
   * runs on every limit check and export. The result is cached per item object
   * and invalidated when its text changes. */
  const byteCache = new WeakMap();
  function itemBytes(item) {
    if (!item || typeof item.text !== 'string') return 0;
    const hit = byteCache.get(item);
    if (hit && hit.text === item.text) return hit.bytes;
    const bytes = getByteSize(item.text);
    byteCache.set(item, { text: item.text, bytes });
    return bytes;
  }

  const sortedItems = (list) => [...list].sort((a, b) => Number(b.pinned) - Number(a.pinned));

  const readFileAsText = (file) =>
    new Promise((resolve, reject) => {
      const reader = new FileReader();
      reader.onload = (e) => resolve(e.target.result);
      reader.onerror = reject;
      reader.readAsText(file);
    });

  const decodeItem = (line) =>
    line.replace(/\\\\|\\n|\\r/g, (m) => (m === '\\n' ? '\n' : m === '\\r' ? '\r' : '\\'));

  function parseLegacyLines(content) {
    const out = [];
    for (const line of content.split(/\r?\n/)) {
      if (!line.trim()) continue;
      const body = line.startsWith('// ') ? line.slice(3) : line.replace(/^\/\//, '');
      const decoded = decodeItem(body);
      if (decoded.length) out.push(decoded);
    }
    return out;
  }

  // Row pitch: --row-h in index.html is the single source of truth.
  const ROW_FALLBACK = 42;
  function readRowH() {
    const v = parseFloat(getComputedStyle(document.documentElement).getPropertyValue('--row-h'));
    return Number.isFinite(v) && v > 0 ? v : ROW_FALLBACK;
  }

  window.rendererUtils = {
    readRowH, getByteSize, itemBytes, fmt, LimitError, SaveError,
    cloneItems, cloneTabs, makeUid, makeTabId, previewOf, sortedItems,
    readFileAsText, parseLegacyLines,
  };
})();
