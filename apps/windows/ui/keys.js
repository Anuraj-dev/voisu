// Pieces the setup screen and Settings → API keys share.
import { invoke } from '@tauri-apps/api/core';

export const PROVIDERS = [
  { id: 'groq', name: 'Groq' },
  { id: 'deepgram', name: 'Deepgram' },
];

export const GLYPH = { valid: '✓', invalid: '✕', missing: '✕', kept: '•' };

export function resultClass(state) {
  return state === 'valid' ? 'ok' : state === 'kept' ? 'muted' : 'bad';
}

// Both open fixed URLs chosen in Rust; the window cannot pick one. They
// reject with the backend's sentence.
export const openKeyPage = (provider) => invoke('open_key_page', { provider });
export const openGuide = () => invoke('open_guide');
