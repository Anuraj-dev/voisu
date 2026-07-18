# Dictionary CLI (add/remove/list) + per-session hot-reload

**Label:** `wayfinder:task` (AFK, implementation)
**Status:** resolved (2026-07-19, PR #57 merged)
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

## Resolution (2026-07-19)

Implemented by gpt-5.6-terra (high, last Terra dispatch before the Codex-reviews-only
routing), fix round by Opus 4.8 (high), reviewed by Sol (1 round). Merged as PR #57.

- `voisu dictionary add|remove|list [--json]`: atomic writes, flock(2)-serialized
  read-modify-write (concurrent edits can't lose updates), idempotent case-insensitive
  add, case-insensitive remove (missing -> exit 4), comments/ordering preserved; add
  rejects terms the comment grammar would mangle (C#/F# still fine).
- Per-Recording hot-reload: one dictionary snapshot at each Recording start feeds both
  Deepgram keyterms and the Groq whisper prompt (GroqProvider::with_prompt); the
  supervised replay tail keeps the captured snapshot — no fs I/O invariant preserved.
- Keyterm cap (user-first, 500-token/100-term) holds on the re-read path.
- Declined: Unicode full case folding (documented as to_lowercase semantics);
  Deepgram reconnect-without-keyterms (out of scope, cap prevents the 400).
- Suites: 375/0 both feature sets; CI green (one #58 flake rerun).
