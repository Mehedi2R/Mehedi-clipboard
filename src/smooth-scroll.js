/* smooth-scroll.js — the single scroll controller for #clipboardList.
 *
 * Why this file exists
 * --------------------
 * WebView2 turns one wheel notch into a raw, unanimated jump of ~3 text lines.
 * In a 380x450 window with 42 px rows that is a third of the viewport per notch,
 * and a trackpad flick fires dozens of wheel events in a row, so the list
 * "suddenly runs away" — the exact symptom being fixed here.
 *
 * CSS `scroll-behavior: smooth` cannot fix it: the list is virtualized, so the
 * browser animation fights row recycling and stutters.
 *
 * Three rules make the motion predictable:
 *   1. Swallow the native wheel scroll (non-passive listener + preventDefault).
 *   2. Keep our own `target` position and ease the real scrollTop toward it,
 *      frame-rate independent, once per animation frame.
 *   3. HARD SPEED LIMIT: the target stays close to the real position and every
 *      animation frame has a travel ceiling. Event bursts can never build up a
 *      long momentum queue, so the list stops shortly after the gesture stops.
 *
 * Self-contained classic script, loaded before renderer.js, exposed as
 * All programmatic list movement is routed through window.smoothScroll too.
 */
(function () {
  'use strict';

  var LIST_ID = 'clipboardList';

  /* ---------------------------- tuning ---------------------------- */

  // Row pitch comes from the shared reader in renderer-utils.js, so CSS stays
  // the single source of truth and the scroll math can never drift.
  function readRowH() {
    var utils = window.rendererUtils;
    return utils && utils.readRowH ? utils.readRowH() : 42;
  }

  var ROW_H = readRowH();
  var LINE_HEIGHT = 40;                  // px per "line" when deltaMode === 1
  var WHEEL_GAIN = 0.55;                 // fraction of a notch that becomes travel
  var MAX_STEP = ROW_H * 1.6;            // one event cannot add more than one row
  var MAX_LOOKAHEAD = ROW_H * 3;         // very short queue: no delayed runaway
  var MAX_SPEED = ROW_H * 22;            // px/second ceiling — the anti-dash rule
  var EASE = 0.26;                       // fraction of remaining distance / frame
  var MIN_DELTA = 0.4;                   // snap-and-stop threshold

  function syncRowMetrics() {
    ROW_H = readRowH();
    MAX_STEP = ROW_H * 1.6;
    MAX_LOOKAHEAD = ROW_H * 3;
    MAX_SPEED = ROW_H * 22;
  }
  window.addEventListener('resize', syncRowMetrics, { passive: true });

  var reduceMotion = false;
  try {
    reduceMotion = window.matchMedia('(prefers-reduced-motion: reduce)').matches;
  } catch (_) { /* matchMedia missing: keep animations on */ }

  /* ---------------------------- state ---------------------------- */

  var el = null;        // scroll container
  var target = 0;       // position we are easing toward
  var frame = 0;        // rAF handle, 0 when idle
  var lastTime = 0;     // timestamp of previous frame
  var attached = false; // attach is idempotent even if this script is reloaded

  function clamp(v, lo, hi) { return v < lo ? lo : v > hi ? hi : v; }
  function maxScroll() { return Math.max(0, el.scrollHeight - el.clientHeight); }

  /** The controller is the sole writer. Browser scroll anchoring is disabled in
   *  CSS, so no asynchronous second correction can race this assignment. */
  function write(top) {
    el.scrollTop = top;
  }

  /** Drop the animation and adopt whatever position the container is at. */
  function resync() {
    if (frame) { cancelAnimationFrame(frame); frame = 0; }
    lastTime = 0;
    target = el ? el.scrollTop : 0;
  }

  /** Cancel the animation and land on an absolute offset immediately. */
  function hardSet(top, limit) {
    resync();
    write(clamp(top, 0, limit === undefined ? maxScroll() : limit));
    target = el.scrollTop;
  }

  function step(ts) {
    frame = 0;
    if (!el) return;

    target = clamp(target, 0, maxScroll());
    var remaining = target - el.scrollTop;

    if (Math.abs(remaining) <= MIN_DELTA) {
      write(target);
      lastTime = 0;
      return;
    }

    // Frame-rate independent easing: 60 Hz and 144 Hz feel identical.
    var elapsed = lastTime ? Math.min(ts - lastTime, 50) : 16.67;
    lastTime = ts;
    var factor = 1 - Math.pow(1 - EASE, elapsed / 16.67);

    var travel = remaining * factor;
    // Never exceed the speed ceiling, even when the target is far away.
    var cap = MAX_SPEED * (elapsed / 1000);
    if (Math.abs(travel) > cap) travel = Math.sign(travel) * cap;
    // But always advance a pixel, or the tail crawls forever.
    if (Math.abs(travel) < 1) travel = remaining > 0 ? 1 : -1;

    write(clamp(el.scrollTop + travel, 0, maxScroll()));
    frame = requestAnimationFrame(step);
  }

  function start() {
    if (!frame) frame = requestAnimationFrame(step);
  }

  /** Normalize a wheel event to pixels, whatever unit the OS reported. */
  function pixelDelta(e) {
    var dy = e.deltaY;
    if (e.deltaMode === 1) dy *= LINE_HEIGHT;           // lines
    else if (e.deltaMode === 2) dy *= el.clientHeight;  // pages
    return dy;
  }

  function onWheel(e) {
    if (!el) return;

    // Trackpad pinch arrives as ctrl+wheel; never let it page-zoom the app.
    if (e.ctrlKey) { e.preventDefault(); return; }
    if (!e.deltaY) return;

    var limit = maxScroll();
    if (limit <= 0) return;

    var dy = pixelDelta(e);
    // At either end let the event through so overscroll-behavior stays native.
    if ((dy < 0 && el.scrollTop <= 0) || (dy > 0 && el.scrollTop >= limit)) return;

    e.preventDefault();

    var travel = clamp(dy * WHEEL_GAIN, -MAX_STEP, MAX_STEP);

    // Reduced motion means no easing, not an uncapped native-sized jump.
    if (reduceMotion) { hardSet(el.scrollTop + travel, limit); return; }

    target = clamp(target + travel, 0, limit);
    // Cap the queued momentum: this is what stops the runaway dash.
    target = clamp(target, el.scrollTop - MAX_LOOKAHEAD, el.scrollTop + MAX_LOOKAHEAD);

    start();
  }

  /* ------------------------- public helpers ------------------------- */

  /** Animate to an absolute offset (keyboard selection navigation). */
  function scrollTo(top) {
    if (!el) return;
    if (reduceMotion) { hardSet(top); return; }
    target = clamp(top, 0, maxScroll());
    lastTime = 0;
    start();
  }

  /** Instant relative move (drag auto-scroll). */
  function scrollBy(delta) {
    if (el) hardSet(el.scrollTop + delta);
  }

  /** Instant absolute move (clear/reset and virtual-list bounds correction). */
  function jumpTo(top) {
    if (el) hardSet(top);
  }

  /** Keep the current position inside a data-derived virtual-list boundary. */
  function clampTo(maximum) {
    if (el && el.scrollTop > maximum) jumpTo(maximum);
  }

  function stop() { resync(); }

  /* ---------------------------- wiring ---------------------------- */

  function attach() {
    if (attached) return;
    el = document.getElementById(LIST_ID);
    if (!el) return;
    attached = true;

    target = el.scrollTop;
    // Non-passive: a passive listener cannot preventDefault, and without that
    // the native jump happens too and the motion looks doubled.
    el.addEventListener('wheel', onWheel, { passive: false });

    // A row drag owns the scroll position while it runs.
    el.addEventListener('pointerdown', stop, { passive: true });

    // Scrollbar thumb, keyboard scrolling and touch panning all bypass us.
    el.addEventListener('scroll', function () {
      if (!frame) target = el.scrollTop;
    }, { passive: true });

    window.addEventListener('blur', stop);
  }

  if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', attach);
  else attach();

  window.smoothScroll = {
    scrollTo: scrollTo,
    scrollBy: scrollBy,
    jumpTo: jumpTo,
    clampTo: clampTo,
    stop: stop,
    target: function () { return target; },
  };
})();
