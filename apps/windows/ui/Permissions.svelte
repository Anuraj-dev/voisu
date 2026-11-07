<script>
  // macOS only: the privacy permissions Voisu still needs, each with a button
  // that asks for it and opens its pane of System Settings. The window
  // rereads them when it comes back to the front.
  import { invoke } from '@tauri-apps/api/core';
  import { missingPermissions, refresh } from './state.svelte.js';

  const missing = $derived(missingPermissions());
  let error = $state('');

  async function open(pane) {
    error = '';
    try {
      await invoke('open_permission_settings', { pane });
    } catch (e) {
      error = typeof e === 'string' ? e : "Couldn't open System Settings.";
    }
    refresh();
  }
</script>

{#if missing.length}
  <section class="card permissions" aria-labelledby="permissions-title">
    <h2 id="permissions-title">Allow Voisu in System Settings</h2>
    <p class="hint">Privacy &amp; Security → turn on Voisu in each list below.</p>
    <ul>
      {#each missing as item (item.pane)}
        <li>
          <span class="text">
            <span>{item.name}</span>
            <span class="hint">{item.why}</span>
          </span>
          <button type="button" class="pill" onclick={() => open(item.pane)}>Open System Settings</button>
        </li>
      {/each}
    </ul>
    {#if error}<p class="error" role="alert"><span aria-hidden="true">✕</span> {error}</p>{/if}
  </section>
{/if}

<style>
  .permissions {
    display: flex;
    flex-direction: column;
    gap: 8px;
    border-color: color-mix(in srgb, var(--accent) 45%, transparent);
  }
  h2 {
    font-size: 14px;
    font-weight: 600;
  }
  ul {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
  }
  li {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    padding: 8px 0;
    border-top: 1px solid var(--overlay-2);
  }
  .text {
    display: flex;
    flex-direction: column;
  }
</style>
