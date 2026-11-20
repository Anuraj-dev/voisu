// The recording Overlay page. It listens: `recording-state` and
// `audio-level` come from the Recording engine (or the tray's "Preview
// overlay"), and `overlay-layout` says where the native side put the pill.
// The one thing it sends back is a window drag, in move mode. Drawing stops
// whenever the pill is idle or showing the check mark, Copied or an error,
// so a hidden overlay costs no frames.
import { listen } from '@tauri-apps/api/event';
import { getCurrentWindow } from '@tauri-apps/api/window';

// Motion from the Linux overlay (voisu-overlay.rs), on the pill's 9 bars.
const BARS = 9;
const CENTRE = (BARS - 1) / 2;
const HISTORY = CENTRE + 1; // newest level at the centre, oldest at the edges
const GAP = 3;
const BAR_RGB = '242, 242, 242';
const ATTACK = 0.65;
const RELEASE = 0.16;
const SWEEP_PERIOD_MS = 1200;
const SWEEP_SIGMA = 1.6;
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
// Hands-free shows a Recording-green dot in place of the two leftmost bars,
// so the state is told by shape and position, not colour alone.
const HANDS_FREE_RGB = '101, 214, 160';
const HANDS_FREE_SLOTS = 2;
// Width of the medium overlay window: the widest text pill plus glow room
// (overlay.rs TEXT_WIDTH + 2 * GLOW_PAD). The native side sizes the window
// for the chosen Overlay size; the page scales everything by the ratio.
const BASE_WIDTH = 336;
// Glow room the pill keeps clear of (overlay.rs GLOW_PAD), at the medium size.
const GLOW_PAD = 8;

const LABELS = {
  moving: 'Overlay: drag it where you want it',
  recording: 'Recording',
  handsFree: 'Recording hands-free',
  processing: 'Processing',
  done: 'Transcript Delivered',
};

const capsule = document.getElementById('capsule');
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
let holdTimer = 0;
let scale = 1;
// The pill's centre as fractions of the window (overlay.rs `Anchor`).
let anchor = { x: 0.5, y: 0.5 };
// Settings' "Move overlay": idle bars in a dashed ring, and draggable.
let moving = false;

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

// Half-sine falloff: 0.45 at the ends, 1.0 at the centre.
function edgeAlpha(i) {
  return 0.45 + 0.55 * Math.sin((Math.PI * (i + 0.5)) / BARS);
}

function barLevel(i) {
  return history[Math.abs(i - CENTRE)];
}

function sweepBrightness(i, now) {
  if (reducedMotion.matches) return REDUCED_BRIGHTNESS;
  const travel = BARS + 6 * SWEEP_SIGMA;
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
  const barWidth = (width - gap * (BARS - 1)) / BARS;
  const drawable = height - 2;
  const floor = Math.max(0.15 * drawable, 1.5);
  const first = state === 'handsFree' ? HANDS_FREE_SLOTS : 0;

  if (state === 'handsFree') {
    const r = Math.min(3.5 * scale, (HANDS_FREE_SLOTS * (barWidth + gap) - gap) / 2);
    ctx.fillStyle = `rgb(${HANDS_FREE_RGB})`;
    ctx.beginPath();
    ctx.arc(r, height / 2, r, 0, 2 * Math.PI);
    ctx.fill();
  }

  for (let i = first; i < BARS; i += 1) {
    let barHeight = floor;
    let alpha = edgeAlpha(i);
    if (state === 'processing') {
      alpha *= sweepBrightness(i, now);
    } else {
      barHeight = floor + barLevel(i) * (drawable - floor);
    }
    const x = i * (barWidth + gap);
    const y = (height - barHeight) / 2;
    ctx.fillStyle = `rgba(${BAR_RGB}, ${alpha})`;
    ctx.beginPath();
    ctx.roundRect(x, y, barWidth, barHeight, Math.min(barWidth, barHeight) / 2);
    ctx.fill();
  }
}

const metering = () => state === 'recording' || state === 'handsFree';

// Recording redraws once per level event (W2 caps those at 30 Hz); the
// processing sweep runs its own loop, capped at 30 fps. Move mode draws its
// idle bars once.
function onFrame(now) {
  frame = 0;
  if (state === 'processing') {
    if (now - lastFrame >= FRAME_MS) {
      lastFrame = now;
      draw(now);
    }
    frame = requestAnimationFrame(onFrame);
  } else if (metering() || (state === 'idle' && moving)) {
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
  state = next;
  capsule.className = `capsule ${next}`;
  capsule.classList.toggle('moving', moving);

  if (next === 'error') {
    stopDrawing();
    messageText.textContent = text || 'Something went wrong';
    message.hidden = false;
    check.hidden = true;
    copied.hidden = true;
    capsule.setAttribute('aria-label', `Error: ${messageText.textContent}`);
    holdTimer = setTimeout(clear, ERROR_HOLD_MS);
    return;
  }
  message.hidden = true;
  check.hidden = next !== 'done';
  copied.hidden = next !== 'copied';
  if (next === 'copied') {
    stopDrawing();
    copied.textContent = text || 'Copied';
    capsule.setAttribute('aria-label', copied.textContent);
    holdTimer = setTimeout(clear, COPIED_HOLD_MS);
    return;
  }
  if (next === 'done') {
    stopDrawing();
    capsule.setAttribute('aria-label', text || LABELS.done);
    holdTimer = setTimeout(clear, DONE_HOLD_MS);
    return;
  }
  capsule.setAttribute('aria-label', LABELS[moving ? 'moving' : next] ?? '');
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
// The native side resizes the window when the pill is shown at another
// Overlay size, or moves it to a monitor with another DPI.
window.addEventListener('resize', () => {
  fitScale();
  place();
  if (metering() || state === 'processing' || moving) requestDraw();
});
// Text widens the pill and the size changes with it: re-centre on the anchor.
new ResizeObserver(place).observe(capsule);

// In move mode a press on the pill hands the drag to the OS, which moves the
// whole window; Settings' Done then reads where the window ended up.
capsule.addEventListener('pointerdown', (event) => {
  if (!moving || event.button !== 0) return;
  event.preventDefault();
  getCurrentWindow()
    .startDragging()
    .catch((error) => console.warn('overlay drag failed', error));
});

// Sent before each placed show, and when move mode starts or ends.
listen('overlay-layout', ({ payload }) => {
  anchor = { x: fraction(payload.x), y: fraction(payload.y) };
  place();
  setMoving(Boolean(payload.moving));
});
listen('recording-state', ({ payload }) => setState(payload.state, payload.message));
listen('audio-level', ({ payload }) => onLevel(payload.level));
