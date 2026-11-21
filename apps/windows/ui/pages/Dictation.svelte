<script>
  import { invoke } from '@tauri-apps/api/core';
  import { listen } from '@tauri-apps/api/event';
  import { ui, isMac, chooseMicReady } from '../state.svelte.js';
  import ToggleRow from '../ToggleRow.svelte';
  import ErrorLine from '../ErrorLine.svelte';

  // One message per section, shown under the section that failed.
  let errors = $state({});
  let triggerBusy = $state(false);
  let autoPasteBusy = $state(false);
  let toneBusy = $state(false);

  // Microphone (Windows): the backend keeps the stream open so a press
  // starts at once. The default first, then from most to least ready.
  const MIC_CHOICES = [
    {
      mode: 'while_running',
      name: 'Always on',
      badge: 'Fastest',
      body: 'Starts the instant you press. Windows shows the mic icon while Voisu runs.',
    },
    {
      mode: 'for_five_minutes',
      name: 'On for 5 minutes after dictating',
      badge: 'Balanced',
      body: 'Instant for 5 minutes after each dictation.',
    },
    {
      mode: 'off',
      name: 'Only while dictating',
      badge: 'Most private',
      body: 'Mic on only while you dictate. Starts about 0.8 s later.',
    },
  ];

  let micGroup = $state();

  // The radios stay enabled during a save, so a choice made then becomes the
  // next save. Once saving settles, they show the backend's actual choice,
  // not the radio last picked (a save that failed, or came back different).
  $effect(() => {
    const mode = ui.app.keepMicReady;
    if (ui.micSaving || !micGroup) return;
    for (const radio of micGroup.querySelectorAll('input')) radio.checked = radio.value === mode;
  });

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
  // The tier the ready worker runs: Fast after a GPU failure under Auto.
  const loadedTier = $derived(local?.engine === 'ready' && local.engineTier ? tier(local.engineTier) : null);
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

{#if !isMac()}
  <div class="group">
    <div class="block">
      <span class="row-text" id="mic-ready-title">Microphone</span>
      <div
        class="mic-options"
        role="radiogroup"
        aria-labelledby="mic-ready-title"
        aria-busy={ui.micSaving}
        bind:this={micGroup}
      >
        {#each MIC_CHOICES as choice (choice.mode)}
          <label class="mic-option">
            <input
              type="radio"
              name="mic-ready"
              value={choice.mode}
              checked={app.keepMicReady === choice.mode}
              aria-labelledby="mic-{choice.mode}-name mic-{choice.mode}-badge"
              aria-describedby="mic-{choice.mode}-body"
              onchange={() => chooseMicReady(choice.mode)}
            />
            <span class="mic-main">
              <span class="mic-head">
                <span class="mic-name" id="mic-{choice.mode}-name">{choice.name}</span>
                <span class="mic-badge" id="mic-{choice.mode}-badge">{choice.badge}</span>
              </span>
              <span class="hint" id="mic-{choice.mode}-body">{choice.body}</span>
            </span>
          </label>
        {/each}
      </div>
      <ErrorLine message={ui.micError} />
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
            {#if loadedTier}
              {loadedTier.name} ({loadedTier.modelName}) is loaded on {local.engineDevice === 'cpu' ? 'the CPU' : local.device}.
            {:else if local.engine === 'starting'}
              Loading {activeTier.name} ({activeTier.modelName})…
            {/if}
            {#if fellBackToCpu}The GPU couldn't run {activeTier.name}, so Local uses the CPU.{/if}
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
  /* Microphone: one card per choice, a native radio on the left. */
  .mic-options {
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .mic-option {
    display: flex;
    align-items: flex-start;
    gap: 10px;
    padding: 9px 12px;
    border: 1px solid var(--overlay-2);
    border-radius: 10px;
    background: transparent;
    cursor: pointer;
    transition:
      background 0.15s,
      border-color 0.15s;
  }
  .mic-option:hover {
    background: var(--overlay-1);
  }
  .mic-option:has(:checked) {
    border-color: color-mix(in srgb, var(--accent) 55%, transparent);
    background: color-mix(in srgb, var(--accent) 7%, transparent);
  }
  .mic-options[aria-busy='true'] .mic-option {
    cursor: progress;
  }
  .mic-option input {
    flex: none;
    width: 16px;
    height: 16px;
    margin: 2px 0 0;
    accent-color: var(--accent);
    cursor: inherit;
  }
  /* The ring moves to the whole card where :has() works; elsewhere the
     radio keeps its own. */
  @supports selector(:has(:focus-visible)) {
    .mic-option:has(:focus-visible) {
      outline: 2px solid var(--accent);
      outline-offset: 2px;
    }
    .mic-option input:focus-visible {
      outline: none;
    }
  }
  .mic-main {
    display: flex;
    flex-direction: column;
    gap: 2px;
    min-width: 0;
  }
  .mic-head {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 4px 8px;
  }
  .mic-name {
    font-weight: 600;
  }
  .mic-badge {
    padding: 0 7px;
    border-radius: 32px;
    background: var(--overlay-2);
    font-size: 11px;
    line-height: 17px;
    color: var(--text-2);
  }
  .mic-option:has(:checked) .mic-badge {
    background: color-mix(in srgb, var(--accent) 18%, transparent);
    color: var(--accent);
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
