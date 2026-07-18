# voisu setup wizard + keyring storage + doctor error classification

**Label:** `wayfinder:task` (AFK, implementation)
**Status:** open
**Blocked by:** 06-keyring-service-probe
**Blocks:** 09-packaging-accounts-setup (phase gate)

## Question

1. `voisu setup`: interactive wizard prompting for Deepgram + Groq keys one at
   a time, live preflight validation per key (cheap authenticated call) before
   saving; re-runnable.
2. Storage: Rust `keyring` crate → Secret Service/KWallet primary; fallback to
   0600 file under `~/.config/voisu/` with a LOUD one-time warning (never
   silent — gh's silent fallback is the anti-pattern). Env vars
   (`DEEPGRAM_API_KEY`/`GROQ_API_KEY`) override at runtime. Daemon never blocks
   startup on keyring (per ticket 06 findings — lazy retry).
3. `voisu doctor`: add live per-provider key round-trip checks; classify
   provider errors everywhere they surface: 401/403 → "key invalid, run voisu
   setup"; 429 + Retry-After → rate-limited; 429 bare → quota; else transient.

RED→GREEN; wizard logic testable via injected IO. Routing: Terra high
(feature), Sol review. Free-tier guidance text comes from research digest §9.
