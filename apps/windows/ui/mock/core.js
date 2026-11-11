// Mock of @tauri-apps/api/core for `npm run dev:mock`: fake state for
// screenshots, chosen by `?screen=`. See SCREENS below. `&platform=mac` shows
// the macOS build: its Trigger Keys, wording and missing permissions.
import { emit, hasListener } from './event.js';

const params = new URLSearchParams(location.search);
const screen = params.get('screen') ?? 'overview';
const mac = params.get('platform') === 'mac';
const DEFAULT_KEY = mac ? 'Ctrl+Alt' : 'Ctrl+Win';

const SCREENS = [
  'setup',
  'setup-partial',
  'setup-error',
  'overview',
  'recording',
  'processing',
  'failed',
  'hook-down',
  'update',
  'star',
  'star-closed',
  'settings',
  'replace-key',
];
if (!SCREENS.includes(screen)) console.warn(`unknown ?screen=${screen}; try ${SCREENS.join(', ')}`);

const inSetup = screen.startsWith('setup');
const state = {
  groqSaved: !inSetup || screen === 'setup-partial',
  deepgramSaved: !inSetup,
  autostart: true,
  version: '0.73.0',
  autoPaste: true,
  overlaySize: 'medium',
  boostWords: ['Voisu', 'Deepgram'],
  tone: 'formal',
  trigger: { key: DEFAULT_KEY, label: mac ? 'Ctrl+Option' : 'Ctrl+Win', warnings: [], isDefault: true },
  triggerPresets: mac
    ? [
        { key: 'Ctrl+Alt', name: 'Ctrl+Option' },
        { key: 'Alt+Win', name: 'Option+Cmd' },
        { key: 'Right Win', name: 'Right Cmd' },
      ]
    : [
        { key: 'Ctrl+Win', name: 'Ctrl+Win' },
        { key: 'Ctrl+`', name: 'Ctrl+`' },
        { key: 'Right Ctrl', name: 'Right Ctrl' },
        { key: 'CapsLock', name: 'Caps Lock' },
        { key: 'Ctrl+Shift+Space', name: 'Ctrl+Shift+Space' },
      ],
  hookOk: screen !== 'hook-down',
  platform: mac ? 'mac' : 'windows',
  // On `hook-down` everything is missing; elsewhere only Accessibility.
  permissions: mac
    ? {
        microphone: screen === 'hook-down' ? 'denied' : 'granted',
        accessibility: false,
        inputMonitoring: screen !== 'hook-down',
      }
    : null,
  updateVersion: screen === 'update' ? '0.74.0' : null,
  starPrompt: true,
  // `star` shows the banner; `star-closed` is after its × (Settings still
  // carries the Support card, as on every screen).
  showStar: screen === 'star' || screen === 'star-closed',
  // Twelve seconds in, so the timer reads 0:12.
  phase: screen === 'recording' ? { state: 'recording', elapsedMs: 12_000 } : null,
};
const transcript = inSetup
  ? null
  : "Can we move the design review to Thursday at three? I'll send the updated mockups before then.";

const name = (provider) => (provider === 'groq' ? 'Groq' : 'Deepgram');
const check = (provider, key, saved) => {
  if (key == null) return saved ? { state: 'kept', message: 'Keeping your saved key.' } : { state: 'missing', message: `Paste a ${name(provider)} key.` };
  return key.includes('wrong')
    ? { state: 'invalid', message: `${name(provider)} rejected this key. Check that you copied all of it.` }
    : { state: 'valid', message: `${name(provider)} accepted this key.` };
};

const COMMANDS = {
  get_state: () => structuredClone(state),
  last_transcript: () => transcript,
  set_trigger_key: ({ key }) => {
    const label = state.triggerPresets.find((p) => p.key === key)?.name ?? key;
    state.trigger = { key, label, warnings: [], isDefault: key === DEFAULT_KEY };
    return state.trigger;
  },
  set_auto_paste: ({ enabled }) => (state.autoPaste = enabled),
  set_tone: ({ tone }) => (state.tone = tone),
  set_overlay_size: ({ size }) => (state.overlaySize = size),
  set_autostart: ({ enabled }) => (state.autostart = enabled),
  set_boost_words: ({ words }) => (state.boostWords = [...new Set(words.map((w) => w.trim()).filter(Boolean))]),
  save_keys: async ({ groq, deepgram }) => {
    await new Promise((r) => setTimeout(r, 600));
    const res = { groq: check('groq', groq, state.groqSaved), deepgram: check('deepgram', deepgram, state.deepgramSaved) };
    res.saved = [res.groq, res.deepgram].every((r) => r.state === 'valid' || r.state === 'kept');
    if (res.saved) {
      state.groqSaved = state.deepgramSaved = true;
      emit('state-changed', null);
    }
    return res;
  },
  open_key_page: ({ provider }) => console.info('mock: open key page', provider),
  open_guide: () => console.info('mock: open guide'),
  open_logs: () => console.info('mock: open logs'),
  install_update: () => console.info('mock: install update'),
  set_star_prompt: ({ enabled }) => {
    state.starPrompt = enabled;
    state.showStar = state.showStar && enabled;
    emit('state-changed', null);
    return enabled;
  },
  close_star_banner: () => {
    console.info('mock: star banner closed');
    state.showStar = false;
    emit('state-changed', null);
  },
  star_on_github: () => {
    console.info('mock: open https://github.com/Anuraj-dev/voisu');
    state.showStar = false;
    emit('state-changed', null);
  },
  open_permission_settings: ({ pane }) => {
    console.info('mock: open System Settings', pane);
    // As if the user turned it on.
    state.permissions[pane] = pane === 'microphone' ? 'granted' : true;
    if (pane === 'inputMonitoring') state.hookOk = true;
    emit('state-changed', null);
  },
};

export async function invoke(cmd, args = {}) {
  const run = COMMANDS[cmd];
  if (!run) throw `mock: no command ${cmd}`;
  return run(args);
}

// Puts the page into the chosen screen once it has rendered.
const until = (test) =>
  new Promise((resolve) => {
    const poll = () => (test() ? resolve(test()) : setTimeout(poll, 20));
    poll();
  });
const $ = (selector) => document.querySelector(selector);
const type = (input, value) => {
  input.value = value;
  input.dispatchEvent(new Event('input', { bubbles: true }));
};

async function drive() {
  if (screen === 'setup-error') {
    type(await until(() => $('#key-groq')), 'gsk_wrong_key');
    type($('#key-deepgram'), 'dg_example_key');
    await until(() => !$('button[type="submit"]').disabled);
    $('button[type="submit"]').click();
  } else if (screen === 'processing' || screen === 'failed') {
    // `recording` is seeded from get_state; these arrive as events.
    await until(() => hasListener('recording-state'));
    if (screen === 'failed') {
      emit('recording-state', { state: 'error', message: "Couldn't reach Groq. Check your internet connection." });
    } else {
      emit('recording-state', { state: 'recording' });
      emit('recording-state', { state: 'processing' });
    }
  } else if (screen === 'star-closed') {
    (await until(() => $('button[aria-label="Close"]'))).click();
    (await until(() => !$('button[aria-label="Close"]') && $('#tab-settings'))).click();
  } else if (screen === 'settings' || screen === 'replace-key') {
    (await until(() => $('#tab-settings'))).click();
    if (screen === 'replace-key') (await until(() => $('button[aria-label="Replace Groq key"]'))).click();
  }
}
drive();
