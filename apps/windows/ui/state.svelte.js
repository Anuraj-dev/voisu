// What every screen of the settings window reads: the backend's state, the
// last Transcript and the current Recording phase. Only the backend writes
// settings; this mirrors them.
import { invoke } from '@tauri-apps/api/core';
import NOTES from './whats-new.json';
import { unseen, itemsFor } from './whats-new.js';

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
    // The pending update's notes, the items of whats-new.json, when its
    // manifest carried them; null otherwise.
    updateNotes: null,
    // The version whose What's new was last shown; null before any was.
    seenVersion: null,
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
  // survives switching pages.
  wordsDraft: '',
  // The sidebar page: home | dictation | words | keys | general | whats-new
  page: 'home',
  // "Restart to update" sits on the banner and on What's new; both share
  // one run and its error.
  updating: false,
  updateError: '',
});

export async function installUpdate() {
  if (ui.updating) return;
  ui.updating = true;
  ui.updateError = '';
  try {
    // Ends this process when the installer starts.
    await invoke('install_update');
  } catch (e) {
    ui.updateError = typeof e === 'string' ? e : 'Could not start the update.';
  } finally {
    ui.updating = false;
  }
}

// The pending update's notes for this platform, or [] when it has none.
export const pendingNotes = () => itemsFor(ui.app.updateNotes, ui.app.platform);

export const keysReady = () => ui.app.groqSaved && ui.app.deepgramSaved;

export const isMac = () => ui.app.platform === 'mac';

// The releases since the last What's new the user saw, up to this version.
// Empty during Setup: a fresh install never sees notes for itself.
export const unseenReleases = () =>
  keysReady() && ui.app.version && ui.app.seenVersion !== ui.app.version
    ? unseen(NOTES, ui.app.seenVersion, ui.app.version, ui.app.platform)
    : [];

// Records this version's What's new as seen; the backend's state-changed
// event then rereads seenVersion, which clears the sidebar dot.
export async function markVersionSeen() {
  try {
    await invoke('mark_version_seen');
  } catch (e) {
    // Not worth an error line: the notes simply show again next launch.
    console.warn('mark_version_seen failed', e);
  }
}

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
