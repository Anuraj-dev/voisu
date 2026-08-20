# Grok 4.6 effort and Sol review bakeoff — 2026-08-20

Transcript Fidelity tickets. Raja asked for grok-4.6 medium/high/xhigh on implementation, Grok 4.6 reviews, and Codex 5.6 Sol read-only reviews after each PR. Findings off PR intent were discarded. Merge used independent Grok 4.6 APPROVE plus green CI.

Grok CLI `--reasoning-effort` was used only for the first two implementers. After that, Raja required Grok work via `spawn_subagent` / workflows so it shows in the TUI (Ctrl+Z / workflow dashboard). Spawned grok-4.6 children inherit config `default_reasoning_effort = xhigh`. Sol stayed on `codex exec --sandbox read-only`.

## Dispatch log

| # | Ticket / PR | Role | Model | Effort | Wall s | Result | Notes |
|---|---|---|---|---|---:|---|---|
| 1 | #199 / PR 210 | implement | grok-4.6 | high (CLI) | 795 | delivered | RED then GREEN; 48 local_baseline + 86 transcript_decision |
| 2 | #202 / PR 211 | implement | grok-4.6 | medium (CLI) | 794 | delivered | standalone `tools/transcript-quality`; 11 tests first pass |
| 3 | PR 210 | review | grok-4.6 | spawn/xhigh | 584 | CHANGES-REQUESTED | 2 bugs: later-cue after opening Goal; preservation bag dropped cue-shaped words. Reproduced with `organize_local_baseline` |
| 4 | PR 210 | review | gpt-5.6-sol | high | 329 | CHANGES-REQUESTED | 3 majors: overlapping later-cue + preservation; also bare opening `notes from the meeting` under Structured (discarded as existing Structured single-section) |
| 5 | PR 211 | review | grok-4.6 | spawn/xhigh | 600 | CHANGES-REQUESTED | 2 bugs: Groq fragment tie; tail labeled as prefix |
| 6 | PR 211 | review | gpt-5.6-sol | medium | 219 | CHANGES-REQUESTED | **blocker Grok missed:** guarded arm ran completeness+organize instead of saved `final_transcript`. Also adjudication, report path, camelCase, unweighted WER |
| 7 | PR 210 | fix | grok-4.6 | spawn/xhigh | 475 | delivered | later-cue after `.!?`; consumed-span preservation |
| 8 | PR 211 | fix | grok-4.6 | spawn/xhigh | 663 | delivered | guarded=`final_transcript`; fragment rule; weighted WER |
| 9 | PR 210 | re-review | grok-4.6 | spawn/xhigh | 371 | APPROVE | gardener after opening Goal stays prose |
| 10 | PR 210 | re-review | gpt-5.6-sol | high | 229 | CHANGES-REQUESTED | leftover: `goal review release notes` consecutive-cue; subsequence ≠ exactly-once. Not merged as extra product decision |
| 11 | PR 211 | re-review | grok-4.6 | spawn/xhigh | 521 | APPROVE | |
| 12 | PR 211 | re-review | gpt-5.6-sol | medium | 265 | CHANGES-REQUESTED | P1s: section-loss vs completeness source; symlink; `deploy now now`; `Please` as code. Second fix round |
| 13 | PR 211 | fix 2 | grok-4.6 | spawn/xhigh | 543 | delivered | fail-closed reports; honest guarded section loss |
| 14 | PR 211 | re-review 2 | grok-4.6 | spawn/xhigh | 250 | APPROVE | merged |
| 15 | #203 / PR 212 | implement | grok-4.6 | spawn/xhigh | 1000 | delivered | `mark-last` |
| 16 | #208 / PR 213 | implement | grok-4.6 | spawn/xhigh | 685 | delivered | embedded lists; later `second`/`third` scanned too far |
| 17 | PR 212 | review | grok-4.6 | spawn/xhigh | 571 | APPROVE | missed symlink+chmod remake |
| 18 | PR 212 | review | gpt-5.6-sol | medium | 235 | CHANGES-REQUESTED | symlink git bypass; 0644 remake; reconstruction feature (discarded, ticket 05) |
| 19 | PR 213 | review | grok-4.6 | spawn/xhigh | 585 | CHANGES-REQUESTED | later ordinals from anywhere; reproduced with probes |
| 20 | PR 213 | review | gpt-5.6-sol | high | 255 | CHANGES-REQUESTED | pause timing unused; ranking `First was Alice` |
| 21 | PR 212 | fix | grok-4.6 | spawn/xhigh | 351 | delivered | canonicalize + chmod remakes |
| 22 | PR 213 | fix | grok-4.6 | spawn/xhigh | 658 | delivered | per-ordinal boundary; ranking/date fail-closed |
| 23 | PR 212 | re-review | grok-4.6 | spawn/xhigh | 356 | APPROVE | merged |
| 24 | PR 213 | re-review | grok-4.6 | spawn/xhigh | 559 | APPROVE | nits only; merged |

