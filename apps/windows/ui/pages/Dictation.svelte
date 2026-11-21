<script>
  import { invoke } from '@tauri-apps/api/core';
  import { listen } from '@tauri-apps/api/event';
  import { ui, isMac } from '../state.svelte.js';
  import ToggleRow from '../ToggleRow.svelte';
  import ErrorLine from '../ErrorLine.svelte';

  // One message per section, shown under the section that failed.
  let errors = $state({});
  let triggerBusy = $state(false);
  let autoPasteBusy = $state(false);
  let toneBusy = $state(false);
  let micBusy = $state(false);

  // Keep microphone ready (Windows): the backend keeps the stream open so a
  // press starts at once. The default first, then from most to least ready.
  const MIC_CHOICES = [
    { mode: 'while_running', name: 'While Voisu is running' },
    { mode: 'for_five_minutes', name: 'For 5 minutes after each use' },
    { mode: 'off', name: 'Off' },
  ];

  async function chooseMicReady(mode) {
    if (micBusy || ui.app.keepMicReady === mode) return;
    micBusy = true;
    errors.mic = '';
    try {
      ui.app.keepMicReady = await invoke('set_keep_mic_ready', { mode });
    } catch (e) {
      errors.mic = 'Could not change Keep microphone ready.';
    } finally {
      micBusy = false;
    }
  }

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

  // The latest start, so a cancel on leaving the page goes after it.
  let moveRequest = Promise.resolve(null);
  // The move mode that start opened (null if none). A finish names it, so
  // one that lands after the pill's ✓ or ✗ cannot end a later move.
  let moveId = null;

  // A start answer can land after its move already ended (a Recording, the
  // pill's ✗) and the state refresh said so: it must not bring the move
  // look back.
  const ended = (id) =>
    ui.app.lastOverlayMove > id || (ui.app.lastOverlayMove === id && !ui.app.overlayMoving);

  const startMove = () =>
    positionAction(async () => {
      moveRequest = invoke('start_overlay_move');
      const id = (await moveRequest) ?? null;
      moveId = id != null && !ended(id) ? id : null;
      ui.app.overlayMoving = moveId != null;
    }, 'Could not show the overlay.');

  // Enter (`save`) or Esc here, like the pill's own ✓ and ✗.
  const finishMove = (save) =>
    positionAction(async () => {
      if (moveId == null) return;
      ui.app.overlayPosition = await invoke('finish_overlay_move', { moveId, save });
      ui.app.overlayMoving = false;
      moveId = null;
    }, 'Could not save the overlay position.');

  const resetPosition = () =>
    positionAction(async () => {
      ui.app.overlayPosition = await invoke('reset_overlay_position');
    }, 'Could not reset the overlay position.');

  // The overlay is not meant to take focus, so its keys are heard here too:
  // Enter saves, Esc cancels. Enter on a working control is left to it (a
  // disabled one does nothing). Repeats never finish: Enter held on "Move
  // overlay" would otherwise save the moment move mode starts, and a key
  // pressed before then reaches here only as repeats.
  const CONTROLS = 'button, a, input, select, textarea, [contenteditable]';
  function onKeydown(event) {
    if (!moving || event.defaultPrevented || event.repeat) return;
    const control = event.target.closest?.(CONTROLS);
    if (event.key === 'Escape') {
      event.preventDefault();
      finishMove(false);
    } else if (event.key === 'Enter' && !(control && !control.disabled)) {
      event.preventDefault();
      finishMove(true);
    }
  }

  // ✓ on the pill finishes the move there and the pill hides, so a save
  // that failed is reported here. The state refresh that follows ends the
  // move look.
  $effect(() => {
    let disposed = false;
    let unlisten = () => {};
    listen('overlay-move-failed', ({ payload }) => {
      errors.position = typeof payload === 'string' ? payload : 'Could not save the overlay position.';
    }).then((fn) => {
      if (disposed) fn();
      else unlisten = fn;
    });
    return () => {
      disposed = true;
      unlisten();
    };
  });

  // Leaving the page cancels any move, so the pill never stays grabbable
  // with nothing on screen to explain it. Always sent, after a start still in flight: a
  // start that lands later would otherwise leave move mode on.
  $effect(() => () => {
    moveRequest
      .catch(() => null)
      .then((id) => id != null && invoke('finish_overlay_move', { moveId: id, save: false }))
      .catch(() => {});
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

{#if !isMac()}
  <div class="group">
    <div class="block" role="group" aria-labelledby="mic-ready-title" aria-describedby="mic-ready-hint">
      <span class="row-text">
        <span id="mic-ready-title">Keep microphone ready</span>
        <span class="hint" id="mic-ready-hint">
          Recording starts instantly instead of after ~0.8 s, so your first word is never cut off. Windows shows the
          mic icon while it's ready.
        </span>
      </span>
      <div class="presets">
        {#each MIC_CHOICES as choice (choice.mode)}
          <button
            type="button"
            class="chip"
            class:on={app.keepMicReady === choice.mode}
            aria-pressed={app.keepMicReady === choice.mode}
            disabled={micBusy}
            onclick={() => chooseMicReady(choice.mode)}
          >
            {choice.name}
          </button>
        {/each}
      </div>
      <ErrorLine message={errors.mic} />
    </div>
  </div>
{/if}

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
  <div class="row" role="group" aria-labelledby="position-label">
    <span class="row-text" aria-live="polite">
      {#if moving}
        <span id="position-label">Drag the overlay where you want it</span>
        <span class="hint">Use ✓ / ✗ on the overlay. Enter saves, Esc cancels.</span>
      {:else}
        <span id="position-label">Overlay position</span>
        <span class="hint">{app.overlayPosition ? 'Where you put it.' : 'Bottom centre.'}</span>
      {/if}
    </span>
    <div class="actions">
      {#if !moving}
        <button type="button" class="pill" id="move-overlay" disabled={positionBusy} onclick={startMove}>
          Move overlay
        </button>
      {/if}
      <button
        type="button"
        class="pill"
        disabled={positionBusy || moving || !app.overlayPosition}
        onclick={resetPosition}
      >
        Reset position
      </button>
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
</style>
