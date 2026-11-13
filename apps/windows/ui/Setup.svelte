<script>
  import { invoke } from '@tauri-apps/api/core';
  import { ui, refresh, keyStoreHint, markVersionSeen } from './state.svelte.js';
  import Permissions from './Permissions.svelte';
  import Logo from './Logo.svelte';
  import { PROVIDERS, GLYPH, resultClass, openKeyPage, openGuide } from './keys.js';

  // True while this screen must stay up although both keys may be saved:
  // during the check and while "You're set" shows.
  let { finishing = $bindable(false) } = $props();

  // How long "You're set" stays before Home takes over.
  const DONE_MS = 2500;

  let values = $state({ groq: '', deepgram: '' });
  let shown = $state({ groq: false, deepgram: false });
  let results = $state({ groq: null, deepgram: null });
  let busy = $state(false);
  let done = $state(false);
  let error = $state('');
  let startAtLogin = $state(true);

  const saved = $derived({ groq: ui.app.groqSaved, deepgram: ui.app.deepgramSaved });
  const unsaved = $derived(PROVIDERS.filter((p) => !saved[p.id]));
  const canSubmit = $derived(
    !busy && !done && unsaved.length > 0 && unsaved.every((p) => values[p.id].trim()),
  );

  function showError(message) {
    error = typeof message === 'string' ? message : 'Could not open the browser.';
  }

  async function submit(event) {
    event.preventDefault();
    if (!canSubmit) return;
    busy = true;
    finishing = true;
    error = '';
    try {
      // null keeps a key that is already saved.
      const res = await invoke('save_keys', {
        groq: saved.groq ? null : values.groq.trim(),
        deepgram: saved.deepgram ? null : values.deepgram.trim(),
      });
      results = { groq: res.groq, deepgram: res.deepgram };
      if (res.saved) {
        values = { groq: '', deepgram: '' };
        // Saving keys switches Start at login on by default; this applies
        // the user's choice after it. A failure here is not worth blocking
        // setup over: the General page can still change it.
        try {
          await invoke('set_autostart', { enabled: startAtLogin });
        } catch (e) {
          console.warn('set_autostart failed', e);
        }
        // A fresh install has nothing new to show: its first notes come with
        // the next update.
        await markVersionSeen();
        done = true;
        await refresh();
        setTimeout(() => (finishing = false), DONE_MS);
      } else {
        finishing = false;
      }
    } catch (e) {
      // The backend rejects with a user-facing sentence; anything else is unexpected.
      error = typeof e === 'string' ? e : 'Something went wrong while saving. Your input is kept; try again.';
      finishing = false;
    } finally {
      busy = false;
    }
  }
</script>

