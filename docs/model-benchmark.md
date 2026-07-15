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

## Running observations
- Sol as reviewer: consistently finds real, subtle defects (concurrency, deadline, probe semantics). High value.
- Terra: good throughput on regular feature work; weaker on security edge cases and subtle async/subprocess hazards.
- Opus: strong on scoped escalation fixes — precise, test-first, no new defects so far (pending verdict).
- Luna: not yet used (tickets 11–13).
