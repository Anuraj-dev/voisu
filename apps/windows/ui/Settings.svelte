<script>
  import { invoke } from '@tauri-apps/api/core';
  import { ui, refresh, keyStoreHint } from './state.svelte.js';
  import { PROVIDERS, GLYPH, resultClass, openGuide } from './keys.js';

  const SIZES = [
    { id: 'small', name: 'Small' },
    { id: 'medium', name: 'Medium' },
    { id: 'large', name: 'Large' },
  ];

  // One message per section, shown under the section that failed.
  let errors = $state({});
  let triggerBusy = $state(false);
  let autoPasteBusy = $state(false);
  let toneBusy = $state(false);
  let autostartBusy = $state(false);
  let wordsBusy = $state(false);
  let wordsStatus = $state('');
  // The provider whose key is being replaced, if any. Saved key text is
  // never fetched or shown; Replace only ever sends a new one.
  let replacing = $state(null);
  let newKey = $state('');
  let showKey = $state(false);
  let keyBusy = $state(false);
  let keyResult = $state({ groq: null, deepgram: null });
  let updating = $state(false);

  const app = $derived(ui.app);
  const wordsDirty = $derived(ui.wordsDraft !== app.boostWords.join('\n'));

  function fail(section, message) {
    errors[section] = message;
  }

  async function chooseTrigger(key) {
    if (triggerBusy) return;
    triggerBusy = true;
    errors.trigger = '';
    try {
      ui.app.trigger = await invoke('set_trigger_key', { key });
    } catch (e) {
      fail('trigger', typeof e === 'string' ? e : 'Could not change the Trigger Key.');
    } finally {
      triggerBusy = false;
    }
  }

  async function toggleAutoPaste(event) {
    const box = event.currentTarget;
    autoPasteBusy = true;
    errors.dictation = '';
    try {
      ui.app.autoPaste = await invoke('set_auto_paste', { enabled: box.checked });
    } catch (e) {
      fail('dictation', 'Could not change Paste automatically.');
    } finally {
      // Render the backend's actual state, not the requested value.
      box.checked = ui.app.autoPaste;
      autoPasteBusy = false;
    }
  }

  async function toggleTone(event) {
    const box = event.currentTarget;
    toneBusy = true;
    errors.dictation = '';
    try {
      ui.app.tone = await invoke('set_tone', { tone: box.checked ? 'informal' : 'formal' });
    } catch (e) {
      fail('dictation', 'Could not change Informal tone.');
    } finally {
      box.checked = ui.app.tone === 'informal';
      toneBusy = false;
    }
  }

  async function saveWords() {
    if (!wordsDirty || wordsBusy) return;
    wordsBusy = true;
    wordsStatus = '';
    errors.words = '';
    try {
      const words = await invoke('set_boost_words', { words: ui.wordsDraft.split('\n') });
      ui.app.boostWords = words;
      // Show what is stored: trimmed, blank lines and repeats removed.
      ui.wordsDraft = words.join('\n');
      wordsStatus = words.length === 1 ? 'Saved 1 word.' : `Saved ${words.length} words.`;
    } catch (e) {
      fail('words', 'Could not save boost words.');
    } finally {
      wordsBusy = false;
    }
  }

  async function chooseSize(size) {
    errors.appearance = '';
    try {
      ui.app.overlaySize = await invoke('set_overlay_size', { size });
    } catch (e) {
      fail('appearance', 'Could not change the overlay size.');
      // The DOM radio moved but the stored size did not; move it back.
      const current = document.querySelector(`input[name="overlay-size"][value="${ui.app.overlaySize}"]`);
      if (current) current.checked = true;
    }
  }

  async function toggleAutostart(event) {
    const box = event.currentTarget;
    autostartBusy = true;
    errors.system = '';
    try {
      ui.app.autostart = await invoke('set_autostart', { enabled: box.checked });
    } catch (e) {
      fail('system', 'Could not change Start at login.');
    } finally {
      // Render what the system reports, not the requested value.
      box.checked = ui.app.autostart;
      autostartBusy = false;
    }
  }

  function startReplace(provider) {
    replacing = provider;
    newKey = '';
    showKey = false;
    errors.keys = '';
    keyResult = { groq: null, deepgram: null };
  }

  function cancelReplace() {
    replacing = null;
    newKey = '';
  }

  async function saveKey(event) {
    event.preventDefault();
    const provider = replacing;
    if (!provider || !newKey.trim() || keyBusy) return;
    keyBusy = true;
    errors.keys = '';
    try {
      // null keeps the other provider's saved key as it is.
      const res = await invoke('save_keys', {
        groq: provider === 'groq' ? newKey.trim() : null,
        deepgram: provider === 'deepgram' ? newKey.trim() : null,
      });
      keyResult = { groq: null, deepgram: null, [provider]: res[provider] };
      if (res.saved) {
        cancelReplace();
        await refresh();
      }
    } catch (e) {
      fail('keys', typeof e === 'string' ? e : 'Something went wrong while saving. Try again.');
    } finally {
      keyBusy = false;
    }
  }

  async function installUpdate() {
    updating = true;
    errors.about = '';
    try {
      // Ends this process when the installer starts.
      await invoke('install_update');
    } catch (e) {
      fail('about', 'Could not start the update.');
    } finally {
      updating = false;
    }
  }

  function open(command) {
    errors.about = '';
    command().catch((e) => fail('about', typeof e === 'string' ? e : 'Could not open it.'));
  }
