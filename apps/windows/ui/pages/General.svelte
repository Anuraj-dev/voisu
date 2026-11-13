<script>
  import { invoke } from '@tauri-apps/api/core';
  import { ui } from '../state.svelte.js';
  import { openGuide } from '../keys.js';
  import ToggleRow from '../ToggleRow.svelte';
  import ErrorLine from '../ErrorLine.svelte';

  // One message per section, shown under the section that failed.
  let errors = $state({});
  let autostartBusy = $state(false);
  let starBusy = $state(false);

  async function toggleAutostart(event) {
    const box = event.currentTarget;
    autostartBusy = true;
    errors.system = '';
    try {
      ui.app.autostart = await invoke('set_autostart', { enabled: box.checked });
    } catch (e) {
      errors.system = 'Could not change Start at login.';
    } finally {
      // Render what the system reports, not the requested value.
      box.checked = ui.app.autostart;
      autostartBusy = false;
    }
  }

  async function toggleStarPrompt(event) {
    const box = event.currentTarget;
    starBusy = true;
    errors.system = '';
    try {
      ui.app.starPrompt = await invoke('set_star_prompt', { enabled: box.checked });
    } catch (e) {
      errors.system = 'Could not change Star prompt.';
    } finally {
      box.checked = ui.app.starPrompt;
      starBusy = false;
    }
  }

  // Always offered, whatever the banner and the Star prompt switch say.
  // Starring here also ends the banner for good.
  function star() {
    errors.support = '';
    invoke('star_on_github').catch((e) => (errors.support = typeof e === 'string' ? e : 'Could not open the browser.'));
  }

  function open(command) {
    errors.about = '';
    command().catch((e) => (errors.about = typeof e === 'string' ? e : 'Could not open it.'));
  }
</script>

<header class="page-head">
  <h1>General</h1>
  <p>Startup, support and help.</p>
</header>

<div class="group">
  <ToggleRow
    id="autostart"
    label="Start at login"
    hint="So Voisu is ready after every restart."
    checked={ui.app.autostart}
    busy={autostartBusy}
    onchange={toggleAutostart}
  />
  <ToggleRow
    id="star-prompt"
    label="Star prompt"
    hint="After steady use, ask for a GitHub star."
    checked={ui.app.starPrompt}
    busy={starBusy}
    onchange={toggleStarPrompt}
  />
  <ErrorLine message={errors.system} />
</div>

<h2 class="group-label" id="support-title">Support Voisu</h2>
<section class="group" aria-labelledby="support-title">
  <div class="row">
    <span class="row-text">Voisu is free and built by one student. A star helps others find it.</span>
    <button type="button" class="pill" onclick={star}>Star on GitHub</button>
  </div>
  <ErrorLine message={errors.support} />
</section>

<h2 class="group-label" id="help-title">Help</h2>
<section class="group" aria-labelledby="help-title">
  <div class="row links">
    <button type="button" class="link" onclick={() => open(() => invoke('open_logs'))}>Open logs folder</button>
    <button type="button" class="link" onclick={() => open(openGuide)}>How to set up</button>
  </div>
  <ErrorLine message={errors.about} />
</section>

<style>
  .links {
    justify-content: flex-start;
    gap: 24px;
  }
</style>