<div class="setup">
<aside class="brand">
  <Logo size={44} blink />
  <header>
    <h1>Set up Voisu</h1>
    <p class="muted">Voisu needs two free API keys: one from Groq, one from Deepgram.</p>
  </header>
  <ol class="outline">
    {#each PROVIDERS as p, i (p.id)}
      <li class:complete={saved[p.id]}>
        <span class="num" aria-hidden="true">{saved[p.id] ? '✓' : i + 1}</span>
        <span>{p.name} key{#if saved[p.id]}<span class="sr">, saved</span>{/if}</span>
      </li>
    {/each}
    <li class:complete={done}>
      <span class="num" aria-hidden="true">{done ? '✓' : 3}</span>
      <span>Hold the Trigger Key and speak</span>
    </li>
  </ol>
  <footer>
    <button type="button" class="link" onclick={() => openGuide().catch(showError)}>How to set up (step-by-step guide)</button>
    <p class="hint">{keyStoreHint()}</p>
  </footer>
</aside>

<form onsubmit={submit} autocomplete="off">
  {#each PROVIDERS as p, i (p.id)}
    <section class="card step" aria-labelledby="step-{p.id}">
      <div class="step-head">
        <h2 id="step-{p.id}">{i + 1}. {p.name}</h2>
        {#if saved[p.id]}
          <span class="ok"><span aria-hidden="true">✓</span> Saved</span>
        {:else}
          <button type="button" class="pill" onclick={() => openKeyPage(p.id).catch(showError)}>Get a free API key</button>
        {/if}
      </div>
      {#if !saved[p.id]}
        <div class="input-wrap">
          <input
            id="key-{p.id}"
            name="{p.id}-key"
            type={shown[p.id] ? 'text' : 'password'}
            autocomplete="off"
            autocapitalize="off"
            spellcheck="false"
            aria-labelledby="step-{p.id}"
            placeholder="Paste your {p.name} key"
            bind:value={values[p.id]}
            oninput={() => (results[p.id] = null)}
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
      {/if}
      {#if results[p.id] && !saved[p.id]}
        <p class="hint {resultClass(results[p.id].state)}" role="status">
          <span aria-hidden="true">{GLYPH[results[p.id].state] ?? '•'}</span>
          {results[p.id].message}
        </p>
      {/if}
    </section>
  {/each}

  {#if !done}
    <label class="card login" for="start-at-login">
      <span class="login-text">
        <span>Start Voisu when I sign in</span>
        <span class="hint">So it's ready after every restart.</span>
      </span>
      <span class="switch">
        <input id="start-at-login" type="checkbox" role="switch" bind:checked={startAtLogin} disabled={busy} />
        <span class="track" aria-hidden="true"><span class="thumb"></span></span>
      </span>
    </label>
  {/if}

  <Permissions />

  {#if done}
    <p class="done ok" role="status">
      <span aria-hidden="true">✓</span> You're set. Hold <kbd>{ui.app.trigger.label}</kbd> and speak.
    </p>
  {:else}
    <button type="submit" class="primary" class:busy disabled={!canSubmit}>
      {busy ? 'Checking…' : 'Save and start'}
    </button>
  {/if}
  {#if error}<p class="error" role="alert"><span aria-hidden="true">✕</span> {error}</p>{/if}
</form>
</div>

<style>
  .setup {
    display: grid;
    grid-template-columns: 244px 1fr;
    min-height: 100vh;
  }
  .brand {
    position: sticky;
    top: 0;
    height: 100vh;
    display: flex;
    flex-direction: column;
    gap: 18px;
    padding: 28px 24px 22px;
    background:
      radial-gradient(140% 80% at 0% 0%, color-mix(in srgb, var(--accent) 10%, transparent), transparent 62%),
      var(--sidebar);
    border-right: 1px solid var(--hairline);
  }
  header {
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  h1 {
    font-size: 22px;
  }
  .outline {
    list-style: none;
    margin: 4px 0 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
  .outline li {
    display: flex;
    align-items: center;
    gap: 10px;
    color: var(--text-2);
  }
  .num {
    flex: none;
    display: grid;
    place-items: center;
    width: 22px;
    height: 22px;
    border-radius: 50%;
    border: 1px solid var(--border);
    font-size: 12px;
    font-weight: 600;
    font-variant-numeric: tabular-nums;
  }
  .outline .complete {
    color: var(--text);
  }
  .complete .num {
    color: var(--accent);
    border-color: color-mix(in srgb, var(--accent) 50%, transparent);
    background: color-mix(in srgb, var(--accent) 12%, transparent);
  }
  .sr {
    position: absolute;
    width: 1px;
    height: 1px;
    overflow: hidden;
    clip-path: inset(50%);
    white-space: nowrap;
  }
  form {
    display: flex;
    flex-direction: column;
    gap: 8px;
    padding: 20px 26px 16px;
    min-width: 0;
  }
  .step {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .step-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    min-height: 28px;
  }
  h2 {
    font-size: 15px;
    font-weight: 600;
  }
  .login {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    cursor: pointer;
  }
  .login-text {
    display: flex;
    flex-direction: column;
  }
  .primary {
    margin-top: 4px;
  }
  .done {
    margin-top: 4px;
    padding: 10px 0;
    text-align: center;
  }
  footer {
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    gap: 6px;
    margin-top: auto;
  }
</style>
