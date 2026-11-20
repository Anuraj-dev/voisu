<script>
  import { invoke } from '@tauri-apps/api/core';
  import { ui } from '../state.svelte.js';
  import ToggleRow from '../ToggleRow.svelte';
  import ErrorLine from '../ErrorLine.svelte';

  const SIZES = [
    { id: 'small', name: 'Small' },
    { id: 'medium', name: 'Medium' },
    { id: 'large', name: 'Large' },
  ];

  // One message per section, shown under the section that failed.
  let errors = $state({});
  let triggerBusy = $state(false);
  let autoPasteBusy = $state(false);
  let toneBusy = $state(false);

  const app = $derived(ui.app);

  async function chooseTrigger(key) {
    if (triggerBusy) return;
    triggerBusy = true;
    errors.trigger = '';
    try {
      ui.app.trigger = await invoke('set_trigger_key', { key });
    } catch (e) {
      errors.trigger = typeof e === 'string' ? e : 'Could not change the Trigger Key.';
    } finally {
      triggerBusy = false;
    }
  }

  async function toggleAutoPaste(event) {
    const box = event.currentTarget;
    autoPasteBusy = true;
    errors.dictation = '';
    try {
      ui.app.autoPaste = await invoke('set_auto_paste', { enabled: box.checked });
    } catch (e) {
      errors.dictation = 'Could not change Paste automatically.';
    } finally {
      // Render the backend's actual state, not the requested value.
      box.checked = ui.app.autoPaste;
      autoPasteBusy = false;
    }
  }

  async function toggleTone(event) {
    const box = event.currentTarget;
    toneBusy = true;
    errors.dictation = '';
    try {
      ui.app.tone = await invoke('set_tone', { tone: box.checked ? 'informal' : 'formal' });
    } catch (e) {
      errors.dictation = 'Could not change Informal tone.';
    } finally {
      box.checked = ui.app.tone === 'informal';
      toneBusy = false;
    }
  }

  async function chooseSize(size) {
    errors.appearance = '';
    try {
      ui.app.overlaySize = await invoke('set_overlay_size', { size });
    } catch (e) {
      errors.appearance = 'Could not change the overlay size.';
      // The DOM radio moved but the stored size did not; move it back.
      const current = document.querySelector(`input[name="overlay-size"][value="${ui.app.overlaySize}"]`);
      if (current) current.checked = true;
    }
  }

  let positionBusy = $state(false);
  const moving = $derived(app.overlayMoving);

  async function positionAction(run, failure) {
    if (positionBusy) return;
    positionBusy = true;
    errors.position = '';
    try {
      await run();
    } catch (e) {
      errors.position = typeof e === 'string' ? e : failure;
    } finally {
      positionBusy = false;
    }
  }

  const startMove = () =>
    positionAction(async () => {
      await invoke('start_overlay_move');
      ui.app.overlayMoving = true;
    }, 'Could not show the overlay.');

  // Done (`save`) or Cancel.
  const finishMove = (save) =>
    positionAction(async () => {
      ui.app.overlayPosition = await invoke('finish_overlay_move', { save });
      ui.app.overlayMoving = false;
    }, 'Could not save the overlay position.');

  const resetPosition = () =>
    positionAction(async () => {
      ui.app.overlayPosition = await invoke('reset_overlay_position');
    }, 'Could not reset the overlay position.');

  // The overlay never takes focus, so Esc is heard here.
  function onKeydown(event) {
    if (moving && event.key === 'Escape' && !event.defaultPrevented) {
      event.preventDefault();
      finishMove(false);
    }
  }

  // Leaving the page while moving cancels, so the pill never stays grabbable
  // with no Done in sight.
  $effect(() => () => {
    if (ui.app.overlayMoving) invoke('finish_overlay_move', { save: false }).catch(() => {});
  });
</script>

<svelte:window onkeydown={onKeydown} />

<header class="page-head">
  <h1>Dictation</h1>
  <p>How you start a Recording and where the Transcript goes.</p>
