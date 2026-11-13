<script>
  // One switch row of a grouped card. The page's handler sets the box back
  // to the backend's actual state after the call.
  let { id, label, hint = '', checked, busy = false, onchange } = $props();
</script>

<label class="row" for={id}>
  <span class="row-text">
    <span>{label}</span>
    {#if hint}<span class="hint">{hint}</span>{/if}
  </span>
  <span class="switch">
    <input {id} type="checkbox" role="switch" {checked} disabled={busy} {onchange} />
    <span class="track" aria-hidden="true"><span class="thumb"></span></span>
  </span>
</label>
