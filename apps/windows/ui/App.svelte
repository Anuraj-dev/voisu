<script>
  import { onMount } from 'svelte';
  import { listen } from '@tauri-apps/api/event';
  import {
    ui,
    keysReady,
    refresh,
    refreshTranscript,
    onRecordingState,
    unseenReleases,
    markVersionSeen,
  } from './state.svelte.js';
  import Setup from './Setup.svelte';
  import Sidebar from './Sidebar.svelte';
  import Banners from './Banners.svelte';
  import WhatsNewDialog from './WhatsNewDialog.svelte';
  import Home from './pages/Home.svelte';
  import Dictation from './pages/Dictation.svelte';
  import Words from './pages/Words.svelte';
  import ApiKeys from './pages/ApiKeys.svelte';
  import General from './pages/General.svelte';
  import WhatsNew from './pages/WhatsNew.svelte';

  const PAGES = { home: Home, dictation: Dictation, words: Words, keys: ApiKeys, general: General, 'whats-new': WhatsNew };
  const Page = $derived(PAGES[ui.page] ?? Home);

  // Setup holds the screen while it checks keys and while it shows its
  // "You're set" line, so a save never flashes the sidebar early.
  let finishing = $state(false);
  const inSetup = $derived(!keysReady() || finishing);

  // After an update, the notes since the last version seen, shown once per
  // launch. A version with no notes for this platform is marked seen quietly.
  let prompted = false;
  let dialogReleases = $state(null);
  $effect(() => {
    if (prompted || !ui.loaded || inSetup || !ui.app.version || ui.app.seenVersion === ui.app.version) return;
    prompted = true;
    const releases = unseenReleases();
    if (releases.length) dialogReleases = releases;
    else markVersionSeen();
  });

  function closeDialog() {
    dialogReleases = null;
    markVersionSeen();
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
</script>

{#if ui.loaded}
  {#if inSetup}
    <main class="setup">
      <Setup bind:finishing />
    </main>
  {:else}
    <div class="shell">
      <Sidebar />
      <main class="content">
        <!-- Above the page, so a waiting update shows whichever page is open. -->
        <Banners />
        <div class="pane">
          {#key ui.page}
            <div class="page">
              <Page />
            </div>
          {/key}
        </div>
      </main>
    </div>
    {#if dialogReleases}
      <WhatsNewDialog
        releases={dialogReleases}
        version={ui.app.version}
        onclose={closeDialog}
        onseeall={() => (ui.page = 'whats-new')}
      />
    {/if}
  {/if}
{:else if ui.error}
  <main class="setup"><p class="error" role="alert">{ui.error}</p></main>
{/if}

<style>
  .setup {
    height: 100vh;
    overflow-y: auto;
  }
  .setup > .error {
    margin: 28px 24px;
  }
  .shell {
    display: grid;
    grid-template-columns: 188px 1fr;
    height: 100vh;
  }
  .content {
    display: flex;
    flex-direction: column;
    min-width: 0;
    min-height: 0;
  }
  /* Pages fit the window; scrolling is only a fallback for long content. */
  .pane {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    padding: 22px 28px 20px;
  }
  :global(.banners) + .pane {
    padding-top: 16px;
  }
  .page {
    animation: page-in 120ms ease-out;
  }
</style>