</header>

<div class="group">
  <div class="block" role="group" aria-labelledby="trigger-title">
    <div class="row">
      <span class="row-text">
        <span id="trigger-title">Trigger Key</span>
        <span class="hint">Hold it to dictate, double-tap for hands-free.</span>
      </span>
      <kbd>{app.trigger.label}</kbd>
    </div>
    <div class="presets">
      {#each app.triggerPresets as preset (preset.key)}
        <button
          type="button"
          class="chip"
          class:on={app.trigger.key === preset.key}
          aria-pressed={app.trigger.key === preset.key}
          disabled={triggerBusy}
          onclick={() => chooseTrigger(preset.key)}
        >
          {preset.name}
        </button>
      {/each}
    </div>
    {#each app.trigger.warnings as warning (warning)}
      <p class="hint warn"><span aria-hidden="true">!</span> {warning}</p>
    {/each}
    <ErrorLine message={errors.trigger} />
    {#if !app.trigger.isDefault && app.triggerPresets.length}
      <button type="button" class="link" disabled={triggerBusy} onclick={() => chooseTrigger(app.triggerPresets[0].key)}>
        Reset to {app.triggerPresets[0].name}
      </button>
    {/if}
  </div>
</div>

<div class="group">
  <ToggleRow
    id="auto-paste"
    label="Paste automatically"
    hint="Off: the Transcript goes to the clipboard for you to paste."
    checked={app.autoPaste}
    busy={autoPasteBusy}
    onchange={toggleAutoPaste}
  />
  <ToggleRow
    id="informal-tone"
    label="Informal tone"
    hint="Abbreviations like btw, no final full stop."
    checked={app.tone === 'informal'}
    busy={toneBusy}
    onchange={toggleTone}
  />
  <ErrorLine message={errors.dictation} />
  <div class="row">
    <span class="row-text">
      <span id="size-label">Overlay size</span>
      <span class="hint">The pill that shows while you dictate.</span>
    </span>
    <div class="segmented" class:off={moving} role="radiogroup" aria-labelledby="size-label">
      {#each SIZES as size (size.id)}
        <label class:on={app.overlaySize === size.id}>
          <input
            type="radio"
            name="overlay-size"
            value={size.id}
            checked={app.overlaySize === size.id}
            disabled={moving}
            onchange={() => chooseSize(size.id)}
          />
          {size.name}
        </label>
      {/each}
    </div>
  </div>
  <ErrorLine message={errors.appearance} />
  <div class="row" role="group" aria-labelledby="position-label">
    <span class="row-text" aria-live="polite">
      {#if moving}
        <span id="position-label">Drag the overlay where you want it</span>
        <span class="hint">Esc cancels.</span>
      {:else}
        <span id="position-label">Overlay position</span>
        <span class="hint">{app.overlayPosition ? 'Where you put it.' : 'Top centre.'}</span>
      {/if}
    </span>
    <div class="actions">
      {#if moving}
        <button type="button" class="pill" disabled={positionBusy} onclick={() => finishMove(false)}>Cancel</button>
        <button type="button" class="primary small" disabled={positionBusy} onclick={() => finishMove(true)}>Done</button>
      {:else}
        <button type="button" class="pill" id="move-overlay" disabled={positionBusy} onclick={startMove}>
          Move overlay
        </button>
        <button type="button" class="pill" disabled={positionBusy || !app.overlayPosition} onclick={resetPosition}>
          Reset position
        </button>
      {/if}
    </div>
  </div>
  <ErrorLine message={errors.position} />
</div>

<style>
  .presets {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
  }
  .warn span {
    font-weight: 700;
    color: var(--processing);
  }
  .link {
    align-self: flex-start;
  }
  .actions {
    display: flex;
    flex: none;
    align-items: center;
    gap: 6px;
  }
  .primary.small {
    padding: 3px 14px;
  }
  .segmented.off {
    opacity: 0.5;
  }
  .segmented.off input {
    cursor: not-allowed;
  }
</style>
