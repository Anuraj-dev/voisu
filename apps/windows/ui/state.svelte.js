// What every screen of the settings window reads: the backend's state, the
// last Transcript and the current Recording phase. Only the backend writes
// settings; this mirrors them.
import { invoke } from '@tauri-apps/api/core';

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
    // Blank until get_state names this platform's Trigger Key; the presets
    // come from the backend too, the default first.
    trigger: { key: '', label: '', warnings: [], isDefault: true },
    triggerPresets: [],
    hookOk: true,
    // 'windows' | 'mac'
    platform: 'windows',
    // macOS only: { microphone: 'granted' | 'notAsked' | 'denied',
    // accessibility, inputMonitoring }; null on Windows.
    permissions: null,
    updateVersion: null,
    // The Star prompt switch, and whether its banner is due.
    starPrompt: true,
    showStar: false,
    // The dictation under way when the window loaded: { state, elapsedMs }.
    phase: null,
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

export const isMac = () => ui.app.platform === 'mac';

export const keyStoreHint = () =>
  isMac() ? 'Keys stay on this Mac, in the macOS Keychain.' : 'Keys stay on this PC, in Windows Credential Manager.';

// The macOS permissions still to grant, in the order they matter. A mic not
// yet asked is left out: the first Recording asks for it.
export function missingPermissions() {
  const p = ui.app.permissions;
  if (!p) return [];
  return [
    !p.inputMonitoring && {
      pane: 'inputMonitoring',
      name: 'Input Monitoring',
      why: 'So Voisu hears the Trigger Key.',
    },
    !p.accessibility && {
      pane: 'accessibility',
      name: 'Accessibility',
      why: 'So Voisu can paste. Without it the Transcript is only copied.',
    },
    p.microphone === 'denied' && {
      pane: 'microphone',
      name: 'Microphone',
      why: 'So Voisu can record you.',
    },
  ].filter(Boolean);
}

export async function refresh() {
  try {
    const stored = ui.app.boostWords.join('\n');
    const wasReady = keysReady();
    const next = await invoke('get_state');
    // Never overwrite boost words the user is editing, including any typed
    // while this call was in flight.
    if (ui.wordsDraft === stored) ui.wordsDraft = next.boostWords.join('\n');
    ui.app = next;
    if (!ui.loaded) {
      seedPhase(next.phase);
    } else if (!wasReady && keysReady() && ui.phase === 'error') {
      // Saving the missing keys resolves a "keys missing" refusal; any other
      // failure stays until the next dictation.
      ui.phase = 'idle';
    }
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

// Events only report changes, so a window opened mid-dictation starts from
// the backend's phase. A past failure is not replayed.
function seedPhase(phase) {
  if (!phase || ui.phase !== 'idle') return;
  if (phase.state === 'recording' || phase.state === 'handsFree' || phase.state === 'processing') {
    ui.phase = phase.state;
    ui.recordingSince = Date.now() - phase.elapsedMs;
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
    // idle, done or copied
    ui.phase = 'idle';
    refreshTranscript();
  }
}
