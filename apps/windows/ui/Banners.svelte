<script>
  // The app-wide banners at the top of the content pane, so they show over
  // whichever page is open: a waiting update and the star prompt. What's new
  // carries the update itself, so the update banner steps aside there.
  import { invoke } from '@tauri-apps/api/core';
  import { ui, installUpdate, pendingNotes } from './state.svelte.js';

  let starError = $state('');
  const showUpdate = $derived(ui.app.updateVersion && ui.page !== 'whats-new');

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
</script>

{#if showUpdate || ui.app.showStar}
  <div class="banners">
    {#if showUpdate}
      <div class="banner" role="status">
        <span class="text">
          <strong>Update available</strong>
          <span class="muted">Voisu {ui.app.updateVersion.replace(/^v/, '')} is ready to install.</span>
          {#if ui.updateError}<span class="error" role="alert">{ui.updateError}</span>{/if}
        </span>
        <span class="actions">
          {#if pendingNotes().length}
            <button type="button" class="link quiet" onclick={() => (ui.page = 'whats-new')}>What's new</button>
          {/if}
          <button type="button" class="pill" onclick={installUpdate} disabled={ui.updating}>
            {ui.updating ? 'Restarting…' : 'Restart to update'}
          </button>
        </span>
      </div>
    {/if}
    {#if ui.app.showStar}
      <div class="banner" role="status">
        <span class="text">
          <strong>Enjoying Voisu?</strong>
          {#if starError}<span class="error" role="alert">{starError}</span>{/if}
        </span>
        <span class="actions">
          <button type="button" class="pill" onclick={() => answerStar('star_on_github')}>Star on GitHub</button>
          <button type="button" class="close" aria-label="Close" title="Close" onclick={() => answerStar('close_star_banner')}>
            <span aria-hidden="true">×</span>
          </button>
        </span>
      </div>
    {/if}
  </div>
{/if}

<style>
  .banners {
    flex: none;
    display: flex;
    flex-direction: column;
    gap: 8px;
    padding: 16px 28px 0;
  }
  .banner {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    padding: 9px 10px 9px 14px;
    border-radius: var(--radius);
    background: color-mix(in srgb, var(--accent) 11%, var(--surface));
    border: 1px solid color-mix(in srgb, var(--accent) 38%, transparent);
  }
  .text {
    display: flex;
    flex-direction: column;
    min-width: 0;
    font-size: 13px;
  }
  .actions {
    flex: none;
    display: flex;
    align-items: center;
    gap: 12px;
  }
  .quiet {
    font-size: 13px;
  }
  .close {
    background: none;
    border: 0;
    padding: 0 4px;
    font-size: 18px;
    line-height: 1;
    color: var(--text-2);
    border-radius: 4px;
  }
  .close:hover {
    color: var(--text);
  }
</style>
