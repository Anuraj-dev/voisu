// The recording Overlay page. It only listens: `recording-state` and
// `audio-level` come from the Recording engine (or the tray's "Preview
// overlay"), and nothing is sent back. Drawing stops whenever the capsule is
// idle or showing an error, so a hidden overlay costs no frames.
import { listen } from '@tauri-apps/api/event';

// Geometry and motion from the Linux overlay (voisu-overlay.rs).
const BARS = 44;
const HISTORY = BARS / 2; // newest level at the centre, oldest at the edges
const GAP = 2;
const BAR_RGB = '242, 242, 242';
const ATTACK = 0.65;
const RELEASE = 0.16;
const SWEEP_PERIOD_MS = 1200;
const SWEEP_SIGMA = 4.4;
const SWEEP_BASE = 0.35;
const REDUCED_BRIGHTNESS = 0.6;
const FRAME_MS = 1000 / 30;
// Hands-free shows a Recording-green dot in place of the leftmost bars, so the
// state is told by shape and position, not colour alone.
const HANDS_FREE_RGB = '101, 214, 160';
const HANDS_FREE_SLOTS = 3;

const LABELS = {
  recording: 'Recording',
  handsFree: 'Recording hands-free',
  processing: 'Processing',
};

const capsule = document.getElementById('capsule');
const canvas = document.getElementById('meter');
const message = document.getElementById('message');
const messageText = document.getElementById('message-text');
const ctx = canvas.getContext('2d');
const reducedMotion = matchMedia('(prefers-reduced-motion: reduce)');

let state = 'idle';
let history = new Array(HISTORY).fill(0);
let smoothed = 0;
let frame = 0;
let lastFrame = 0;
let sweepStart = 0;

function fitCanvas() {
  const dpr = window.devicePixelRatio || 1;
  const width = Math.round(canvas.clientWidth * dpr);
  const height = Math.round(canvas.clientHeight * dpr);
  if (canvas.width !== width || canvas.height !== height) {
    canvas.width = width;
    canvas.height = height;
  }
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
}

// Half-sine falloff: 0.45 at the ends, 1.0 at the centre.
function edgeAlpha(i) {
  return 0.45 + 0.55 * Math.sin((Math.PI * (i + 0.5)) / BARS);
}

function barLevel(i) {
  const fromCentre = i < HISTORY ? HISTORY - 1 - i : i - HISTORY;
  return history[fromCentre];
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
  fitCanvas();
  const width = canvas.clientWidth;
  const height = canvas.clientHeight;
  ctx.clearRect(0, 0, width, height);

  const barWidth = (width - GAP * (BARS - 1)) / BARS;
  const drawable = height - 2;
  const floor = Math.max(0.1 * drawable, 1.5);
  const first = state === 'handsFree' ? HANDS_FREE_SLOTS : 0;

  if (state === 'handsFree') {
    const r = 3.5;
    ctx.fillStyle = `rgb(${HANDS_FREE_RGB})`;
    ctx.beginPath();
    ctx.arc(r + 1, height / 2, r, 0, 2 * Math.PI);
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
    const x = i * (barWidth + GAP);
    const y = (height - barHeight) / 2;
    ctx.fillStyle = `rgba(${BAR_RGB}, ${alpha})`;
    ctx.beginPath();
    ctx.roundRect(x, y, barWidth, barHeight, Math.min(barWidth, barHeight) / 2);
    ctx.fill();
  }
}

// Recording redraws once per level event (W2 caps those at 30 Hz); the
// processing sweep runs its own loop, capped at 30 fps.
function onFrame(now) {
  frame = 0;
  if (state === 'processing') {
    if (now - lastFrame >= FRAME_MS) {
      lastFrame = now;
      draw(now);
    }
    frame = requestAnimationFrame(onFrame);
  } else if (state === 'recording' || state === 'handsFree') {
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
  const wasMetering = state === 'recording' || state === 'handsFree';
  const metering = next === 'recording' || next === 'handsFree';
  if (metering && !wasMetering) {
    history = new Array(HISTORY).fill(0);
    smoothed = 0;
  }
  if (next === 'processing' && state !== 'processing') {
    sweepStart = performance.now();
    lastFrame = 0;
  }
  state = next;
  capsule.className = `capsule ${next}`;

  if (next === 'error') {
    stopDrawing();
    messageText.textContent = text || 'Something went wrong';
    message.hidden = false;
    capsule.setAttribute('aria-label', `Error: ${messageText.textContent}`);
    return;
  }
  message.hidden = true;
  capsule.setAttribute('aria-label', LABELS[next] ?? '');
  if (next === 'idle') {
    stopDrawing();
  } else {
    requestDraw();
  }
}

function onLevel(level) {
  if (state !== 'recording' && state !== 'handsFree') return;
  const target = Math.min(Math.max(Number(level) || 0, 0), 1);
  smoothed += (target - smoothed) * (target > smoothed ? ATTACK : RELEASE);
  history.pop();
  history.unshift(smoothed);
  requestDraw();
}

listen('recording-state', ({ payload }) => setState(payload.state, payload.message));
listen('audio-level', ({ payload }) => onLevel(payload.level));
