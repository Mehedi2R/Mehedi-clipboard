/**
 * SINGLE SOURCE OF TRUTH for every JS-side limit and validation rule.
 *
 * Loaded two ways, from this one file only:
 *   - browser (classic <script> in index.html)  -> window.sharedLimits
 *   - Node / tooling (`require('../shared/limits')`) -> module.exports
 *
 * shared/limits.js is only a re-export shim pointing here, and
 * src-tauri/src/limits.rs has a unit test that reads this file and fails
 * the build if the Rust constants ever drift from the numbers below.
 */
(function (root, factory) {
  'use strict';
  var api = factory();
  if (typeof module !== 'undefined' && module.exports) module.exports = api;
  if (root) root.sharedLimits = api;
})(typeof window !== 'undefined' ? window : null, function () {
  'use strict';

  // KEEP IN SYNC WITH src-tauri/src/limits.rs (enforced by limits_js_parity test)
  // Unpinned history cap (Windows-clipboard style rolling window): the 101st
  // unpinned copy evicts the oldest unpinned row at the bottom of the list.
  var MAX_ITEMS = 100;
  // Hard ceiling for a stored file: unpinned window + pinned rows.
  var MAX_STORED_ITEMS = 200;
  var MAX_ITEM_BYTES = 4 * 1024 * 1024; // max 4 MB per item - bigger is refused, never saved
  // "Heavy" rows are the ones that make rendering/serialising expensive.
  var MAX_HEAVY_ITEM_BYTES = 256 * 1024; // > 256 KB counts as heavy
  var MAX_HEAVY_ITEMS = 25;              // at most 25 heavy unpinned rows
  var MAX_TABS = 10;
  // Whole-store budget. MAX_ITEMS * MAX_ITEM_BYTES would allow ~4 GB, which the
  // save path cannot serialize. KEEP IN SYNC with limits.rs.
  var MAX_TOTAL_BYTES = 64 * 1024 * 1024;
  var MAX_NAME_LENGTH = 200;

  /* One encoder for the whole process. getByteSize runs in hot loops (limit
   * checks, import filtering, export), and a fresh TextEncoder per call meant
   * thousands of throw-away allocations per save. */
  var ENCODER = typeof TextEncoder !== 'undefined' ? new TextEncoder() : null;

  /** UTF-8 byte size - works in Node (Buffer) and in the browser (TextEncoder) */
  function getByteSize(str) {
    if (typeof Buffer !== 'undefined' && Buffer.byteLength) return Buffer.byteLength(str, 'utf8');
    if (ENCODER) return ENCODER.encode(str).length;
    // Fallback: rough byte estimate for BMP characters
    return str.length * 2;
  }

  function truncateByBytes(str, maxBytes) {
    if (getByteSize(str) <= maxBytes) return str;
    var lo = 0;
    var hi = str.length;
    while (lo < hi) {
      var mid = Math.floor((lo + hi + 1) / 2);
      if (getByteSize(str.slice(0, mid)) <= maxBytes) lo = mid;
      else hi = mid - 1;
    }
    return str.slice(0, lo);
  }

  // A short, non-empty string that also fits the shared name/id ceiling
  function isName(v) {
    return typeof v === 'string' && v.length > 0 && v.length <= MAX_NAME_LENGTH;
  }

  function normalizeEntry(entry) {
    var raw = typeof entry === 'string' ? { text: entry, pinned: false } : entry;
    if (!raw || typeof raw !== 'object' || typeof raw.text !== 'string') return null;
    var clean = { text: raw.text, pinned: raw.pinned === true };
    if (isName(raw.uid)) clean.uid = raw.uid;
    return clean;
  }

  function normalizeTab(tab) {
    if (!tab || typeof tab !== 'object') return null;
    if (!isName(tab.id) || !isName(tab.name)) return null;
    if (!Array.isArray(tab.ids) || tab.ids.length > MAX_STORED_ITEMS) return null;
    var ids = tab.ids.filter(function (id) {
      return typeof id === 'string' && id.length <= MAX_NAME_LENGTH;
    });
    return { id: tab.id, name: tab.name, ids: ids };
  }


  function isHeavy(text) {
    // Cheap pre-filter: 1 char is at least 1 byte, so a short string can never
    // be heavy and never needs encoding.
    return text.length > MAX_HEAVY_ITEM_BYTES ? true : getByteSize(text) > MAX_HEAVY_ITEM_BYTES;
  }

  /** Rolling-window retention (Windows Clipboard behaviour).
   *
   * Pinned rows are never evicted. The list is newest-first, so we keep walking
   * from the top and drop the first unpinned row that no longer fits:
   *   - at most MAX_ITEMS unpinned rows
   *   - at most MAX_HEAVY_ITEMS unpinned rows above MAX_HEAVY_ITEM_BYTES
   * Everything dropped is by definition the oldest unpinned entry of its class.
   */
  function applyRetention(list) {
    if (!Array.isArray(list)) return [];
    var out = [];
    var unpinned = 0;
    var heavy = 0;
    for (var i = 0; i < list.length; i += 1) {
      var item = list[i];
      if (!item) continue;
      if (item.pinned) { out.push(item); continue; }
      if (unpinned >= MAX_ITEMS) continue;
      var hv = isHeavy(item.text || '');
      if (hv && heavy >= MAX_HEAVY_ITEMS) continue;
      unpinned += 1;
      if (hv) heavy += 1;
      out.push(item);
    }
    return out;
  }

  /** Cheap shape check for the IPC bridge.
   *
   * The payload is validated three times on every save (renderer -> bridge ->
   * Rust); only the renderer (fast UX rejection) and Rust (trust boundary) need
   * to measure bytes. This keeps the bridge's normalization and its structural
   * guarantees, but walks no strings. */
  function validateShape(data, label) {
    label = label || 'saveData';
    var raw = Array.isArray(data) ? { items: data, tabs: [] } : data;
    if (!raw || typeof raw !== 'object') throw new TypeError(label + ': an object was expected');
    if (!Array.isArray(raw.items)) throw new TypeError(label + '.items: an array was expected');
    if (raw.items.length > MAX_STORED_ITEMS) {
      throw new RangeError(label + ': at most ' + MAX_STORED_ITEMS + ' items are allowed');
    }
    if (!Array.isArray(raw.tabs) || raw.tabs.length > MAX_TABS) {
      throw new RangeError(label + ': at most ' + MAX_TABS + ' tabs are allowed');
    }
    var seen = Object.create(null);
    var items = raw.items.map(function (entry, i) {
      var clean = normalizeEntry(entry);
      if (!clean) throw new TypeError(label + '.items: item ' + i + ' is invalid');
      // Cheap (no string walking): duplicate uids let one delete remove two rows.
      if (clean.uid) {
        if (seen[clean.uid]) throw new TypeError(label + '.items: duplicate uid at item ' + i);
        seen[clean.uid] = true;
      }
      return clean;
    });
    var tabs = raw.tabs.map(function (tab, i) {
      var clean = normalizeTab(tab);
      if (!clean) throw new TypeError(label + ': tab ' + i + ' is invalid');
      return clean;
    });
    return { version: 2, items: items, tabs: tabs };
  }

  /** Lenient validation - drops only the bad entries when reading old data. */
  function sanitizeLenient(list) {
    if (!Array.isArray(list)) return [];
    var out = [];
    var seen = Object.create(null);
    var total = 0;
    list.slice(0, MAX_STORED_ITEMS).forEach(function (entry) {
      var clean = normalizeEntry(entry);
      if (!clean || !clean.text) return;
      clean.text = truncateByBytes(clean.text, MAX_ITEM_BYTES);
      var bytes = getByteSize(clean.text);
      // Stop before the whole-store budget: a file that grew past it (older
      // build, hand-edited) must still open instead of taking the app down.
      if (total + bytes > MAX_TOTAL_BYTES) return;
      // Drop the uid of a duplicate rather than the item itself; the renderer
      // assigns a fresh one on load.
      if (clean.uid) {
        if (seen[clean.uid]) delete clean.uid;
        else seen[clean.uid] = true;
      }
      total += bytes;
      out.push(clean);
    });
    // Old stores (built when the cap was 1000) are trimmed down to the new
    // rolling window right here, on the first read.
    return applyRetention(out);
  }

  function sanitizeData(data) {
    var raw = Array.isArray(data) ? { items: data, tabs: [] } : data;
    if (!raw || typeof raw !== 'object') return { version: 2, items: [], tabs: [] };
    var items = sanitizeLenient(raw.items);
    var validIds = new Set(items.map(function (i) { return i.uid; }).filter(Boolean));
    var tabs = Array.isArray(raw.tabs)
      ? raw.tabs.slice(0, MAX_TABS).map(normalizeTab).filter(Boolean).map(function (tab) {
        return {
          id: tab.id,
          name: tab.name,
          ids: tab.ids.filter(function (id) { return validIds.has(id); }),
        };
      })
      : [];
    return { version: 2, items: items, tabs: tabs };
  }

  return {
    MAX_ITEMS: MAX_ITEMS,
    MAX_STORED_ITEMS: MAX_STORED_ITEMS,
    MAX_ITEM_BYTES: MAX_ITEM_BYTES,
    MAX_HEAVY_ITEM_BYTES: MAX_HEAVY_ITEM_BYTES,
    MAX_HEAVY_ITEMS: MAX_HEAVY_ITEMS,
    MAX_TABS: MAX_TABS,
    MAX_TOTAL_BYTES: MAX_TOTAL_BYTES,
    MAX_NAME_LENGTH: MAX_NAME_LENGTH,
    getByteSize: getByteSize,
    normalizeEntry: normalizeEntry,
    normalizeTab: normalizeTab,
    validateShape: validateShape,
    sanitizeData: sanitizeData,
    isHeavy: isHeavy,
    applyRetention: applyRetention,
  };
});
