<script>
  import { onMount } from 'svelte';
  import NOTES from '../whats-new.json';
  import { history } from '../whats-new.js';
  import { ui, installUpdate, pendingNotes, unseenReleases, markVersionSeen } from '../state.svelte.js';
  import ReleaseItems from '../ReleaseItems.svelte';

  const releases = $derived(history(NOTES, ui.app.version, ui.app.platform));
  const pending = $derived(ui.app.updateVersion?.replace(/^v/, '') ?? null);

  // Reading the notes here counts as seeing them, as closing the dialog does.
  onMount(() => {
    if (unseenReleases().length) markVersionSeen();
  });

  // 2026-11-11 → 11 Nov 2026, the same everywhere: these are release dates,
  // not the user's.
  const MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];
  function formatDate(date) {
    const [y, m, d] = String(date).split('-').map(Number);
    return y && m && d ? `${d} ${MONTHS[m - 1]} ${y}` : '';
  }
</script>

<header class="page-head">
  <h1>What's new</h1>
  <p>Changes in each Voisu release, newest first.</p>
</header>

<div class="group">
  <div class="row">
    <span class="row-text">
      <span>Voisu {ui.app.version}</span>
      {#if pending}
        <span class="hint ok">{pending} is ready</span>
      {:else}
        <span class="hint">Up to date</span>
      {/if}
    </span>
    {#if pending}
      <button type="button" class="pill" onclick={installUpdate} disabled={ui.updating}>
        {ui.updating ? 'Restarting…' : 'Restart to update'}
      </button>
    {/if}
  </div>
  {#if ui.updateError}<p class="error" role="alert"><span aria-hidden="true">✕</span> {ui.updateError}</p>{/if}
</div>

{#if pending && pendingNotes().length}
  <h2 class="group-label" id="pending-title">Ready to install</h2>
  <section class="group pending" aria-labelledby="pending-title">
    <div class="release">
      <h3>{pending}</h3>
      <ReleaseItems items={pendingNotes()} />
    </div>
  </section>
{/if}

<h2 class="group-label" id="history-title">Releases</h2>
<section class="group" aria-labelledby="history-title">
  {#each releases as release (release.version)}
    <article class="release" aria-labelledby="release-{release.version}">
      <h3 id="release-{release.version}">
        {release.version}
        {#if release.version === ui.app.version}<span class="installed">Installed</span>{/if}
        <span class="date">{formatDate(release.date)}</span>
      </h3>
      <ReleaseItems items={release.items} />
    </article>
  {:else}
    <p class="muted">No notes for this version.</p>
  {/each}
</section>

<style>
  .release {
    display: flex;
    flex-direction: column;
    gap: 9px;
    padding-top: 13px;
    padding-bottom: 14px;
  }
  h3 {
    display: flex;
    align-items: baseline;
    gap: 8px;
    margin: 0;
    font-size: 14px;
    font-weight: 600;
    font-variant-numeric: tabular-nums;
  }
  .date {
    margin-left: auto;
    font-size: 12px;
    font-weight: 400;
    color: var(--text-2);
  }
  .installed {
    font-size: 11px;
    font-weight: 600;
    color: var(--text-2);
    padding: 0 7px;
    border: 1px solid var(--border);
    border-radius: 32px;
    line-height: 17px;
  }
  .pending {
    border-color: color-mix(in srgb, var(--accent) 38%, transparent);
    background: color-mix(in srgb, var(--accent) 5%, var(--overlay-1));
  }
</style>
