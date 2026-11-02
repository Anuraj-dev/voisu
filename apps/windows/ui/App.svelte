<script>
  import { onMount } from 'svelte';
  import { invoke } from '@tauri-apps/api/core';
  import { listen } from '@tauri-apps/api/event';

  const PROVIDERS = [
    { id: 'groq', name: 'Groq' },
    { id: 'deepgram', name: 'Deepgram' },
  ];
  const GLYPH = { valid: '✓', invalid: '✕', missing: '✕', kept: '•' };

  let app = $state({ groqSaved: false, deepgramSaved: false, autostart: false, version: '', logDir: '' });
  let values = $state({ groq: '', deepgram: '' });
  let shown = $state({ groq: false, deepgram: false });
  let results = $state({ groq: null, deepgram: null });
  let busy = $state(false);
  let banner = $state('');
  let error = $state('');
  let autostartBusy = $state(false);
  // Held in the backend's memory only; never written to disk.
  let transcript = $state(null);
  let dictationError = $state('');
  let copied = $state(false);

  const saved = $derived({ groq: app.groqSaved, deepgram: app.deepgramSaved });
  const blank = $derived(!values.groq.trim() && !values.deepgram.trim());
  const canSubmit = $derived(!busy && !(blank && saved.groq && saved.deepgram));

  async function refresh() {
    try {
      app = await invoke('get_state');
    } catch (e) {
      error = 'Could not read app state.';
    }
  }

  async function refreshTranscript() {
    try {
      const next = await invoke('last_transcript');
      if (next !== transcript) copied = false;
      transcript = next;
    } catch (e) {
      // Leave the last shown transcript in place.
    }
  }

  function onRecordingState(event) {
    const { state, message } = event.payload;
    if (state === 'error') {
      dictationError = message ?? 'Dictation failed.';
    } else if (state === 'recording') {
      dictationError = '';
    } else if (state === 'idle') {
      refreshTranscript();
    }
  }

  async function copyTranscript() {
    if (!transcript) return;
    try {
      await navigator.clipboard.writeText(transcript);
      copied = true;
    } catch (e) {
      error = 'Could not copy to the clipboard.';
    }
  }

  onMount(() => {
    refresh();
    refreshTranscript();
    const unlisteners = [];
    let disposed = false;
    const subscribe = (name, handler) =>
      listen(name, handler).then((fn) => {
        if (disposed) fn();
        else unlisteners.push(fn);
      });
    subscribe('state-changed', () => {
      refresh();
      refreshTranscript();
    });
    subscribe('recording-state', onRecordingState);
    return () => {
      disposed = true;
      unlisteners.forEach((fn) => fn());
    };
  });

  async function submit(event) {
    event.preventDefault();
    if (!canSubmit) return;
    busy = true;
    banner = '';
    error = '';
    try {
      const res = await invoke('save_keys', {
        groq: values.groq.trim() || null,
        deepgram: values.deepgram.trim() || null,
      });
      results = { groq: res.groq, deepgram: res.deepgram };
      if (res.saved) {
        values = { groq: '', deepgram: '' };
        shown = { groq: false, deepgram: false };
        banner = 'Saved. Voisu is running in the tray — you can close this window.';
        await refresh();
      }
    } catch (e) {
      // The backend rejects with a user-facing sentence; anything else is unexpected.
      error = typeof e === 'string' ? e : 'Something went wrong while saving. Your input is kept; try again.';
    } finally {
      busy = false;
    }
  }

  async function toggleAutostart(event) {
    const box = event.currentTarget;
    const requested = box.checked;
    autostartBusy = true;
    try {
      app.autostart = await invoke('set_autostart', { enabled: requested });
    } catch (e) {
      error = 'Could not change Start at login.';
    } finally {
      // Render the backend's actual state, not the requested value.
      box.checked = app.autostart;
      autostartBusy = false;
    }
  }

  function openKeyPage(provider) {
    invoke('open_key_page', { provider }).catch(() => {
      error = 'Could not open the browser.';
    });
  }

  function resultClass(state) {
    return state === 'valid' ? 'ok' : state === 'kept' ? 'muted' : 'bad';
  }
