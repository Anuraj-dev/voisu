// The recording Overlay page. It listens: `recording-state` and
// `audio-level` come from the Recording engine (or the tray's "Preview
// overlay"), and `overlay-layout` says where the native side put the pill.
// It sends back that it drew a layout and, in move mode, the drag and the
// pill's ✓ (save) or ✗ (cancel). Drawing stops whenever the pill is idle or
// showing the check mark, Copied or an error, so a hidden overlay costs no
// frames.
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { createDrag } from './drag.js';
import { startsDrag } from './press.js';
import { startingAlpha } from './starting.js';

// Motion from the Linux overlay (voisu-overlay.rs), on the pill's thin
// sound bars: 26 white 2 px lines, 2 px apart, so the row reads as a fine
// waveform. The move pill's narrower meter draws as many as fit.
const BARS = 26;
const HISTORY = Math.ceil(BARS / 2); // newest level at the centre, oldest at the edges
const GAP = 2;
const BAR_WIDTH = 2;
const BAR_RGB = '242, 242, 242';
const ATTACK = 0.65;
const RELEASE = 0.16;
const SWEEP_PERIOD_MS = 1200;
const SWEEP_SIGMA = 3.7; // about the same share of the row as 1.6 of 9 bars
const SWEEP_BASE = 0.35;
const REDUCED_BRIGHTNESS = 0.6;
const FRAME_MS = 1000 / 30;
// The native side keeps an error up for 3 s even if `idle` follows at once
// (overlay.rs ERROR_DISPLAY); the page holds it a little longer so it never
// blanks before the window hides.
const ERROR_HOLD_MS = 3500;
// The Delivered check mark clears with the window (overlay.rs DONE_DISPLAY,
// 600 ms): it confirms a paste that already landed, so it stays brief.
const DONE_HOLD_MS = 600;
// Copied (macOS could only put the Transcript on the clipboard): the native
// side hides it after 2 s (overlay.rs COPIED_DISPLAY); like an error, the
// page holds it a little longer so the text never blanks first.
const COPIED_HOLD_MS = 2500;
// Hands-free shows a Recording-green dot in place of the leftmost bars,
// so the state is told by shape and position, not colour alone.
const HANDS_FREE_RGB = '101, 214, 160';
const HANDS_FREE_SLOTS = 4;
// Width of the overlay window: the widest text pill plus glow room
// (overlay.rs TEXT_WIDTH + 2 * GLOW_PAD). The native side shrinks the window
// only on a work area too small for it; the page scales everything by the
// ratio, so it is 1 everywhere else.
const BASE_WIDTH = 336;
// Glow room the pill keeps clear of (overlay.rs GLOW_PAD), unscaled.
const GLOW_PAD = 8;

const LABELS = {
  moving: 'Overlay: drag it where you want it',
  starting: 'Opening the microphone',
  recording: 'Recording',
  handsFree: 'Recording hands-free',
  processing: 'Processing',
  done: 'Transcript Delivered',
};

const capsule = document.getElementById('capsule');
const face = document.getElementById('face');
const canvas = document.getElementById('meter');
const message = document.getElementById('message');
const messageText = document.getElementById('message-text');
const copied = document.getElementById('copied');
const check = document.getElementById('check');
const ctx = canvas.getContext('2d');
const reducedMotion = matchMedia('(prefers-reduced-motion: reduce)');

let state = 'idle';
let history = new Array(HISTORY).fill(0);
let smoothed = 0;
let frame = 0;
let lastFrame = 0;
let sweepStart = 0;
let breathStart = 0;
let holdTimer = 0;
let scale = 1;
// The pill's centre as fractions of the window (overlay.rs `Anchor`).
let anchor = { x: 0.5, y: 0.5 };
// Settings' "Move overlay": idle bars in a dashed ring, and draggable.
let moving = false;
// Which move mode is on (overlay.rs `Presenter::move_id`): ✓ and ✗ name
// it, so one that lands after that move ended cannot finish the next.
let moveId = null;

function fitScale() {
  const next = window.innerWidth / BASE_WIDTH;
  scale = Number.isFinite(next) && next > 0 ? next : 1;
  document.documentElement.style.setProperty('--s', String(scale));
}

