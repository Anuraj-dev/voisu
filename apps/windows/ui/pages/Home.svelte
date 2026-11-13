<script>
  import { ui, isMac } from '../state.svelte.js';
  import Permissions from '../Permissions.svelte';
  import ErrorLine from '../ErrorLine.svelte';

  let now = $state(Date.now());
  let copiedFor = $state(null);
  let error = $state('');

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
    if (!ui.app.hookOk && isMac()) {
      // The event tap starts by itself once the permission is on.
      return {
        tone: 'failure',
        title: 'Allow Input Monitoring',
        detail: "The Trigger Key isn't working yet. Turn on Voisu in System Settings → Privacy & Security → Input Monitoring.",
      };
    }
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

  const glance = $derived([
    { label: 'Delivery', value: ui.app.autoPaste ? 'Pastes automatically' : 'Copies to clipboard' },
    { label: 'Writing', value: ui.app.tone === 'informal' ? 'Informal' : 'Formal' },
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
</script>

<!-- The timer sits outside the accessibility tree, so the live region
     speaks status changes only, not every tick. -->
<section class="hero {hero.tone}" aria-live="polite">
  <h1>
    <span class="dot" aria-hidden="true"></span>{hero.title}{#if hero.timer}<span class="timer" aria-hidden="true">{elapsed}</span>{/if}
  </h1>
  {#if hero.hint === 'ready'}
    <p class="muted">Hold <kbd>{ui.app.trigger.label}</kbd>, speak, release. Double-tap for hands-free, Esc to cancel.</p>
  {:else if hero.hint === 'handsFree'}
    <p class="muted">Hands-free. Press <kbd>{ui.app.trigger.label}</kbd> to transcribe. Esc cancels.</p>
  {:else if hero.detail}
    <p class="muted">{hero.detail}</p>
  {/if}
</section>

<Permissions />

<dl class="glance">
  <div>
    <dt>Trigger Key</dt>
    <dd><kbd>{ui.app.trigger.label}</kbd></dd>
  </div>
  {#each glance as item (item.label)}
    <div>
      <dt>{item.label}</dt>
      <dd>{item.value}</dd>
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

<ErrorLine message={error} />

<style>
  .hero {
    --tone: var(--accent);
    display: flex;
    flex-direction: column;
    gap: 6px;
    padding: 18px 20px 18px;
    margin-bottom: 14px;
    border-radius: var(--radius);
    background:
      radial-gradient(120% 140% at 0% 0%, color-mix(in srgb, var(--tone) 13%, transparent), transparent 62%),
      var(--overlay-1);
    border: 1px solid color-mix(in srgb, var(--tone) 24%, var(--overlay-2));
  }
  .processing {
    --tone: var(--processing);
  }
  .failure {
    --tone: var(--failure);
  }
  h1 {
    display: flex;
    align-items: center;
    gap: 10px;
    font-size: 22px;
    font-variant-numeric: tabular-nums;
  }
  .timer {
    margin-left: 2px;
    color: var(--text-2);
    font-weight: 500;
  }
  .dot {
    flex: none;
    width: 10px;
    height: 10px;
    border-radius: 50%;
    background: var(--tone);
    box-shadow: 0 0 0 4px color-mix(in srgb, var(--tone) 18%, transparent);
  }
  .recording .dot {
    animation: pulse 1.2s ease-in-out infinite;
  }
  @keyframes pulse {
    50% {
      opacity: 0.35;
    }
  }
  .hero + :global(.permissions) {
    margin-bottom: 14px;
  }
  .glance {
    display: grid;
    grid-template-columns: repeat(3, 1fr);
    margin: 0 0 14px;
    border: 1px solid var(--overlay-2);
    border-radius: var(--radius);
    background: var(--overlay-1);
  }
  .glance div {
    display: flex;
    flex-direction: column;
    gap: 4px;
    min-width: 0;
    padding: 10px 14px 11px;
  }
  .glance div + div {
    border-left: 1px solid var(--hairline);
  }
  dt {
    font-size: 11.5px;
    font-weight: 600;
    letter-spacing: 0.05em;
    text-transform: uppercase;
    color: var(--text-2);
  }
  dd {
    margin: 0;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  dd kbd {
    font-size: 13px;
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
    max-height: 120px;
    overflow-y: auto;
  }
  .transcript + :global(.error) {
    margin-top: 12px;
  }
</style>
