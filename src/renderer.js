/* ------------------------------------------------------------------
 * Bridge guard  (must run before anything else touches the bridge)
 * ------------------------------------------------------------------
 * If tauri-bridge.js / limits.js / renderer-utils.js / renderer-dom.js fail
 * to load, every symbol below is undefined and this script dies at module
 * scope: the user gets a blank white window with no explanation. Fail loudly
 * instead, with a copyable reason. */
(function bridgeGuard() {
  'use strict';
  const api = window.clipboardAPI;
  const missing = [];
  if (!api || typeof api.getData !== 'function' || typeof api.getLimits !== 'function') {
    missing.push('clipboardAPI (tauri-bridge.js)');
  }
  if (!window.sharedLimits) missing.push('sharedLimits (limits.js)');
  if (!window.rendererUtils) missing.push('rendererUtils (renderer-utils.js)');
  if (!window.rendererDom) missing.push('rendererDom (renderer-dom.js)');
  if (missing.length === 0) return;

  const reason = 'Startup failed — these scripts did not load: ' + missing.join(', ');
  const paint = () => {
    const el = (tag, css, text) => {
      const node = document.createElement(tag);
      node.style.cssText = css;
      node.textContent = text;
      return node;
    };
    const box = el('div', 'position:fixed;inset:0;z-index:99999;display:flex;flex-direction:column;gap:10px;'
      + 'justify-content:center;align-items:center;padding:24px;text-align:center;'
      + 'font:13px/1.5 Arial,sans-serif;background:#fff;color:#b00020;', '');
    box.setAttribute('role', 'alert');
    box.appendChild(el('strong', '', 'Mehedi Clipboard could not start'));
    box.appendChild(el('div', 'color:#333;max-width:420px;word-break:break-word;user-select:text;',
      reason + '. Reinstalling the app usually fixes this.'));
    (document.body || document.documentElement).appendChild(box);
  };
  if (document.body) paint();
  else document.addEventListener('DOMContentLoaded', paint, { once: true });

  console.error(reason);
  throw new Error(reason);
})();

const api = window.clipboardAPI;
const {
  maxItemBytes: MAX_ITEM_BYTES = 4 * 1024 * 1024,
  maxItems: MAX_ITEMS = 100,
  maxStoredItems: MAX_STORED_ITEMS = 200,
  maxHeavyItemBytes: MAX_HEAVY_ITEM_BYTES = 256 * 1024,
  maxHeavyItems: MAX_HEAVY_ITEMS = 25,
  maxTabs: MAX_TABS = 10,
  maxTotalBytes: MAX_TOTAL_BYTES = 64 * 1024 * 1024,
} = api.getLimits();

const {
  getByteSize, itemBytes, fmt, LimitError, SaveError,
  cloneItems, cloneTabs, makeUid, makeTabId,
  previewOf, sortedItems, readFileAsText, parseLegacyLines, readRowH,
} = window.rendererUtils;

const {
  $, on, isOverlayOpen, openOverlay, closeOverlay, closeOnBackdrop, focusSoon,
  createEl, clearChildren, isTypingTarget, activateOnEnterOrSpace, swallowNextClick, showToast,
} = window.rendererDom;

/* ------------------------------------------------------------------
 * Limits
 * ------------------------------------------------------------------ */
// One wording for every "over the per-item byte limit" message
const byteLimitMessage = (prefix, bytes) =>
  `${prefix}: ${fmt(bytes)} / ${fmt(MAX_ITEM_BYTES)} bytes (4 MB max).`;

/* Rolling window (Windows Clipboard behaviour): pinned rows are untouchable,
 * the unpinned block is capped at MAX_ITEMS and at MAX_HEAVY_ITEMS heavy rows.
 * Adding entry number 101 therefore drops the oldest unpinned row at the very
 * bottom of the list. The same function runs in the bridge and in Rust. */
const isHeavy = (text) => api.isHeavy(text);
const unpinnedCount = (list) => list.reduce((n, i) => n + (i.pinned ? 0 : 1), 0);
const heavyCount = (list) =>
  list.reduce((n, i) => n + (!i.pinned && isHeavy(i.text) ? 1 : 0), 0);

function withRetention(list) {
  const kept = api.applyRetention(list);
  const dropped = list.length - kept.length;
  if (dropped > 0 && selectedId && !kept.some((i) => i.uid === selectedId)) selectedId = null;
  return kept;
}

// Returns false (and toasts) when the text cannot be stored
function textFitsLimits(text, emptyMessage) {
  if (typeof text !== 'string' || text.length === 0) {
    if (emptyMessage) showToast(emptyMessage, true);
    return false;
  }
  const bytes = getByteSize(text);
  if (bytes <= MAX_ITEM_BYTES) return true;
  showToast(byteLimitMessage('Text is too large', bytes), true);
  return false;
}

function checkLimits(nextItems, nextTabs) {
  if (nextItems.length > MAX_STORED_ITEMS) {
    throw new LimitError(`Too many items: ${fmt(nextItems.length)} / ${fmt(MAX_STORED_ITEMS)} max.`);
  }
  if (unpinnedCount(nextItems) > MAX_ITEMS) {
    throw new LimitError(`Too many history items: ${fmt(MAX_ITEMS)} unpinned max.`);
  }
  if (heavyCount(nextItems) > MAX_HEAVY_ITEMS) {
    throw new LimitError(`Too many large items: ${fmt(MAX_HEAVY_ITEMS)} max.`);
  }
  if (nextTabs.length > MAX_TABS) {
    throw new LimitError(`Too many tabs: ${fmt(nextTabs.length)} / ${fmt(MAX_TABS)} max.`);
  }
  const tooLong = nextItems.find((item) => itemBytes(item) > MAX_ITEM_BYTES);
  if (tooLong) throw new LimitError(byteLimitMessage('Item too large', itemBytes(tooLong)));

  // Per-item caps alone allow 1000 x 4 MB ~= 4 GB on disk, which makes the
  // save (serialize + atomic write) run out of memory and lose the store.
  // The whole-store budget is the real ceiling.
  let total = 0;
  for (const item of nextItems) total += itemBytes(item);
  if (total > MAX_TOTAL_BYTES) {
    throw new LimitError(`Storage full: ${fmt(total)} / ${fmt(MAX_TOTAL_BYTES)} bytes.`);
  }

  // Unique-item-ID guard removed: identical text is now de-duplicated by content
  // (promoted to the top) instead of being rejected as a duplicate id.
}

/* ------------------------------------------------------------------
 * State
 * ------------------------------------------------------------------ */
let items = [];             // [{ uid, text, pinned }]
let sessionTabs = [];       // [{ id, name, ids: [] }] - imported file groups, persisted with items
let activeTabId = 'all';    // 'all' | tab id
let selectedId = null;      // uid of the selected row (Ctrl+C / arrow navigation)
let menuOpenFor = null;     // uid of the item whose menu is open, or null
let lastPaste = null;       // { uid, before, after, at }
let editingUid = null;
let renamingTabId = null;
let settingsCache = null;
let dragTabId = null;
let hideDropOverlay = null;   // set by setupFileDrop()
// Row revisions: bumped on every content change so the virtualized list can
// never keep a stale row in the DOM.
const itemRev = new Map();
let revSeq = 0;

// itemRev is pruned once it drifts past the live item count: in a tray-resident
// app the Map would otherwise grow for the whole session.
const REV_PRUNE_SLACK = 256;
function pruneItemRev() {
  if (itemRev.size <= items.length + REV_PRUNE_SLACK) return;
  const live = new Set(items.map((i) => i.uid));
  for (const uid of itemRev.keys()) if (!live.has(uid)) itemRev.delete(uid);
}
const UNDO_WINDOW_MS = 10000;
let undoTimer = null;

