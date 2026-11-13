<script>
  import { ui, unseenReleases } from './state.svelte.js';
  import Logo from './Logo.svelte';
  import Icon from './Icon.svelte';

  const PAGES = [
    { id: 'home', name: 'Home' },
    { id: 'dictation', name: 'Dictation' },
    { id: 'words', name: 'Words' },
    { id: 'keys', name: 'API keys' },
    { id: 'general', name: 'General' },
  ];

  const unseen = $derived(unseenReleases().length > 0);

  // Up and Down move focus through every item, What's new included; Enter
  // or Space opens the page.
  function onKey(event) {
    const items = [...event.currentTarget.querySelectorAll('[data-page]')];
    const i = items.indexOf(document.activeElement);
    if (i < 0) return;
    const next = {
      ArrowDown: (i + 1) % items.length,
      ArrowUp: (i - 1 + items.length) % items.length,
      Home: 0,
      End: items.length - 1,
    }[event.key];
    if (next === undefined) return;
    event.preventDefault();
    items[next].focus();
  }
</script>

{#snippet item(id, name, dot = false)}
  <button
    type="button"
    class="item"
    data-page={id}
    aria-current={ui.page === id ? 'page' : undefined}
    title={name}
    onclick={() => (ui.page = id)}
  >
    <Icon name={id} />
    <span class="label">{name}</span>
    {#if dot}<span class="dot" title="New notes"></span><span class="sr">, new notes</span>{/if}
  </button>
{/snippet}

<!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
<nav aria-label="Voisu" onkeydown={onKey}>
  <div class="brand">
    <Logo size={26} />
    <span class="label">Voisu</span>
  </div>
  <ul>
    {#each PAGES as page (page.id)}
      <li>{@render item(page.id, page.name)}</li>
    {/each}
  </ul>
  <div class="foot">
    {@render item('whats-new', "What's new", unseen)}
    <p class="version">v{ui.app.version}</p>
  </div>
</nav>

<style>
  nav {
    display: flex;
    flex-direction: column;
    height: 100%;
    padding: 16px 10px 12px;
    background: var(--sidebar);
    border-right: 1px solid var(--hairline);
  }
  .brand {
    display: flex;
    align-items: center;
    gap: 9px;
    padding: 0 8px 18px;
    font-size: 15px;
    font-weight: 600;
    letter-spacing: -0.01em;
  }
  ul {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .foot {
    margin-top: auto;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .item {
    display: flex;
    align-items: center;
    gap: 10px;
    width: 100%;
    padding: 6px 10px;
    border: 0;
    border-radius: 8px;
    background: none;
    color: var(--text-2);
    text-align: left;
    transition: background 0.12s, color 0.12s;
  }
  .item:hover {
    color: var(--text);
    background: var(--overlay-1);
  }
  .item[aria-current='page'] {
    color: var(--text);
    background: var(--overlay-2);
  }
  .item[aria-current='page'] :global(svg) {
    color: var(--accent);
  }
  .item:focus-visible {
    outline-offset: -2px;
  }
  .dot {
    margin-left: auto;
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: var(--accent);
    box-shadow: 0 0 0 3px color-mix(in srgb, var(--accent) 18%, transparent);
  }
  .sr {
    position: absolute;
    width: 1px;
    height: 1px;
    overflow: hidden;
    clip-path: inset(50%);
    white-space: nowrap;
  }
  /* A narrow window keeps the icons and hides the words, so the default
     portrait window leaves the page its room. The labels stay readable to
     screen readers and show as tooltips. */
  @media (max-width: 620px) {
    nav {
      padding: 16px 8px 12px;
    }
    .brand {
      justify-content: center;
      padding: 0 0 18px;
    }
    .item {
      justify-content: center;
      position: relative;
      padding: 8px 0;
    }
    .label {
      position: absolute;
      width: 1px;
      height: 1px;
      overflow: hidden;
      clip-path: inset(50%);
      white-space: nowrap;
    }
    .dot {
      position: absolute;
      top: 5px;
      right: 9px;
      margin: 0;
    }
    .version {
      display: none;
    }
  }
  .version {
    padding: 0 10px;
    font-size: 11.5px;
    color: var(--text-2);
    opacity: 0.7;
    font-variant-numeric: tabular-nums;
  }
</style>