// Puts the pill's centre on the anchor. A pill widened by text grows from
// that centre and shifts only as far as it must to stay clear of the glow
// room (the native side already fitted the window to the work area).
function place() {
  const pad = GLOW_PAD * scale;
  const fit = (centre, size, room) => {
    const max = room - pad - size;
    return max < pad ? (room - size) / 2 : Math.min(Math.max(centre - size / 2, pad), max);
  };
  const x = fit(anchor.x * window.innerWidth, capsule.offsetWidth, window.innerWidth);
  const y = fit(anchor.y * window.innerHeight, capsule.offsetHeight, window.innerHeight);
  capsule.style.setProperty('--x', `${x}px`);
  capsule.style.setProperty('--y', `${y}px`);
}

// Scaled CSS sizes are fractional, so the backing store follows the exact
// layout box (not the integer clientWidth) to stay one texel per device pixel.
function fitCanvas() {
  const dpr = window.devicePixelRatio || 1;
  const box = canvas.getBoundingClientRect();
  const width = Math.round(box.width * dpr);
  const height = Math.round(box.height * dpr);
  if (canvas.width !== width || canvas.height !== height) {
    canvas.width = width;
    canvas.height = height;
  }
  ctx.setTransform(width / box.width || dpr, 0, 0, height / box.height || dpr, 0, 0);
  return box;
}

// Half-sine falloff over a row of `count` bars: 0.45 at the ends, 1.0 at the
// centre.
function edgeAlpha(i, count) {
  return 0.45 + 0.55 * Math.sin((Math.PI * (i + 0.5)) / count);
}

// An even row has two centre bars; both show the newest level.
function barLevel(i, count) {
  return history[Math.floor(Math.abs(i - (count - 1) / 2))];
}

function sweepBrightness(i, count, now) {
  if (reducedMotion.matches) return REDUCED_BRIGHTNESS;
  const travel = count + 6 * SWEEP_SIGMA;
  const progress = ((now - sweepStart) % SWEEP_PERIOD_MS) / SWEEP_PERIOD_MS;
  const position = progress * travel - 3 * SWEEP_SIGMA;
  const d = i + 0.5 - position;
  const bump = Math.exp(-(d * d) / (2 * SWEEP_SIGMA * SWEEP_SIGMA));
  return SWEEP_BASE + (1 - SWEEP_BASE) * bump;
}

function draw(now) {
  const { width, height } = fitCanvas();
  ctx.clearRect(0, 0, width, height);

  const gap = GAP * scale;
  const barWidth = BAR_WIDTH * scale;
  // Fixed thin bars, as many as fit (all 26 in the plain pill), the row
  // centred in the meter.
  const count = Math.min(BARS, Math.floor((width + gap) / (barWidth + gap) + 1e-6));
  const left = Math.max(0, (width - (barWidth * count + gap * (count - 1))) / 2);
  const drawable = height - 2;
  const floor = Math.max(0.15 * drawable, 1.5);
  const first = state === 'handsFree' ? HANDS_FREE_SLOTS : 0;

  if (state === 'handsFree') {
    const r = Math.min(3.5 * scale, (HANDS_FREE_SLOTS * (barWidth + gap) - gap) / 2);
    ctx.fillStyle = `rgb(${HANDS_FREE_RGB})`;
    ctx.beginPath();
    ctx.arc(left + r, height / 2, r, 0, 2 * Math.PI);
    ctx.fill();
  }

  for (let i = first; i < count; i += 1) {
    let barHeight = floor;
    let alpha = edgeAlpha(i, count);
    if (state === 'processing') {
      alpha *= sweepBrightness(i, count, now);
    } else if (state === 'starting') {
      // Flat and dim: nothing is heard until the live bars take over.
      alpha *= startingAlpha(now - breathStart, reducedMotion.matches);
    } else {
      barHeight = floor + barLevel(i, count) * (drawable - floor);
    }
    const x = left + i * (barWidth + gap);
    const y = (height - barHeight) / 2;
    ctx.fillStyle = `rgba(${BAR_RGB}, ${alpha})`;
    ctx.beginPath();
    ctx.roundRect(x, y, barWidth, barHeight, Math.min(barWidth, barHeight) / 2);
    ctx.fill();
  }
}

