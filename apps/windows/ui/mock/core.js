// Mock of @tauri-apps/api/core for `npm run dev:mock`: fake state for
// screenshots, chosen by `?screen=`. See SCREENS below. `&platform=mac` shows
// the macOS build on any screen: its Trigger Keys, wording and missing
// permissions.
import { emit, hasListener } from './event.js';

const params = new URLSearchParams(location.search);
// `overview` is the old name of `home`; `settings` opens Dictation, where
// the old Settings tab's first section now lives.
const ALIASES = { overview: 'home', settings: 'dictation' };
const asked = params.get('screen') ?? 'home';
const screen = ALIASES[asked] ?? asked;
const mac = params.get('platform') === 'mac';
const DEFAULT_KEY = mac ? 'Ctrl+Alt' : 'Ctrl+Win';

// Sidebar pages, opened by clicking their item once the app renders.
const PAGES = ['dictation', 'words', 'keys', 'general', 'whats-new'];
const SCREENS = [
  'setup',
  'setup-partial',
  'setup-error',
  'home',
  'recording',
  'processing',
  'failed',
  'hook-down',
  // A pending update whose manifest carried notes.
  'update',
  // Just updated from 0.77.0: the What's new dialog opens.
  'updated',
  'star',
  'star-closed',
  ...PAGES,
  // What's new with the pending update's notes under "Ready to install".
  'whats-new-pending',
  'replace-key',
  // Dictation with the overlay in move mode.
  'overlay-moving',
];
if (!SCREENS.includes(screen)) console.warn(`unknown ?screen=${asked}; try ${SCREENS.join(', ')}`);
const pending = screen === 'update' || screen === 'whats-new-pending';

const inSetup = screen.startsWith('setup');
const state = {
  groqSaved: !inSetup || screen === 'setup-partial',
  deepgramSaved: !inSetup,
  autostart: true,
  version: '0.79.0',
  autoPaste: true,
  overlayPosition: null,
  overlayMoving: false,
  lastOverlayMove: 0,
  boostWords: ['Voisu', 'Deepgram'],
  tone: 'formal',
  keepMicReady: 'while_running',
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
  updateVersion: pending ? '0.79.0' : null,
  updateNotes: pending
    ? [
        { kind: 'new', text: 'Pick a microphone in Dictation settings.' },
        { kind: 'improved', text: 'The overlay follows the screen your cursor is on.' },
        { kind: 'fixed', text: 'Paste no longer drops the first letter in some terminals.', platforms: ['windows'] },
      ]
    : null,
  // A fresh install has seen nothing; `updated` last saw 0.77.0.
  seenVersion: inSetup ? null : screen === 'updated' ? '0.77.0' : '0.79.0',
  starPrompt: true,
  // `star` shows the banner; `star-closed` is after its × (Settings still
  // carries the Support card, as on every screen).
  showStar: screen === 'star' || screen === 'star-closed',
  // Twelve seconds in, so the timer reads 0:12.
  phase: screen === 'recording' ? { state: 'recording', elapsedMs: 12_000 } : null,
  mode: 'cloud',
  // Local mode is Windows only. `?local=fast` (or `accurate`, `both`) starts
  // with that model on disk; otherwise Dictation offers the download.
  // `&gpu=1` finds a GPU.
  local: mac ? null : mockLocal(params.get('local'), params.get('gpu') === '1'),
};

function mockLocal(onDisk, onGpu) {
  const has = (id) => onDisk === 'both' || onDisk === id || (id === 'fast' && onDisk === 'installed');
  const local = {
    available: true,
    detecting: false,
    device: onGpu ? 'NVIDIA GeForce RTX 5050 Laptop GPU' : 'CPU',
    onGpu,
    model: 'auto',
    autoTier: onGpu ? 'accurate' : 'fast',
    activeTier: null,
    tiers: [
      { id: 'accurate', name: 'Accurate', modelName: 'distil-large-v3.5', installed: has('accurate'), size: 818_658_033 },
      { id: 'fast', name: 'Fast', modelName: 'small.en', installed: has('fast'), size: 264_829_639 },
    ],
    engine: 'off',
    engineTier: null,
    engineDevice: null,
    installed: false,
    downloading: null,
    downloaded: 0,
    total: 0,
    error: null,
    cloudUntilDownloaded: false,
  };
  refreshLocal(local);
  return local;
}

// What the backend derives: sizes still to fetch, notes, the tier Auto runs.
function refreshLocal(local) {
  const clip = 352_078;
  for (const t of local.tiers) {
    t.bytes = t.installed ? 0 : t.size - (local.tiers.some((o) => o.installed) ? clip : 0);
    t.note =
      t.id === 'accurate'
        ? local.onGpu
          ? 'Accurate recommended on your GPU'
          : 'Accurate is much slower on the CPU; Fast is recommended without a GPU.'
        : null;
  }
  local.installed = local.tiers.some((t) => t.installed);
  const tier = (id) => local.tiers.find((t) => t.id === id);
  const preferred = local.model === 'auto' ? local.autoTier : local.model;
  local.activeTier = !local.installed
    ? null
    : local.model !== 'auto' || tier(preferred).installed
      ? preferred
      : local.tiers.find((t) => t.installed).id;
  // The mock loads a new tier at once.
  if (local.engine === 'ready') local.engineTier = local.activeTier;
}

