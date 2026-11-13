<script>
  // The sidebar's 16px stroke icons, one family: 1.5 stroke, round caps,
  // currentColor.
  let { name } = $props();
</script>

<svg
  width="16"
  height="16"
  viewBox="0 0 16 16"
  fill="none"
  stroke="currentColor"
  stroke-width="1.5"
  stroke-linecap="round"
  stroke-linejoin="round"
  aria-hidden="true"
  focusable="false"
>
  {#if name === 'home'}
    <path d="M2.75 7.1 8 2.75l5.25 4.35v5.65a1 1 0 0 1-1 1H10.1V9.9H5.9v3.85H3.75a1 1 0 0 1-1-1Z" />
  {:else if name === 'dictation'}
    <rect x="5.75" y="1.75" width="4.5" height="7.5" rx="2.25" />
    <path d="M3.25 7.75a4.75 4.75 0 0 0 9.5 0M8 12.5v1.75" />
  {:else if name === 'words'}
    <path d="M2.75 3.75h10.5M2.75 8h10.5M2.75 12.25h6" />
  {:else if name === 'keys'}
    <circle cx="5.25" cy="10.75" r="2.75" />
    <path d="m7.2 8.8 6.05-6.05M11 5l1.75 1.75M9.4 6.6l1.4 1.4" />
  {:else if name === 'general'}
    <path d="M2.75 4.75h5.5M12 4.75h1.25M2.75 11.25H4M7.75 11.25h5.5" />
    <circle cx="10.1" cy="4.75" r="1.85" />
    <circle cx="5.9" cy="11.25" r="1.85" />
  {:else if name === 'whats-new'}
    <path d="M7.25 2.5 8.4 5.6l3.1 1.15-3.1 1.15-1.15 3.1-1.15-3.1L3 6.75 6.1 5.6Z" />
    <path d="M12.25 10.25v3M10.75 11.75h3" />
  {/if}
</svg>