// Virtualization: the row pitch (height + margin) is read from --row-h, the
// single source of truth shared with the CSS and the scroll controller.
let ROW_H = readRowH();
// One resize listener for the whole app (row pitch + menu + relayout).
window.addEventListener('resize', () => {
  ROW_H = readRowH();
  closeMenu();
  invalidateAll();
  scheduleListRender();
});
const OVERSCAN = 12;

// Render once per frame instead of on every scroll event - keeps scrolling smooth
let listRenderFrame = 0;
let listRenderPending = false;   // a render was skipped because a drag is running
function scheduleListRender() {
  // never rebuild rows while a drag is armed or running
  if (rowDrag || listRenderFrame) return;
  listRenderFrame = requestAnimationFrame(() => {
    listRenderFrame = 0;
    renderList();
  });
}

const findItem = (uid) => items.find((i) => i.uid === uid) || null;

function bumpRev(uid) {
  revSeq += 1;
  itemRev.set(uid, revSeq);
}

// Replace one item in-place, keeping every other row untouched
function patchItem(uid, patch) {
  items = items.map((i) => (i.uid === uid ? { ...i, ...patch } : i));
  bumpRev(uid);
}

function isUndoAvailable(uid) {
  if (!lastPaste) return false;
  if (Date.now() - lastPaste.at > UNDO_WINDOW_MS) {
    lastPaste = null;
    return false;
  }
  return lastPaste.uid === uid;
}

/* ------------------------------------------------------------------
 * Boot
 * ------------------------------------------------------------------ */
document.addEventListener('DOMContentLoaded', () => {
  loadClipboard();
  loadSettings();

  wireModal('modalOverlay', 'modalCancel', 'modalSave', closeAddModal, saveFromModal);
  wireModal('editOverlay', 'editCancel', 'editSave', closeEditModal, saveFromEditModal);
  wireModal('settingsOverlay', 'settingsCancel', 'settingsSave', closeSettingsModal, saveSettings);
  wireModal('renameOverlay', 'renameCancel', 'renameSave', closeRenameModal, saveFromRenameModal);
  on('renameInput', 'keydown', (e) => {
    if (e.key === 'Enter') { e.preventDefault(); saveFromRenameModal(); }
  });

  on('addBtn', 'click', openAddModal);
  on('winCloseBtn', 'click', async () => {
    // The write must reach the disk before the window goes away.
    await flushSave().catch(() => {});
    if (window.clipboardAPI && window.clipboardAPI.hideWindow) window.clipboardAPI.hideWindow();
  });
  on('settingsBtn', 'click', openSettingsModal);
  on('exportBtn', 'click', exportToFile);
  on('clearAllInline', 'click', clearAllUnpinned);
  on('importFile', 'change', (e) => {
    const files = Array.from(e.target.files || []);
    e.target.value = '';
    importFromFiles(files);
  });

  on('clipboardList', 'scroll', () => {
    closeMenu();
    scheduleListRender();
  }, { passive: true });

  document.addEventListener('keydown', onGlobalKeydown);

  document.addEventListener('pointerdown', (e) => {
    if (!menuOpenFor) return;
    const menu = $('itemMenu');
    if (menu && menu.contains(e.target)) return;
    if (e.target.closest && e.target.closest('.menu-btn')) return;
    closeMenu();
    swallowNextClick();
  }, true);

  window.addEventListener('contextmenu', closeMenu);

  hideDropOverlay = setupFileDrop();
});

// cancel / save / backdrop wiring is identical for every modal
function wireModal(overlayId, cancelId, saveId, close, save) {
  on(cancelId, 'click', close);
  on(saveId, 'click', save);
  closeOnBackdrop(overlayId, close);
}

function onGlobalKeydown(e) {
  const typing = isTypingTarget();
  const mod = e.ctrlKey || e.metaKey;

  if (mod && (e.key === 'c' || e.key === 'C')) {
    const tag = document.activeElement && document.activeElement.tagName;
    if (tag === 'INPUT' || tag === 'TEXTAREA') return;
    if (window.getSelection && String(window.getSelection()).length > 0) return;
    const sel = findItem(selectedId);
    if (!sel) return;
    e.preventDefault();
    copyText(sel.text);
    return;
  }

  if (mod && e.key === 'Enter') {
    if (isModalOpen()) { e.preventDefault(); saveFromModal(); return; }
    if (isEditOpen()) { e.preventDefault(); saveFromEditModal(); return; }
  }

  if (e.key === 'Escape') {
    if (isModalOpen()) closeAddModal();
    if (isEditOpen()) closeEditModal();
    if (isSettingsOpen()) closeSettingsModal();
    if (isRenameOpen()) closeRenameModal();
    closeMenu();
    return;
  }

  if (mod && (e.key === 'v' || e.key === 'V')) {
    if (typing) return;
    e.preventDefault();
    pasteFromSystemClipboard();
    return;
  }

  // keyboard navigation
  if (typing || anyModalOpen()) return;
  if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
    e.preventDefault();
    moveSelection(e.key === 'ArrowDown' ? 1 : -1);
    return;
  }
  if (e.key === 'Enter') {
    const sel = findItem(selectedId);
    if (!sel) return;
    e.preventDefault();
    copyText(sel.text);
  }
}

/* ------------------------------------------------------------------
 * Storage helpers
 * ------------------------------------------------------------------ */
function normalize(list) {
  if (!Array.isArray(list)) return [];
  const out = [];
  for (const entry of list) {
    const clean = api.normalizeEntry(entry);
    if (!clean || clean.text.length === 0) continue;
    out.push({ uid: clean.uid || makeUid(), text: clean.text, pinned: clean.pinned });
  }
  return out;
}

async function loadClipboard() {
  try {
    const stored = (await api.getData()) || {};
    items = normalize(Array.isArray(stored) ? stored : stored.items);
    const validIds = new Set(items.map((item) => item.uid));
    // A hand-edited or half-written store must not cost the user the items it
    // still holds: unusable tabs are dropped, the rest of the load continues.
    sessionTabs = (Array.isArray(stored.tabs) ? stored.tabs : [])
      .filter((tab) => tab && typeof tab.id === 'string' && typeof tab.name === 'string')
      .map((tab) => ({
        id: tab.id,
        name: tab.name,
        ids: (Array.isArray(tab.ids) ? tab.ids : []).filter((id) => validIds.has(id)),
      }));
    rebuild();
  } catch (error) {
    console.error('Load failed:', error);
    showToast("Couldn't load saved data", true);
  }
}

/* Debounced save: changes are collapsed into one write after
 * SAVE_DEBOUNCE_MS of quiet. persist() resolves with the real disk result so
 * commit() can still roll back on failure. */
const SAVE_DEBOUNCE_MS = 120;
let saveTimer = null;      // pending setTimeout id
let savePending = null;    // { promise, resolve, reject } shared by batched callers

function schedulePendingSave() {
  if (!savePending) {
    let resolve;
    let reject;
    const promise = new Promise((res, rej) => { resolve = res; reject = rej; });
    // A caller that ignores the promise must not raise "unhandled rejection".
    promise.catch(() => {});
    savePending = { promise, resolve, reject };
  }
  if (saveTimer) clearTimeout(saveTimer);
  saveTimer = setTimeout(runSave, SAVE_DEBOUNCE_MS);
  return savePending.promise;
}