const metering = () => state === 'recording' || state === 'handsFree';
// The processing sweep and the starting breath run their own loop; a still
// starting cue (reduced motion) draws once.
const animating = () => state === 'processing' || (state === 'starting' && !reducedMotion.matches);

// Recording redraws once per level event (W2 caps those at 30 Hz); the
// animations are capped at 30 fps. Move mode draws its idle bars once.
function onFrame(now) {
  frame = 0;
  if (animating()) {
    if (now - lastFrame >= FRAME_MS) {
      lastFrame = now;
      draw(now);
    }
    frame = requestAnimationFrame(onFrame);
  } else if (metering() || state === 'starting' || (state === 'idle' && moving)) {
    draw(now);
  }
}

function requestDraw() {
  if (!frame) frame = requestAnimationFrame(onFrame);
}

function stopDrawing() {
  if (frame) cancelAnimationFrame(frame);
  frame = 0;
}

function setState(next, text) {
  // `done`, `copied` and `error` hold until their own timer, even if `idle`
  // follows.
  if (next === 'idle' && (state === 'error' || state === 'done' || state === 'copied')) return;
  // A press still opening the mic leaves move mode alone (overlay.rs
  // `Presenter::on_state`): it may be cancelled before its first audio.
  if (moving && next === 'starting') return;
  // A real Recording ends move mode (the native side has already left it).
  if (next !== 'idle') moving = false;
  clearTimeout(holdTimer);
  const wasMetering = metering();
  if ((next === 'recording' || next === 'handsFree') && !wasMetering) {
    history = new Array(HISTORY).fill(0);
    smoothed = 0;
  }
  if (next === 'processing' && state !== 'processing') {
    sweepStart = performance.now();
    lastFrame = 0;
  }
  if (next === 'starting' && state !== 'starting') {
    breathStart = performance.now();
    lastFrame = 0;
  }
  state = next;
  capsule.className = `capsule ${next}`;
  capsule.classList.toggle('moving', moving);

  if (next === 'error') {
    stopDrawing();
    messageText.textContent = text || 'Something went wrong';
    message.hidden = false;
    check.hidden = true;
    copied.hidden = true;
    face.setAttribute('aria-label', `Error: ${messageText.textContent}`);
    holdTimer = setTimeout(clear, ERROR_HOLD_MS);
    return;
  }
  message.hidden = true;
  check.hidden = next !== 'done';
  copied.hidden = next !== 'copied';
  if (next === 'copied') {
    stopDrawing();
    copied.textContent = text || 'Copied';
    face.setAttribute('aria-label', copied.textContent);
    holdTimer = setTimeout(clear, COPIED_HOLD_MS);
    return;
  }
  if (next === 'done') {
    stopDrawing();
    face.setAttribute('aria-label', text || LABELS.done);
    holdTimer = setTimeout(clear, DONE_HOLD_MS);
    return;
  }
  face.setAttribute('aria-label', LABELS[moving ? 'moving' : next] ?? '');
  if (next === 'idle') {
    // Leave nothing behind for the next show to flash before its first frame.
    stopDrawing();
    ctx.clearRect(0, 0, canvas.width, canvas.height);
    messageText.textContent = '';
    if (moving) {
      history = new Array(HISTORY).fill(0);
      requestDraw();
    }
  } else {
    requestDraw();
  }
}

function clear() {
  state = 'cleared';
  setState('idle');
}

// Move mode starts or ends. Either way the pill drops whatever it showed and
// goes idle, with the move look only while moving.
function setMoving(next) {
  if (next === moving) return;
  moving = next;
  clear();
}

function onLevel(level) {
  if (!metering()) return;
  const target = Math.min(Math.max(Number(level) || 0, 0), 1);
  smoothed += (target - smoothed) * (target > smoothed ? ATTACK : RELEASE);
  history.pop();
  history.unshift(smoothed);
  requestDraw();
}

