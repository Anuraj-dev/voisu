<script>
  // Shown once after an update: the notes of every release since the one
  // last seen, grouped by kind. Closing it any way (Got it, Esc, a click on
  // the backdrop) calls `onclose`, which records the version as seen.
  import { onMount } from 'svelte';
  import { byKind } from './whats-new.js';
  import Logo from './Logo.svelte';

  let { releases, version, onclose, onseeall } = $props();

  let dialog;
  const groups = $derived(byKind(releases.flatMap((release) => release.items)));
  const since = $derived(releases.length > 1 ? releases.at(-1).version : null);

  onMount(() => dialog.showModal());

  // A click on the dialog element itself is a click on its backdrop: the
  // content fills the box, so it never targets the dialog.
  function onclick(event) {
    if (event.target === dialog) dialog.close();
  }

  function seeAll() {
    dialog.close();
    onseeall();
  }
</script>

<!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_noninteractive_element_interactions -->
<dialog bind:this={dialog} aria-labelledby="whats-new-title" onclose={onclose} {onclick}>
  <div class="box">
    <header>
      <Logo size={36} />
      <div>
        <p class="eyebrow">Voisu was updated</p>
        <h2 id="whats-new-title">What's new in {version}</h2>
        {#if since}<p class="hint">Everything since {since}.</p>{/if}
      </div>
    </header>
    <div class="body">
      {#each groups as group (group.id)}
        <section aria-labelledby="kind-{group.id}">
          <h3 id="kind-{group.id}" class={group.id}>{group.name}</h3>
          <ul>
            {#each group.items as item, i (i)}
              <li>{item.text}</li>
            {/each}
          </ul>
        </section>
      {/each}
    </div>
    <footer>
      <button type="button" class="link" onclick={seeAll}>See all updates</button>
      <!-- svelte-ignore a11y_autofocus -->
      <button type="button" class="primary" onclick={() => dialog.close()} autofocus>Got it</button>
    </footer>
  </div>
</dialog>

<style>
  dialog {
    width: min(440px, calc(100vw - 48px));
    max-height: calc(100vh - 48px);
    padding: 0;
    border: 1px solid var(--border);
    border-radius: 16px;
    background: #1c1f24;
    color: var(--text);
    box-shadow: 0 24px 64px rgba(0, 0, 0, 0.5);
    overflow: hidden;
  }
  dialog[open] {
    animation: dialog-in 170ms cubic-bezier(0.2, 0.8, 0.2, 1);
  }
  dialog::backdrop {
    background: rgba(8, 9, 11, 0.6);
    animation: fade-in 170ms ease-out;
  }
  @keyframes dialog-in {
    from {
      opacity: 0;
      transform: scale(0.96) translateY(4px);
    }
  }
  @keyframes fade-in {
    from {
      opacity: 0;
    }
  }
  .box {
    display: flex;
    flex-direction: column;
    max-height: calc(100vh - 50px);
  }
  header {
    display: flex;
    align-items: center;
    gap: 14px;
    padding: 20px 22px 16px;
    background: radial-gradient(120% 160% at 0% 0%, color-mix(in srgb, var(--accent) 14%, transparent), transparent 60%);
    border-bottom: 1px solid var(--hairline);
  }
  .eyebrow {
    font-size: 12px;
    font-weight: 600;
    color: var(--accent);
  }
  h2 {
    margin: 0;
    font-size: 18px;
    font-weight: 600;
    letter-spacing: -0.01em;
  }
  .body {
    display: flex;
    flex-direction: column;
    gap: 14px;
    padding: 16px 22px;
    overflow-y: auto;
  }
  h3 {
    margin: 0 0 6px;
    font-size: 11.5px;
    font-weight: 600;
    letter-spacing: 0.05em;
    text-transform: uppercase;
  }
  h3.new {
    color: var(--accent);
  }
  h3.improved {
    color: var(--processing);
  }
  h3.fixed {
    color: var(--success);
  }
  ul {
    margin: 0;
    padding: 0 0 0 16px;
    display: flex;
    flex-direction: column;
    gap: 5px;
  }
  li::marker {
    color: var(--text-2);
  }
  footer {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    padding: 12px 22px 16px;
    border-top: 1px solid var(--hairline);
  }
  .primary {
    padding: 8px 22px;
  }
</style>