async function runSave() {
  if (saveTimer) { clearTimeout(saveTimer); saveTimer = null; }
  const pending = savePending;
  savePending = null;
  if (!pending) return;
  try {
    const result = await api.saveData({ items, tabs: sessionTabs });
    // the main process now returns { ok, code, message } (legacy boolean still supported)
    if (result !== true && !(result && result.ok === true)) {
      throw new SaveError(result && result.code, result && result.message);
    }
    pending.resolve(true);
  } catch (error) {
    pending.reject(error);
  }
}

/** Write immediately - used before the window hides or the app closes. */
const flushSave = () => (saveTimer || savePending ? runSave() : Promise.resolve());

function persist() {
  // Limit checks stay synchronous so an over-limit change is rejected (and
  // rolled back) before anything is scheduled.
  checkLimits(items, sessionTabs);
  const saved = schedulePendingSave();
  render();   // optimistic repaint - the UI no longer waits on the disk
  return saved;
}

// Snapshot / rollback of everything a save can touch
const snapshot = () => ({ items: cloneItems(items), tabs: cloneTabs(sessionTabs), activeTabId });

function restore(snap, withActiveTab) {
  items = snap.items;
  sessionTabs = snap.tabs;
  if (withActiveTab) activeTabId = snap.activeTabId;
  invalidateAll();
}

async function commit(change, successMessage) {
  const snap = snapshot();
  try {
    change();
    await persist();
    if (successMessage) showToast(successMessage);
    return true;
  } catch (error) {
    restore(snap);
    render();
    console.error('Save failed:', error);
    showToast(saveMessage(error), true);
    return false;
  }
}

const saveMessage = (error, fallback) =>
  error && (error.isLimitError || error.isSaveError)
    ? error.message
    : fallback || "Couldn't save changes";

/* ------------------------------------------------------------------
 * Rendering (virtualized list)
 * ------------------------------------------------------------------ */
/* The visible list was recomputed - filtered *and* re-sorted - on every
 * animation frame while scrolling, i.e. up to 5,000 items sorted 60x per
 * second. `items` and `sessionTabs` are always replaced immutably, so identity
 * is a sound cache key: the result is reused until the data really changes. */
let viewCache = { items: null, tabs: null, tabId: null, list: null };

function visibleItems() {
  if (viewCache.list && viewCache.items === items
    && viewCache.tabs === sessionTabs && viewCache.tabId === activeTabId) {
    return viewCache.list;
  }

  if (viewCache.items !== items) pruneItemRev();

  let base = items;
  const tab = activeTabId === 'all' ? null : sessionTabs.find((t) => t.id === activeTabId);
  if (tab) {
    const set = new Set(tab.ids);
    base = items.filter((i) => set.has(i.uid));
  }
  const list = sortedItems(base);
  viewCache = { items, tabs: sessionTabs, tabId: activeTabId, list };
  return list;
}

// Any in-place edit (patchItem keeps the same array identity for untouched
// rows but always replaces `items`, so this is only needed for safety nets such
// as a rollback that restores a previously cached array).
function invalidateView() {
  viewCache = { items: null, tabs: null, tabId: null, list: null };
}

/* The dataset changed wholesale: drop the memoized view AND the virtualization
   window key so the next render rebuilds every row. Used to be an 8x repeated
   `lastWindowKey = ''; invalidateView();` pair with drifting indentation. */
function invalidateAll() {
  lastWindowKey = '';
  invalidateView();
}

/** invalidateAll() + a full re-render - the most common follow-up. */
function rebuild() {
  invalidateAll();
  render();
}

function render() {
  renderTabs();
  renderList();
  updateClearAllVisibility();
}

function updateClearAllVisibility() {
  const clearAllInline = $('clearAllInline');
  if (clearAllInline) clearAllInline.style.display = items.some((i) => !i.pinned) ? '' : 'none';
}

/* ---- Tabs ---- */
function makeTabButton(className, label, ariaLabel, title, onClick) {
  const btn = createEl('button', {
    type: 'button',
    className,
    textContent: label,
    title,
    attrs: { 'aria-label': ariaLabel },
  });
  btn.onclick = (e) => {
    e.stopPropagation();
    onClick();
  };
  return btn;
}

function wireTabDrag(tab, id) {
  tab.draggable = true;
  tab.addEventListener('dragstart', (e) => {
    dragTabId = id;
    tab.classList.add('dragging');
    e.dataTransfer.effectAllowed = 'move';
    try { e.dataTransfer.setData('text/plain', id); } catch (_) { /* ignore */ }
  });
  tab.addEventListener('dragend', () => {
    dragTabId = null;
    tab.classList.remove('dragging');
  });
  tab.addEventListener('dragover', (e) => {
    if (!dragTabId || dragTabId === id) return;
    e.preventDefault();
    e.stopPropagation();
    tab.classList.add('drag-over');
  });
  tab.addEventListener('dragleave', () => tab.classList.remove('drag-over'));
  tab.addEventListener('drop', async (e) => {
    if (!dragTabId || dragTabId === id) return;
    e.preventDefault();
    e.stopPropagation();
    tab.classList.remove('drag-over');
    const from = sessionTabs.findIndex((t) => t.id === dragTabId);
    const to = sessionTabs.findIndex((t) => t.id === id);
    dragTabId = null;
    if (from < 0 || to < 0) return;
    await commit(() => {
      const next = cloneTabs(sessionTabs);
      const [moved] = next.splice(from, 1);
      next.splice(to, 0, moved);
      sessionTabs = next;
    }, 'Tabs reordered');
  });
}

function buildTab(id, label, closable) {
  const active = activeTabId === id;
  const tab = createEl('div', {
    className: 'tab' + (active ? ' active' : ''),
    tabIndex: 0,
    attrs: { role: 'tab', 'aria-selected': String(active) },
  }, [
    createEl('span', { className: 'tab-label', textContent: label, title: label }),
  ]);
  tab.onclick = () => {
    activeTabId = id;
    closeMenu();
    render();
  };
  activateOnEnterOrSpace(tab);

  if (closable) {
    wireTabDrag(tab, id);
    tab.appendChild(makeTabButton('tab-rename', '✎', `Rename ${label} tab`, 'Rename tab', () => renameTab(id)));
    tab.appendChild(makeTabButton('tab-close', '×', `Close ${label} tab`, undefined, () => closeTab(id)));
  }
  return tab;
}

function renderTabs() {
  const bar = $('tabsBar');
  clearChildren(bar);
  if (sessionTabs.length === 0) return;
  bar.appendChild(buildTab('all', `All (${items.length})`, false));
  sessionTabs.forEach((t) => bar.appendChild(buildTab(t.id, `${t.name} (${t.ids.length})`, true)));
}

async function closeTab(id) {
  await commit(() => {
    sessionTabs = sessionTabs.filter((t) => t.id !== id);
    if (activeTabId === id) activeTabId = 'all';
    closeMenu();
  });
}

// window.prompt() is not implemented in the Tauri webview, so renaming uses
// the app's own modal.
function renameTab(id) {
  const tab = sessionTabs.find((t) => t.id === id);
  if (!tab) return;
  closeMenu();
  renamingTabId = id;
  const input = $('renameInput');
  input.value = tab.name;
  openOverlay('renameOverlay');
  focusSoon(input);
  setTimeout(() => { try { input.select(); } catch (_) { /* ignore */ } }, 0);
}

function closeRenameModal() {
  closeOverlay('renameOverlay');
  renamingTabId = null;
}

async function saveFromRenameModal() {
  const id = renamingTabId;
  if (!id) return;
  const tab = sessionTabs.find((t) => t.id === id);
  if (!tab) { closeRenameModal(); return; }
  const name = $('renameInput').value.trim().slice(0, 200);
  if (!name) { showToast('Tab name cannot be empty', true); return; }
  if (name === tab.name) { closeRenameModal(); return; }
  const saved = await commit(() => {
    sessionTabs = sessionTabs.map((t) => (t.id === id ? { ...t, ids: [...t.ids], name } : t));
  }, 'Tab renamed');
  if (saved) closeRenameModal();
}

