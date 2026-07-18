# Add cargo-audit (and clippy if installable) to the CI gate

**Label:** `wayfinder:task` (AFK, implementation — parallel-safe)
**Status:** open
**Blocked by:** — (frontier; safe to run even while latency work is in flight —
touches only CI config, zero overlap with latency files)
**Blocks:** —

## Question

The codebase has never had lint- or CVE-level scrutiny: clippy, cargo-audit,
and cargo-deny are all unavailable on Raja's machine (audit session,
2026-07-18), and CI doesn't run them. Add to the CI workflow:

1. `cargo audit` against the committed `Cargo.lock` (install via
   `cargo install cargo-audit` or the maintained GitHub Action). Triage the
   first run's findings in this ticket — fix, upgrade, or document each
   advisory exception; don't just make the job green.
2. `cargo clippy --all-targets --workspace -- -D warnings` — CI has a full
   toolchain even though the local machine lacks the component. Expect a
   first-run cleanup: fix trivial lints inline; anything non-trivial that
   touches `system.rs`/daemon files goes to ticket 05's hygiene sweep instead
   (those files are latency-contended).
3. Optional if cheap: `cargo deny` for license/duplicate checks (audit noted
   two coexisting `webpki-roots` versions).

Constraint: keep CI runtime reasonable; audit/clippy jobs must not gate on the
resource-heavy paths reserved for future e2e (see map fog — e2e is CI-only,
separate concern). Evidence: [audit report](../assets/audit-2026-07-18.md),
security minor #2 + tooling caveat.
