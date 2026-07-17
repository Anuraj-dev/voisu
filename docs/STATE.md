# Voisu — State
> Cloud-first Linux desktop dictation app (Fedora KDE Plasma / Wayland) · Last checkpoint: 2026-07-17

## 🚧 In progress / next
- **Overlay login-start fix is implemented and review-clean but uncommitted.** Next: commit the tested tree when Raja approves, rebuild the exact RPM, install base + optional Overlay RPMs, rerun `voisu service install`, then observe a real logout/login and capsule rendering on KWin.
- Host acceptance still required: `voisu-overlay --supervise` active after login; Recording → Processing → Success/Failure → hidden Idle; kill the Overlay mid-Recording and prove daemon/Transcript/Delivery independence; clean uninstall/removal.
- Remaining release evidence after that: portal revocation, upgrade/removal, and explicit fallback scenarios. APT/DEB remains out of scope.

## Status
- The optional Overlay RPM now owns `/usr/bin/voisu-overlay` and `/usr/lib/systemd/user/voisu-overlay.service`; the unit runs `--supervise`, belongs independently to `graphical-session.target`, and has ordering but no dependency on `voisu.service`.
- `voisu service install|uninstall` best-effort enables/starts or disables/stops the Overlay only when systemd's effective fragment remains packaged and `ExecStart` runs only `/usr/bin/voisu-overlay`. Overlay failures are warnings; required daemon results remain authoritative. `service start|stop|restart` remain daemon-only.
- The Fedora smoke harness now snapshots and restores optional Overlay enablement/active state when `voisu-overlay` was already installed.
- Review: Fable 5 was unavailable after retries (`502 unknown provider`). Raja approved GPT-5.6 Sol fallback; the first high-effort review found two medium issues (user-unit shadow trust and smoke state leakage), both fixed test-first; medium re-review returned `NO FINDINGS`.
- Automated gates: `cargo test --workspace` — 221 passed, 2 live tests ignored, 0 failed; GTK-free workspace check and `cargo check -p voisu-app --features overlay` pass; `bash -n`, `rpmspec -P`, `systemd-analyze verify`, and `git diff --check` pass.
- Existing daemon path remains reliable: bounded versioned IPC, dual-provider Recording pipeline, validated Transcript decision, portal-mediated Delivery with clipboard preservation, and graphical-session-owned `voisu.service`.

## Architecture map
- Domain, IPC, Transcript decision, diagnostics -> `crates/voisu-core/src/lib.rs`
- Fedora capture/provider/clipboard/portal/libei adapters -> `crates/voisu-app/src/system.rs`
- Daemon + optional Overlay user-service lifecycle -> `crates/voisu-app/src/service.rs`
- Public CLI -> `crates/voisu-app/src/bin/voisu.rs`
- GTK Overlay observer/runtime -> `crates/voisu-app/src/bin/voisu-overlay.rs`
- Overlay presentation + restart policy -> `crates/voisu-app/src/overlay.rs`, `crates/voisu-app/src/feedback.rs`
- RPM units/spec/build/smoke -> `packaging/`
- Approved design/plan -> `docs/superpowers/specs/2026-07-17-overlay-login-start-design.md`, `docs/superpowers/plans/2026-07-17-overlay-login-start.md`
- Fedora procedure/evidence -> `docs/packaging-fedora.md`, `docs/release-evidence.md`

## Stack & run
- Stack: Rust 2024 + Tokio + serde + zbus 5 + GTK4 (opt-in) + runtime libei · Run: `cargo run -p voisu-app --bin voisu-daemon` · Test: `cargo test --workspace`

## Key decisions (top 3–5)
- Overlay presentation is observer-only and disposable; daemon lifecycle, Recording, Transcript production, and Delivery never depend on it.
- Keep GTK4 + gtk4-layer-shell for the real KWin layer-shell surface; do not migrate to Electron/XWayland hacks.
- Start the optional Overlay through its own graphical-session user unit; integrate setup into existing `voisu service install|uninstall` as non-fatal best-effort behavior.
- Trust effective systemd state, not only an on-disk packaged filename; user-owned Overlay shadows or command overrides are not managed automatically.
- Portals are the normal Fedora path for Trigger Key and direct Delivery; no raw input devices or `uinput`.

## Gotchas
- Use `CONTEXT.md` terms exactly; ordinary synonyms are intentionally banned.
- Default workspace builds are GTK-free; compile the optional Overlay with `cargo check -p voisu-app --features overlay`.
- `packaging/build-rpm.sh` refuses a dirty checkout and binds artifacts to the checked-out commit; no exact RPM rebuild until the tested changes are committed.
- This sandbox denies Unix-domain/private D-Bus binds and cannot perform interactive sudo or a real graphical-login observation. Raja runs host commands in Konsole with `|& tee /tmp/...log`.
- `--report-backend` proves backend selection, not KWin acceptance; only a visible capsule proves the mapped layer-shell surface.
- `rustfmt` and `clippy` are unavailable.
