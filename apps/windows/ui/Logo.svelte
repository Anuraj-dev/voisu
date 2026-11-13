<script>
  // The three-bar mark: mint bars, the middle one tallest. No tile, so it
  // sits on any surface.
  let { size = 24 } = $props();
</script>

<svg width={size} height={size} viewBox="58 58 140 140" aria-hidden="true" focusable="false">
  <g fill="var(--accent)">
    <rect x="71" y="96" width="26" height="60" rx="13" />
    <rect x="115" y="68" width="26" height="120" rx="13" />
    <rect x="159" y="88" width="26" height="76" rx="13" />
  </g>
</svg>

<style>
  svg {
    flex: none;
  }
</style>