/* ---- Rows ---- */
// Pointer based row reorder - HTML5 drag&drop is unreliable inside WebView2,
// so rows are moved with pointer events (mouse, pen and touch; up, down and
// into empty space). A drag never changes an item's pinned state.
let rowDrag = null;
let dragJustEnded = false;
let dragMoveFrame = 0;
let lastDragEvent = null;

function clearRowDragMarks() {
  document.querySelectorAll('.clip-item.drag-over, .clip-item.drag-below')
    .forEach((el) => el.classList.remove('drag-over', 'drag-below'));
}

// Pointer events (mouse + pen + touch) with pointer capture - the row keeps
// receiving moves even when the cursor leaves it or the list scrolls.
function wireRowDrag(row, item) {
  row.draggable = false;
  row.addEventListener('dragstart', (e) => e.preventDefault());
  // Only pinned rows can be reordered - unpinned rows keep their natural order.
  if (!item.pinned) return;
  row.classList.add('draggable-row');
  row.style.touchAction = 'none';   // stop WebView2 from stealing the first move
  row.addEventListener('pointerdown', (e) => {
    if (e.pointerType === 'mouse' && e.button !== 0) return;
    if (e.target.closest && e.target.closest('button, input, textarea, a')) return;
    const r = row.getBoundingClientRect();
    rowDrag = {
      uid: item.uid, row, pointerId: e.pointerId,
      startX: e.clientX, startY: e.clientY,
      grabOffsetY: e.clientY - r.top,   // where inside the row the drag started
      rowHeight: r.height,
      active: false, targetUid: null, after: false,
    };
    // Capture immediately so every following move belongs to this row, even
    // if the pointer leaves it or the list scrolls under it.
    try { row.setPointerCapture(e.pointerId); } catch (_) { /* ignore */ }
    attachRowDragListeners();
  });
}

function onRowDragMove(e) {
  if (!rowDrag || (rowDrag.pointerId !== undefined && e.pointerId !== rowDrag.pointerId)) return;
  if (!rowDrag.active) {
    if (Math.abs(e.clientY - rowDrag.startY) < 3 && Math.abs(e.clientX - rowDrag.startX) < 3) return;
    rowDrag.active = true;
    rowDrag.row.classList.add('dragging');
    document.body.classList.add('row-dragging');
    closeMenu();
  }
  if (e.cancelable) e.preventDefault();
  // Coalesce moves into one frame - the marker follows the cursor smoothly
  // instead of thrashing the DOM on every pointer event.
  lastDragEvent = { clientX: e.clientX, clientY: e.clientY };
  if (dragMoveFrame) return;
  dragMoveFrame = requestAnimationFrame(() => {
    dragMoveFrame = 0;
    applyDragPosition();
  });
}

function applyDragPosition() {
  if (!rowDrag || !rowDrag.active || !lastDragEvent) return;
  const { clientY } = lastDragEvent;
  clearRowDragMarks();
  rowDrag.targetUid = null;

  const container = $('clipboardList');
  const box = container.getBoundingClientRect();
  // The scroll controller is the only code allowed to write list scrollTop.
  const edge = clientY > box.bottom - 14 ? 14 : clientY < box.top + 14 ? -14 : 0;
  if (edge) window.smoothScroll.scrollBy(edge);

  // The row itself follows the cursor, so one row of travel = one slot.
  rowDrag.row.style.transform = `translateY(${clientY - rowDrag.startY}px)`;

  // Only pinned rows are valid drop targets (pinned items move among pinned).
  const pinnedRows = Array.from(container.querySelectorAll('.clip-item.draggable-row'))
    .filter((el) => el.dataset.uid && el.dataset.uid !== rowDrag.uid);
  if (pinnedRows.length === 0) return;

  // Compare the *projected centre of the dragged row* (not the raw pointer)
  // against the other rows' centres. Moving down/up by a single row height is
  // therefore enough to swap with the direct neighbour - no over-dragging and
  // no retrying.
  const centre = clientY - rowDrag.grabOffsetY + rowDrag.rowHeight / 2;

  let over = null;
  let after = false;
  for (const el of pinnedRows) {
    const r = el.getBoundingClientRect();
    if (centre >= r.top + r.height / 2) { over = el; after = true; }   // passed this row
    else { if (!over) { over = el; after = false; } break; }
  }
  if (!over) return;

  over.classList.add(after ? 'drag-below' : 'drag-over');
  rowDrag.targetUid = over.dataset.uid;
  rowDrag.after = after;
}

async function onRowDragEnd(e) {
  const drag = rowDrag;
  rowDrag = null;
  if (dragMoveFrame) { cancelAnimationFrame(dragMoveFrame); dragMoveFrame = 0; }
  lastDragEvent = null;

  // Nothing armed, or the pointer never moved far enough to start a drag:
  // just run the render that was deferred while the drag was pending.
  const flushDeferredRender = () => {
    if (!listRenderPending) return;
    listRenderPending = false;
    renderList();
  };
  if (!drag) { flushDeferredRender(); return; }

  if (e && drag.pointerId !== undefined) {
    try { drag.row.releasePointerCapture(drag.pointerId); } catch (_) { /* ignore */ }
  }
  document.body.classList.remove('row-dragging');
  drag.row.classList.remove('dragging');
  drag.row.style.transform = '';   // drop the follow-the-cursor offset
  clearRowDragMarks();
  if (!drag.active) { flushDeferredRender(); return; }

  listRenderPending = false;
  // A finished drag must never fall through to "click = copy"
  dragJustEnded = true;
  swallowNextClick();
  setTimeout(() => { dragJustEnded = false; }, 350);
  if (!drag.targetUid) { rebuild(); return; }
  await dropRowOn(drag.uid, drag.targetUid, drag.after);
}

// Where the moved row is allowed to land.
// Rule: only pinned items are reorderable and a drag NEVER changes the pinned
// flag, so both the moved row and the target are always in the pinned group.
function insertIndexFor(list, movedPinned, targetUid, after) {
  const unpinnedAt = list.findIndex((i) => !i.pinned);
  const firstUnpinned = unpinnedAt < 0 ? list.length : unpinnedAt;
  const target = list.findIndex((i) => i.uid === targetUid);
  if (target < 0 || !list[target].pinned || !movedPinned) return firstUnpinned;
  return after ? target + 1 : target;
}

async function dropRowOn(movedUid, targetUid, after) {
  if (movedUid === targetUid) return;
  const from = items.findIndex((i) => i.uid === movedUid);
  const toIdx = items.findIndex((i) => i.uid === targetUid);
  if (from < 0 || toIdx < 0) return;
  // Reordering is limited to pinned items
  if (!items[from].pinned || !items[toIdx].pinned) { rebuild(); return; }
  await commit(() => {
    const next = cloneItems(items);
    const [moved] = next.splice(from, 1);      // pinned flag stays untouched
    next.splice(insertIndexFor(next, moved.pinned, targetUid, after), 0, moved);
    items = next;
    selectedId = moved.uid;
  });
  rebuild();
}

// Attached on pointerdown, removed on drop: no session-wide pointermove cost.
function onRowDragFinish(e) {
  void onRowDragEnd(e).finally(detachRowDragListeners);
}

const ROW_DRAG_EVENTS = [
  ['pointermove', onRowDragMove],
  ['pointerup', onRowDragFinish],
  ['pointercancel', onRowDragFinish],
];