const fraction = (value) => {
  const n = Number(value);
  return Number.isFinite(n) ? Math.min(Math.max(n, 0), 1) : 0.5;
};

fitScale();
place();
// The native side resizes the window when it moves it to a monitor with
// another DPI or one too small for it.
window.addEventListener('resize', () => {
  fitScale();
  place();
  if (metering() || state === 'processing' || state === 'starting' || moving) requestDraw();
});
// Turning reduced motion on or off mid-cue starts or settles the breath.
reducedMotion.addEventListener('change', () => {
  if (state === 'starting') requestDraw();
});
// Text widens the pill and the size changes with it: re-centre on the anchor.
new ResizeObserver(place).observe(capsule);

// In move mode the window follows the pointer while the pill is held (see
// drag.js). Not the OS move loop: that can hand this window the keyboard,
// and on the Mac it needs the press event.
const drag = createDrag((start, id) => invoke('drag_overlay', { moveId: id, start }));

capsule.addEventListener('pointerdown', (event) => {
  if (!startsDrag(event, moving) || moveId == null) return;
  event.preventDefault();
  capsule.setPointerCapture(event.pointerId);
  drag.press(moveId);
});
capsule.addEventListener('pointermove', () => drag.move());
capsule.addEventListener('pointerup', () => drag.release());
capsule.addEventListener('lostpointercapture', () => drag.release());

// ✓ keeps the spot the pill was dropped at, ✗ puts it back. The native side
// ends move mode and hides the pill either way, and Settings shows any
// error. One call at a time, so a click and a key together finish once.
// The drop's last move lands first, so ✓ saves where the pill was let go.
let finishing = false;
async function finishMove(save) {
  if (!moving || moveId == null || finishing) return;
  finishing = true;
  const id = moveId;
  try {
    await drag.idle();
    await invoke('finish_overlay_move', { moveId: id, save });
  } catch (error) {
    console.warn('overlay move finish failed', error);
  } finally {
    finishing = false;
  }
}
document.getElementById('move-save').addEventListener('click', () => finishMove(true));
document.getElementById('move-cancel').addEventListener('click', () => finishMove(false));
// Clicking ✓ or ✗ leaves keyboard focus where it was, so no stray focus ring
// and Enter is never taken by a button that happened to be clicked.
for (const button of capsule.querySelectorAll('.move-button')) {
  button.addEventListener('mousedown', (event) => event.preventDefault());
}

// Should the pill ever take the keyboard, Enter saves and Esc cancels, as in
// Settings. Enter on a focused ✓ or ✗ is left to that button. A key held
// down from before arrives only as repeats, so repeats never finish.
window.addEventListener('keydown', (event) => {
  if (!moving || event.defaultPrevented || event.repeat) return;
  if (event.key === 'Escape') {
    event.preventDefault();
    finishMove(false);
  } else if (event.key === 'Enter' && !event.target.closest?.('button')) {
    event.preventDefault();
    finishMove(true);
  }
});

// Where to draw the pill, pushed before each placed show and when move mode
// starts or ends. The native side holds a show until the page says it drew
// the newest layout (or briefly times out), so the pill never flashes at the
// old spot. `seq` orders pushes against the pull below.
let drawnSeq = -1;
function applyLayout(payload) {
  if (!payload || !(payload.seq > drawnSeq)) return;
  drawnSeq = payload.seq;
  anchor = { x: fraction(payload.x), y: fraction(payload.y) };
  moveId = payload.moving ? (payload.move_id ?? null) : null;
  // The move look first, so the pill is placed at the width it is drawn.
  setMoving(Boolean(payload.moving));
  place();
  if (!moving) drag.cancel();
  invoke('overlay_layout_drawn', { seq: payload.seq }).catch(() => {});
}
listen('overlay-layout', ({ payload }) => applyLayout(payload)).then(() =>
  // A push sent before the listener was up is caught by pulling once.
  invoke('overlay_layout').then(applyLayout, () => {}),
);
listen('recording-state', ({ payload }) => setState(payload.state, payload.message));
listen('audio-level', ({ payload }) => onLevel(payload.level));
