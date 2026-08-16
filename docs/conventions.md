# Conventions — Voisu
- Stack: Rust workspace (`crates/voisu-app`, `crates/voisu-core`). GTK4 for the optional Overlay.
- Run the app: `cargo run -p voisu-app --bin voisu -- --help` (CLI); install units and use `systemctl --user` for the daemon/overlay as in `README.md`.
- Run tests: `cargo test --workspace` — mandatory RED → GREEN → REFACTOR cycles; test observable behavior via public interfaces.
- Naming: use the ubiquitous language in `CONTEXT.md` exactly (Recording, Transcript, Source Transcript,
  Merge Result, Trigger Key, Delivery, Overlay, Recording Deadline, Quality Failure, Provider Deadline).
  Each term lists banned synonyms — do not use them in code, docs, or commits.
- Structure: daemon and Overlay are separate processes; daemon must work without GTK.