function attachRowDragListeners() {
  ROW_DRAG_EVENTS.forEach(([type, fn]) => document.addEventListener(type, fn));
}

function detachRowDragListeners() {
  ROW_DRAG_EVENTS.forEach(([type, fn]) => document.removeEventListener(type, fn));
}

function buildRow(item) {
  const itemDiv = createEl('div', {
    className: 'clip-item' + (selectedId === item.uid ? ' selected' : ''),
    tabIndex: 0,
    attrs: { role: 'button', 'aria-label': `Copy ${previewOf(item.text)}` },
  });
  itemDiv.dataset.uid = item.uid;
  wireRowDrag(itemDiv, item);
  itemDiv.onclick = () => {
    if (dragJustEnded) return;   // a finished drag must not copy the row
    selectedId = item.uid;
    renderList();
    copyText(item.text);
  };

  activateOnEnterOrSpace(itemDiv);

  if (item.pinned) {
    itemDiv.appendChild(createEl('img', { className: 'pin-mark', src: 'assets/pin.png', alt: 'Pinned' }));
  }
  itemDiv.appendChild(createEl('span', { className: 'clip-text', textContent: previewOf(item.text) }));

  const menuBtn = createEl('button', {
    className: 'menu-btn',
    textContent: '⋮',
    title: 'see more',
    attrs: { 'aria-label': 'More actions' },
  });
  menuBtn.onclick = (e) => {
    e.stopPropagation();
    if (menuOpenFor === item.uid) { closeMenu(); return; }
    selectedId = item.uid;
    const anchorRect = e.currentTarget.getBoundingClientRect();
    renderList();
    openMenu(anchorRect, item);
  };
  itemDiv.appendChild(menuBtn);
  return itemDiv;
}

// Only visible rows stay in the DOM - the UI stays fast with thousands of items
// The container is not wiped while scrolling; rows change only when needed
let listShell = null;      // { topPad, rowsHost, bottomPad }
let lastWindowKey = '';    // which range/data is currently in the DOM

// Cheap signature of the visible slice - the key changes when the data changes
function windowSignature(list, first, last) {
  let sig = '';
  for (let i = first; i < last; i += 1) {
    const it = list[i];
    sig += `${it.uid}:${it.pinned ? 1 : 0}:${itemRev.get(it.uid) || 0}:${it.text.length}|`;
  }
  return sig;
}

function ensureListShell(container) {
  if (listShell && listShell.topPad.parentNode === container) return listShell;
  container.innerHTML = '';
  const topPad = createEl('div', { attrs: { 'aria-hidden': 'true' } });
  const rowsHost = createEl('div');
  const bottomPad = createEl('div', { attrs: { 'aria-hidden': 'true' } });
  container.append(topPad, rowsHost, bottomPad);
  listShell = { topPad, rowsHost, bottomPad };
  invalidateAll();
  return listShell;
}

function renderEmptyList(container) {
  listShell = null;
  invalidateAll();
  container.innerHTML = '';
  container.appendChild(createEl('div', {
    className: 'empty-note',
    textContent: activeTabId === 'all' ? 'No saved clipboard items.' : 'No items in this tab.',
  }));
}

// Which slice of the list is worth putting in the DOM right now
function windowRange(container, list) {
  const viewport = container.clientHeight || 400;

  // A shrinking data set can leave the virtual position out of range. Route the
  // correction through the sole controller instead of writing scrollTop here.
  window.smoothScroll.clampTo(Math.max(0, list.length * ROW_H - viewport));

  const first = Math.max(0, Math.floor(container.scrollTop / ROW_H) - OVERSCAN);
  const visibleCount = Math.ceil(viewport / ROW_H) + OVERSCAN * 2;
  return { first, last: Math.min(list.length, first + visibleCount) };
}

function renderList() {
  // A rebuild while a pointer drag is armed would detach the dragged row and
  // its pointer capture, killing the drag. Defer it until the drag finishes.
  if (rowDrag) { listRenderPending = true; return; }
  const container = $('clipboardList');
  const list = visibleItems();

  if (list.length === 0) { renderEmptyList(container); return; }

  const { first, last } = windowRange(container, list);
  const shell = ensureListShell(container);
  const key = `${first}|${last}|${list.length}|${selectedId || ''}|${activeTabId}|${windowSignature(list, first, last)}`;
  if (key === lastWindowKey) return;
  lastWindowKey = key;

  shell.topPad.style.height = `${first * ROW_H}px`;
  shell.bottomPad.style.height = `${Math.max(0, (list.length - last) * ROW_H)}px`;

  const frag = document.createDocumentFragment();
  for (let i = first; i < last; i += 1) frag.appendChild(buildRow(list[i]));
  shell.rowsHost.replaceChildren(frag);
}

function scrollRowIntoView(index) {
  const container = $('clipboardList');
  const top = index * ROW_H;
  if (top < container.scrollTop) window.smoothScroll.scrollTo(top);
  else if (top + ROW_H > container.scrollTop + container.clientHeight) {
    window.smoothScroll.scrollTo(top + ROW_H - container.clientHeight);
  }
}

function moveSelection(delta) {
  const list = visibleItems();
  if (list.length === 0) return;
  const current = list.findIndex((i) => i.uid === selectedId);
  const wanted = current < 0 ? (delta > 0 ? 0 : list.length - 1) : current + delta;
  const next = Math.min(Math.max(wanted, 0), list.length - 1);
  selectedId = list[next].uid;
  closeMenu();
  scrollRowIntoView(next);
  renderList();
}

/* ------------------------------------------------------------------
 * Clear All - removes everything except pinned items
 * ------------------------------------------------------------------ */
async function clearAllUnpinned() {
  closeMenu();
  if (!items.some((i) => !i.pinned)) return;
  await commit(() => {
    const keep = items.filter((i) => i.pinned);
    const keepIds = new Set(keep.map((i) => i.uid));
    items = keep;
    sessionTabs = sessionTabs
      .map((t) => ({ ...t, ids: t.ids.filter((id) => keepIds.has(id)) }))
      .filter((t) => t.ids.length > 0);
    if (activeTabId !== 'all' && !sessionTabs.some((t) => t.id === activeTabId)) activeTabId = 'all';
    if (selectedId && !keepIds.has(selectedId)) selectedId = null;
    lastPaste = null;
    window.smoothScroll.jumpTo(0);
  });
}

/* ------------------------------------------------------------------
 * Add / Edit modals
 * ------------------------------------------------------------------ */
const isModalOpen = () => isOverlayOpen('modalOverlay');
const isEditOpen = () => isOverlayOpen('editOverlay');
const isSettingsOpen = () => isOverlayOpen('settingsOverlay');
const isRenameOpen = () => isOverlayOpen('renameOverlay');
const anyModalOpen = () => isModalOpen() || isEditOpen() || isSettingsOpen() || isRenameOpen();

// Show an overlay with its textarea prefilled and focused
function openTextModal(overlayId, textareaId, value) {
  closeMenu();
  const ta = $(textareaId);
  ta.value = value;
  openOverlay(overlayId);
  focusSoon(ta);
}

const openAddModal = () => openTextModal('modalOverlay', 'modalTextarea', '');
const closeAddModal = () => closeOverlay('modalOverlay');

function openEditModal(uid) {
  const item = findItem(uid);
  if (!item) return;
  editingUid = uid;
  openTextModal('editOverlay', 'editTextarea', item.text);
}

function closeEditModal() {
  closeOverlay('editOverlay');
  editingUid = null;
}

