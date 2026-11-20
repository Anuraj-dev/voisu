// Drives the overlay page under `npm run dev:mock` (loaded by core.js):
// `overlay.html?state=` picks recording, handsFree, processing, done,
// copied, error or moving.
import { emit, hasListener } from './event.js';

const STATES = {
  recording: [{ state: 'recording' }],
  handsFree: [{ state: 'handsFree' }],
  processing: [{ state: 'recording' }, { state: 'processing' }],
  done: [{ state: 'done' }],
  copied: [{ state: 'copied', message: 'Copied · allow Accessibility' }],
  error: [{ state: 'error', message: "Couldn't reach Groq. Check your internet connection." }],
};
const LEVELS = [0.25, 0.6, 0.9, 0.5, 0.75, 0.35, 0.8, 0.45];

async function drive() {
  const params = new URLSearchParams(location.search);
  const name = params.get('state');
  if (!name) return;
  while (!hasListener('overlay-layout') || !hasListener('recording-state')) {
    await new Promise((resolve) => setTimeout(resolve, 20));
  }
  // `?x=&y=` puts the pill elsewhere in the window, as near a screen edge.
  const x = Number(params.get('x') ?? 0.5);
  const y = Number(params.get('y') ?? 0.5);
  emit('overlay-layout', { x, y, moving: name === 'moving', move_id: name === 'moving' ? 1 : null, seq: 1 });
  for (const payload of STATES[name] ?? []) emit('recording-state', payload);
  // Done, Copied and Error clear themselves; keep them up for a screenshot.
  if (name === 'done' || name === 'copied' || name === 'error') {
    setInterval(() => emit('recording-state', STATES[name][0]), 400);
  }
  if (name === 'recording' || name === 'handsFree') {
    for (const level of LEVELS) emit('audio-level', { level });
  }
}
drive();
