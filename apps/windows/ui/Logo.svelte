<script>
  // The "Caret" mark: three mint bars, the middle one tallest, ending in a
  // text cursor. No tile, so it sits on any surface. `blink` blinks the
  // cursor; the global reduced-motion rule stops it.
  let { size = 24, blink = false } = $props();
</script>

<svg width={size} height={size} viewBox="40 40 176 176" aria-hidden="true" focusable="false">
  <g fill="var(--accent)">
    <rect x="54" y="100" width="24" height="56" rx="12" />
    <rect x="94" y="66" width="24" height="124" rx="12" />
    <rect x="134" y="86" width="24" height="84" rx="12" />
  </g>
  <g fill="var(--text)" class:blink>
    <rect x="183" y="60" width="10" height="136" rx="5" />
    <rect x="172" y="58" width="32" height="10" rx="5" />
    <rect x="172" y="188" width="32" height="10" rx="5" />
  </g>
</svg>

<style>
  svg {
    flex: none;
  }
  .blink {
    animation: caret-blink 1.1s steps(1) infinite;
  }
  @keyframes caret-blink {
    50% {
      opacity: 0;
    }
  }
</style>