</script>

{#snippet problem(section)}
  {#if errors[section]}
    <p class="error" role="alert"><span aria-hidden="true">✕</span> {errors[section]}</p>
  {/if}
{/snippet}

{#snippet toggle(id, label, hint, checked, busy, onchange)}
  <label class="row" for={id}>
    <span class="row-text">
      <span>{label}</span>
      {#if hint}<span class="hint">{hint}</span>{/if}
    </span>
    <span class="switch">
      <input {id} type="checkbox" role="switch" {checked} disabled={busy} {onchange} />
      <span class="track" aria-hidden="true"><span class="thumb"></span></span>
    </span>
  </label>
{/snippet}

<section aria-labelledby="dictation-title">
  <h2 id="dictation-title">Dictation</h2>
  <div class="card">
    <div class="block" role="group" aria-labelledby="trigger-title">
      <div class="row">
        <span id="trigger-title">Trigger Key</span>
        <kbd>{app.trigger.label}</kbd>
      </div>
      <div class="presets">
        {#each app.triggerPresets as preset (preset.key)}
          <button
            type="button"
            class="chip"
            class:on={app.trigger.key === preset.key}
            aria-pressed={app.trigger.key === preset.key}
            disabled={triggerBusy}
            onclick={() => chooseTrigger(preset.key)}
          >
            {preset.name}
          </button>
        {/each}
      </div>
      {#each app.trigger.warnings as warning (warning)}
        <p class="hint warn"><span aria-hidden="true">!</span> {warning}</p>
      {/each}
      {@render problem('trigger')}
      {#if !app.trigger.isDefault && app.triggerPresets.length}
        <button type="button" class="link" disabled={triggerBusy} onclick={() => chooseTrigger(app.triggerPresets[0].key)}>
          Reset to {app.triggerPresets[0].name}
        </button>
      {/if}
    </div>
    {@render toggle(
      'auto-paste',
      'Paste automatically',
      'Off: the Transcript goes to the clipboard for you to paste.',
      app.autoPaste,
      autoPasteBusy,
      toggleAutoPaste,
    )}
    {@render toggle(
      'informal-tone',
      'Informal tone',
      'Abbreviations like btw, no final full stop.',
      app.tone === 'informal',
      toneBusy,
      toggleTone,
    )}
    {@render problem('dictation')}
    <div class="block">
      <div class="row">
        <label for="boost-words" class="row-text">
          <span>Boost words</span>
          <span class="hint">Names and terms to hear and spell your way, one per line.</span>
        </label>
        <button type="button" class="pill" onclick={saveWords} disabled={!wordsDirty || wordsBusy}>
          {wordsBusy ? 'Saving…' : 'Save words'}
        </button>
      </div>
      <textarea
        id="boost-words"
        rows="3"
        spellcheck="false"
        autocomplete="off"
        placeholder={'Voisu\nDeepgram'}
        bind:value={ui.wordsDraft}
        oninput={() => (wordsStatus = '')}
        disabled={wordsBusy}
      ></textarea>
      {#if wordsStatus}<p class="hint ok" role="status">{wordsStatus}</p>{/if}
      {@render problem('words')}
    </div>
  </div>
</section>

<section aria-labelledby="appearance-title">
  <h2 id="appearance-title">Appearance</h2>
  <div class="card">
    <div class="row">
      <span id="size-label">Overlay size</span>
      <div class="segmented" role="radiogroup" aria-labelledby="size-label">
        {#each SIZES as size (size.id)}
          <label class:on={app.overlaySize === size.id}>
            <input
              type="radio"
              name="overlay-size"
              value={size.id}
              checked={app.overlaySize === size.id}
              onchange={() => chooseSize(size.id)}
            />
            {size.name}
          </label>
        {/each}
      </div>
    </div>
    {@render problem('appearance')}
  </div>
</section>

<section aria-labelledby="system-title">
  <h2 id="system-title">System</h2>
  <div class="card">
    {@render toggle('autostart', 'Start at login', '', app.autostart, autostartBusy, toggleAutostart)}
    {@render problem('system')}
  </div>
</section>

<section aria-labelledby="keys-title">
  <h2 id="keys-title">API keys</h2>
  <div class="card">
    {#each PROVIDERS as p (p.id)}
      <div class="block">
        <div class="row">
          <span>{p.name}</span>
          <span class="key-state">
            {#if app[`${p.id}Saved`]}
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
                oninput={() => (keyResult[p.id] = null)}
                disabled={keyBusy}
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
              <button type="submit" class="primary small" class:busy={keyBusy} disabled={!newKey.trim() || keyBusy}>
                {keyBusy ? 'Checking…' : 'Check and save'}
              </button>
              <button type="button" class="pill" onclick={cancelReplace} disabled={keyBusy}>Cancel</button>
            </div>
          </form>
        {/if}
        {#if keyResult[p.id]}
          <p class="hint {resultClass(keyResult[p.id].state)}" role="status">
            <span aria-hidden="true">{GLYPH[keyResult[p.id].state] ?? '•'}</span>
            {keyResult[p.id].message}
          </p>
        {/if}
      </div>
    {/each}
    {@render problem('keys')}
    <p class="hint">{keyStoreHint()}</p>
  </div>
</section>

<section aria-labelledby="about-title">
  <h2 id="about-title">About</h2>
  <div class="card">
    <div class="row">
      <span>Version</span>
      <span class="muted">{app.version}</span>
    </div>
    <div class="row">
      <span>Updates</span>
      {#if app.updateVersion}
        <span class="key-state">
          <span>{app.updateVersion.replace(/^v/, '')} is ready</span>
          <button type="button" class="pill" onclick={installUpdate} disabled={updating}>Restart to update</button>
        </span>
      {:else}
        <span class="muted">Up to date</span>
      {/if}
    </div>
    <div class="links">
      <button type="button" class="link" onclick={() => open(() => invoke('open_logs'))}>Open logs folder</button>
      <button type="button" class="link" onclick={() => open(openGuide)}>How to set up</button>
    </div>
    {@render problem('about')}
  </div>
</section>

<style>
  section + section {
    margin-top: 20px;
  }
  h2 {
    margin: 0 0 8px 2px;
    font-size: 12px;
    font-weight: 600;
    letter-spacing: 0.04em;
    text-transform: uppercase;
    color: var(--text-2);
  }
  .card {
    display: flex;
    flex-direction: column;
    gap: 14px;
  }
  .block {
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
  .row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    min-height: 26px;
  }
  label.row {
    cursor: pointer;
  }
  .row-text {
    display: flex;
    flex-direction: column;
  }
  .presets {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
  }
  .warn span {
    font-weight: 700;
    color: var(--processing);
  }
  .link {
    align-self: flex-start;
  }
  textarea {
    resize: vertical;
    min-height: 64px;
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: 10px;
    padding: 8px 12px;
    outline: 0;
  }
  textarea:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }
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
  .links {
    display: flex;
    gap: 20px;
  }
</style>
