# Conventions — Voisu
- Stack: Rust (planned; not scaffolded yet). GTK4 for the optional Overlay.
- Run the app: TODO (no code yet)
- Run tests: TODO — mandatory RED → GREEN → REFACTOR cycles; test observable behavior via public interfaces.
- Naming: use the ubiquitous language in `CONTEXT.md` exactly (Recording, Transcript, Source Transcript,
  Merge Result, Trigger Key, Delivery, Overlay, Recording Deadline, Quality Failure, Provider Deadline).
  Each term lists banned synonyms — do not use them in code, docs, or commits.
- Structure: daemon and Overlay are separate processes; daemon must work without GTK.
