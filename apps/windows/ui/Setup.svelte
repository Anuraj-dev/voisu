<script>
  import { invoke } from '@tauri-apps/api/core';
  import { ui, refresh } from './state.svelte.js';
  import { PROVIDERS, GLYPH, resultClass, openKeyPage, openGuide } from './keys.js';

  // True while this screen must stay up although both keys may be saved:
  // during the check and while "You're set" shows.
  let { finishing = $bindable(false) } = $props();

  // How long "You're set" stays before the Overview takes over.
  const DONE_MS = 2500;

  let values = $state({ groq: '', deepgram: '' });
  let shown = $state({ groq: false, deepgram: false });
  let results = $state({ groq: null, deepgram: null });
  let busy = $state(false);
  let done = $state(false);
  let error = $state('');

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

<header>
  <h1>Set up Voisu</h1>
  <p class="muted">Voisu needs two free API keys: one from Groq, one from Deepgram.</p>
</header>

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

<footer>
  <button type="button" class="link" onclick={() => openGuide().catch(showError)}>How to set up (step-by-step guide)</button>
  <p class="hint">Keys stay on this PC, in Windows Credential Manager.</p>
</footer>

<style>
  header {
    display: flex;
    flex-direction: column;
    gap: 6px;
    margin-bottom: 20px;
  }
  form {
    display: flex;
    flex-direction: column;
    gap: 12px;
  }
  .step {
    display: flex;
    flex-direction: column;
    gap: 10px;
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
    margin-top: 20px;
  }
</style>
