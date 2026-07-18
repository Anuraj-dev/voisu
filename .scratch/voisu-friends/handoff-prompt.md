Continue Voisu as the orchestrator. Read docs/STATE.md and docs/INDEX.md first (repo CLAUDE.md rules apply).

Context: the distribution/roadmap decisions are MADE and researched — do not re-litigate them. The evidence digest is .scratch/voisu-research/2026-07-18-distribution-decisions.md; the new wayfinder map is .scratch/voisu-friends/map.md (15 tickets, phase A features → phase B packaging), mirrored on GitHub as issue #32 with tickets #33–#47 (local files canonical; close/comment both when resolving a ticket). Benchmark rows run through 134.

Work, in order:

1. **Fix batch (outside the map, all small, one PR each):**
   a. Flip the Deepgram default from OFF to ON (Raja, 2026-07-18: jargon accuracy is the everyday default; `voisu deepgram on|off` and VOISU_DISABLE_DEEPGRAM keep working; Groq-only stays the opt-in fast path). crates/voisu-app/src/config.rs + daemon default path + affected tests, RED→GREEN.
   b. Hardening-05 hygiene sweep per .scratch/voisu-hardening/issues/05: Box/shrink BoundaryError in Err positions, drop the now-unneeded result_large_err/large_enum_variant crate-root allows, and raise the 3 s wait_for_marker bound in crates/voisu-app/tests/daemon_cli_lifecycle.rs (~line 2105) to ~15 s.
   c. Keyterm cap fix (NEW BUG, research digest §6): merged_terms() feeds Deepgram keyterms uncapped (voisu-daemon.rs:443 → system.rs deepgram_streaming_url); Deepgram 400-errors past 500 tokens across keyterms, killing the stream. Cap by priority (user terms first), mirroring the whisper_prompt truncation pattern, RED→GREEN.

2. **Then work the map** at .scratch/voisu-friends/map.md ticket by ticket (frontier order; one ticket resolution at a time, claim before working). Phase A: ADR (01), delivery_mode enum (02), focus-tracking research (03), guarded mode (04), dictionary CLI+hot-reload (05), keyring probe (06), setup wizard (07), GNOME plain-window fallback (08). Phase B (blocked behind A): packaging accounts HITL checklist (09) then deb/AUR/COPR/apt-repo/release-CI/live-validation (10–15).

Process (non-negotiable):
- Branch per task, PR to main, merge only on CI green (all three gates). No AI credits in commits/PRs.
- Routing: workhorses are Opus 4.8 high and gpt-5.6-terra high (gpt-5.6-luna medium for packaging/config tickets); ALL reviews go to gpt-5.6-sol via cladex (first review high, re-reviews medium, Sol never above high) — except any ticket Sol itself implemented (04 is Sol's), which Opus 4.8 high reviews instead. Sonnet 5 for bulk reading/research scouts.
- Escalation: if an implementer fails 2 review rounds on the same ticket, discard that agent and either take the ticket yourself (Fable driver, inline) or respawn fresh at a higher model/effort. Don't grind a third round with the same agent.
- Every dispatch prompt carries the doc fence: agents must not touch docs/STATE.md, docs/sessions/, or docs/model-benchmark.md. Log every dispatch as a benchmark row continuing from row 135.
- Consult the agent-flow skill before spawning; prompt-craft Mode B for non-trivial dispatch prompts.

HITL reminders for Raja:
- After the next RPM build+install: delete the sandbox validation drop-ins at ~/.config/systemd/user/voisu.service.d/ and voisu-overlay.service.d/ (packaged units carry the directives now).
- Ticket 06 needs one reboot/login cycle from Raja; ticket 09 is his guided account-setup checklist.

End the session with /checkpoint.
