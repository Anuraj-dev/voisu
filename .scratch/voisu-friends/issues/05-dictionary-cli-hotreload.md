# Dictionary CLI (add/remove/list) + per-session hot-reload

**Label:** `wayfinder:task` (AFK, implementation)
**Status:** open
**Blocked by:** EXTERNAL — fix batch merging first (keyterm cap fix touches the
same dictionary→keyterm seam)
**Blocks:** 09-packaging-accounts-setup (phase gate)

## Question

1. `voisu dictionary add <term>` / `remove <term>` / `list [--json]` on the
   existing subcommand dispatcher in `voisu.rs`, editing
   `~/.config/voisu/dictionary.txt` (idempotent add, case-insensitive match on
   remove, preserve comments/user ordering).
2. Hot-reload: re-read `merged_terms()` at each new dictation session start
   instead of once at daemon boot (snapshot per session; never mid-utterance) —
   fits the existing rebuild_replay_adapters pattern around
   voisu-daemon.rs:443/1231.
3. Keyterm cap from the fix batch applies on the re-read path too (user terms
   first, 500-token/100-term Deepgram budget, mirroring whisper_prompt
   truncation).

RED→GREEN; tests for CLI file edits and that a term added between sessions
reaches the next session's keyterms. Routing: Terra high, Sol review.
