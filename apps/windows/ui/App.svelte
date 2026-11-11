<script>
  import { onMount } from 'svelte';
  import { invoke } from '@tauri-apps/api/core';
  import { listen } from '@tauri-apps/api/event';
  import { ui, keysReady, refresh, refreshTranscript, onRecordingState } from './state.svelte.js';
  import Setup from './Setup.svelte';
  import Overview from './Overview.svelte';
  import Settings from './Settings.svelte';

  const TABS = [
    { id: 'overview', name: 'Overview' },
    { id: 'settings', name: 'Settings' },
  ];

  let tab = $state('overview');
  // Setup holds the screen while it checks keys and while it shows its
  // "You're set" line, so a save never flashes the tabs early.
  let finishing = $state(false);
  const inSetup = $derived(!keysReady() || finishing);

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
    subscribe('state-changed', async () => {
      await refresh();
      refreshTranscript();
    });
    subscribe('recording-state', (event) => onRecordingState(event.payload));
    // The hook and Start at login can change outside the window; reread them
    // whenever it comes back to the front.
    window.addEventListener('focus', refresh);
    return () => {
      disposed = true;
      unlisteners.forEach((fn) => fn());
      window.removeEventListener('focus', refresh);
    };
  });

  let updating = $state(false);
  let updateError = $state('');

  async function installUpdate() {
    updating = true;
    updateError = '';
    try {
      // Ends this process when the installer starts.
      await invoke('install_update');
    } catch (e) {
      updateError = typeof e === 'string' ? e : 'Could not start the update.';
    } finally {
      updating = false;
    }
  }

  let starError = $state('');

  // Starring ends the banner for good; × hides it for 30 days the first time
  // and for good the second. The backend's state-changed event rereads it.
  async function answerStar(command, args) {
    starError = '';
    try {
      await invoke(command, args);
    } catch (e) {
      starError = typeof e === 'string' ? e : 'Something went wrong.';
    }
  }

  function onTabKey(event) {
    const i = TABS.findIndex((t) => t.id === tab);
    const next = {
      ArrowRight: (i + 1) % TABS.length,
      ArrowLeft: (i - 1 + TABS.length) % TABS.length,
      Home: 0,
      End: TABS.length - 1,
    }[event.key];
    if (next === undefined) return;
    event.preventDefault();
    tab = TABS[next].id;
    document.getElementById(`tab-${tab}`)?.focus();
  }
</script>

{#if ui.loaded}
  {#if inSetup}
    <main class="scroll">
      <Setup bind:finishing />
    </main>
  {:else}
    <main class="tabbed">
      <!-- Above the tabs, so a waiting update shows whichever tab is open. -->
      {#if ui.app.updateVersion}
        <div class="update" role="status">
          <span class="update-text">
            <strong>Update available</strong>
            <span>Voisu {ui.app.updateVersion.replace(/^v/, '')} is ready to install.</span>
            {#if updateError}<span class="update-error" role="alert">{updateError}</span>{/if}
          </span>
          <button type="button" class="pill" onclick={installUpdate} disabled={updating}>
            {updating ? 'Restarting…' : 'Restart to update'}
          </button>
        </div>
      {/if}
      {#if ui.app.showStar}
        <div class="update" role="status">
          <span class="update-text">
            <strong>Enjoying Voisu?</strong>
            {#if starError}<span class="update-error" role="alert">{starError}</span>{/if}
          </span>
          <span class="star-actions">
            <button type="button" class="pill" onclick={() => answerStar('star_on_github')}>Star on GitHub</button>
            <button type="button" class="close" aria-label="Close" title="Close" onclick={() => answerStar('close_star_banner')}>
              <span aria-hidden="true">×</span>
            </button>
          </span>
        </div>
      {/if}
      <!-- svelte-ignore a11y_interactive_supports_focus -->
      <div class="tabs" role="tablist" aria-label="Voisu" onkeydown={onTabKey}>
        {#each TABS as t (t.id)}
          <button
            type="button"
            role="tab"
            id="tab-{t.id}"
            aria-selected={tab === t.id}
            aria-controls={tab === t.id ? `panel-${t.id}` : undefined}
            tabindex={tab === t.id ? 0 : -1}
            onclick={() => (tab = t.id)}
          >
            {t.name}
          </button>
        {/each}
      </div>
      <div class="panel" role="tabpanel" id="panel-{tab}" aria-labelledby="tab-{tab}" tabindex="0">
        {#if tab === 'overview'}
          <Overview />
        {:else}
          <Settings />
        {/if}
      </div>
    </main>
  {/if}
{:else if ui.error}
  <main class="scroll"><p class="error" role="alert">{ui.error}</p></main>
{/if}

<style>
  main {
    height: 100vh;
  }
  .scroll {
    overflow-y: auto;
    padding: 28px 24px 20px;
  }
  .tabbed {
    display: flex;
    flex-direction: column;
  }
  .update {
    flex: none;
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    padding: 10px 24px;
    background: color-mix(in srgb, var(--accent) 16%, transparent);
    border-bottom: 1px solid color-mix(in srgb, var(--accent) 45%, transparent);
  }
  .update-text {
    display: flex;
    flex-direction: column;
    font-size: 13px;
  }
  .star-actions {
    flex: none;
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .close {
    background: none;
    border: 0;
    padding: 0 4px;
    margin-right: -10px;
    font-size: 18px;
    line-height: 1;
    color: var(--text-2);
    border-radius: 4px;
  }
  .close:hover {
    color: var(--text);
  }
  .update-error {
    color: var(--failure);
  }
  .tabs {
    flex: none;
    display: flex;
    gap: 4px;
    padding: 10px 24px 0;
    border-bottom: 1px solid var(--overlay-2);
  }
  [role='tab'] {
    background: none;
    border: 0;
    border-bottom: 2px solid transparent;
    margin-bottom: -1px;
    padding: 8px 12px;
    color: var(--text-2);
    font-weight: 600;
    border-radius: 6px 6px 0 0;
  }
  [role='tab']:hover {
    color: var(--text);
  }
  [role='tab'][aria-selected='true'] {
    color: var(--text);
    border-bottom-color: var(--accent);
  }
  [role='tab']:focus-visible {
    outline-offset: -2px;
  }
  .panel {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    padding: 20px 24px 16px;
  }
  .panel:focus-visible {
    outline-offset: -2px;
  }
</style>