</script>

<main>
  <header>
    <h1>Voisu</h1>
    <p class="lede">
      Paste your Groq and Deepgram API keys. Voisu keeps them in Windows Credential Manager.
    </p>
  </header>

  <form onsubmit={submit} autocomplete="off">
    {#each PROVIDERS as p (p.id)}
      <div class="field">
        <div class="label-row">
          <label for="key-{p.id}">{p.name} API key</label>
          {#if saved[p.id]}<span class="badge">Saved</span>{/if}
        </div>
        <div class="input-wrap">
          <input
            id="key-{p.id}"
            name="{p.id}-key"
            type={shown[p.id] ? 'text' : 'password'}
            autocomplete="off"
            autocapitalize="off"
            spellcheck="false"
            placeholder={saved[p.id] ? 'Saved — leave blank to keep' : `Paste your ${p.name} key`}
            bind:value={values[p.id]}
            disabled={busy}
          />
          <button
            type="button"
            class="reveal"
            aria-pressed={shown[p.id]}
            aria-label="{shown[p.id] ? 'Hide' : 'Show'} {p.name} key"
            onclick={() => (shown[p.id] = !shown[p.id])}
          >
            {shown[p.id] ? 'Hide' : 'Show'}
          </button>
        </div>
        <div class="under">
          <button type="button" class="link" onclick={() => openKeyPage(p.id)}>Get a key</button>
          <span class="result {results[p.id] ? resultClass(results[p.id].state) : ''}" role="status">
            {#if results[p.id]}
              <span aria-hidden="true">{GLYPH[results[p.id].state] ?? '•'}</span>
              {results[p.id].message}
            {/if}
          </span>
        </div>
      </div>
    {/each}

    <button type="submit" class="primary" class:busy disabled={!canSubmit}>
      {busy ? 'Checking…' : 'Validate & save'}
    </button>

    <p class="banner" role="status">
      {#if banner}<span aria-hidden="true">✓</span> {banner}{/if}
    </p>
    {#if error}<p class="error" role="alert"><span aria-hidden="true">✕</span> {error}</p>{/if}
  </form>

  <div class="card">
    <label class="switch-row" for="autostart">
      <span>Start at login</span>
      <span class="switch">
        <input
          id="autostart"
          type="checkbox"
          role="switch"
          checked={app.autostart}
          disabled={autostartBusy}
          onchange={toggleAutostart}
        />
        <span class="track" aria-hidden="true"><span class="thumb"></span></span>
      </span>
    </label>
  </div>

  <section class="card transcript" aria-labelledby="transcript-title">
    <div class="transcript-head">
      <h2 id="transcript-title">Last transcript</h2>
      <button type="button" class="copy" onclick={copyTranscript} disabled={!transcript}>
        {copied ? 'Copied' : 'Copy'}
      </button>
    </div>
    {#if transcript}
      <p class="transcript-text">{transcript}</p>
    {:else}
      <p class="note">Hold Ctrl+Win and speak. Release to transcribe; double-tap for hands-free, Esc to cancel.</p>
    {/if}
    {#if dictationError}<p class="error" role="alert"><span aria-hidden="true">✕</span> {dictationError}</p>{/if}
  </section>

  <footer>
    <div>Version {app.version}</div>
    <div class="logs">Logs: <span class="path">{app.logDir}</span></div>
  </footer>
</main>

<style>
  main {
    padding: 24px 24px 16px;
    display: flex;
    flex-direction: column;
    gap: 16px;
    min-height: 100vh;
  }
  h1 {
    margin: 0;
    font-size: 22px;
    font-weight: 600;
    letter-spacing: -0.01em;
  }
  .lede {
    margin: 4px 0 0;
    color: var(--text-2);
  }
  form {
    display: flex;
    flex-direction: column;
    gap: 14px;
  }
  .field {
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .label-row {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  label {
    font-weight: 600;
  }
  .badge {
    font-size: 12px;
    color: var(--success);
    background: var(--overlay-2);
    border-radius: 32px;
    padding: 1px 8px;
  }
  .input-wrap {
    display: flex;
    background: var(--overlay-1);
    border: 1px solid var(--border);
    border-radius: 10px;
  }
  .input-wrap:focus-within {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }
  .input-wrap input {
    flex: 1;
    min-width: 0;
    background: transparent;
    border: 0;
    outline: 0;
    padding: 9px 12px;
    border-radius: 10px;
  }
  .input-wrap input::placeholder {
    color: var(--text-2);
    opacity: 1;
  }
  .input-wrap input:disabled {
    opacity: 0.7;
  }
  .reveal {
    background: transparent;
    border: 0;
    color: var(--text-2);
    padding: 0 12px;
    cursor: pointer;
    border-radius: 10px;
  }
  .reveal:hover {
    color: var(--text);
    background: var(--overlay-2);
  }
  button:focus-visible,
  .switch input:focus-visible + .track {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  .under {
    display: flex;
    align-items: baseline;
    gap: 12px;
    min-height: 20px;
  }
  .link {
    background: none;
    border: 0;
    padding: 0;
    color: var(--processing);
    text-decoration: underline;
    cursor: pointer;
    flex: none;
  }
  .result {
    font-size: 13px;
    color: var(--text-2);
  }
  .result.ok {
    color: var(--success);
  }
  .result.bad {
    color: var(--failure);
  }
  .result.muted {
    color: var(--text-2);
  }
  .primary {
    align-self: stretch;
    background: var(--accent);
    color: var(--surface);
    border: 0;
    border-radius: 32px;
    padding: 11px 20px;
    font-weight: 600;
    cursor: pointer;
  }
  .primary:disabled {
    background: var(--overlay-2);
    color: var(--text-2);
    cursor: not-allowed;
  }
  .primary.busy:disabled {
    background: var(--processing);
    color: var(--surface);
    cursor: progress;
  }
  .banner {
    margin: 0;
    min-height: 20px;
    color: var(--success);
  }
  .error {
    margin: 0;
    color: var(--failure);
  }
  .card {
    background: var(--overlay-1);
    border: 1px solid var(--overlay-2);
    border-radius: 12px;
    padding: 12px 14px;
  }
  .switch-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    font-weight: 400;
    cursor: pointer;
  }
  .switch {
    position: relative;
    display: inline-block;
    width: 40px;
    height: 22px;
  }
  .switch input {
    position: absolute;
    inset: 0;
    width: 100%;
    height: 100%;
    margin: 0;
    opacity: 0;
    cursor: pointer;
  }
  .track {
    position: absolute;
    inset: 0;
    border-radius: 32px;
    background: var(--overlay-2);
    border: 1px solid var(--border);
    transition: background 0.15s;
    pointer-events: none;
  }
  .thumb {
    position: absolute;
    top: 2px;
    left: 2px;
    width: 16px;
    height: 16px;
    border-radius: 50%;
    background: var(--text);
    transition: transform 0.15s;
  }
  .switch input:checked + .track {
    background: var(--accent);
    border-color: var(--accent);
  }
  .switch input:checked + .track .thumb {
    transform: translateX(18px);
    background: var(--surface);
  }
  .switch input:disabled + .track {
    opacity: 0.6;
  }
  .note {
    margin: 0;
    color: var(--text-2);
  }
  .transcript {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .transcript-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
  }
  h2 {
    margin: 0;
    font-size: 14px;
    font-weight: 600;
  }
  .copy {
    background: var(--overlay-2);
    color: var(--text);
    border: 1px solid var(--border);
    border-radius: 32px;
    padding: 3px 12px;
    cursor: pointer;
  }
  .copy:disabled {
    color: var(--text-2);
    cursor: not-allowed;
  }
  .transcript-text {
    margin: 0;
    white-space: pre-wrap;
    user-select: text;
    max-height: 160px;
    overflow-y: auto;
  }
  footer {
    margin-top: auto;
    color: var(--text-2);
    font-size: 12px;
  }
  .logs .path {
    user-select: text;
    word-break: break-all;
  }
</style>
