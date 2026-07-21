# Contributing to Voisu

Voisu is a cloud-first Linux desktop dictation daemon, written in Rust. First
supported target: Fedora KDE Plasma on Wayland.

## Build

```sh
cargo build --workspace
```

The default build is GTK-free. The optional Overlay is behind a feature flag:

```sh
cargo build -p voisu-app --features overlay --bin voisu-overlay
```

## Test

```sh
cargo test --workspace
```

On memory- or CPU-constrained machines (and matching the RPM `%check`
environment), scope the temp dir and thread count:

```sh
TMPDIR=/var/tmp RUST_TEST_THREADS=4 cargo test --workspace
```

Run as a normal user — running the suite as root breaks a permission-mode test.

## How we work

- **Vertical slices, RED → GREEN → REFACTOR.** Write a failing test that
  describes the behavior, make it pass, then clean up. Each change is a thin
  end-to-end slice, not a horizontal layer.
- **Test observable behavior through public interfaces.** Assert on what a user
  or caller can observe, not on private internals.
- **Use the domain language exactly.** `CONTEXT.md` defines the ubiquitous
  language (Recording, Transcript, Source Transcript, Merge Result, Trigger Key,
  Delivery, Overlay, Recording Deadline, Quality Failure, Provider Deadline).
  Each term lists banned synonyms — do not use them in code, docs, or commits.
- **Structure.** The daemon and the Overlay are separate processes; the daemon
  must build and run without GTK.

## Support constraint

Fedora KDE Plasma on Wayland is the first supported target. Voisu never requires
raw input-device or privileged `uinput` access on the normal path — the Trigger
Key and text Delivery go through desktop portals. Do not add features that depend
on privileged input access on that path.

## Commits

Use [Conventional Commits](https://www.conventionalcommits.org/). Merged commits
drive **automatic version bumping**:

| Commit type | Version effect |
|---|---|
| `feat:` | minor bump |
| `fix:` / `perf:` | patch bump |
| `!` or `BREAKING CHANGE:` | major bump |
| `docs:` `chore:` `ci:` `test:` `style:` `refactor:` | no bump |

Do not edit the version by hand — the next auto-bump overwrites it.

## Pull requests

CI is the oracle (there is no local clippy/lint tooling assumed). A PR must pass:

- **Tests + flake gate** — `cargo test --workspace`, including the flake guard.
- **Clippy** — `cargo clippy` with `-D warnings` (warnings fail the build).
- **cargo-audit** — no known-vulnerable dependencies.

Keep PRs to a single vertical slice with its tests. Build scripts under
`packaging/` require a clean, committed checkout.
