// What every screen of the settings window reads: the backend's state, the
// last Transcript and the current Recording phase. Only the backend writes
// settings; this mirrors them.
import { invoke } from '@tauri-apps/api/core';

// The written form, as the backend stores it. The presets themselves come
// from the backend, which accepts nothing else.
export const DEFAULT_TRIGGER = 'Ctrl+Win';

export const ui = $state({
  app: {
    groqSaved: false,
    deepgramSaved: false,
    autostart: false,
    version: '',
    autoPaste: true,
    overlaySize: 'medium',
    boostWords: [],
    tone: 'formal',
    trigger: { key: DEFAULT_TRIGGER, label: DEFAULT_TRIGGER, warnings: [], isDefault: true },
    triggerPresets: [],
    hookOk: true,
    updateVersion: null,
  },
  // False until the first get_state, so nothing renders on guessed state.
  loaded: false,
  error: '',
  // Held in the backend's memory only; never written to disk.
  transcript: null,
  // idle | recording | handsFree | processing | error
  phase: 'idle',
  phaseMessage: '',
  // Date.now() when the current Recording started; the hero ticks from it.
  recordingSince: 0,
  // One boost word per line; saved only on "Save words". Kept here so it
  // survives switching tabs.
  wordsDraft: '',
});

export const keysReady = () => ui.app.groqSaved && ui.app.deepgramSaved;

export async function refresh() {
  try {
    const stored = ui.app.boostWords.join('\n');
    const next = await invoke('get_state');
    // Never overwrite boost words the user is editing, including any typed
    // while this call was in flight.
    if (ui.wordsDraft === stored) ui.wordsDraft = next.boostWords.join('\n');
    ui.app = next;
    ui.loaded = true;
  } catch (e) {
    ui.error = 'Could not read app state.';
  }
}

export async function refreshTranscript() {
  try {
    ui.transcript = await invoke('last_transcript');
  } catch (e) {
    // Leave the last shown Transcript in place.
  }
}

export function onRecordingState({ state, message }) {
  if (state === 'recording' || state === 'handsFree') {
    // Hands-free continues the Recording that is already running.
    if (ui.phase !== 'recording' && ui.phase !== 'handsFree') ui.recordingSince = Date.now();
    ui.phase = state;
  } else if (state === 'processing') {
    ui.phase = 'processing';
  } else if (state === 'error') {
    ui.phase = 'error';
    ui.phaseMessage = message ?? 'Dictation failed.';
  } else {
    // idle or done
    ui.phase = 'idle';
    refreshTranscript();
  }
}
