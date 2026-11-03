/* Small DOM helpers shared by renderer.js (no app data state).
 * Loaded as a classic script before renderer.js and exposed on window.rendererDom. */
(function () {
  'use strict';

  const $ = (id) => document.getElementById(id);

  const on = (id, event, handler, options) => {
    const el = $(id);
    if (el) el.addEventListener(event, handler, options);
  };

  // Overlay-based modals all share the same "show" class contract
  const isOverlayOpen = (id) => !!$(id) && $(id).classList.contains('show');
  const openOverlay = (id) => $(id) && $(id).classList.add('show');
  const closeOverlay = (id) => $(id) && $(id).classList.remove('show');

  // Close the modal when the backdrop itself (not its content) is pressed
  const closeOnBackdrop = (id, close) =>
    on(id, 'mousedown', (e) => { if (e.target.id === id) close(); });

  const focusSoon = (el) => { if (el) setTimeout(() => el.focus(), 0); };

  function createEl(tag, props = {}, children = []) {
    const el = document.createElement(tag);
    for (const [key, value] of Object.entries(props)) {
      if (value === undefined || value === null) continue;
      if (key === 'attrs') for (const [name, val] of Object.entries(value)) el.setAttribute(name, val);
      else if (key === 'className') el.className = value;
      else el[key] = value;
    }
    children.filter(Boolean).forEach((child) => el.appendChild(child));
    return el;
  }

  const clearChildren = (el) => { if (el) el.innerHTML = ''; };

  function isTypingTarget() {
    const el = document.activeElement;
    if (!el) return false;
    return el.tagName === 'INPUT' || el.tagName === 'TEXTAREA' || el.isContentEditable === true;
  }

  // Activate a custom "button" with Enter / Space like a native one
  const activateOnEnterOrSpace = (el) => {
    el.onkeydown = (event) => {
      if (event.key !== 'Enter' && event.key !== ' ') return;
      event.preventDefault();
      el.click();
    };
  };

  // Ignore the click that follows a "click outside to dismiss" pointerdown
  function swallowNextClick() {
    const handler = (ev) => { ev.stopPropagation(); ev.preventDefault(); };
    document.addEventListener('click', handler, { capture: true, once: true });
    setTimeout(() => document.removeEventListener('click', handler, true), 300);
  }

  // Toast (auto-dismiss)
  let toastTimer = null;
  function showToast(message, isError) {
    const toast = $('toast');
    if (!toast) return;
    toast.textContent = message;
    toast.className = isError ? 'show error' : 'show';
    if (toastTimer) clearTimeout(toastTimer);
    toastTimer = setTimeout(() => { toast.className = ''; }, 2200);
  }

  window.rendererDom = {
    $, on, isOverlayOpen, openOverlay, closeOverlay, closeOnBackdrop, focusSoon,
    createEl, clearChildren, isTypingTarget, activateOnEnterOrSpace, swallowNextClick, showToast,
  };
})();
