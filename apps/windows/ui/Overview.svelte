<script>
  import { invoke } from '@tauri-apps/api/core';
  import { ui } from './state.svelte.js';

  let now = $state(Date.now());
  let copiedFor = $state(null);
  let error = $state('');
  let updating = $state(false);

  const recording = $derived(ui.phase === 'recording' || ui.phase === 'handsFree');
  const copied = $derived(copiedFor !== null && copiedFor === ui.transcript);

  // Ticks only while a Recording runs.
  $effect(() => {
    if (!recording) return;
    now = Date.now();
    const id = setInterval(() => (now = Date.now()), 250);
    return () => clearInterval(id);
  });

  const elapsed = $derived.by(() => {
    const seconds = Math.max(0, Math.floor((now - ui.recordingSince) / 1000));
    return `${Math.floor(seconds / 60)}:${String(seconds % 60).padStart(2, '0')}`;
  });

  // tone: ready | recording | processing | failure
  const hero = $derived.by(() => {
    if (!ui.app.hookOk) {
      return {
        tone: 'failure',
        title: 'Voisu needs a restart',
        detail: "The Trigger Key isn't working. Quit Voisu from the tray and open it again.",
      };
    }
    switch (ui.phase) {
      case 'recording':
        return { tone: 'recording', title: 'Recording', timer: true, detail: 'Release to transcribe. Esc cancels.' };
      case 'handsFree':
        return { tone: 'recording', title: 'Recording', timer: true, hint: 'handsFree' };
      case 'processing':
        return { tone: 'processing', title: 'Working on your Transcript…', detail: '' };
      case 'error':
        return { tone: 'failure', title: 'Last dictation failed', detail: ui.phaseMessage };
      default:
        return { tone: 'ready', title: 'Ready to dictate', hint: 'ready' };
    }
  });

  const rows = $derived([
    { label: 'Delivery', value: ui.app.autoPaste ? 'Pastes automatically' : 'Copies to clipboard' },
    { label: 'Writing', value: ui.app.tone === 'informal' ? 'Informal' : 'Formal' },
    { label: 'Start at login', value: ui.app.autostart ? 'On' : 'Off' },
  ]);

  async function copyTranscript() {
    if (!ui.transcript) return;
    try {
      await navigator.clipboard.writeText(ui.transcript);
      copiedFor = ui.transcript;
    } catch (e) {
      error = 'Could not copy to the clipboard.';
    }
  }

  async function installUpdate() {
    updating = true;
    error = '';
    try {
      // Ends this process when the installer starts.
      await invoke('install_update');
    } catch (e) {
      error = typeof e === 'string' ? e : 'Could not start the update.';
    } finally {
      updating = false;
    }
  }
</script>

<!-- The timer sits outside the accessibility tree, so the live region
     speaks status changes only, not every tick. -->
<section class="hero {hero.tone}" aria-live="polite">
  <h1>
    <span class="dot" aria-hidden="true"></span>{hero.title}{#if hero.timer}<span aria-hidden="true">&nbsp;{elapsed}</span>{/if}
  </h1>
  {#if hero.hint === 'ready'}
    <p class="muted">Hold <kbd>{ui.app.trigger.label}</kbd>, speak, release. Double-tap for hands-free, Esc to cancel.</p>
  {:else if hero.hint === 'handsFree'}
    <p class="muted">Hands-free. Press <kbd>{ui.app.trigger.label}</kbd> to transcribe. Esc cancels.</p>
  {:else if hero.detail}
    <p class="muted">{hero.detail}</p>
  {/if}
</section>

{#if ui.app.updateVersion}
  <div class="card update">
    <span>Voisu {ui.app.updateVersion.replace(/^v/, '')} is ready</span>
    <button type="button" class="pill" onclick={installUpdate} disabled={updating}>Restart to update</button>
  </div>
{/if}

<dl class="rows">
  <div>
    <dt>Trigger Key</dt>
    <dd><kbd>{ui.app.trigger.label}</kbd></dd>
  </div>
  {#each rows as row (row.label)}
    <div>
      <dt>{row.label}</dt>
      <dd>{row.value}</dd>
    </div>
  {/each}
</dl>

<section class="card transcript" aria-labelledby="transcript-title">
  <div class="head">
    <h2 id="transcript-title">Last Transcript</h2>
    <button type="button" class="pill" onclick={copyTranscript} disabled={!ui.transcript}>
      {copied ? 'Copied' : 'Copy'}
    </button>
  </div>
  {#if ui.transcript}
    <p class="text">{ui.transcript}</p>
  {:else}
    <p class="muted">Nothing yet.</p>
  {/if}
</section>

{#if error}<p class="error" role="alert"><span aria-hidden="true">✕</span> {error}</p>{/if}

<footer class="hint">Voisu v{ui.app.version}</footer>

<style>
  .hero {
    display: flex;
    flex-direction: column;
    gap: 6px;
    padding: 4px 0 20px;
  }
  h1 {
    display: flex;
    align-items: center;
    gap: 10px;
    font-variant-numeric: tabular-nums;
  }
  .dot {
    flex: none;
    width: 10px;
    height: 10px;
    border-radius: 50%;
    background: var(--accent);
  }
  .recording .dot {
    animation: pulse 1.2s ease-in-out infinite;
  }
  .processing .dot {
    background: var(--processing);
  }
  .failure .dot {
    background: var(--failure);
  }
  @keyframes pulse {
    50% {
      opacity: 0.35;
    }
  }
  .update {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    margin-bottom: 16px;
    border-color: var(--border);
  }
  .rows {
    margin: 0 0 20px;
    border-top: 1px solid var(--overlay-2);
  }
  .rows div {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    padding: 9px 0;
    border-bottom: 1px solid var(--overlay-2);
  }
  dt {
    color: var(--text-2);
  }
  dd {
    margin: 0;
    text-align: right;
  }
  .transcript {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .head {
    display: flex;
    align-items: center;
    justify-content: space-between;
  }
  h2 {
    font-size: 14px;
    font-weight: 600;
  }
  .text {
    white-space: pre-wrap;
    user-select: text;
    max-height: 160px;
    overflow-y: auto;
  }
  .error {
    margin-top: 12px;
  }
  footer {
    margin-top: 20px;
  }
</style>
