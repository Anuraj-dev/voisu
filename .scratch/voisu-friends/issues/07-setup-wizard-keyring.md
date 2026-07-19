# voisu setup wizard + keyring storage + doctor error classification

**Label:** `wayfinder:task` (AFK, implementation)
**Status:** closed (2026-07-20, PR #62)

## Resolution

Shipped in PR #62 (merged 50d919a) after 5 review rounds. `voisu setup`: injected-IO wizard
(WizardIo/SecretStore/KeyValidator traits), termios hidden entry with RAII EchoGuard +
SIGINT/SIGQUIT-safe restore, per-key live preflight before save, re-runnable with keep/replace
+ keyring-vs-plaintext location surfaced, env-override notice fires on variable PRESENCE,
exit 4 unless ≥1 provider usable. Storage: NO `keyring` crate (both backends drag duplicate
D-Bus stacks vs the tree's zbus 5 — deviation reviewer-endorsed); built on the existing
secret-tool boundary: store-side bounded retry (immediate+250ms/1s/3s per ticket-06 evidence),
LOUD 0600-file fallback distinguishing unavailable/locked/tool-missing, flock-serialized
(CredentialsLock mirrors DictionaryLock), real plaintext→keyring migration with content-aware
prune classification (gone/survived/unverifiable keyed on the target provider's line — sole
source of truth in FileSecretStore::remove), load path = single bounded call, never blocks
startup. `voisu doctor`: ProviderKeyStatus::classify (401/403 invalid → run setup; 429+Retry-After
rate-limited; bare 429 quota; else transient), EnvOverrideInvalid FAILs naming the variable.
Both suites 431/0 (baseline was 395). Known deferral (PR body): wizard-scale keyring deadline
(unlock dialog vs 2s PROCESS_DEADLINE). HITL: live ksecretd smoke + real-key validation.
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
