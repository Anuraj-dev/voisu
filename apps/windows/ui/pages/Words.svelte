<script>
  import { invoke } from '@tauri-apps/api/core';
  import { ui } from '../state.svelte.js';
  import ErrorLine from '../ErrorLine.svelte';

  let busy = $state(false);
  let status = $state('');
  let error = $state('');

  const dirty = $derived(ui.wordsDraft !== ui.app.boostWords.join('\n'));

  async function save() {
    if (!dirty || busy) return;
    busy = true;
    status = '';
    error = '';
    try {
      const words = await invoke('set_boost_words', { words: ui.wordsDraft.split('\n') });
      ui.app.boostWords = words;
      // Show what is stored: trimmed, blank lines and repeats removed.
      ui.wordsDraft = words.join('\n');
      status = words.length === 1 ? 'Saved 1 word.' : `Saved ${words.length} words.`;
    } catch (e) {
      error = 'Could not save boost words.';
    } finally {
      busy = false;
    }
  }
</script>

<header class="page-head">
  <h1 id="words-title">Words</h1>
  <p id="words-hint">Names and terms to hear and spell your way, one per line.</p>
</header>

<div class="group">
  <textarea
    id="boost-words"
    rows="10"
    spellcheck="false"
    autocomplete="off"
    aria-labelledby="words-title"
    aria-describedby="words-hint"
    placeholder={'Voisu\nDeepgram'}
    bind:value={ui.wordsDraft}
    oninput={() => (status = '')}
    disabled={busy}
  ></textarea>
  <div class="row">
    {#if status}
      <p class="hint ok" role="status"><span aria-hidden="true">✓</span> {status}</p>
    {:else}
      <p class="hint">Boost words apply from the next Recording.</p>
    {/if}
    <button type="button" class="pill" onclick={save} disabled={!dirty || busy}>
      {busy ? 'Saving…' : 'Save words'}
    </button>
  </div>
  <ErrorLine message={error} />
</div>

<style>
  .group > textarea {
    display: block;
    width: 100%;
    resize: none;
    padding: 12px 14px;
    border: 0;
    border-radius: var(--radius) var(--radius) 0 0;
    background: transparent;
    outline: 0;
    line-height: 1.5;
    font-variant-ligatures: none;
  }
  textarea:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: -2px;
  }
</style>
