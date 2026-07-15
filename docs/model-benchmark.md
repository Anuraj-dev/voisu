# Model benchmark — Voisu build

> Standing experiment (Raja, 2026-07-15): compare codex models (Sol/Terra/Luna) vs Claude Opus
> subagents as coder agents across tickets 01–13. One row per dispatch. Final report after ticket 13:
> quality per task type, escalation rates, cost → routing recommendation.

| # | Ticket | Task | Model (effort) | Result | Review findings vs its work | Fix rounds | Notes |
|---|---|---|---|---|---|---|---|
| 1 | 01 | Feature impl | Sol (medium) | delivered | 10 findings (round 1) | 3 rounds total to APPROVE | Solid architecture; review-heavy |
| 2 | 01 | Review | Sol (high→) | 3 rounds, real defects each round | — | — | Caught mutex-across-await, biased-select, deadline bugs |
| 3 | 01 | Fix rounds | Sol/Terra + Opus (final 3 fixes) | delivered | — | — | Opus handled point fixes cleanly |
| 4 | 02 | Feature impl | Terra (high) | delivered, 30 tests | 9 security findings (2 BLOCKER) | 1 round used | Functional but security-naive first pass |
| 5 | 02 | Security fix round | Terra (high) | delivered bbc86d3, 36 tests | 8/9 resolved, 1 not + 2 new MAJORs | its round exhausted | Missed stdin-deadline subtlety; introduced 2 new probe bugs |
| 6 | 02 | Escalated fix round | Opus (high) | delivered 0bc3944, 40 tests | pending Sol re-review (bhrgtou07) | — | RED→GREEN, threaded pipe handling, clean summary; ~5.4 min, 85k tokens |
| 7 | 02 | Reviews | Sol (high) | 4 rounds total, each caught real issues; final APPROVE | — | — | False-WARN, trickle-hold, zombie-reap catches were genuinely subtle |
| 8 | 02 | Fix round 2 | Opus (high) | delivered acfa7d3, 43 tests | 2/3 resolved, 1 edge remained | — | ~2.6 min, 86k tokens |
| 9 | 02 | Fix round 3 | Opus (high) | delivered cfe336f, 44 tests → APPROVE | 0 findings | closed ticket | ~1.7 min, 93k tokens; 3 rounds to fully clear subtle process-cleanup edges |
| 10 | 03 | Feature impl (end-to-end slice) | Sol (medium) | delivered 44879f9, 47 tests | round 1: pump-termination BLOCKER + blocking startup + budget bugs | 1 round used | Ambitious slice; missed capture-pump failure lifecycle |
| 11 | 03 | Fix round 1 | Sol (medium) | delivered 66ad789 | plaintext endpoints, test-mode bypass, dropped coordinator remained | round exhausted | Partial; left security holes |
| 12 | 03 | Escalated fixes | Opus (high) | 436fcb7 + df573ad, 58 tests | deferral-queue ordering + PID-reuse race found by review | 2 rounds | Recovering-state redesign (reject-not-defer) done cleanly |
| 13 | 03 | Final fix (PID-reuse race) | Opus (high) | delivered f51dbbd, 65 tests 3x green → APPROVE | 0 findings | closed ticket | Flag-only CancelRegistry + owner-side kill; ~4.6 min, 240k tokens |
| 14 | 03 | Reviews | Sol (high first, medium re-reviews) | 5 rounds; real defects every round until final APPROVE | — | — | Sol medium re-review still caught PID-reuse race + ordering violations; policy validated |

