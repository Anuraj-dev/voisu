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
| 31 | 07 | Feature impl | Opus 4.8 (high) | delivered d593f8a, 138 tests; flagged own zbus deviation honestly | round 1: BLOCKER (deviation ruled in-scope) + HIGH + MEDIUM | 1 round | Correct analysis of the portal contract; deferred the hard part (~18 min, 192k tokens) |
| 32 | 07 | zbus client round | Opus 4.8 (high, resumed) | b4f39ba: full portal client + real D-Bus mock service on private buses | round 2: 2 MEDIUM + 1 LOW (restart, response race, Close unverified) | — | Substantial D-Bus work landed in one round; strong test infrastructure |
| 33 | 07 | Robustness round | Opus 4.8 (high, resumed) | 900f456: NameOwnerChanged rebind loop, broad Response subscription, session close on bind failure → APPROVE | 0 findings | closed ticket | Restart/divergent-handle tests prove the paths end to end |
| 34 | 07 | Reviews | Sol (high first, medium re-reviews) | 3 rounds; BLOCKING scope ruling + protocol-level races | — | — | Sol verified portal contract docs via web; ruled deviation blocking with spec citations |
| 35 | 08 | Feature impl | Sol (medium) | delivered 770c605, 147 tests: RemoteDesktop portal + NativeEiSender | round 1: 4 HIGH + 3 MEDIUM (restore token, NULL sentinel, DEVICE_RESUMED, PONG/DISCONNECT ordering, scrub gap) | 1 round | Deep libei protocol work; worktree also survived a git-checkout wipe reconstructed from codex logs |
| 36 | 08 | Fix round | Sol (medium) | a5771a9: libei 1.6 TEXT + XKB Ctrl+V fallback for host 1.5, rotating 0600 restore tokens, truthful compositor_submitted, CI xkbcommon | APPROVE (after Sol high first review) | closed ticket | Replaced an abandoned mid-session Opus rewrite wholesale; zero dead code left |
| 37 | 08 | Reviews | Sol (high first, medium re-review) | 2 rounds; TEXT NUL termination, libei 1.5 compat, acceptance overclaiming | — | — | Caught honesty issues (overclaimed acceptance evidence), not just protocol bugs |
| 38 | 09 | Feature impl | Sol (medium) | 9b58f99 (PR #16), 161 tests: systemd user service manager, 8 CLI acceptance tests, systemd-analyze verify green | Sol review blocked by Codex usage cap; self-review fixed ordering cycle, root-owned package, stop-timeout false success | self-reviewed | Merged on exact-head green CI per fallback instruction; flag for retro-review if desired |
| 39 | 10 | Feature impl | Sol (medium) | delivered 86b2225/d6bd6b0/316db24, 166 tests: failure-recovery hardening + systemd rate limit | round 1: 2 BLOCKER (no PDEATHSIG on provider children; non-discriminating acceptance tests) + 1 HIGH (portal tests didn't prove real clipboard Delivery) + 1 MEDIUM (live smoke clobbers real voisu.service) | 1 round used | Repeated the project-wide pattern: solid slice, weak on child-process ownership + discriminating-test discipline; ~340k tokens |
| 40 | 10 | Fix round | Sol (medium, resumed) | 0865286: shared guarded PDEATHSIG pre_exec hook w/ PPID race check (new process.rs), discriminating probes, production-boundary portal tests, panic-safe smoke cleanup; 170 tests | round 2: 0 findings → APPROVE | closed ticket | All 4 findings cleared in one round; survived a mid-round process kill and resumed cleanly (~288k tokens) |
| 41 | 10 | Reviews | Sol (high first, medium re-review) | 2 rounds; round 1 caught PDEATHSIG fork-race semantics w/ man-page citation + called out non-discriminating tests wholesale | — | — | Sol medium re-review verified every claimed fix against the diff incl. new process.rs; merged as aa8055a (PR #17), exact-head CI green |
| 42 | #14 | Scoped fix impl | Sol (medium) | d52c7d7 await-then-pop | round 1: BLOCKER (drop of abort future still detaches) + HIGH (yield_now false-pass test) | 1 round used | Fixed the named pattern, missed the drop-path semantics of JoinHandle |
| 43 | #14 | Fix round | Sol (medium) | 8adebea Drop-time reaper | round 2: BLOCKER (reaper itself detached; spawn_blocking not abortable) + HIGH | round exhausted | Second miss on the same detach class → escalated |
| 44 | #14 | Escalated fixes | Opus 4.8 (high, resumed x2) | d09aa35 actor-owned ProviderReaper; f08bd1a shutdown handshake + off-loop drains; 9986d44 no-detach-on-timeout + TimeoutStopSec=60s + Starting diagnostics | rounds 3–4 narrowed to timeout-expiry paths, round 5: 0 findings → APPROVE | closed issue | RED proofs every round; 3 rounds to fully clear shutdown-ordering edges (~510k tokens total) |
| 45 | #14 | Reviews | Sol (medium all rounds) | 5 rounds; real BLOCKERs in rounds 1–4 (detach-on-drop, reaper detach, shutdown ordering, deadline-expiry) | — | — | Sol medium sustained deep Tokio ownership analysis across 5 rounds on a "small" fix; issue was 4x bigger than scoped |
| 46 | 11 | Feature impl | Luna (medium) | delivered 9eb0810: GTK4 Layer Shell capsule + PresentationController + observer IPC | round 1: 2 HIGH (no click-through input region; untyped string overlay events) + 2 MEDIUM | 1 round used | First Luna dispatch of the project; correct structure and fast, but shallow on Wayland input-region and IPC typing details |
| 47 | 11 | Fix round | Luna (medium) | cd73551: empty GDK input region, typed ID-versioned OverlayEvent, DESIGN token treatments | round 2: HIGH (lifecycle responses leaked into normal Status) + HIGH (Processing→Hidden latent bug) | round exhausted | Cleared its own round-1 findings but introduced/missed observer-isolation defects → escalated |
| 48 | 11 | Escalated fixes | Opus 4.8 (high, resumed) | 0d7a2fd observer-only lifecycle + Processing-Hidden fix (+9 tests); ce06c11 instance-scoped event IDs + genuine 30/32-permit saturation test | round 3 narrowed to restart ID collision; round 4: 0 findings → APPROVE | closed ticket | RED proofs for both rounds; found and killed the exact daemon-restart collision Sol predicted |
| 49 | 11 | GTK compile fix | Luna (medium) | 1636157: gtk4-layer-shell 0.8 trait API, v4_10 accessibility feature, reduced-motion default | driver-verified (trivial diff); folded into round-4 APPROVE | — | Clean mechanical API adaptation with compile loop, ~99k tokens; deprecated show/hide swept by driver |
| 50 | 11 | Screenshot gate | Driver (Fable) | stub-daemon harness on live Fedora KDE Wayland; 6 states captured, 1 defect found (opaque window behind capsule), fixed 5cc46fc, recaptured clean | — | — | Gate the sandbox could never run; driver vision critique against DESIGN.md tokens |
| 51 | 11 | Reviews | Sol (high first, medium re-reviews) | 4 rounds; round 1 caught click-through + typing, round 3 predicted the restart ID collision, round 4 APPROVE | — | — | Round-4 run died with a host shutdown and re-ran stateless without loss |
| 52 | 12 | Feature impl | Terra (high) | delivered 1a2886f: pure feedback selector + bounded overlay supervisor + --report-backend, 3 contract tests | round 1: 2 HIGH (notification backend dead — windowless GApplication quits; missing-GTK detection impossible in a GTK-linked binary) + 4 MEDIUM | 1 round used | First Terra dispatch: strong pure-layer decomposition and fast (~99k tokens), but shipped an untested-in-practice backend and an honesty gap |
| 53 | 12 | Fix round | Terra (high) | 9749ef8: ApplicationHoldGuard, JournalLog backend, xwayland-fallback probe, ExitCode propagation | round 2: 1 HIGH (GTK map probe still not compositor truth; false permanent fallback possible) + 2 MEDIUM (Idle-flash regression from unconditional present(); contradictory decisions.md) | round exhausted | Fixed 5 of 6 cleanly but the hard finding got a plausible-looking unsound probe + a new visual regression → escalated |
| 54 | 12 | Escalated fixes | Opus 4.8 (high) | 23ab68d: dropped map pretense for honest local-realization semantics + supervise-records-protocol-error story, no startup present(), immediate polling, decisions.md supersession; +2 discriminating tests | round 3: 0 findings → APPROVE | closed ticket | Chose honesty over cleverness on the unsound probe; ~85k tokens, single round |
| 55 | 12 | Reviews | Sol (high first, medium re-reviews) | 3 rounds; round 1 caught the dead notification backend + impossible-detection honesty gap, round 2 caught the unsound map heuristic with GTK-internals reasoning | — | — | Sol high round 1 again the deepest: 2 HIGH both structural, none cosmetic |
| 56 | 13 | Feature impl | Luna (high) | delivered a6b7934: RPM spec + build/smoke scripts + packaged unit + service migration + evidence scaffold, 201 tests | round 1: 5 BLOCKER + 3 HIGH + 4 MEDIUM (Fedora ownership facts, offline build, artifact binding) | 1 round used | ~296k tokens; first Luna-high benchmark — broad correct structure fast, but factual Fedora packaging claims unverified |
| 57 | 13 | Fix round | Luna (xhigh) | a4e978e: all 12 round-1 findings — vendored offline build, dump-based checks, LICENSE, canonical commit, 202 tests | round 2: 3 HIGH + 1 MEDIUM survived (precedence inverted, binding bypassable, vendor non-deterministic) | round exhausted | ~301k tokens; first Luna-xhigh benchmark — cleared the mechanical dozen but missed systemd/RPM semantics → escalated |
| 58 | 13 | Escalated fixes | Opus 4.8 (high, resumed) | ca43905 effective-unit resolution + full-manifest smoke binding + deterministic vendor; a65787b shadowed-unit migration + LoadState/multi-exec validation + independent-vendor self-test | rounds 3–4 still found real parser/restore defects (shadow case initially unreachable, permissive unit-file parsing) | round exhausted (2 rounds) | ~143k + ~232k tokens; strong systemd research and RED proofs, but ExecStart parsing discipline fell short twice → driver took over |
| 59 | 13 | Driver fixes | Driver (Fable) | 674b93e SIGPIPE-141 (exposed by the first real host rpmbuild run); 8d37e38 strict conservative unit-file parser + block-anchored show parser + end-state smoke verification; f625a73 section-aware parsing + block-opening anchor + stop verification; 390883d fresh-install active-service restore; 213 tests, every fix RED-proven | rounds 5–6 narrowed to edge semantics; round 7: 0 findings → APPROVE | closed review cycle | Host RPM gate executed for the first time: offline vendored rpmbuild + %check green, base/overlay/debuginfo + SRPM produced, rpmlint polish |
| 60 | 13 | Reviews | Sol (high first, medium re-reviews) | 7 rounds; round 1 the deepest of the project (12 confirmed findings with Fedora package-list citations); rounds 3–6 kept finding real semantic edges (XDG shadow precedence, section-blind parsing, silent restore) | — | — | ~51k–115k tokens/round at medium; sustained precision across 7 rounds without a single cosmetic-only round |

## Final report — Sol / Terra / Luna vs Opus (tickets 01–13, 2026-07-16)

60 dispatches across 14 delivery efforts (tickets 01–13 + issue #14). Every implementation was reviewed
by Sol (high first review, medium re-reviews) until APPROVE; every fix claim was verified against the diff.

### Scorecard by role

| Model (role) | Dispatches | Closed its ticket without escalation | Typical failure mode | Verdict |
|---|---|---|---|---|
| Sol — implementer (medium) | 01,03,04,05,08,09,10,#14 | 3 of 8 (08, 09, 10) | detached tasks / cancellation-ownership; repeated the same class across tickets | Good architecture fast; budget one escalation round |
| Sol — reviewer (high→medium) | every ticket | — | none observed; 7 sustained rounds on ticket 13, zero cosmetic-only rounds | The single highest-ROI Codex spend of the project |
| Terra — implementer (high) | 02, 12 | 0 of 2 | security edges (02), honesty gaps + unsound probe (12) | Fast, clean decomposition; always pair with a hard review |
| Luna — implementer (medium/high/xhigh) | 11, 13 (+2 fix rounds) | 0 of 2 | platform semantics: Wayland input regions, Fedora ownership facts, systemd precedence | Best for mechanical/glue/frontend work at medium |
| Opus 4.8 — escalation fixer (high, resumed) | 02–05,#14,11,12,13 | cleared the round it was given in ~70% of rounds | parser/edge discipline under repeated adversarial review (13 rounds 3–4) | The workhorse: RED proofs, honest claims, rarely introduces defects |
| Driver (Fable) | screenshot gate, host RPM gate, 13 rounds 4–6 fixes | — | — | Gates no sandbox can run + final-mile fixes when both tiers exhausted |

### Luna effort experiment (medium → high → xhigh)

Ticket 11 (medium) and ticket 13 (high impl ~296k tokens, xhigh fix ~301k tokens): raising effort did not
buy semantic depth. Xhigh cleared all 12 mechanical review findings but still missed systemd precedence
and RPM binding semantics — the same class medium-Luna missed on Wayland in ticket 11. Cost was flat
(~300k either way). Conclusion: when Luna misses, escalate the MODEL, not the effort.

### Escalation economics

- Codex implementation dispatches ended in Opus escalation in ~60% of tickets; Codex review dispatches
  never needed rescue and repeatedly found post-Opus defects (tickets 11–13).
- Opus escalations cost ~85k–240k tokens per round (worst case #14: ~510k total) and closed every
  escalation eventually except ticket 13's parser tail, which the driver finished.
- The two tickets implemented Opus-first (06, 07) still took 3–5 review rounds — review depth, not
  implementer choice, was the constant quality driver.

### Routing recommendation (going forward)

1. Keep **Sol high for first reviews, Sol medium for re-reviews** — protect this quota above all else.
2. **Heavy/architectural backend**: Sol medium remains the right first bat, but pre-plan the Opus
   escalation round; for work whose core risk is process/lifecycle/cancellation ownership, go
   **Opus-first** — that class defeated Sol implementation five times.
3. **Regular feature work**: Terra high is fine with a mandatory security/honesty review round.
4. **Mechanical, glue, packaging scaffolds, frontend**: Luna medium; never Luna above high — use the
   savings on review rounds instead.
5. Keep the **driver** on gates that need the real desktop/host (screenshots, rpmbuild, live smoke) —
   both real-hardware defects of this project (opaque capsule window, SIGPIPE-141) were invisible to
   every sandboxed agent.

## Accuracy effort (feature/transcription-accuracy, 2026-07-17 →)

| # | Ticket | Task | Model (effort) | Result | Review findings vs its work | Fix rounds | Notes |
|---|---|---|---|---|---|---|---|
| 61 | acc-04 | Feature impl (Groq full-audio + dictionary) | Opus 4.8 (high) | delivered 19cd716, 229 tests green (driver-verified) | round 1: 1 BLOCKER (detached finalize JoinHandle) + 1 HIGH (gate not re-checked after finish() drain) + 3 MEDIUM + 1 LOW | 1 round used | Clean full-audio gate + shared dictionary module; survived a mid-run session-limit kill and resumed losslessly; ~122k tokens. Repeated the project-classic cancellation-ownership miss |
| 62 | acc-06 | Feature impl (divergence gate + provider-failure visibility) | Opus 4.8 (high) | delivered 54e29ff, 229 tests green (driver-verified) | round 1: 1 BLOCKER (failure evidence discarded when all providers fail) + 4 HIGH (dead stage variants, bypassable overlap guard, always-Groq "better source", signed-URL export leak) + 1 MEDIUM | 1 round used | Thresholds proven against recording-11 word salad before tests; but gate policy + scrub boundary security-naive — the established Opus round-1 pattern; ~171k tokens |
| 64 | acc-05 | Feature impl (Deepgram nova-3 WS streaming) | Fable 5 (medium) | delivered 132f225, 229 tests green (driver-verified), 2 consecutive runs | pending Sol first review | — | Full DeepgramStream replacement + sync mock WS server; caught 3 real defects via its own suite (non-cancellable dial, hung pre-connect cancel, silent drain-race empty Transcript); honest deviation reporting; ~276k tokens, ~45 min |
| 63 | acc-04/06 | First reviews | Sol (high) | 2 reviews; both CHANGES REQUIRED with confirmed structural findings | — | — | 04: caught the detached finalize JoinHandle class again; 06: deepest finding was the always-Groq fallback policy + signed-URL scrub bypass; ~139k/~132k tokens |
| 65 | acc-04 | Fix round | Opus 4.8 (high, resumed) | delivered 239ef1a, 238 tests; found+fixed a follow-on double-poll panic its own fix exposed | round 2: 0 findings → APPROVE | closed impl (pending integration) | RED proofs per finding incl. hanging-TCP reaper-ownership test; byte-length token upper bound is a provable over-count; ~239k tokens |
| 66 | acc-05 | Reviews r1 | Sol (high) | CHANGES REQUIRED: 1 BLOCKER (redial hides mid-Recording audio gap = fluent-nonsense risk) + 2 HIGH (drain accepts truncation as success w/ Deepgram doc citation; ws userinfo loophole leaks token plaintext) + 3 MEDIUM + 1 LOW | — | — | Verified CloseStream contract against Deepgram docs; the exact silent-truncation class the PRD targets; ~154k tokens |
| 67 | acc-06 | Fix round 1 | Opus 4.8 (high, resumed) | delivered d63b8a4, 238 tests; quality-score policy + content-word overlap + URL scrubbing | round 2: 6 HIGH (bias persisted in reconciliation-failure fallback; score gameable by unique-word salad; winner erased on loser-cleanup failure; start-path gaps; scrub scan/scheme gaps; delivery_fallback_reason unscrubbed) | round exhausted → round 2 dispatched | Sol medium probing the fix seams found real policy + security edges |
| 68 | acc-05 | Fix round 1 | Fable 5 (medium, resumed) | delivered abf4fd9, 236 tests green 2x (driver-verified via agent report); RED proof per finding; redesigned redial policy (audio_delivered gate), Metadata-evidenced drain, structural userinfo rejection | re-review: 0 findings → APPROVE | closed impl (pending integration) | Survived 2 session-limit kills with lossless SendMessage resume; its non-discriminating-bytes fix caught a real harness race (truncated PCM emission); ~375k tokens total across the ticket |
| 69 | acc-05 | Re-review | Sol (medium) | VERDICT: APPROVE, 0 findings | — | — | ~150k tokens; verified all 7 fixes incl. redial-policy redesign against ProviderReaper contract |
| 70 | acc-06 | Fix round 2 | Opus 4.8 (high, resumed) | delivered d06062a, 244 tests; simplified gate to degeneracy-or-fragment | round 3: 3 HIGH (gate now bypassed by any non-degenerate nonsense — over-correction; fallback still gameable 0.918 vs 0.733; capture-begin failure persists zero provider entries + Aborted-vs-NotStarted stage inconsistency) | STRIKE 3 → agent discarded | The pendulum pattern: bypassable → gameable → removed. Opus never solved the adversarial policy core across 3 attempts (~404k tokens this round) |
| 71 | acc-06 | Re-reviews r2+r3 | Sol (medium) | r3: REQUEST_CHANGES, 3 HIGH with concrete score arithmetic (0.918 vs 0.733) and untested paths | — | — | ~120k tokens; caught that the "fix" tests encoded the wrong contract (reconciling fluent nonsense) rather than proving §3.4 |
| 72 | acc-06 | Rescue impl (fresh context) | Fable 5 (high) | delivered bd34220, 248 tests + overlay clean (driver-verified); three-tier gate (degeneracy/fragment/cross-source agreement <0.2) + evidence-ordered select_better_source + complete startup accounting | round 4: 2 HIGH (occurrence-counted confirmation inflatable by word-copying salad; exact-token containment gates homophone divergence then picks wrong provider) | fix round 1 dispatched | ~134k tokens, ~13 min — solved in one pass the policy core Opus missed for 3 rounds (~700k); remaining findings are adversarial edge refinements, not design rejections |
| 75 | acc-06 | Re-review r4 | Sol (medium) | REQUEST_CHANGES, 2 HIGH — constructed the homophone counter-example ("cache writes failed" vs "cash rights sailed") showing gate + wrong-winner composition | — | — | ~119k tokens; quality of adversarial probing stayed high into round 4 |
| 76 | acc-06 | Rescue fix round 1 | Fable 5 (high, resumed) | delivered bc01840, 250 tests + overlay clean (driver-verified); distinct-based confirmation, content-TTR degeneracy clause, phonetic escape hatch | round 5: 2 HIGH + 1 MEDIUM (TTR clause false-positives real command repetition "start stop reset"×3 = 0.33; phonetic matching non-bijective — 6 short words all match one "rat"; regression test non-discriminating) | fix round 2 dispatched (strike 2) | ~162k tokens; survived 2 API connection drops with lossless resumes; each new mechanism spawned its own adversarial edge — the gate policy is genuinely hard |
| 77 | acc-06 | Re-review r5 | Sol (medium) | REQUEST_CHANGES, 2 HIGH + 1 MEDIUM with concrete counter-examples per mechanism | — | — | only ~43k tokens — reviews getting cheaper as the diff narrows |
| 78 | acc-06 | Rescue fix round 2 | Fable 5 (high, resumed) | delivered 3d2e2c2, 253 tests + overlay clean (driver-verified); is_stolen_word_loop tier, one-to-one phonetic matching, discriminance-proven regression test | round 6: 3 HIGH + 1 MEDIUM + 1 acceptable LOW (recycled-word conjunct not actually implemented; 4-distinct-word loop slips between tier thresholds; nonsense loop wins via intrinsic cohesion; asymmetric alignment = provider-position-dependent decisions) | STRIKE 3 → second discard | ~190k tokens; finding 1 was an implementation-vs-claim gap — the first honesty miss of the rescue |
| 79 | acc-06 | Re-review r6 | Sol (medium) | REQUEST_CHANGES with the tier-gap class made explicit (thresholds ≥5 and <5 leave 4 uncovered) | — | — | ~88k tokens; explicitly separated must-fix from acceptable residual (sea/see) when asked |
| 80 | acc-06 | 2nd rescue impl (fresh context, simplify mandate) | Fable 5 (high) | delivered 4f71124, 258 tests + overlay clean (driver-verified); unified symmetric phonetic_matching feeds gate + selection; deleted 2 tiers; low-confidence §3.5 annotation for undecidable selections | round 7: 1 must-fix HIGH (one-match discontinuity in hollow clause) + residuals explicitly ruled acceptable | fix round dispatched | ~157k tokens; the simplify mandate worked — Sol accepted the design, first round of this whole ticket with a single finding |
| 81 | acc-06 | Re-review r7 | Sol (medium) | REQUEST_CHANGES narrowed to 1 must-fix; explicitly separated acceptable residuals per instruction | — | — | ~54k tokens; convergence achieved — findings per round: 6→3→2→3→5→1 |
| 82 | acc-06 | 2nd rescue fix round | Fable 5 (high, resumed) | delivered b2b83a0, 259 tests + overlay clean (driver-verified); hollow floor aligned to CONTENT_OVERLAP_FLOOR so no band opens | round 8: 0 findings → APPROVE | TICKET CLOSED (8 rounds total) | ~167k tokens this round; ticket 06 grand total ≈ 1.9M impl tokens across 3 agents / 8 commits — the adversarial-policy outlier of the effort |
| 83 | acc-06 | Re-review r8 | Sol (medium) | VERDICT: APPROVE | — | — | ~52k tokens; verified boundary arithmetic at the 0.2 floor |
| 73 | live-bug | Recording-deadline diagnosis | Sonnet 5 (high) | root-caused 60 s default capture deadline (system.rs:1277) from journalctl evidence in one pass; proved accuracy branch doesn't fix it | — | — | ~51k tokens, 90 s; log-evidence-first discipline paid off — code guesses (Groq 25MB, provider deadline) all wrong |
| 74 | live-bug | Deadline fix (60 s → 600 s default) | Opus 4.8 (high) | delivered b7b01a4, 245 tests + overlay check clean (driver-verified commit) | driver spot-review only (1-file change) | — | ~40k tokens, ~5 min; extracted pure `resolve_recording_deadline` seam to pin the default without env races — good judgment on test seam choice; clean concurrent operation next to the rescue agent |

### Post-merge addendum (2026-07-17, live smoke day)

The first live desktop smoke runs after the ticket 13 merge surfaced **four** more defects no sandboxed
agent could have seen, all diagnosed and fixed by the driver on the real machine (RED→GREEN, full gate
3x): the smoke harness parsed rpm's "not installed" notice as a NEVRA; `wl-copy`'s clipboard-serving
child was misread as a deadline timeout (broke doctor and Delivery); real `pw-record` exits 1 silently
on SIGINT so every live graceful stop failed (the fakes had modeled `exit 0` — reality disagreed); and
the stored provider credentials turned out to be placeholders, caught only by `auth verify` against the
real APIs. This quadruples the evidence for recommendation 5: the sandbox proves contracts, only the
host proves the tools.
| 84 | live-gate | Live-blocker diagnosis + fixes (rustls provider, PDEATHSIG thread reap, 2 test races) | Fable 5 driver (inline) | root-caused two live blockers with evidence chain (probe example, replay, /proc timing); delivered a0899b3, 99d0f9e, 1a63b72, f04dbbe; RED-proven regression test | Sol high r1: 2 release findings (ring licenses, test false-green) | fixed in 0615736 | driver did the work inline — systematic-debugging discipline; ~10 s Tokio blocking-pool keep-alive vs per-THREAD PR_SET_PDEATHSIG was the key insight |
| 85 | live-gate | Review r1 of live-blocker fixes | Sol (high) | VERDICT: FINDINGS (2): ring license compliance in RPM, zombie/PID-reuse false-green in reap test | — | — | ~161k tokens; both findings valid and release-relevant; confirmed handoff ownership + runtime-builder equivalence |
| 86 | live-gate | Re-review of 0615736 | Sol (medium) | VERDICT: APPROVE | — | — | ~63k tokens; verified /proc stat field arithmetic and vendor %prep paths |
| 87 | live-gate | CLI keyterm accuracy pass (dictionary reorder + compounds + truncation-guard test) | Fable 5 driver (inline) | root cause was prompt-budget truncation, not missing terms; P2 WER 19.3%→10.5%, all CLI compounds now correct; 299 tests | skipped (trivial diff per review policy) | — | live retest overall: raw 10.8%, formatting-adjusted 9.2% — first sub-10 result |
| 88 | delivery-keymap | EIS keymap fd pread fix (system.rs read_keymap_fd + memfd regression test) | prior-session driver (adopted from worktree); Fable 5 driver verified+rebased | fix matched root-cause hypothesis exactly; test GREEN, 300/300 workspace | Sol (high) — first review | APPROVE, no findings, ~112k tokens | staged-uncommitted work in /tmp worktree was adoptable; committed, rebased onto post-#23 main clean |

## Hardening criticals via cladex (H1/H2, 2026-07-18)

> New this session: all Sol dispatches ran via **cladex** — a local CLIProxyAPI bridge running Sol inside
> the Claude Code harness instead of the codex CLI ("Sol via cladex"). Effort passed with the native
> `--effort` flag (wire-verified: proxy log shows the level applied). Costs are **NOMINAL** (Claude Code
> pricing math over proxy traffic; actual billing is the Codex Plus subscription). Durations are
> wall-clock for the headless session.

| # | Ticket | Task | Model (effort) | Result | Review findings vs its work | Fix rounds | Notes |
|---|---|---|---|---|---|---|---|
| 89 | H1 | Feature impl: supervise process_recording, mirror supervise_replay | Sol (medium, cladex) | delivered 1cbb82e, 301 tests, clean TDD w/ RED evidence | r1: 1 BLOCKER + 1 MAJOR + 1 MINOR | 2 rework rounds to clear | 10.8 min, 70 turns, 143k in / 15k out, $2.67 nominal |
| 90 | H1 | Review round 1 | Sol (high, cladex) | REQUEST_CHANGES — real BLOCKER: supervisor itself could re-panic on poisoned diagnostics lock | — | — | 3.9 min, $1.37; high-value catch |
| 91 | H1 | Rework round 1 | Sol (medium, cladex) | delivered, 303 tests; poison-tolerant lock + honest panic classification | r2: rollback-unsafe persisted enum variant (MAJOR) + disabled-provider inaccuracy (MINOR) | — | 11.0 min, 78 turns, $2.70 |
| 92 | H1 | Re-review round 2 | Sol (medium, cladex) | REQUEST_CHANGES, both findings legit (rollback wipe scenario subtle) | — | — | 2.8 min, $0.88 |
| 93 | H1 | Rework round 2 | Sol (medium, cladex) | delivered, 304 tests; followed orchestrator binding decisions (reuse Aborted stage, configured-provider plumbing) | r3: 1 HIGH remained (eprintln! panics on failed stderr write on guaranteed-completion path) | — | 7.5 min, $3.62 |
| 94 | H1 | Re-review round 3 | Sol (medium, cladex) | REQUEST_CHANGES — the eprintln HIGH; genuinely same wedge class | — | — | 3.5 min, $0.95 |
| 95 | H1 | Point fix: log_best_effort helper, 5 sites both supervisors | Driver (Fable) inline + Opus 4.8 (high) | delivered fcd9066 | r4: APPROVE, 0 findings | closed ticket | Opus: 3.1 min, 37.8k tokens, first-try clean |
| 96 | H1 | Confirmation review round 4 | Sol (medium, cladex) | APPROVE | — | — | 45 s, $0.30; PR #25 merged, CI green |
| 97 | H2 | Feature impl: stop_child → spawn_blocking | Sol (medium, cladex) | delivered b30d3e0, 301 tests; excellent single-worker responsiveness regression | r1: 1 MAJOR — cancellation-unsafe (timeout drops future, detaches cleanup) | 2 rework rounds to clear | 8.6 min, 51 turns, $2.11 |
| 98 | H2 | Review round 1 | Sol (high, cladex) | REQUEST_CHANGES — the cancellation MAJOR; subtle (old inline blocking was accidentally cancel-proof) | — | — | 3.7 min, $0.68 |
| 99 | H2 | Rework round 1: reaper adoption + drain_to_completion | Sol (medium, cladex) | delivered, 302 tests; driver rebased onto merged H1 and reconciled drain sites into supervise_recording/replay (306 after rebase) | r2: 1 MAJOR (pre-stop Drop path still detached) + 1 MINOR (250 ms test bound flaky) | — | 8.4 min, 69 turns, $2.48 |
| 100 | H2 | Re-review round 2 | Sol (medium, cladex) | REQUEST_CHANGES, both legit | — | — | 3.5 min, $0.98 |
| 101 | H2 | Rework round 2: Drop-path adoption (adopt_capture_blocking, poison-tolerant retain) | Opus 4.8 (high) | delivered be18833, 307 tests, RED-first | r3: APPROVE, 0 findings | closed ticket | 6.6 min, 70.8k tokens; matches standing routing rec (ownership/lifecycle → Opus) |
| 102 | H2 | Confirmation review round 3 | Sol (medium, cladex) | APPROVE | — | — | 1.5 min, $0.54; PR #26 pending CI merge |

### Running observations (H1/H2, cladex session)
- **cladex (Sol inside the Claude Code harness) worked flawlessly across 12 dispatches** — full tool use,
  TDD, JSON usage capture; `--effort` passes through to Codex reasoning effort (wire-verified). A viable
  codex-CLI replacement: tokens bill to the Codex quota, none to the Claude quota, and the harness's cache
  reads (21M+ cumulative) are the dominant token class.
- **Sol's review ladder again earned its cost**: every round's findings were real (poisoned-lock re-wedge,
  rollback history wipe, eprintln-panic class, cancellation detach, pre-stop Drop bypass) — zero false
  REQUEST_CHANGES across 7 review rounds on the two tickets.
- **Sol as implementer**: strong first passes, but each introduced one subtle second-order defect
  (rollback-unsafe variant; cancellation-unsafety) that review caught. Opus 4.8 high closed both tickets'
  final scoped lifecycle fixes first-try with zero findings — consistent with the ticket-03-era rec.
- **Session split after Raja's 50/50 directive**: the Claude side took point fixes, scoped lifecycle
  rework, and docs; Sol kept architecture-grade implementation + all reviews.

## Final report — accuracy effort + live gates (rows 61–87, 2026-07-18)

Extends the tickets-01–13 report above; that snapshot stands as history. 27 dispatches across the
transcription-accuracy branch (acc-04/05/06), the recording-deadline live bug, and the post-merge live
smoke/release gate. Two new coder models debut: **Fable 5** as an implementer/rescuer and **Sonnet 5**
as a diagnostician. Sol remained the sole reviewer (high first, medium re-reviews) throughout.

### Scorecard by role

| Model (role) | Dispatches | How it closed | Failure mode | Verdict |
|---|---|---|---|---|
| Opus 4.8 — implementer/fixer (high) | acc-04 (impl+fix), acc-06 (impl+2 fix rounds), live-deadline fix | acc-04 closed in 1 fix round; live-deadline clean; **acc-06 discarded on strike 3** after 3 attempts | The "pendulum" on adversarial policy: bypassable → gameable → removed; never solved the gate core | Still the reliable fixer for scoped ownership/lifecycle work; do NOT hand it open-ended adversarial-*policy* design |
| Fable 5 — implementer/rescuer (medium/high) | acc-05 (impl+fix), acc-06 (2 rescue attempts), live-gate driver, CLI keyterm | acc-05 closed clean (model debut); **acc-06: 1st rescue discarded on strike 3, 2nd rescue closed it** in 8-round total | Same adversarial-edge pendulum on the 1st rescue; one honesty miss (claim-vs-impl gap, row 78) | Legit new implementer tier — solved the acc-06 core Opus couldn't, honest deviation reporting; needs a **simplify mandate + fresh context** to avoid the edge pendulum |
| Sol — reviewer (high→medium) | every ticket; 8 rounds on acc-06 alone | unbroken; APPROVE only on genuinely clean diffs | none observed | Highest-ROI spend, reaffirmed: built the homophone counter-example ("cache writes failed" vs "cash rights sailed"), verified provider docs via web, separated must-fix from acceptable residuals on request; reviews got cheaper as diffs narrowed (~43k–54k late rounds) |
| Sonnet 5 — diagnostician (high) | live recording-deadline root-cause | one 90 s pass from journalctl evidence | none observed | The right tool for evidence-first live diagnosis; ~51k tokens, log-evidence-first beat every code guess |
| Driver (Fable, inline) | live-gate blockers, CLI keyterm, live-smoke defects | RED→GREEN on the real host | — | Owns the real-machine gate — where 4 host-only defects and both live blockers surfaced |

### The acc-06 outlier

acc-06 (divergence gate + provider-failure visibility) is the adversarial-policy outlier of the whole
project: **≈1.9M impl tokens across 3 agents / 8 commits / 8 review rounds** before APPROVE. Opus was
discarded on strike 3 after three attempts at the gate policy; a first Fable rescue solved the *core* in
one pass but was itself discarded on strike 3 chasing adversarial edges; only a **second Fable rescue with
an explicit simplify mandate and fresh context** closed it (deleted 2 tiers, unified symmetric phonetic
matching). Findings-per-round converged 6→3→2→3→5→1→0.

### New lessons (beyond the 01–13 report)

1. **Adversarial-policy design is a distinct hard class.** It defeated both Opus and a first Fable rescue
   (3 strikes each). What broke it was not more effort or more rounds but **simplify the mandate + fresh
   context**. This is the policy-design analogue of ticket 13's "escalate the model, not the effort."
2. **The three-strike escalation rule earned its keep** — it fired twice on acc-06 and each discard-and-
   respawn was the correct call; grinding a stuck agent further would have burned tokens for no progress.
3. **Fable 5 is a real implementer tier now**, not just the driver persona: closed acc-05 (Deepgram WS
   streaming) solo and cracked the acc-06 gate core no other model reached.
4. **Opus stays best for scoped ownership/lifecycle fixes** (acc-04, live-deadline) but should not own
   open-ended reconciliation/gate *policy* design.
5. **Sandbox proves contracts, host proves tools** — reaffirmed a 3rd and 4th time: the live smoke day
   surfaced 4 host-only defects (NEVRA parse, wl-copy child misread as timeout, pw-record SIGINT exit 1,
   placeholder credentials) invisible to every sandboxed agent.
6. **Outcome:** first sub-10 WER — raw 10.8%, formatting-adjusted **9.2%**.

### Routing recommendation (updated)

1. **Sol reviews unchanged and reaffirmed** — high first, medium re-reviews; protect this quota first.
2. **Adversarial-policy / reconciliation-gate design → Fable 5 high with an explicit simplify mandate**
   and a hard Sol review; discard-and-simplify on strike 3 rather than grinding. Do not route this class
   to Opus as open-ended design.
3. **WS/streaming provider integration → Fable 5 medium** — proven on acc-05.
4. **Scoped ownership/lifecycle/cancellation fixes → Opus 4.8 high** — unchanged.
5. **Live/evidence-first diagnosis → Sonnet 5 high**, log-evidence-first, cheap.
6. **Keep the driver on real-desktop/host gates** — the only place host-only defects ever appear.

## Latency effort — Sol/Opus head-to-head experiment (rows 103–113, 2026-07-18)

Raja's directive for this window: a deliberate 50/50 head-to-head between the two benchmark winners.
Both models take BOTH roles, alternating per ticket (L-01: Opus implements / Sol reviews; L-04: Sol
implements / DOUBLE independent review — Sol high AND Opus high on the same brief, to benchmark Opus
as a reviewer against Sol's proven baseline). Sol dispatches over cladex; costs are nominal (real
billing = Codex Plus quota). Opus token counts are Claude Max subagent usage.

| # | Ticket | Task | Model (effort) | Result | Review findings vs its work | Fix rounds | Notes |
|---|---|---|---|---|---|---|---|
| 103 | L-01 | Impl: Deepgram default-OFF runtime toggle (config.toml + CLI + DisabledProvider no-network stand-in) | Opus 4.8 (high) | delivered 320 tests (307+13), overlay clean; adapter substitution kept the coordinator untouched | r1 (Sol high): REQUEST_CHANGES — 3 major 2 minor | 2 | 19.7 min, 175.8k tokens, 79 tool uses; SCOPE CREEP: rewrote model-benchmark.md report section unprompted — reverted by driver |
| 104 | L-01 | First review of the toggle diff | Sol (high, cladex) | REQUEST_CHANGES: success-path-only NotStarted retag, config re-read in supervised replay tail, TOML table-scope bug, env-inheritance test leak, truncating write | — | — | 5.3 min, 51 turns, $1.58 nominal, 101.6k in / 11.2k out; all 3 majors driver-verified real |
| 105 | L-01 | Rework round 1: all 5 findings confirmed + fixed | Opus 4.8 (high) | 326 tests, overlay clean; commit bc47265 | r2 (Sol medium): REQUEST_CHANGES — 1 high 2 medium | — | 9.2 min, 217.6k cumulative tokens; no scope creep this round |
| 106 | L-01 | Re-review round 2 | Sol (medium, cladex) | REQUEST_CHANGES: unreadable-config destructive replace; dictionary read/eprintln in enabled replay tail (pre-existing, caught vs the claimed invariant); normalization missing panic/startup record paths | — | — | 3.0 min, 28 turns, $0.98 nominal; caught the implementer's "zero filesystem access" claim being false for enabled mode |
| 107 | L-01 | Rework round 2: read-error propagation, startup keyterm snapshot threaded into the replay tail, normalization on panic/startup/shutdown records | Opus 4.8 (high) | all 3 confirmed + fixed, 328 tests, overlay clean; commit c5679a6 | r3: APPROVE, 0 findings | 2 total | 6.5 min, 247.8k cumulative tokens, 137 cumulative tool uses |
| 108 | L-01 | Confirmation review round 3 | Sol (medium, cladex) | APPROVE — ran targeted regression tests itself | — | — | 1.4 min, 15 turns, $0.47 nominal; PR #27 merged CI-green |
| 109 | L-04 | Impl: Groq WAV→FLAC upload (pure-Rust flacenc, in-memory encode, ~42% payload cut, no duration gate — 3 ms short-clip encode) | Sol (medium, cladex) | delivered 330 tests (328+2 RED-first), overlay clean; commit 762e608; corrected the ticket's assumption (no Deepgram batch path exists — it streams linear16 WS) | double review r1: both APPROVE; then 1 CI defect (row 112) | 1 | 19.6 min, 111 turns, $5.49 nominal, 206.6k in / 22.1k out; SCOPE CREEP: wrote docs/STATE.md + session-log "checkpoint" despite doc-skip instruction — reverted by driver (mirrors Opus's row-103 creep) |
| 110 | L-04 | Double-review A: FLAC diff | Opus 4.8 (high) | APPROVE — 1 minor (API-forced transient memory, bounded) + 2 nits + 1 informational; verified into flacenc SOURCE (frame retention, STREAMINFO offsets, build.rs RPM hermeticity), ran the tests itself | — | — | 4.0 min, 48.3k tokens, 17 tool uses; coverage-mode instructions (report all + confidence) |
| 111 | L-04 | Double-review B: FLAC diff (same brief, independent) | Sol (high, cladex) | APPROVE — 0 findings; verified widening, dep lock/features via offline --locked build, curl argv byte-identity, memory bound vs chunk geometry | — | — | 4.5 min, 52 turns, $1.10 nominal, 74.6k in / 8.4k out |
| 112 | L-04 | CI failure triage + fix: FLAC sample-count assertions raced the fake pw-record's post-signal trap bytes (>=N+1 demanded best-effort post-stop capture; CI lost the race) | Driver (Fable, inline) | 2-line test fix pinning the deterministic pre-stop bound (>=3,200 / >=500,000); root-caused before touching | — | 1 (vs Sol impl) | Charged as a Sol implementation defect. BOTH reviewers missed it — Sol high (0 findings) and Opus high (4 findings, none this) approved the race. First shared blind spot |
| 113 | L-04 | Merge | Driver | PR #28 CI-green (incl. 3x flake gate), merged e4d2c8e | — | — | Test baseline now 330 |

### Head-to-head verdict (this window)

**Implementation.** One rough parity with different failure shapes. Opus (L-01, architectural-ish:
config layer + provider lifecycle) needed **2 review rounds / 8 findings** but every finding was fixed
correctly on the round it was reported, RED-first. Sol (L-04, scoped codec swap) produced a **cleaner
first diff (0 review findings)** but shipped the one defect that actually broke CI — a racy test
assertion neither reviewer caught. Both models scope-creeped into orchestrator-owned docs exactly once
each (103, 109) despite explicit doc-skip instructions.

**Review.** Sol remains the stronger fault-finder on complex lifecycle diffs: 8 real findings across
L-01 rounds 1–2, including catching the implementer's false "zero filesystem access" claim. Opus's
review debut (L-04) was genuinely good — agreeing verdict, deeper source-level verification (flacenc
internals, RPM hermeticity of transitive deps), 4 low-severity findings Sol didn't report, at roughly
half the wall-clock of a Sol high round and zero Codex quota. But L-04 was a clean scoped diff; Opus
as reviewer is **unproven on the class where Sol earns its keep** (multi-round lifecycle/supervision
diffs). Joint miss on the trap-byte race shows cross-model double review is not a superset guarantee.

**Cost-effectiveness (nominal).** Sol this window: $9.62 nominal (impl $5.49 + reviews $4.13),
~25.5 min wall. Opus: ~466k subagent tokens (impl 247.8k + review 48.3k + L-04 scope-creep overhead),
~39 min wall, zero Codex quota. Per role: Sol review ≈ $1.10–1.58 first / $0.47–0.98 re-review; Opus
review ≈ 48k tokens flat. Opus reviews are effectively free under Claude Max — the strategic play
stays: **Sol reviews for lifecycle/supervision-class diffs, Opus double-review for scoped diffs and
as a cheap second opinion; implementation alternates freely (both are competent, differently shaped).**

### Routing recommendation (updated after rows 103–113)

1. Keep Sol as primary reviewer for architecture/lifecycle/supervision diffs (high first, medium re-reviews).
2. **Opus is now a validated reviewer for scoped diffs** — use it to double-review anything touching
   packaging-critical or dependency surfaces (its hermeticity/source-level digging was the best of the
   window) and as the default reviewer when Codex quota is tight.
3. Double review earns its cost on high-risk diffs only; it did not catch the one real defect here —
   add "timing/race audit of test assertions" to both reviewers' briefs going forward.
4. Implementation: alternate freely between Opus high and Sol medium; expect Opus to need review rounds
   but converge reliably, Sol to ship cleaner first diffs with rarer but sharper defects.
5. Both models need an explicit "do not touch docs/STATE/checkpoint/benchmark files" fence in every
   dispatch prompt — one scope-creep incident each this window.

## Post-latency ride-alongs (history-pretty feature + hardening 03/04) — rows 114–121

| # | Ticket | Task | Model (effort) | Result | Review findings vs its work | Fix rounds | Notes |
|---|---|---|---|---|---|---|---|
| 114 | HIST | Impl: human-first `voisu history` (pure history_view renderer, tail headline, TTY-gated paging, --json byte-compat) | Opus 4.8 (high) | delivered 343 tests (330+13 RED-first), overlay clean; doctor correctly left alone; NO doc creep (fence worked) | r1 (Sol high): REQUEST_CHANGES — 1 high 1 medium | 1 | 12.7 min, 100.1k tokens, 38 tool uses; commit 7525a37 |
| 115 | HIST | First review | Sol (high, cladex) | REQUEST_CHANGES: terminal escape injection from network transcripts (HIGH), saturating_sub fakes "tail 0ms" on reversed timings (MED); verified send_command refactor, --json byte-compat, TTY gating, UTF-8 truncation clean | — | — | 2.9 min, 16 turns, $0.98 nominal, 120.9k in / 5.6k out |
| 116 | H-04 | Impl: CI clippy+audit gates + RustSec pre-triage (web-verified, no hits) + webpki-roots shim analysis | Opus 4.8 (high) | ci.yml only; audit gate green first run; clippy red as predicted (13 voisu-core errors matching its honest pre-guess) | driver triage | — | 5.7 min, 55.6k tokens, 18 tool uses; worktree isolation; PR #29 |
| 117 | H-04 | Clippy round-1 triage: doc-paragraph fixes + justified crate-level result_large_err allow (BoundaryError boxing → hardening-05) | Driver (Fable, inline) | voisu-core compiles clean | — | — | comment-only + attribute change |
| 118 | HIST | Rework: C0/C1 sanitization at the truncate_inline choke point + checked_sub tail | Opus 4.8 (high) | both confirmed + fixed, 3 RED-first tests, 346 total, overlay clean | r2 (Sol medium): APPROVE — verified single-choke-point claim + U+2028/29 + enum-label trust | 1 | 9.8 min, 139.6k cumulative tokens; PR #30 merged CI-green |
| 119 | HIST | Re-review round 2 | Sol (medium, cladex) | APPROVE | — | — | 2.6 min, 28 turns, $0.65 nominal |
| 120 | H-04 | Clippy iteration rounds 2–8 (driver): crate-root allows for voisu-app lib + bins + tests (lib.rs allows do NOT reach bin/test crate roots), 3 real one-line fixes, WS fn allows, and a final real fix — explicit `truncate(false)` on the flock lock file (suspicious_open_options) | Driver (Fable, inline) | 7 CI rounds to converge (clippy stops at the first failing target, so errors surfaced one crate root at a time); audit+test gates green every round; PR #29 merged | — | — | INCIDENT: round-6 allow commit landed directly on main (cwd slip after PR-30 merge; push won the race vs TaskStop). Benign (attribute/comment-only), disclosed, PR #29 re-triggered against updated main. Lesson: pin cwd (`git -C`) in every compound git command when a worktree is active |
| 121 | H-03 | Draft: systemd unit sandboxing for both user units + packaging-fedora.md rationale section | Driver (Fable, inline) | PR #31 opened, `systemd-analyze verify` clean; merge HELD until after live latency eval (unvalidated sandbox directives must not confound measurements) | — | — | inline (small config draft, no dispatch); MemoryDenyWriteExecute on daemon only, overlay AF_UNIX-only |

**Ride-along notes.** The doc fence (explicit "do not touch docs/STATE/checkpoint/benchmark" line in
every dispatch prompt) held for all three post-latency dispatches — zero scope creep after it was
adopted. Rows 114–119 reinforce the row-103–113 pattern: Opus converges reliably with one review
round; Sol at high found a genuine security defect (terminal escape injection) on its first pass.
The H-04 clippy convergence cost (7 CI rounds) was a driver/tooling problem, not a model problem:
no local clippy on this machine forced CI-iteration, and clippy's stop-at-first-failing-target
behavior serialized the discovery. One real bug-class fix fell out of the gate: the flock lock file
now states `truncate(false)` intent explicitly.

## Research fleet: distribution/roadmap decision support (Sonnet 5 scouts) — rows 122–133

All twelve dispatches were Sonnet 5 read-only web-research scouts (driver-orchestrated, doc fence in
every prompt, held 12/12). Row 133 was an adversarial fact-check pass over the fleet's load-bearing
claims — 6/8 CONFIRMED, 2 PARTLY TRUE, 0 WRONG — so the fleet's findings are validated, not vibes.

| # | Task | Model (effort) | Result | Duration | Tokens | Tools |
|---|---|---|---|---|---|---|
| 122 | Electron vs GTK for Wayland overlay + cross-distro | Sonnet 5 | GTK4 wins hard: Chromium/Ozone has no layer-shell path; all comparable tools (Handy, whisper-overlay, hyprwhspr) use GTK+layer-shell; Tauri (not Electron) is the only sane web-tech fallback | 92 s | 46.8k | 11 |
| 123 | Native deb/pacman/rpm packaging for Rust daemon+user units | Sonnet 5 | cargo-deb (.deb, units as assets + custom postinst), AUR source + cargo-aur -bin, keep RPM; nfpm only if config sprawl hurts; exact Ubuntu/Arch dep names delivered | 136 s | 49.0k | 12 |
| 124 | Flatpak/AppImage viability | Sonnet 5 | Flatpak LATER (portal/libei architecture already sandbox-shaped, but no systemd user-unit mechanism — open flatpak#2787); AppImage NEVER | 106 s | 49.8k | 13 |
| 125 | STT latency landscape validation | Sonnet 5 | Voisu measurements consistent with public data; "Deepgram fast" = streaming/interim latency, dictation cares about time-to-final where Groq short-clip round trip wins; Deepgram-only w/ client Finalize ≈300–500 ms plausible, not clearly faster | 101 s | 46.4k | 10 |
| 126 | Deepgram keyterm + custom-vocab best practices | Sonnet 5 | FOUND REAL BUG: merged_terms() sent uncapped; Deepgram 400-errors past 500 tokens/100 keyterms (whole stream dies). Best practice 20–50 curated terms; recommends dictionary CLI + hot-reload + replacements tier | 120 s | 61.0k | 20 |
| 127 | Delivery-mode UX (auto-type vs clipboard) | Sonnet 5 | Market default = auto-insert ON, clipboard opt-out; recommends delivery_mode enum (type/clipboard, reserve guarded); focus-guard = unshipped-anywhere differentiator | 104 s | 48.4k | 12 |
| 128 | Compositor compatibility matrix | Sonnet 5 | Hyprland+KDE Tier 1 (Hyprland EIS needs live smoke test); GNOME Tier 2 — no layer-shell ever (mutter#973), no wlr-data-control (mutter#524), manual Remote Desktop toggle; ship daemon+CLI sans overlay on GNOME; Ubuntu 24.10+ floor | 141 s | 53.6k | 17 |
| 129 | Dictation product landscape 2025–26 | Sonnet 5 | "Wispr Flow for Linux" gap is real+validated; top missing features by evidence: (1) AI cleanup layer, (2) context awareness, (3) auto-learned vocab; cloud-first defensible, local fallback = medium-priority hedge | 111 s | 58.0k | 16 |
| 130 | BYOK onboarding + secrets | Sonnet 5 | Pure BYOK viable on free tiers (Groq 8 h audio/day free; Deepgram $200 ≈ a year at 1–2 h/day); keyring crate + loud 0600 fallback; voisu setup wizard w/ live validation; error classification in doctor | 114 s | 48.8k | 12 |
| 131 | Distribution/update channels | Sonnet 5 | Ranked: AUR -bin (cargo-aur + deploy action) → COPR (webhook, vendored crates — builders offline) → self-hosted apt (Pages/Cloudsmith); skip Launchpad PPA; single on-tag GH Actions workflow | 132 s | 55.1k | 16 |
| 132 | Dual-provider precedent + economics | Sonnet 5 | Cost non-issue ($0–17.56/mo worst case; free tiers cover friends); ROVER literature supports diverse-pair fusion (~9–12% rel WER), but NO shipped product runs dual-vendor STT — industry pairs single STT + LLM cleanup | 107 s | 49.5k | 12 |
| 133 | Adversarial fact-check of rows 122–132 claims | Sonnet 5 | 6/8 CONFIRMED (keyterm 400-cap, GNOME refusals, Plasma 6.1 InputCapture, Finalize semantics, Groq limits/non-streaming, Whisper 224 tokens); 2 PARTLY TRUE (Electron "impossible"→"no native path, XWayland hacks exist"; Flatpak live issue is #2787 not #3178); 0 WRONG | 140 s | 58.3k | 24 |

**Fleet notes.** Twelve scouts, ~101 min of wall-clock work compressed into ~25 min of parallel
elapsed time, ~625k subagent tokens total, zero doc-fence violations, zero repo writes. The
adversarial verification pass (row 133) is worth keeping as a standing pattern for
decision-support research: it cost one extra dispatch and caught two overstatements before they
reached the decision log. Scout 126 alone paid for the fleet by finding a latent production bug
(uncapped keyterms → Deepgram 400 → dead stream) that no test covers.

| 134 | GNOME-specific overlay deep dive (follow-up requested by Raja mid-grilling) | Sonnet 5 | Found a real path: companion GNOME Shell extension (St widget + D-Bus listener, GSConnect/Custom-OSD precedent) = true always-on-top overlay on GNOME; plain-window keep-above is a no-op on Wayland BY DESIGN; XWayland override-redirect works but is an unsupported side-effect; recommends tiny extension (tier 1) + plain window (tier 2 default), auto-detect | 137 s | 58.6k | 15 |
