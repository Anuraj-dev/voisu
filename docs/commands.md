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
| `voisu key set` / `key check <groq\|deepgram>` | Save or check a Groq or Deepgram key (key on stdin) |
| `voisu deepgram on` / `off` | Enable or disable Deepgram streaming |
| `voisu delivery [paste\|copy]` | Paste the Transcript into your window, or only copy it |
| `voisu writing [clean\|exact]` | `clean` fixes punctuation and formatting. `exact` keeps your words as spoken |
| `voisu layout [plain\|auto\|sections]` | Choose how text is laid out: plain, automatic, or sections |
| `voisu tone [formal\|informal]` | `formal` keeps full words. `informal` uses chat abbreviations like btw and drops the final full stop |
| `voisu dictionary add` / `remove <term>` / `list` | Personal pronunciation dictionary |
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
