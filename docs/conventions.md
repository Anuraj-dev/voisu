# Conventions — Voisu
- Stack: Rust workspace (`crates/voisu-app`, `crates/voisu-cloud`, `crates/voisu-core`). GTK4 for the optional Overlay.
- Run the app: `cargo run -p voisu-app --bin voisu -- --help` (CLI); install units and use `systemctl --user` for the daemon/overlay as in `README.md`.
- Run tests: `cargo test --workspace` — mandatory RED → GREEN → REFACTOR cycles; test observable behavior via public interfaces.
- Before pushing: `tools/dev check` runs the CI format, lint, doc and unit-test gates locally (`--full` adds the whole suite).
- PR titles are Conventional Commits (`feat(tui): …`, `fix(daemon): …`). The squash commit takes the title and `version-bump.yml` reads its type; CI fails a title without one.
- New worktree on a fresh clone: run `tools/dev setup` once so every worktree gets the private agent files.
- Naming: use the ubiquitous language in `CONTEXT.md` exactly (Recording, Transcript, Source Transcript,
  Merge Result, Trigger Key, Delivery, Overlay, Recording Deadline, Quality Failure, Provider Deadline).
  Each term lists banned synonyms — do not use them in code, docs, or commits.
- Structure: daemon and Overlay are separate processes; daemon must work without GTK.
