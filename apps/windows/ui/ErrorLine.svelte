<script>
  // A section's failure, shown under the section that failed.
  let { message = '' } = $props();
</script>

{#if message}
  <p class="error" role="alert"><span aria-hidden="true">✕</span> {message}</p>
{/if}
