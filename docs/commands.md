# Command reference

`voisu` controls the daemon (`voisu-daemon`). All history and diagnostics stay on your machine.

| Command | Purpose |
| --- | --- |
| `voisu` | Open the interface on a terminal. Opens on setup when nothing is set up |
| `voisu start` / `stop` / `toggle` / `status` | Start and stop a Recording, or see what Voisu is doing |
| `voisu shortcut` | Show the desktop-approved Trigger Key |
| `voisu setup` | Set up step by step: Cloud or Local, keys or Consented Install, Trigger Key |
| `voisu engine [cloud\|local]` | Transcribe online with Cloud, or on this computer with an installed Local model |
| `voisu local remove [--yes]` | Remove a Consented Install. `--yes` is required when stdin is not a terminal |
| `voisu local-unload [off\|SECONDS]` | Free an idle Local model's memory after this long (default never, from 30 to 86400 seconds; `off` or `never` keeps it loaded; one already unloaded reloads at the next Recording). Linux, see [Local unload](linux.md#local-unload) |
| `voisu local-device [auto\|gpu\|cpu]` | Where the whisper.cpp Local model runs: `auto` (default) and `gpu` use the GPU through Vulkan, `cpu` runs it on the processor. Voisu never switches to the CPU on its own. Linux |
| `voisu local-model [auto\|accurate\|fast]` | Which Local model runs: `accurate` (distil-large-v3.5), `fast` (small.en, about a third of the size, for the CPU), or `auto` (default: Accurate on a GPU, Fast on the CPU). A model that is not installed downloads only after you agree, and replaces the installed one. Linux, see [Local model](linux.md#local-model) |
| `voisu key set` / `key check <groq\|deepgram>` | Save or check a Groq or Deepgram key (key on stdin) |
| `voisu deepgram on` / `off` | Enable or disable Deepgram streaming |
| `voisu delivery [paste\|copy]` | Paste the Transcript into your window, or only copy it |
| `voisu writing [clean\|exact]` | `clean` fixes punctuation and formatting. `exact` keeps your words as spoken |
| `voisu layout [plain\|auto\|sections]` | Choose how text is laid out: plain, automatic, or sections |
| `voisu tone [formal\|informal]` | `formal` keeps full words. `informal` uses chat abbreviations like btw and drops the final full stop |
| `voisu overlay-position [move\|reset]` | Where the on-screen Overlay pill sits (Linux). Bare, it prints the spot as fractions of the screen across and down, such as `0.500,0.964` (the default, bottom centre). `move` widens the pill, with ✗ on its left and ✓ on its right: drag it anywhere, then click ✓ or press Enter to save, or ✗ or Esc to cancel. The next pill is centred where you dropped it. The spot is a fraction of the screen, so on several monitors the pill keeps showing on whichever one the desktop puts it (the focused one), at the same relative spot. A Recording, or a minute without input, cancels the move; so does a ✓ that Voisu can't confirm idle within two seconds, rather than saving over a Recording that may have started. `reset` puts it back at the bottom centre. The pill moves at once. Needs the Overlay running on a Wayland desktop with layer-shell (KDE Plasma, Hyprland); on GNOME the Overlay is the Shell extension, which can't be moved |
| `voisu dictionary add` / `remove <term>` / `list` | Personal pronunciation dictionary. Local mode also corrects entries of two or more syllables by sound |
| `voisu history [--json\|--verbose]` | List recent Recordings |
| `voisu export <correlation-id>` | Redacted diagnostic export for one Recording |
| `voisu doctor [--verbose]` | Host readiness diagnostics |
| `voisu service install` / `start` / `stop` / `restart` / `status` / `uninstall` | Manage the user services |
| `voisu update [--check\|--dry-run] [--switch-to-bin]` | Update Voisu through its package manager (Linux, see [Update](linux.md#update)) |
| `voisu update-check [on\|off]` | Turn the daily update check on or off (Linux, see [Update check](linux.md#update-check)) |
| `voisu star` | Open Voisu's GitHub page to star it; Voisu stops asking (Linux) |
| `voisu star-prompt [on\|off]` | Turn the star request on or off (Linux, see [Star prompt](linux.md#star-prompt)) |

---

[Back to the README](../README.md)
