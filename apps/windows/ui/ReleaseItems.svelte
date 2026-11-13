<script>
  // One release's notes: each item with its kind label. Shared by the What's
  // new page and the dialog after an update.
  import { KINDS } from './whats-new.js';

  let { items } = $props();

  const name = (kind) => KINDS.find((k) => k.id === kind)?.name ?? kind;
</script>

<ul>
  {#each items as item, i (i)}
    <li>
      <span class="kind {item.kind}">{name(item.kind)}</span>
      <span class="text">{item.text}</span>
    </li>
  {/each}
</ul>

<style>
  ul {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 7px;
  }
  li {
    display: grid;
    grid-template-columns: 70px 1fr;
    align-items: baseline;
    gap: 10px;
  }
  .kind {
    justify-self: start;
    padding: 0 7px;
    border-radius: 6px;
    font-size: 11px;
    font-weight: 600;
    letter-spacing: 0.03em;
    line-height: 18px;
    color: var(--k);
    background: color-mix(in srgb, var(--k) 14%, transparent);
  }
  .new {
    --k: var(--accent);
  }
  .improved {
    --k: var(--processing);
  }
  .fixed {
    --k: var(--success);
  }
</style>