async function saveFromEditModal() {
  const uid = editingUid;
  if (!uid) return;
  const val = $('editTextarea').value;
  if (!textFitsLimits(val, 'Text cannot be empty')) return;
  const current = findItem(uid);
  if (current && current.text === val) { closeEditModal(); return; }
  const saved = await commit(() => patchItem(uid, { text: val }), 'Item updated');
  if (saved) closeEditModal();
}

async function saveFromModal() {
  const ta = $('modalTextarea');
  const val = ta.value;
  if (!textFitsLimits(val, 'Nothing to save')) return;
  const saved = await commit(() => prependItem(val), 'Saved');
  if (!saved) return;
  ta.value = '';
  closeAddModal();
}

// New rows always go on top and become the selection
// Move an existing unpinned row to the top of the unpinned section.
function promoteToTop(uid) {
  const target = items.find((i) => i.uid === uid);
  if (!target) return items;
  const rest = items.filter((i) => i.uid !== uid);
  const pinned = rest.filter((i) => i.pinned);
  const unpinned = rest.filter((i) => !i.pinned);
  return [...pinned, target, ...unpinned];
}

function prependItem(text) {
  // Same text stored once: reuse the existing row and move it up instead of
  // creating a second entry with a new id.
  const dup = items.find((i) => i.text.length === text.length && i.text === text);
  if (dup) {
    if (!dup.pinned) items = withRetention(promoteToTop(dup.uid));
    selectedId = dup.uid;
    return dup;
  }
  const entry = { uid: makeUid(), text, pinned: false };
  // New row on top, then the rolling window evicts from the bottom (unpinned only).
  items = withRetention([entry, ...items]);
  selectedId = entry.uid;
  return entry;
}

/* ------------------------------------------------------------------
 * Clipboard input
 * ------------------------------------------------------------------ */
async function readClipboardText() {
  try {
    const clip = await api.readClipboard();
    return typeof clip === 'string' ? clip : '';
  } catch (err) {
    console.error(err);
    return '';
  }
}

async function pasteFromSystemClipboard() {
  const clip = await readClipboardText();
  if (clip.length === 0) { showToast('Clipboard is empty', true); return; }
  await addTextItem(clip);
}

async function addTextItem(val) {
  if (typeof val !== 'string' || val.length === 0) return;
  if (!textFitsLimits(val)) return;
  closeMenu();
  await commit(() => prependItem(val), 'Pasted as new line');
}

/* ------------------------------------------------------------------
 * Settings
 * ------------------------------------------------------------------ */
async function loadSettings() {
  try {
    settingsCache = await api.getSettings();
  } catch (error) {
    console.error('settings load failed:', error);
  }
}

// The backend reports which accelerator is really registered; when nothing is,
// the dialog must say so instead of showing a hotkey that does nothing.
function renderShortcutStatus() {
  const el = $('shortcutStatus');
  if (!el) return;
  const active = settingsCache && settingsCache.activeShortcut;
  el.textContent = active ? `Active shortcut: ${active}` : 'No shortcut active: that combination is in use.';
  el.classList.toggle('warn', !active);
}

function openSettingsModal() {
  closeMenu();
  const s = settingsCache || {};
  $('shortcutInput').value = s.shortcut || 'CommandOrControl+Shift+V';
  $('autoStartToggle').checked = s.autoStart === true;
  $('autoStartToggle').disabled = s.autoStartSupported === false;
  $('autoCaptureToggle').checked = s.autoCapture === true;
  renderShortcutStatus();
  openOverlay('settingsOverlay');
}

const closeSettingsModal = () => closeOverlay('settingsOverlay');

async function saveSettings() {
  const payload = {
    shortcut: $('shortcutInput').value.trim(),
    autoStart: $('autoStartToggle').checked,
    autoCapture: $('autoCaptureToggle').checked,
  };
  try {
    const result = await api.setSettings(payload);
    settingsCache = result.settings || settingsCache;
    renderShortcutStatus();
    if (!result.shortcutOk) {
      const active = settingsCache.activeShortcut;
      showToast(active ? `Shortcut in use. Using ${active}.` : 'Shortcut in use. No shortcut is active.', true);
      $('shortcutInput').value = settingsCache.shortcut || '';
      return;
    }
    if (result.autoStartOk === false) {
      $('autoStartToggle').checked = settingsCache.autoStart === true;
      showToast("Windows refused the autostart change", true);
      return;
    }
    if (!result.persisted) {
      showToast('Settings applied, but not saved', true);
      return;
    }
    showToast('Settings saved');
    closeSettingsModal();
  } catch (error) {
    console.error('settings save failed:', error);
    showToast("Couldn't save settings", true);
  }
}

/* ------------------------------------------------------------------
 * ⋮ menu
 * ------------------------------------------------------------------ */
function closeMenu() {
  const menu = $('itemMenu');
  if (menu) menu.style.display = 'none';
  menuOpenFor = null;
}

function buildMenuAction(label, iconSrc, handler, danger) {
  const btn = createEl('button', {
    type: 'button',
    className: danger ? 'danger' : '',
    attrs: { 'aria-label': label },
  }, [
    createEl('img', { src: iconSrc, alt: '' }),
    createEl('span', { textContent: label }),
  ]);
  btn.onclick = () => {
    closeMenu();
    handler();
  };
  return btn;
}

// Keep the menu inside the window, preferring above the anchor
function placeMenu(menu, rect) {
  menu.style.display = 'block';
  menu.style.left = '0px';
  menu.style.top = '0px';
  const menuRect = menu.getBoundingClientRect();
  let left = rect.right;
  let top = rect.top - menuRect.height;
  if (left + menuRect.width > window.innerWidth - 4) left = window.innerWidth - menuRect.width - 4;
  if (left < 4) left = 4;
  if (top < 4) top = rect.bottom + 4;
  if (top + menuRect.height > window.innerHeight - 4) top = window.innerHeight - menuRect.height - 4;
  menu.style.left = `${left}px`;
  menu.style.top = `${top}px`;
}

function openMenu(anchorRect, item) {
  const menu = $('itemMenu');
  clearChildren(menu);
  menuOpenFor = item.uid;

  const actions = [
    [item.pinned ? 'Unpin' : 'Pin', 'assets/pin.png', () => togglePin(item.uid), false],
    ['Edit', 'assets/edit.png', () => openEditModal(item.uid), false],
    ['Paste', 'assets/paste.png', () => pasteItem(item.uid), false],
  ];
  if (isUndoAvailable(item.uid)) actions.push(['Undo Paste', 'assets/paste.png', () => undoPaste(), false]);
  actions.push(['Delete', 'assets/delete.png', () => deleteItem(item.uid), true]);

  actions.forEach((args) => menu.appendChild(buildMenuAction(...args)));

  placeMenu(menu, anchorRect);
}

/* ------------------------------------------------------------------
 * Actions
 * ------------------------------------------------------------------ */
async function togglePin(uid) {
  closeMenu();
  const current = findItem(uid);
  if (!current) return;
  const next = !current.pinned;
  if (next && items.filter((i) => i.pinned).length >= MAX_STORED_ITEMS - MAX_ITEMS) {
    showToast(`Pin limit reached: ${fmt(MAX_STORED_ITEMS - MAX_ITEMS)} pinned items max.`, true);
    return;
  }
  await commit(() => patchItem(uid, { pinned: next }), next ? 'Pinned' : 'Unpinned');
}

async function deleteItem(uid) {
  await commit(() => {
    items = items.filter((i) => i.uid !== uid);
    // Drop tabs that lost their last item, instead of showing "(0)".
    sessionTabs = sessionTabs
      .map((t) => ({ ...t, ids: t.ids.filter((x) => x !== uid) }))
      .filter((t) => t.ids.length > 0);
    if (activeTabId !== 'all' && !sessionTabs.some((t) => t.id === activeTabId)) activeTabId = 'all';
    if (selectedId === uid) selectedId = null;
  }, 'Deleted');
}

