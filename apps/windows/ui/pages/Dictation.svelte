<script>
  import { invoke } from '@tauri-apps/api/core';
  import { listen } from '@tauri-apps/api/event';
  import { ui } from '../state.svelte.js';
  import ToggleRow from '../ToggleRow.svelte';
  import ErrorLine from '../ErrorLine.svelte';

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

  // Local mode (Windows only): null on macOS, where nothing below shows.
  const local = $derived(app.local);
  const mb = (bytes) => Math.round(bytes / 1_000_000);
  let modeBusy = $state(false);

  const tier = (id) => local?.tiers.find((t) => t.id === id);
  const MODEL_CHOICES = [
    { id: 'auto', label: 'Auto' },
    { id: 'fast', label: 'Fast' },
    { id: 'accurate', label: 'Accurate' },
  ];
  // A choice whose model isn't downloaded waits here for the user to agree
  // to its download; null shows the saved setting.
  let pendingModel = $state(null);
  const shownModel = $derived(pendingModel ?? local?.model ?? 'auto');
  // The tier a choice runs on this PC.
  const tierFor = (id) => tier(id === 'auto' ? local.autoTier : id);
  const shownTier = $derived(local ? tierFor(shownModel) : null);
  const autoUnknown = $derived(shownModel === 'auto' && !!local?.detecting);
  const activeTier = $derived(local?.activeTier ? tier(local.activeTier) : null);
  // Auto runs the other downloaded tier until this PC's own is downloaded.
  const offerAutoTier = $derived(
    local?.installed && local.model === 'auto' && !pendingModel && !tier(local.autoTier).installed,
  );
  const fellBackToCpu = $derived(local?.onGpu && local.engine === 'ready' && local.engineDevice === 'cpu');

  async function chooseMode(mode) {
    if (modeBusy) return;
    modeBusy = true;
    errors.mode = '';
    try {
      ui.app.mode = await invoke('set_mode', { mode });
    } catch (e) {
      errors.mode = typeof e === 'string' ? e : 'Could not change how Voisu transcribes.';
    } finally {
      modeBusy = false;
    }
  }

  // Auto works once any tier is downloaded; a named tier needs its own.
  async function chooseModel(id) {
    if (modeBusy) return;
    errors.mode = '';
    const ready = id === 'auto' ? local.installed : tier(id).installed;
    if (!ready) {
      pendingModel = id;
      return;
    }
    pendingModel = null;
    modeBusy = true;
    try {
      ui.app.local.model = await invoke('set_local_model', { model: id });
    } catch (e) {
      errors.mode = typeof e === 'string' ? e : 'Could not change the Local model.';
    } finally {
      modeBusy = false;
    }
  }

  // Consent is this click, beside the size it fetches: nothing downloads
  // until the user asks. It fetches exactly the tier and size shown, and
  // `model` is the setting applied once it installs (Auto stays Auto). The
  // backend turns Local on for a first download unless Cloud is chosen
  // meanwhile. A failure lands in local.error through the state refresh.
  async function download(model, shown) {
    errors.mode = '';
    pendingModel = null;
    try {
      await invoke('download_local_model', {
        tier: shown.id,
        bytes: shown.bytes,
        model,
        switchToLocal: !local.installed,
      });
    } catch (e) {
      // Shown from local.error.
    }
  }

  async function cancelDownload() {
    try {
      await invoke('cancel_local_download');
    } catch (e) {
      // The download finishes or fails on its own; the refresh shows which.
    }
  }

  async function removeModel() {
    if (modeBusy) return;
    modeBusy = true;
    errors.mode = '';
    pendingModel = null;
    try {
      await invoke('remove_local_model');
    } catch (e) {
      errors.mode = typeof e === 'string' ? e : 'Could not remove the Local model.';
    } finally {
      modeBusy = false;
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

{#if local}
  <h2 class="group-label" id="transcription-title">Transcription</h2>
  <div class="group" role="group" aria-labelledby="transcription-title">
    <div class="block">
      <div class="row">
        <span class="row-text">
          <span>Transcribe with</span>
          <span class="hint">
            {#if local.cloudUntilDownloaded}
              Using Cloud until the model finishes. Local turns on when it has downloaded.
            {:else if app.mode === 'local'}
              Local: on this PC. Nothing you say leaves it.
            {:else}
              Cloud: Deepgram and Groq. Fastest, needs your keys.
            {/if}
          </span>
        </span>
        <div class="presets">
          <button
            type="button"
            class="chip"
            class:on={app.mode === 'cloud' && !local.cloudUntilDownloaded}
            aria-pressed={app.mode === 'cloud' && !local.cloudUntilDownloaded}
            disabled={modeBusy}
            onclick={() => chooseMode('cloud')}
          >
            Cloud
          </button>
          <button
            type="button"
            class="chip"
            class:on={app.mode === 'local' || local.cloudUntilDownloaded}
            aria-pressed={app.mode === 'local' || local.cloudUntilDownloaded}
            disabled={modeBusy || !(local.installed || local.downloading)}
            title={local.installed || local.downloading ? '' : 'Download a Local model first.'}
            onclick={() => chooseMode('local')}
          >
            Local
          </button>
        </div>
      </div>
    </div>
    {#if !local.available}
      <p class="hint">Local mode isn't available in this build of Voisu.</p>
    {:else}
      <div class="block">
        <div class="row">
          <span class="row-text">
            <span>Local model</span>
            <span class="hint">
              {#if local.detecting}
                Checking this PC for a GPU…
              {:else if local.onGpu}
                GPU found: {local.device}. Auto picks Accurate.
              {:else}
                No GPU found, so it runs on the processor. Auto picks Fast.
              {/if}
            </span>
          </span>
          <div class="presets">
            {#each MODEL_CHOICES as choice (choice.id)}
              <button
                type="button"
                class="chip"
                class:on={shownModel === choice.id}
                aria-pressed={shownModel === choice.id}
                disabled={modeBusy || !!local.downloading}
                onclick={() => chooseModel(choice.id)}
              >
                {choice.label}
              </button>
            {/each}
          </div>
        </div>
        {#if shownTier?.note && !offerAutoTier}
          <p class="hint" class:warn={!local.onGpu}>
            {#if !local.onGpu}<span aria-hidden="true">!</span>{/if}
            {shownTier.note}
          </p>
        {/if}
        {#if app.mode === 'local' && activeTier && !local.downloading}
          <p class="hint" aria-live="polite">
            {#if local.engine === 'ready'}
              {activeTier.name} ({activeTier.modelName}) is loaded on {local.engineDevice === 'cpu' ? 'the CPU' : local.device}.
            {:else if local.engine === 'starting'}
              Loading {activeTier.name} ({activeTier.modelName})…
            {/if}
            {#if fellBackToCpu}The GPU couldn't run it, so it uses the CPU.{/if}
          </p>
        {/if}
      </div>
      {#if local.downloading}
        <div class="block" aria-live="polite">
          <div class="row">
            <span class="row-text">
              <span>Downloading {tier(local.downloading).name} ({tier(local.downloading).modelName})</span>
              <span class="hint">{mb(local.downloaded)} of {mb(local.total)} MB. Stopping keeps what's done.</span>
            </span>
            <button type="button" class="pill" onclick={cancelDownload}>Cancel</button>
          </div>
          <progress max={local.total} value={local.downloaded} aria-label="Local model download"></progress>
        </div>
      {:else if (!local.installed || pendingModel) && shownTier}
        <div class="block">
          <span class="row-text">
            <span>Download {shownTier.name} ({shownTier.modelName})</span>
            <span class="hint">
              {mb(shownTier.bytes)} MB, once; then it works offline. English only.
              {#if !local.onGpu}It runs on the processor, so the Transcript comes a few seconds after you stop; a
                recent 4-core or better CPU with AVX2 works best.{/if}
              Cloud stays available.
            </span>
          </span>
          <div class="actions">
            <!-- Until the GPU probe answers, Auto's tier and size aren't known. -->
            <button
              type="button"
              class="primary small"
              disabled={autoUnknown}
              title={autoUnknown ? 'Checking this PC for a GPU…' : ''}
              onclick={() => download(shownModel, shownTier)}
            >
              Download {mb(shownTier.bytes)} MB
            </button>
            {#if pendingModel && local.installed}
              <button type="button" class="pill" onclick={() => (pendingModel = null)}>Not now</button>
            {/if}
          </div>
        </div>
      {:else if offerAutoTier}
        <div class="row">
          <span class="row-text">
            <span>{tier(local.autoTier).name} ({tier(local.autoTier).modelName}) suits this PC</span>
            <span class="hint">Auto uses {activeTier?.name} until you download it.</span>
          </span>
          <button type="button" class="pill" onclick={() => download('auto', tier(local.autoTier))}>
            Download {mb(tier(local.autoTier).bytes)} MB
          </button>
        </div>
      {/if}
      {#if local.installed && !local.downloading}
        <div class="row">
          <span class="row-text">
            <span>On this PC</span>
            <span class="hint">{local.tiers.filter((t) => t.installed).map((t) => t.name).join(' and ')}.</span>
          </span>
          <button type="button" class="pill" disabled={modeBusy} onclick={removeModel}>Remove</button>
        </div>
      {/if}
    {/if}
    <ErrorLine message={errors.mode || local.error} />
    {#if local.error && app.mode === 'local'}
      <button type="button" class="link" disabled={modeBusy} onclick={() => chooseMode('cloud')}>
        Switch to Cloud
      </button>
    {/if}
  </div>
{/if}

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
    align-self: flex-start;
  }
  progress {
    appearance: none;
    width: 100%;
    height: 6px;
    border: 0;
    border-radius: 3px;
    background: var(--overlay-2);
    overflow: hidden;
  }
  progress::-webkit-progress-bar {
    background: var(--overlay-2);
  }
  progress::-webkit-progress-value {
    background: var(--accent);
    transition: width 0.25s ease-out;
  }
  progress::-moz-progress-bar {
    background: var(--accent);
  }
  @media (prefers-reduced-motion: reduce) {
    progress::-webkit-progress-value {
      transition: none;
    }
  }
</style>
