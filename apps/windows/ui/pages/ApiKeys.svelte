<script>
  import { invoke } from '@tauri-apps/api/core';
  import { ui, refresh, keyStoreHint } from '../state.svelte.js';
  import { PROVIDERS, GLYPH, resultClass } from '../keys.js';
  import ErrorLine from '../ErrorLine.svelte';

  // The provider whose key is being replaced, if any. Saved key text is
  // never fetched or shown; Replace only ever sends a new one.
  let replacing = $state(null);
  let newKey = $state('');
  let showKey = $state(false);
  let busy = $state(false);
  let error = $state('');
  let result = $state({ groq: null, deepgram: null });

  function startReplace(provider) {
    replacing = provider;
    newKey = '';
    showKey = false;
    error = '';
    result = { groq: null, deepgram: null };
  }

  function cancelReplace() {
    replacing = null;
    newKey = '';
  }

  async function saveKey(event) {
    event.preventDefault();
    const provider = replacing;
    if (!provider || !newKey.trim() || busy) return;
    busy = true;
    error = '';
    try {
      // null keeps the other provider's saved key as it is.
      const res = await invoke('save_keys', {
        groq: provider === 'groq' ? newKey.trim() : null,
        deepgram: provider === 'deepgram' ? newKey.trim() : null,
      });
      result = { groq: null, deepgram: null, [provider]: res[provider] };
      if (res.saved) {
        cancelReplace();
        await refresh();
      }
    } catch (e) {
      error = typeof e === 'string' ? e : 'Something went wrong while saving. Try again.';
    } finally {
      busy = false;
    }
  }
</script>

<header class="page-head">
  <h1>API keys</h1>
  <p>Groq and Deepgram turn your speech into text.</p>
</header>

<div class="group">
  {#each PROVIDERS as p (p.id)}
    <div class="block">
      <div class="row">
        <span>{p.name}</span>
        <span class="key-state">
          {#if ui.app[`${p.id}Saved`]}
            <span class="ok"><span aria-hidden="true">✓</span> Saved</span>
          {:else}
            <span class="muted">Not saved</span>
          {/if}
          {#if replacing !== p.id}
            <button type="button" class="pill" onclick={() => startReplace(p.id)} aria-label="Replace {p.name} key">
              Replace
            </button>
          {/if}
        </span>
      </div>
      {#if replacing === p.id}
        <form class="replace" onsubmit={saveKey} autocomplete="off">
          <div class="input-wrap">
            <!-- svelte-ignore a11y_autofocus -->
            <input
              type={showKey ? 'text' : 'password'}
              autocomplete="off"
              autocapitalize="off"
              spellcheck="false"
              aria-label="New {p.name} key"
              placeholder="Paste your new {p.name} key"
              bind:value={newKey}
              oninput={() => (result[p.id] = null)}
              disabled={busy}
              autofocus
            />
            <button
              type="button"
              class="reveal"
              aria-pressed={showKey}
              aria-label="{showKey ? 'Hide' : 'Show'} {p.name} key"
              onclick={() => (showKey = !showKey)}
            >
              {showKey ? 'Hide' : 'Show'}
            </button>
          </div>
          <div class="actions">
            <button type="submit" class="primary small" class:busy disabled={!newKey.trim() || busy}>
              {busy ? 'Checking…' : 'Check and save'}
            </button>
            <button type="button" class="pill" onclick={cancelReplace} disabled={busy}>Cancel</button>
          </div>
        </form>
      {/if}
      {#if result[p.id]}
        <p class="hint {resultClass(result[p.id].state)}" role="status">
          <span aria-hidden="true">{GLYPH[result[p.id].state] ?? '•'}</span>
          {result[p.id].message}
        </p>
      {/if}
    </div>
  {/each}
  <ErrorLine message={error} />
</div>

<p class="hint store">{keyStoreHint()}</p>

<style>
  .key-state {
    display: flex;
    align-items: center;
    gap: 12px;
  }
  .replace {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .actions {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .primary.small {
    padding: 4px 14px;
  }
  .store {
    margin: 10px 2px 0;
  }
</style>
