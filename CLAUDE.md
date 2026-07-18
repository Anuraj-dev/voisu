# Agent entry — Voisu

BEFORE doing anything in this repo:
1. Read `docs/STATE.md` — the current state of the project (what's done, what's in progress, gotchas).
2. Skim `docs/INDEX.md` — the map of every doc and what it's for.

Then read ONLY the further docs your task needs. Do not scan the repo blindly — that wastes tokens; the
docs exist so you don't have to.

At the END of a work session, run `/checkpoint` — it rewrites `docs/STATE.md` and logs the session so the
next agent (or you tomorrow) starts cheap.

- Conventions: `docs/conventions.md`
- Full decision log (why we chose things): `docs/decisions.md`
- Complex features are planned in `docs/specs/` (see `/spec`).

## Model & effort routing (pinned for this repo)

Updated by Raja 2026-07-19: Codex quota is nearly exhausted — Codex/GPT models are for REVIEWS ONLY.
All implementation goes to Claude models.

| Work | Model | Effort |
|---|---|---|
| Code review — FIRST review of a ticket | gpt-5.6-sol | high (Sol never goes above high) |
| Code review — re-reviews after the first, until merge | gpt-5.6-sol | medium |
| ALL implementation (features, fixes, glue, config, packaging) | claude-opus-4-8 subagent | high |
| Genuinely complex / architectural tickets (concurrency, IPC, provider streaming, reconciliation) | claude-fable-5 subagent | medium |
| Review of code a Claude model wrote is still Sol; review of Fable-written code may go to Opus if Sol quota is critical | — | — |
| ALL bulk reading, exploration, test/log triage, lookups | claude-sonnet-5 | high |

## Source of truth

- `CONTEXT.md` defines product language.
- `docs/adr/` records hard-to-reverse decisions.
- `.scratch/voisu-wayfinder/map.md` is the planning map.
- The approved specification and implementation tickets govern delivery.

## Standing constraints

- Fedora KDE Plasma on Wayland is the first supported target.
- Work in vertical RED -> GREEN -> REFACTOR cycles.
- Test observable behavior through public interfaces.
- Never require raw input-device or privileged `uinput` access on the normal
  Fedora path.
- Logs and diagnostics are local unless the user explicitly exports them.
- Do not build the GTK overlay before the daemon milestone is reliable.
- HyprVox may inform architecture, but Voisu is an independent Rust codebase.
- Preserve attribution for any MIT-licensed implementation adapted from
  HyprVox or other projects.