// "Paste" = replace this row with the text already copied to the system clipboard
async function pasteItem(uid) {
  const target = findItem(uid);
  if (!target) return;
  const clip = await readClipboardText();
  if (clip.length === 0) { showToast('Clipboard is empty', true); return; }
  if (clip === target.text) { showToast('Same text — nothing changed'); return; }
  const before = target.text;
  const saved = await commit(() => patchItem(uid, { text: clip }));
  if (!saved) return;
  lastPaste = { uid, before, after: clip, at: Date.now() };
  if (undoTimer) clearTimeout(undoTimer);
  undoTimer = setTimeout(() => {
    lastPaste = null;
    undoTimer = null;
    closeMenu();
  }, UNDO_WINDOW_MS);
  showToast('Pasted. Undo available for 10s in the ⋮ menu.');
}

async function undoPaste() {
  if (!lastPaste || Date.now() - lastPaste.at > UNDO_WINDOW_MS) {
    lastPaste = null;
    showToast('Undo window expired', true);
    return;
  }
  const { uid, before } = lastPaste;
  if (!findItem(uid)) {
    lastPaste = null;
    showToast('Nothing to undo', true);
    return;
  }
  const saved = await commit(() => patchItem(uid, { text: before }), 'Paste undone');
  if (saved) lastPaste = null;
}

async function hideTool() {
  try {
    await flushSave().catch(() => {});
    if (api && api.hideWindow) await api.hideWindow();
  } catch (e) {
    console.error(e);
  }
}

async function copyText(text) {
  // Auto-paste: hide the tool and paste straight into the previous app's input box
  if (api && api.pasteToActive) {
    try {
      const res = await api.pasteToActive(text);
      if (res === true || (res && res.ok === true)) return true;
      // The keystroke could not be delivered. The backend brings the window back,
      // so say what happened instead of pretending the paste worked.
      if (res && res.copied === true) {
        showToast('Auto-paste failed. Press Ctrl+V to paste.', true);
        return true;
      }
      console.warn('auto-paste failed:', res && res.message);
    } catch (e) {
      console.warn('auto-paste failed, falling back to copy:', e);
    }
  }
  let res = false;
  try {
    res = api && api.copyText ? await api.copyText(text) : false;
  } catch (e) {
    console.warn('clipboard write failed:', e);
  }
  if (res !== true && !(res && res.ok === true)) {
    showToast(`Copy failed${res && res.message ? ` — ${res.message}` : ''}`, true);
    return false;
  }
  await hideTool();
  return true;
}

/* ------------------------------------------------------------------
 * Import / Export
 * ------------------------------------------------------------------ */
// Import .json/.txt files via drag and drop. Returns the overlay's hide().
function setupFileDrop() {
  const overlay = $('dropOverlay');
  let hideTimer = null;

  const hasFiles = (e) => !!e.dataTransfer && Array.from(e.dataTransfer.types || []).includes('Files');

  const hide = () => {
    if (hideTimer) { clearTimeout(hideTimer); hideTimer = null; }
    overlay.classList.remove('show');
  };

  // No enter/leave counter: internal tab and row drags also fire window-level
  // dragleave. dragover keeps the overlay alive and a watchdog hides it.
  const show = () => {
    overlay.classList.add('show');
    if (hideTimer) clearTimeout(hideTimer);
    hideTimer = setTimeout(hide, 400);
  };

  window.addEventListener('dragover', (e) => {
    if (dragTabId) return;   // internal reorder drag, not a file
    if (!hasFiles(e)) return;
    e.preventDefault();
    e.dataTransfer.dropEffect = 'copy';
    show();
  });

  window.addEventListener('dragleave', (e) => {
    // relatedTarget is null only when the pointer really left the window
    if (e.relatedTarget === null) hide();
  });

  window.addEventListener('dragend', hide);

  window.addEventListener('drop', (e) => {
    hide();
    if (!e.dataTransfer || !e.dataTransfer.files || e.dataTransfer.files.length === 0) return;
    e.preventDefault();
    const files = Array.from(e.dataTransfer.files).filter((f) => /\.(json|txt)$/i.test(f.name));
    if (files.length === 0) {
      showToast('Only .json and .txt files can be imported', true);
      return;
    }
    importFromFiles(files);
  });

  return hide;
}

// json array / { items } / legacy "// line" format -> plain texts
// Whole-file ceiling for a single import. A dropped multi-hundred-MB text file
// would otherwise be read into one JS string and JSON.parsed, freezing (or
// OOM-killing) the webview process.
const MAX_IMPORT_BYTES = 64 * 1024 * 1024;
// Above this, don't even attempt JSON.parse: a parse failure on a huge string
// falls back to parseLegacyLines, which walks the same string a second time.
const MAX_JSON_PARSE_CHARS = 16 * 1024 * 1024;

function parseImportContent(content) {
  let imported = null;
  if (content.length <= MAX_JSON_PARSE_CHARS) {
    try {
      const parsed = JSON.parse(content);
      // A valid-JSON file that is not an export (e.g. `123`, `null`, `{}`)
      // must fall back to line parsing instead of yielding zero items.
      if (Array.isArray(parsed)) imported = parsed;
      else if (parsed && Array.isArray(parsed.items)) imported = parsed.items;
    } catch (_) { /* not JSON: fall back to the legacy line format */ }
  }
  if (!imported) imported = parseLegacyLines(content);
  // Keep the pinned flag so a backup restores exactly the state it was
  // exported in (pinned stay pinned, unpinned stay unpinned).
  return normalize(imported).map((item) => ({ text: item.text, pinned: !!item.pinned }));
}

async function importFromFiles(files) {
  if (!Array.isArray(files) || files.length === 0) return;

  // Check the tab limit up front
  if (sessionTabs.length + files.length > MAX_TABS) {
    const room = Math.max(0, MAX_TABS - sessionTabs.length);
    showToast(
      `Tab limit reached: ${fmt(sessionTabs.length)} / ${fmt(MAX_TABS)} used, room for ${fmt(room)} more.`,
      true
    );
    return;
  }

  let room = MAX_ITEMS - unpinnedCount(items);
  if (room <= 0) {
    showToast(
      `History is full: ${fmt(MAX_ITEMS)} unpinned items. Delete some items first.`,
      true
    );
    return;
  }

  // real deep copy for rollback (not a reference)
  const snap = snapshot();

  const stats = { imported: 0, skippedTooLong: 0, longestSkipped: 0, skippedOverCapacity: 0 };
  let firstNewTabId = null;

  for (const file of files) {
    if (room <= 0) {
      stats.skippedOverCapacity += 1;
      continue;
    }
    if (file.size > MAX_IMPORT_BYTES) {
      showToast(`${file.name} is too large: ${fmt(file.size)} / ${fmt(MAX_IMPORT_BYTES)} bytes max.`, true);
      continue;
    }
    let content;
    try {
      content = await readFileAsText(file);
    } catch (e) {
      console.error(e);
      showToast(`Failed to read ${file.name}`, true);
      continue;
    }

    let entries = parseImportContent(content).filter((entry) => {
      const bytes = getByteSize(entry.text);
      if (bytes <= MAX_ITEM_BYTES) return true;
      stats.skippedTooLong += 1;
      stats.longestSkipped = Math.max(stats.longestSkipped, bytes);
      return false;
    });

    // item limit checked up front - only as much as fits is taken
    if (entries.length > room) {
      stats.skippedOverCapacity += entries.length - room;
      entries = entries.slice(0, room);
    }
    if (entries.length === 0) continue;
    room -= entries.length;

    const fresh = entries.map((e) => ({ uid: makeUid(), text: e.text, pinned: e.pinned }));
    // Merge group-wise so the restored file keeps its own pinned/unpinned
    // layout instead of everything landing in the unpinned block.
    items = [
      ...fresh.filter((f) => f.pinned),
      ...items.filter((i) => i.pinned),
      ...fresh.filter((f) => !f.pinned),
      ...items.filter((i) => !i.pinned),
    ];
    stats.imported += fresh.length;

    const id = makeTabId();
    sessionTabs = [
      ...sessionTabs,
      { id, name: file.name.replace(/\.(txt|json)$/i, ''), ids: fresh.map((f) => f.uid) },
    ];
    if (!firstNewTabId) firstNewTabId = id;
  }

  if (stats.imported === 0) {
    restore(snap);
    showToast(importFailureMessage(stats), true);
    return;
  }

  activeTabId = files.length > 1 ? 'all' : (firstNewTabId || 'all');
  try {
    await persist();
    // Stay quiet on a clean import - only warn when a limit was actually hit
    if (stats.skippedTooLong > 0 || stats.skippedOverCapacity > 0) {
      showToast(importSuccessMessage(stats, files.length), true);
    }
  } catch (error) {
    restore(snap, true);
    render();
    console.error('Import save failed:', error);
    showToast(saveMessage(error, "Couldn't save imported items"), true);
  }
}

