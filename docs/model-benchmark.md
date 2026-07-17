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