## What landed

| Issue | PR | Merged |
|---|---|---|
| #199 lossless organizer | [#210](https://github.com/Anuraj-dev/voisu/pull/210) | 2026-08-20 |
| #202 private evaluator | [#211](https://github.com/Anuraj-dev/voisu/pull/211) | 2026-08-20 |
| #203 mark/promote | [#212](https://github.com/Anuraj-dev/voisu/pull/212) | 2026-08-20 |
| #208 embedded lists | [#213](https://github.com/Anuraj-dev/voisu/pull/213) | 2026-08-20 |

Main after these: `0.37.0` (`ae84630`). Automated `chore(release)` bumps followed each merge.

## Reviewer delta (same PRs)

**Sol found, first Grok pass missed**

- PR 211: guarded pipeline arm was completeness+organize, not the saved current Transcript. That is the ticket’s arm 2. Sol medium, 219s.
- PR 212: lexical `git_root_of` lost to a symlink into the repo; remakes left `0644` files. Grok had APPROVE.

**Grok found, with live probes**

- PR 210: `goal keep… The ecological context…` still split after an opening Structure Cue. Grok ran `organize_local_baseline` on the gardener paragraph.
- PR 213: after a boundary `first`, `second`/`third` were taken from later sentences (`First of march` / `finished second`). Grok’s probes were the merge-blocking case.

**Sol leftover that we discarded**

- Bare opening `notes from the meeting` → Structured `Notes:` (pre-existing `structured_single`).
- Unpunctuated `goal review release notes` vs genuine `goal … test context it fails` dictation (same left-token class).
- Enabling `dpr-eval-late-retain` to snapshot reconstruction before ticket 05 exists.

Sol r1 logs also spent a lot of turns reading `internal/` INDEX/STATE that were not the diff. Prompting “diff first, do not scan the repo” was not enough on high. Grok reviewers stayed on the named files.

## Effort notes (implementation)

High vs medium on the two CLI implementers was a wash on wall clock (~795s each). Task shape dominated:

- **high / #199 (core invariant):** first-cue-at-zero and prefix tests were right; later-cue rule was “not a determiner,” which is the bug the P0 ticket still had after the first pass.
- **medium / #202 (new tool):** volume shipped; the arm-2 contract was wrong until Sol said so.
- **spawn xhigh / #203, #208:** similar “ships a working slice, review finds the boundary hole.”

No grok-4.5 implement this session (nothing was a tiny bulk-read). No grok-4.6 xhigh *CLI* implement (config default xhigh applied to every spawn after the CLI pair).

## Routing takeaway

- Keep **Grok 4.6 as implementer** for this repo’s organizer/tooling work; budget a review-fix round for boundary conditions.
- Keep **Sol as the first PR reviewer** when the contract has multiple arms or privacy/path rules. It caught the guarded-arm substitution that Grok’s first review treated as fine.
- Keep **Grok as a second reviewer** when you need a probe against `organize_local_baseline`. It is slower (~6–10 min vs Sol ~3.5–5.5 min) and better at executable counterexamples.
- Do not treat Sol leftover nits after Grok APPROVE + green CI as a second fix cycle unless they are in-intent majors. Cycle cap still pays.

## Not done this session

- #200 host RPM: `packaging/build-rpm.sh` died with `Disk quota exceeded` (workspace tests). Install + controlled test 8 replay + one natural Recording still need Raja (sudo/pkexec and speech).
- #201 completeness-aware source selection (blocked on #200).
- #204–#207, #209: reconstruction path, pilots, vocabulary, promote/reject. 06/07/10 need Raja at the mic.