function importFailureMessage(stats) {
  if (stats.skippedTooLong > 0) {
    return `Nothing imported: ${fmt(stats.skippedTooLong)} item(s) over the 4 MB limit.`;
  }
  if (stats.skippedOverCapacity > 0) {
    return `Nothing imported: item limit ${fmt(items.length)} / ${fmt(MAX_ITEMS)} reached.`;
  }
  return 'Nothing to import';
}

function importSuccessMessage(stats, fileCount) {
  let message = `Imported ${fmt(stats.imported)} items from ${fileCount} file(s)`;
  if (stats.skippedTooLong > 0) message += `; skipped ${fmt(stats.skippedTooLong)} over 4 MB`;
  if (stats.skippedOverCapacity > 0) message += `; skipped ${fmt(stats.skippedOverCapacity)} over the item limit`;
  return message;
}

async function exportToFile() {
  if (items.length === 0) { showToast('Nothing to export', true); return; }

  const source = visibleItems();
  if (source.length === 0) { showToast('Nothing to export in this view', true); return; }

  const longestBytes = source.reduce((max, item) => Math.max(max, itemBytes(item)), 0);
  if (longestBytes > MAX_ITEM_BYTES) {
    showToast(byteLimitMessage("Can't export: an item is", longestBytes), true);
    return;
  }
  if (source.length > MAX_ITEMS) {
    showToast(`Can't export: ${fmt(source.length)} / ${fmt(MAX_ITEMS)} items max.`, true);
    return;
  }

  const fileContent = JSON.stringify(source.map(({ text, pinned }) => ({ text, pinned })), null, 2);

  // Export follows the current view: name the scope in the toast and in the
  // file name so a tab export is not mistaken for a full backup.
  const activeTab = activeTabId === 'all' ? null : sessionTabs.find((t) => t.id === activeTabId);
  const safeScope = (activeTab ? activeTab.name : 'all').replace(/[^\w.-]+/g, '-').slice(0, 40) || 'all';
  const suggestedName = safeScope === 'all'
    ? 'clipboard-backup.json'
    : `clipboard-backup-${safeScope}.json`;

  // Native Save dialog instead of a blob download - large files never get canceled
  try {
    const result = await api.exportFile(fileContent, suggestedName);
    if (!result || result.ok !== true) {
      if (result && result.code === 'canceled') return;
      showToast(SaveError.describe(result && result.code, result && result.message), true);
      return;
    }
    showToast(
      activeTab
        ? `Exported tab "${activeTab.name}": ${fmt(source.length)} of ${fmt(items.length)} items`
        : `Exported all ${fmt(source.length)} items`
    );
  } catch (error) {
    console.error('export failed:', error);
    showToast("Couldn't export data", true);
  }
}

/* ------------------------------------------------------------------
 * Backend -> renderer events
 *
 * Registered at module scope, BEFORE rendererReady() is called: the backend
 * must not be told "I am listening" while nothing is attached yet.
 * rendererReady() also waits for every listen() to resolve (tauri-bridge.js).
 * ------------------------------------------------------------------ */
/* While the app sits in the tray the backend stores captured rows itself.
 * Reload the store so the list is up to date the moment the window is shown
 * again; any local edit still inside the save debounce is written first so the
 * reload can never overwrite it. */
if (typeof api.onHistoryUpdated === 'function') {
  let reloadQueued = false;
  api.onHistoryUpdated(async () => {
    if (reloadQueued) return;
    reloadQueued = true;
    try {
      await flushSave();
      await loadClipboard();
    } catch (error) {
      console.error('history reload failed:', error);
    } finally {
      reloadQueued = false;
    }
  });
}

/* A setting changed outside the dialog (tray toggle or a startup repair). */
if (typeof api.onSettingsChanged === 'function') {
  api.onSettingsChanged((next) => {
    if (!next) return;
    settingsCache = next;
    if (isOverlayOpen('settingsOverlay')) {
      $('autoStartToggle').checked = next.autoStart === true;
      $('autoCaptureToggle').checked = next.autoCapture === true;
      renderShortcutStatus();
    }
  });
}

/* Backend-side warnings (failed autostart registration, ...) reach the user. */
if (typeof api.onNotice === 'function') {
  api.onNotice((info) => {
    if (!info || typeof info.message !== 'string') return;
    showToast(info.message, info.error === true);
  });
}

api.onShortcutFailed((info) => {
  const wanted = (info && info.shortcut) || 'the shortcut';
  const active = info && info.active;
  showToast(
    active
      ? `Shortcut ${wanted} is in use. Using ${active} instead.`
      : `Shortcut ${wanted} is in use. Pick another in Settings.`,
    true
  );
});

// The window is about to hide: write anything still inside the save debounce.
if (typeof api.onFlushNow === 'function') {
  api.onFlushNow(() => { flushSave().catch(() => {}); });
}

/* The app is quitting: the backend waits (with a timeout) for flushComplete()
 * so the last edit before "Quit" is not lost. */
if (typeof api.onAppWillQuit === 'function') {
  api.onAppWillQuit(async () => {
    try {
      await flushSave();
    } catch (error) {
      console.error('flush before quit failed:', error);
    } finally {
      if (typeof api.flushComplete === 'function') api.flushComplete().catch(() => {});
    }
  });
}

/* ------------------------------------------------------------------
 * Lifecycle — one registration per event.
 * Hiding the window must close transient UI and write anything still inside
 * the save debounce; quitting must additionally drop the pending undo timer.
 * ------------------------------------------------------------------ */
function onAppHidden() {
  closeMenu();
  if (hideDropOverlay) hideDropOverlay();
  flushSave().catch(() => {});
}

window.addEventListener('blur', onAppHidden);
document.addEventListener('visibilitychange', () => {
  if (document.visibilityState === 'hidden') onAppHidden();
});
window.addEventListener('beforeunload', () => {
  if (undoTimer) {
    clearTimeout(undoTimer);
    undoTimer = null;
  }
  flushSave().catch(() => {});
});
if (api && typeof api.rendererReady === 'function') {
  api.rendererReady().catch(() => {});
}
