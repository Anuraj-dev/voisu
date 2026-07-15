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

## Delegation to Claude (MANDATORY for codex/GPT agents)

You have a cheap, disposable worker pool: the `claude` CLI. Spend YOUR context and
reasoning ONLY on architecture, diagnosis, and final code. Everything token-heavy
goes to Claude:

- **Bulk reading / repo exploration / log & test-output triage / one-fact lookups**
  — do NOT read many files yourself. Run:
  `claude -p "Read <paths/area>, report <exactly what you need> as a bullet summary" --model claude-sonnet-5 --effort high`
  and consume the summary. Sonnet at high effort is the worker for ALL of this,
  including trivial lookups (a path, a signature, one grep).
- **Independent light side-tasks** (a small util + tests, doc updates) you don't want
  to context-switch into:
  `claude -p "<task with file paths and acceptance criteria>. Return a unified diff summary." --model claude-opus-4-8 --effort high`
  — then verify the diff yourself.

Rules: always state the expected output format in the `-p` prompt ("return a bullet
summary", "return a table of file → purpose"); never paste large file contents into
your own reasoning when a Claude summary suffices; treat `claude -p` calls as free
relative to your own context. Always pass `--effort high` on these calls.

## Model & effort routing (pinned for this repo)

| Work | Model | Effort |
|---|---|---|
| Code review — FIRST review of a ticket | gpt-5.6-sol | high (Sol never goes above high) |
| Code review — re-reviews after the first, until merge | gpt-5.6-sol | medium |
| Tough / architectural tickets (concurrency, IPC, provider streaming, reconciliation) | gpt-5.6-sol | medium |
| Regular feature work | gpt-5.6-terra | high |
| Light work (glue, config, packaging, small fixes) | gpt-5.6-luna | medium |
| Very small quick changes (avoid a codex roundtrip) | claude-opus-4-8 | high |
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