// A fake download: about five seconds end to end; Cancel keeps its place.
let downloadTimer = null;
// Whether the running download turns Local on; choosing Cloud clears it.
let downloadWantsLocal = false;
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
  set_mode: ({ mode }) => {
    const local = state.local;
    if (mode === 'local' && !local?.installed) {
      if (!local?.downloading) throw 'Download the Local model first.';
      local.cloudUntilDownloaded = true;
      downloadWantsLocal = true;
      emit('state-changed', null);
      return 'cloud';
    }
    state.mode = mode;
    if (local) {
      local.error = null;
      if (mode === 'cloud') {
        local.cloudUntilDownloaded = false;
        downloadWantsLocal = false;
      }
      Object.assign(
        local,
        mode === 'local'
          ? { engine: 'ready', engineTier: local.activeTier, engineDevice: local.onGpu ? 'vulkan:0' : 'cpu' }
          : { engine: 'off', engineTier: null, engineDevice: null },
      );
    }
    emit('state-changed', null);
    return mode;
  },
  set_local_model: ({ model }) => {
    const local = state.local;
    const tier = local.tiers.find((t) => t.id === model);
    if (tier && !tier.installed) throw `Download ${tier.name} first.`;
    local.model = model;
    refreshLocal(local);
    emit('state-changed', null);
    return model;
  },
  download_local_model: ({ tier: id, bytes, model, switchToLocal }) =>
    new Promise((resolve, reject) => {
      const local = state.local;
      const tier = local.tiers.find((t) => t.id === id);
      if (!tier || (model !== 'auto' && model !== id)) return reject("Voisu doesn't know that Local model.");
      if (tier.bytes !== bytes) return reject('The download size changed. Check it and try again.');
      Object.assign(local, {
        downloading: tier.id,
        downloaded: tier.size - tier.bytes,
        total: tier.size,
        error: null,
        cloudUntilDownloaded: false,
      });
      downloadWantsLocal = switchToLocal;
      emit('state-changed', null);
      downloadTimer = setInterval(() => {
        if (!local.downloading) {
          clearInterval(downloadTimer);
          local.cloudUntilDownloaded = false;
          emit('state-changed', null);
          return resolve('cancelled');
        }
        local.downloaded = Math.min(local.total, local.downloaded + local.total / 20);
        if (local.downloaded >= local.total) {
          tier.installed = true;
          local.downloading = null;
          local.model = model;
          if (downloadWantsLocal) state.mode = 'local';
          local.cloudUntilDownloaded = false;
          refreshLocal(local);
          clearInterval(downloadTimer);
          resolve('installed');
        }
        emit('state-changed', null);
      }, 250);
    }),
  cancel_local_download: () => {
    state.local.downloading = null;
    emit('state-changed', null);
  },
  remove_local_model: () => {
    state.mode = 'cloud';
    const local = state.local;
    for (const t of local.tiers) t.installed = false;
    Object.assign(local, { downloaded: 0, error: null, engine: 'off', engineTier: null, engineDevice: null });
    refreshLocal(local);
    emit('state-changed', null);
  },
  set_tone: ({ tone }) => (state.tone = tone),
  set_keep_mic_ready: ({ mode }) => ({ mode: (state.keepMicReady = mode), saved: true }),
  start_overlay_move: () => {
    if (!state.overlayMoving) {
      console.info('mock: overlay shown in move mode');
      state.overlayMoving = true;
      state.lastOverlayMove += 1;
      emit('state-changed', null);
    }
    return state.lastOverlayMove;
  },
  // The overlay page: its layout comes from the `?state=` driver.
  overlay_layout: () => null,
  overlay_layout_drawn: () => {},
  drag_overlay: () => {},
  // ✓ saves a spot left of centre, as if the pill were dragged there. A
  // finish for an earlier move changes nothing.
  finish_overlay_move: ({ moveId, save }) => {
    if (!state.overlayMoving || moveId !== state.lastOverlayMove) return state.overlayPosition;
    if (save) state.overlayPosition = { x: 0.3, y: 0.0 };
    state.overlayMoving = false;
    emit('state-changed', null);
    return state.overlayPosition;
  },
  reset_overlay_position: () => (state.overlayPosition = null),
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
  mark_version_seen: () => {
    state.seenVersion = state.version;
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

const open = async (page, ready = () => true) =>
  (await until(() => ready() && $(`[data-page="${page}"]`))).click();

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
    await open('general', () => !$('button[aria-label="Close"]'));
  } else if (PAGES.includes(screen)) {
    await open(screen);
  } else if (screen === 'whats-new-pending') {
    await open('whats-new');
  } else if (screen === 'overlay-moving') {
    await open('dictation');
    (await until(() => $('#move-overlay'))).click();
  } else if (screen === 'replace-key') {
    await open('keys');
    (await until(() => $('button[aria-label="Replace Groq key"]'))).click();
  }
}
// The overlay page has its own driver.
if (location.pathname.endsWith('/overlay.html')) import('./overlay.js');
else drive();