## Running observations
- Sol as reviewer: consistently finds real, subtle defects (concurrency, deadline, probe semantics). High value.
- Terra: good throughput on regular feature work; weaker on security edge cases and subtle async/subprocess hazards.
- Opus: strong on scoped escalation fixes — precise, test-first, no new defects so far (pending verdict).
- Luna: not yet used (tickets 11–13).
- Sol medium as re-reviewer (new policy from ticket 03): still catches HIGH-severity concurrency races — the cost cut did not lose review quality so far.
| 15 | 04 | Feature impl | Sol (medium) | delivered 3e2eecc, 71 tests | round 1: 2 HIGH (unawaited deadline cancel, no curl cap) + 2 MEDIUM | 1 round | Solid slice; missed cancellation-ownership discipline |
| 16 | 04 | Fix round 1 | Sol (medium) | delivered (uncommitted), semaphore cap + awaited abort | own new reap test failed deterministically | round exhausted | Introduced detached-request-task bug it then had to chase |
| 17 | 04 | Fix round 2 (root cause) | Sol (medium) | retained-handle VecDeques; 75 tests | parallel-harness flakes remained | — | Correct fix; fixtures still timing-fragile |
| 18 | 04 | Flake-hardening round | Sol (medium) | 2s test deadline + PID-marker gating → 7f2bf21 | round 2 review: 1 HIGH (error-path early return) | — | Good fixture work; missed error branch |
| 19 | 04 | Combined leak+error-path round | Sol (medium) | PDEATHSIG + pgroup Drop + bounded stubs | broke reap test 3/4 runs (pop_front refactor) | round exhausted | Fixed the incident but regressed success-path ownership |
| 20 | 04 | Escalated fix (regression) | Opus 4.8 (high) | restored peek-then-pop → 7be2329, 76 tests | round 3 review: 1 HIGH (drain in error branch) | — | Found root cause in 1 pass w/ zombie-state proof; ~21 min, 117k tokens |
| 21 | 04 | Final fix (drain detach) | Opus 4.8 (high, resumed agent) | e1197db + discriminating pipe-holder test → APPROVE | 0 findings | closed ticket | Proved test discriminates by reinstating bug; flagged abort() follow-up (#14) |
| 22 | 04 | Reviews | Sol (high first, medium re-reviews) | 4 rounds; real HIGH each round until APPROVE | — | — | Sol medium again caught cancellation-window races; one run hung on stdin (env quirk, not model) |
| 23 | 05 | Feature impl | Sol (medium) | delivered b5c0e29, 92 tests | round 1: 1 HIGH (recon timeout drops spawn_blocking handle) + 1 MEDIUM (script gap) | 1 round | Good pipeline design; repeated the cancellation-ownership miss |
| 24 | 05 | Fix round | Opus 4.8 (high, resumed) | 59b1caa: pinned-future cancel+grace-await, CancelRegistry into core, token confusable check | round 2: 1 MEDIUM (incomplete Unicode ranges) | — | Both fixes proven discriminating; clean trait promotion |
| 25 | 05 | Range fixes | Opus 4.8 (high, resumed) | bcabfef + 9769886 → APPROVE | round 3 caught Latin-Ext-F gap; round 4: 0 findings | closed ticket | Caught own non-discriminating test and redesigned it |
| 26 | 05 | Reviews | Sol (high first, medium re-reviews) | 4 rounds; HIGH + 3 escalating-precision MEDIUMs | — | — | Sol medium even verified Unicode block charts via web search |
| 27 | 06 | Feature impl | Opus 4.8 (high) | delivered f27d410, 112 tests, all criteria | round 1: 5 HIGH + 3 MEDIUM (security/privacy) | 1 round | Functionally complete + clean state machine; security-naive on file/redaction boundaries (~23 min, 189k tokens) |
| 28 | 06 | Security fix round | Opus 4.8 (high, resumed) | 6082689, 124 tests, adversarial proofs per fix | round 2: 2 HIGH + 3 MEDIUM (edges of the fixes) | — | Big round handled cleanly; exfiltration/TOCTOU/scrub layers all landed |
| 29 | 06 | Edge fix rounds | Opus 4.8 (high, resumed) | 4d3a590 + 82bf0b9 + 50db963 → APPROVE | rounds 3-5 narrowed to fail-closed URL parsing edges, then 0 | closed ticket | Discriminating tests every round; kept dep-free by choice |
| 30 | 06 | Reviews | Sol (high first, medium re-reviews) | 5 rounds; every round found real security edges | — | — | Sol high produced the deepest security review of the project so far (8 findings, all confirmed) |
