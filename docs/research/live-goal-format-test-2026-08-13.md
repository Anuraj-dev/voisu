# Live Goal format test — 2026-08-13

**Ticket:** [Live Goal test; keep only if it works](https://github.com/Anuraj-dev/voisu/issues/189)  
**Harness:** `crates/voisu-app/tests/live_goal_format.rs`  
**Command:** `VOISU_LIVE_GOAL_FORMAT=1 cargo test -p voisu-app --test live_goal_format live_goal_format_sends_leftover_notes_through_production_path -- --ignored --nocapture --test-threads=1`  
**Host:** Groq key valid (`voisu doctor`). `VOISU_ENABLE_DPR=1`. `VOISU_ENABLE_QWEN_FORMAT` unset. No `qwen-format.conf`.

This file records the real live run. It does not enable the formatter. Packaging stays flag-off.

## Fixtures

Leftover Goal / mixed notes only. Each still admitted after local organize.

1. `Goal is to deploy the application right now`
2. `goal ship the rust parser`
3. `goal is to deploy the application right now context is the production cluster notes check the rollback`

## Counts

| | |
|---|---|
| Attempts | 3 |
| Accepted Deliveries | 0 |
| Local-baseline fallbacks | 3 |
| `candidate_schema` | 1 |
| `rate_limited` | 2 |
| Protected mutations | 0 |
| Outro / prompt junk | 0 |
| Leading or trailing whitespace defects | 0 |
| Single Delivery per fixture | 3/3 |

Closed codes only. No provider body, prompt, or credential.

## Verdict

Cloud Goal is **not** useful on this host today. The path is safe (one Delivery, local baseline, no whitespace/outro/junk), but the model did not land a usable Goal format. The formatter flag stays **off**. Do not add `qwen-format.conf`. Do not default-on `VOISU_ENABLE_QWEN_FORMAT` in the package.

The opt-in harness stays so a later host run can be compared against these counts.
