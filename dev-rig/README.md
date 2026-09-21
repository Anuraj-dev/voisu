# Voisu dev rig — Narilabs (qwen3-asr) third-provider test

Test-only rig, lives entirely in this worktree (`test/narilabs-third-provider`,
based on origin/main 0.62.1 — the same release as the installed pacman package).
Nothing here is committed; the managed `voisu.service` install is never touched.

## What it adds

- **Third cloud provider:** Nari Labs realtime STT (`wss://api.narilabs.com/v1/realtime?intent=transcription`,
  model default `qwen3-asr:free`, override with `VOISU_NARILABS_MODEL` e.g. `qwen3-asr-fast:free`).
  Streams 16 kHz PCM live, like Deepgram; key via `VOISU_NARILABS_API_KEY` env or
  `voisu-dev auth set narilabs` (keyring, shared with the managed install).
- **Per-provider toggles** (daemon reads them once at start; restart the service after toggling):
  - `voisu-dev groq on|off` — new, default on
  - `voisu-dev deepgram on|off` — existing
  - `voisu-dev narilabs on|off` — new, **default off** (opt-in)
  - Enabled-but-keyless narilabs degrades to an inert provider (recording never aborts);
    enabled-with-key behaves exactly like Deepgram does today.
- **Comparison log:** the dev unit sets `VOISU_DEBUG_PROVIDER_TRANSCRIPTS=1`;
  after each Recording the journal gets one line per provider (label, ok/failure,
  full transcript text, timing) so you can A/B all enabled models from the same audio.

## Install

```
cd /home/raja/Anuraj-dev/voisu-narilabs-test
cargo build --release
./dev-rig/install.sh
```

`install.sh` seeds `~/.config/voisu-dev/` from the managed install (config.toml +
credentials file if you use the file fallback), installs
`~/.config/systemd/user/voisu-dev.service` and `~/.local/bin/voisu-dev`, and
daemon-reloads. It does not enable or start anything.

## Run / test loop

```
~/.local/bin/voisu-dev auth set narilabs        # your Nari Labs API key
~/.local/bin/voisu-dev mode cloud
~/.local/bin/voisu-dev narilabs on              # + optionally groq off / deepgram off
systemctl --user restart voisu-dev.service      # start also stops voisu.service
# dictate with your existing hotkey — it reaches the dev daemon (same socket)
journalctl --user -u voisu-dev.service -f       # watch provider-comparison lines
```

Switch back to the managed install:

```
systemctl --user stop voisu-dev.service && systemctl --user start voisu.service
```

## Boot default during testing

Since 2026-09-13 the dev rig is the login default so it survives reboots:

```
systemctl --user disable voisu.service      # done
systemctl --user enable voisu-dev.service   # done
```

Switch back when testing ends:

```
systemctl --user disable voisu-dev.service && systemctl --user enable voisu.service
```

(Whatever is enabled autostarts at login and owns the shared control socket, so the
hotkey follows it. The disabled unit is untouched and starts fine by hand anytime.)

**Paste-staleness gotcha:** if auto-paste silently degrades to clipboard-only after a
Hyprland config reload or compositor hiccup, history records
`delivery_fallback_reason: "verified Paste Action is no longer active"` — the daemon
invalidates its verified paste chord for the rest of the session instead of
re-discovering. A daemon restart (`systemctl --user restart voisu-dev.service`)
re-discovers it. Worth a product ticket: re-discover live instead of invalidating.

## Notes & gotchas

- **Delivery on Hyprland:** `type` mode requires the RemoteDesktop portal, which
  Hyprland does not implement (documented P1 in `docs/hyprland_problems.md`) — the
  daemon falls back to clipboard and history records
  `delivery_fallback_reason: "RemoteDesktop portal unavailable"`. Use `clipboard`
  (what the managed install uses; includes the verified Hyprland paste chord →
  auto-paste) or `guarded`. Auto-paste additionally requires the rig's
  `~/.config/voisu-dev/hypr -> ~/.config/hypr` symlink (install.sh creates it):
  paste-action discovery reads `$XDG_CONFIG_HOME/hypr/hyprland.lua`, and without
  the link it silently finds no paste action. Verify with
  `voisu-dev doctor` → `Paste action verified / Paste backend hyprland`.
- `voisu.service` and `voisu-dev.service` are mutually exclusive
  (`Conflicts=`). Do not `enable` the dev unit unless the rig should win every login.
- The overlay unit (`voisu-overlay.service`) is shared and untouched; it serves
  whichever daemon owns the socket.
- Never run `voisu-dev service ...` — that subcommand would manage
  `voisu.service`. Use systemctl for the rig.
- Toggles live in `~/.config/voisu-dev/voisu/config.toml` (`groq_enabled`,
  `deepgram_enabled`, `narilabs_enabled`); the managed install ignores unknown
  keys, so the files can't poison each other anyway.
- `voisu auth verify narilabs` reports "verification not supported for this
  provider" honestly — Nari Labs has no HTTP probe endpoint (WebSocket only).
- Teardown when done testing: `systemctl --user stop voisu-dev.service`,
  delete `~/.config/voisu-dev ~/.local/state/voisu-dev ~/.local/share/voisu-dev
  ~/.local/bin/voisu-dev ~/.config/systemd/user/voisu-dev.service`, then
  `git worktree remove --force /home/raja/Anuraj-dev/voisu-narilabs-test`.
