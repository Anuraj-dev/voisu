# Overlay Login Start Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship and enable an optional systemd user service that runs `voisu-overlay --supervise` immediately and at Fedora KDE/Wayland graphical login without coupling the daemon lifecycle to the Overlay.

**Architecture:** The optional RPM subpackage owns a separate `voisu-overlay.service` attached to `graphical-session.target`. The existing `voisu service install|uninstall` public interface detects only a trusted packaged Overlay unit and manages it best-effort; required daemon service operations retain their existing behavior and result authority.

**Tech Stack:** Rust 2024, Cargo integration tests, systemd user units, Fedora RPM spec/macros, Bash packaging scripts, GTK4 + gtk4-layer-shell behind the existing `overlay` feature.

## Global Constraints

- The Overlay remains a separate, disposable, observer-only process.
- The daemon never spawns, signals, waits on, or depends on the Overlay.
- `voisu service start|stop|restart` manage only `voisu.service`.
- Overlay management failure must never fail daemon installation or uninstallation.
- The base workspace and base RPM remain GTK-free; Overlay compilation remains opt-in through `--features overlay`.
- Use `CONTEXT.md` terms exactly: Overlay, Recording, Transcript, Delivery, Provider Deadline, Quality Failure, Trigger Key.
- Portals remain the normal Fedora path; do not add raw input-device or `uinput` access.
- Work in vertical RED → GREEN → REFACTOR cycles through public interfaces.
- Do not run socket/systemd/live acceptance inside the managed sandbox; Raja runs host commands in Konsole with `|& tee /tmp/...log`.
- `rustfmt` and `clippy` are unavailable.
- Do not commit unless Raja explicitly requests a commit.

## File Structure

- Create `packaging/voisu-overlay.service`: independent graphical-session-owned Overlay observer service.
- Modify `packaging/voisu.spec`: install and own the unit in the optional subpackage and add Overlay-scoped systemd-user scriptlets.
- Modify `packaging/build-rpm.sh`: bind the new unit into the exact-commit source-archive contract.
- Modify `crates/voisu-app/src/service.rs`: trusted packaged-unit detection, best-effort Overlay enable/disable, and report composition.
- Modify `crates/voisu-app/tests/service_cli.rs`: public CLI behavior using the fake `systemctl` boundary.
- Modify `docs/packaging-fedora.md`: optional Overlay setup, upgrade, login-start, and removal procedure.
- Modify `docs/release-evidence.md`: automated proof and host-pending acceptance rows.
- Modify `docs/decisions.md`: append the systemd-user lifecycle decision.

---

### Task 1: Enable the packaged Overlay during service installation

**Files:**
- Modify: `crates/voisu-app/tests/service_cli.rs:10-265,319-390`
- Modify: `crates/voisu-app/src/service.rs:16-147,590-622,757-810`

**Interfaces:**
- Consumes: public CLI `voisu service install`; existing `systemctl(arguments: &[&str])`; existing `packaged_unit_dirs()` test seam.
- Produces: `const OVERLAY_UNIT_NAME: &str`; `OptionalOverlayAction`; `manage_optional_overlay`; `append_optional_overlay_report`; successful daemon install followed by best-effort `enable --now voisu-overlay.service` when the trusted packaged unit exists.

- [ ] **Step 1: Extend the public CLI fixture with a packaged Overlay unit**

Add the fixture method:

```rust
fn packaged_overlay_unit_file(&self) -> PathBuf {
    self.packaged_unit_dir.join("voisu-overlay.service")
}

fn install_packaged_overlay_unit(&self) {
    fs::write(
        self.packaged_overlay_unit_file(),
        "[Service]\nExecStart=/usr/bin/voisu-overlay --supervise\n",
    )
    .unwrap();
}
```

Extend the fake `systemctl` immediately after it logs argv so a test can fail only Overlay management:

```sh
fail_unit=${FAKE_SYSTEMCTL_FAIL_UNIT:-}
last=
for argument in "$@"; do last=$argument; done
if test -n "$fail_unit" && test "$last" = "$fail_unit"; then
  exit 1
fi
```

- [ ] **Step 2: Write the failing install behavior test**

```rust
#[test]
fn packaged_install_enables_and_starts_the_optional_overlay_service() {
    let fixture = ServiceFixture::new(Path::new(env!("CARGO_BIN_EXE_voisu-daemon")));
    fixture.install_packaged_unit();
    fixture.install_packaged_overlay_unit();

    let installed = fixture.run(&["service", "install"]);

    assert!(installed.status.success(), "{}", stderr(&installed));
    assert!(stdout(&installed).contains("optional Overlay service enabled and started"));
    let calls = fs::read_to_string(&fixture.systemctl_log).unwrap();
    assert!(calls.contains("--user enable voisu.service"));
    assert!(calls.contains("--user enable --now voisu-overlay.service"));
}
```

- [ ] **Step 3: Run the targeted test and verify RED**

Run:

```bash
cargo test -p voisu-app --test service_cli packaged_install_enables_and_starts_the_optional_overlay_service -- --exact
```

Expected: FAIL because no Overlay systemctl call or report exists.

- [ ] **Step 4: Add minimal best-effort Overlay management**

Add near `UNIT_NAME`:

```rust
const OVERLAY_UNIT_NAME: &str = "voisu-overlay.service";
```

Add private lifecycle helpers:

```rust
#[derive(Clone, Copy)]
enum OptionalOverlayAction {
    Enable,
    Disable,
}

fn manage_optional_overlay(action: OptionalOverlayAction) -> Option<String> {
    let present = packaged_unit_dirs().into_iter().any(|directory| {
        let path = directory.join(OVERLAY_UNIT_NAME);
        fs::symlink_metadata(path)
            .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
    });
    if !present {
        return None;
    }

    let (arguments, success_message, failure_prefix): (&[&str], &str, &str) = match action {
        OptionalOverlayAction::Enable => (
            &["enable", "--now", OVERLAY_UNIT_NAME],
            "optional Overlay service enabled and started",
            "optional Overlay service was not enabled",
        ),
        OptionalOverlayAction::Disable => (
            &["disable", "--now", OVERLAY_UNIT_NAME],
            "optional Overlay service disabled and stopped",
            "optional Overlay service was not disabled",
        ),
    };

    Some(match systemctl_required(arguments) {
        Ok(()) => success_message.to_owned(),
        Err(error) => format!("warning: {failure_prefix}: {error}"),
    })
}

fn append_optional_overlay_report(
    mut report: UserServiceReport,
    overlay_message: Option<String>,
) -> UserServiceReport {
    if let Some(message) = overlay_message {
        report.message.push_str("; ");
        report.message.push_str(&message);
    }
    report
}
```

Change only the `Install` dispatch in `manage_user_service`:

```rust
UserServiceAction::Install => {
    let report = install()?;
    Ok(append_optional_overlay_report(
        report,
        manage_optional_overlay(OptionalOverlayAction::Enable),
    ))
}
```

Leave `Start`, `Stop`, and `Restart` untouched.

- [ ] **Step 5: Run the targeted test and verify GREEN**

Run the command from Step 3.

Expected: PASS.

- [ ] **Step 6: Write the failing non-fatal install test**

```rust
#[test]
fn overlay_enable_failure_does_not_fail_daemon_service_install() {
    let fixture = ServiceFixture::new(Path::new(env!("CARGO_BIN_EXE_voisu-daemon")));
    fixture.install_packaged_unit();
    fixture.install_packaged_overlay_unit();

    let installed = fixture
        .command(&["service", "install"])
        .env("FAKE_SYSTEMCTL_FAIL_UNIT", "voisu-overlay.service")
        .output()
        .unwrap();

    assert!(installed.status.success(), "{}", stderr(&installed));
    assert!(stdout(&installed).contains("warning: optional Overlay service was not enabled"));
    let calls = fs::read_to_string(&fixture.systemctl_log).unwrap();
    assert!(calls.contains("--user enable voisu.service"));
    assert!(calls.contains("--user enable --now voisu-overlay.service"));
}
```

- [ ] **Step 7: Run the non-fatal test and verify GREEN without extra production changes**

Run:

```bash
cargo test -p voisu-app --test service_cli overlay_enable_failure_does_not_fail_daemon_service_install -- --exact
```

Expected: PASS. If it fails, adjust only report composition or error capture; do not weaken required daemon systemctl handling.

- [ ] **Step 8: Verify the absent-unit regression case**

Run:

```bash
cargo test -p voisu-app --test service_cli install_is_idempotent_atomic_and_free_of_stale_session_or_checkout_values -- --exact
cargo test -p voisu-app --test service_cli packaged_install_migrates_a_stale_user_service_without_shadowing_the_package -- --exact
```

Expected: both PASS, with no Overlay systemctl call unless the fixture created `voisu-overlay.service`.

---

### Task 2: Disable the packaged Overlay independently during uninstall

**Files:**
- Modify: `crates/voisu-app/tests/service_cli.rs:849-882`
- Modify: `crates/voisu-app/src/service.rs:72-81`

**Interfaces:**
- Consumes: `manage_optional_overlay(OptionalOverlayAction::Disable)` from Task 1; public CLI `voisu service uninstall`.
- Produces: best-effort Overlay disable/stop attempted before required daemon uninstall; successful daemon result remains authoritative.

- [ ] **Step 1: Write the failing uninstall behavior test**

```rust
#[test]
fn packaged_uninstall_disables_and_stops_the_optional_overlay_service() {
    let fixture = ServiceFixture::new(Path::new(env!("CARGO_BIN_EXE_voisu-daemon")));
    fixture.install_packaged_unit();
    fixture.install_packaged_overlay_unit();

    let removed = fixture.run(&["service", "uninstall"]);

    assert!(removed.status.success(), "{}", stderr(&removed));
    assert!(stdout(&removed).contains("optional Overlay service disabled and stopped"));
    let calls = fs::read_to_string(&fixture.systemctl_log).unwrap();
    assert!(calls.contains("--user disable --now voisu-overlay.service"));
    assert!(calls.contains("--user disable --now voisu.service"));
}
```

- [ ] **Step 2: Run the targeted test and verify RED**

Run:

```bash
cargo test -p voisu-app --test service_cli packaged_uninstall_disables_and_stops_the_optional_overlay_service -- --exact
```

Expected: FAIL because uninstall does not manage the Overlay.

- [ ] **Step 3: Implement minimal independent uninstall sequencing**

Change only the `Uninstall` dispatch:

```rust
UserServiceAction::Uninstall => {
    let overlay_message = manage_optional_overlay(OptionalOverlayAction::Disable);
    let report = uninstall()?;
    Ok(append_optional_overlay_report(report, overlay_message))
}
```

This intentionally attempts Overlay cleanup first. If required daemon uninstall fails, its error remains the command result.

- [ ] **Step 4: Run the targeted test and verify GREEN**

Run the command from Step 2.

Expected: PASS.

- [ ] **Step 5: Write the failing non-fatal uninstall test**

```rust
#[test]
fn overlay_disable_failure_does_not_fail_daemon_service_uninstall() {
    let fixture = ServiceFixture::new(Path::new(env!("CARGO_BIN_EXE_voisu-daemon")));
    fixture.install_packaged_unit();
    fixture.install_packaged_overlay_unit();

    let removed = fixture
        .command(&["service", "uninstall"])
        .env("FAKE_SYSTEMCTL_FAIL_UNIT", "voisu-overlay.service")
        .output()
        .unwrap();

    assert!(removed.status.success(), "{}", stderr(&removed));
    assert!(stdout(&removed).contains("warning: optional Overlay service was not disabled"));
    let calls = fs::read_to_string(&fixture.systemctl_log).unwrap();
    assert!(calls.contains("--user disable --now voisu-overlay.service"));
    assert!(calls.contains("--user disable --now voisu.service"));
}
```

- [ ] **Step 6: Run the non-fatal test and verify GREEN**

Run:

```bash
cargo test -p voisu-app --test service_cli overlay_disable_failure_does_not_fail_daemon_service_uninstall -- --exact
```

Expected: PASS.

- [ ] **Step 7: Run the complete service CLI suite**

Run:

```bash
cargo test -p voisu-app --test service_cli
```

Expected: all service lifecycle tests PASS. The existing daemon-only install and uninstall message assertions remain valid because absent Overlay units do not modify reports.

- [ ] **Step 8: Refactor only if duplication remains**

Keep Overlay behavior behind the two private helpers. Do not add a public `overlay` CLI verb, modify daemon IPC, or route `service start|stop|restart` through Overlay management. Re-run the complete `service_cli` suite after any cleanup.

---

### Task 3: Ship the Overlay systemd user unit in the optional RPM

**Files:**
- Create: `packaging/voisu-overlay.service`
- Modify: `packaging/voisu.spec:55-108`
- Modify: `packaging/build-rpm.sh:31-39`

**Interfaces:**
- Consumes: `/usr/bin/voisu-overlay --supervise`; Fedora `%{_userunitdir}`; systemd RPM user macros.
- Produces: optional RPM owns `/usr/bin/voisu-overlay` and `/usr/lib/systemd/user/voisu-overlay.service` with graphical-session login enablement.

- [ ] **Step 1: Record the packaging contract before creating the unit**

Add to `packaging/build-rpm.sh` after the existing daemon-unit archive assertion:

```bash
grep -qx "voisu-${version}/packaging/voisu-overlay.service" "$topdir/source-archive.list"
```

Verify the asserted file is currently absent:

```bash
test ! -e packaging/voisu-overlay.service
```

Expected: command succeeds, establishing RED for the new archive contract if the full packaging script were run from a committed tree.

- [ ] **Step 2: Create the approved unit**

Create `packaging/voisu-overlay.service` exactly as:

```ini
[Unit]
Description=Voisu overlay observer
PartOf=graphical-session.target
After=voisu.service

[Service]
Type=simple
ExecStart=/usr/bin/voisu-overlay --supervise
Restart=on-failure

[Install]
WantedBy=graphical-session.target
```

- [ ] **Step 3: Install and own the unit in the Overlay subpackage**

In `%package overlay`, add the same systemd runtime macro dependency pattern used by the base package:

```spec
%{?systemd_requires}
```

In `%install`, add:

```spec
install -D -m 0644 packaging/voisu-overlay.service %{buildroot}%{_userunitdir}/voisu-overlay.service
```

Add subpackage-scoped scriptlets without changing the base package scriptlets:

```spec
%post overlay
%systemd_user_post voisu-overlay.service

%preun overlay
%systemd_user_preun voisu-overlay.service

%postun overlay
%systemd_user_postun voisu-overlay.service
```

Add to `%files overlay`:

```spec
%{_userunitdir}/voisu-overlay.service
```

- [ ] **Step 4: Verify the static packaging contract**

Run:

```bash
grep -qx 'ExecStart=/usr/bin/voisu-overlay --supervise' packaging/voisu-overlay.service
grep -qx 'PartOf=graphical-session.target' packaging/voisu-overlay.service
grep -qx 'After=voisu.service' packaging/voisu-overlay.service
grep -qx 'WantedBy=graphical-session.target' packaging/voisu-overlay.service
rg -n '%(post|preun|postun) overlay|systemd_user_(post|preun|postun) voisu-overlay.service|_userunitdir}/voisu-overlay.service' packaging/voisu.spec
```

Expected: every approved lifecycle line and all three subpackage scriptlets are present. Confirm the unit contains no `Wants=voisu.service` or `Requires=voisu.service`:

```bash
! grep -Eq '^(Wants|Requires)=voisu\.service$' packaging/voisu-overlay.service
```

Expected: success.

- [ ] **Step 5: Verify the default and opt-in build boundaries**

Run:

```bash
cargo check --workspace
cargo check -p voisu-app --features overlay
```

Expected: both PASS; the first remains GTK-free and the second compiles the optional Overlay.

The full `packaging/build-rpm.sh` intentionally rejects a dirty checkout. Run it only after Raja requests a commit or use the documented host rebuild against the eventual tested commit.

---

### Task 4: Document setup, evidence, and the load-bearing packaging choice

**Files:**
- Modify: `docs/packaging-fedora.md:1-176`
- Modify: `docs/release-evidence.md:32-63`
- Modify: `docs/decisions.md` at end of file

**Interfaces:**
- Consumes: the implemented `voisu service install|uninstall` behavior and packaged unit paths.
- Produces: exact operator procedure and honest separation between repository proof and host-observed login/rendering evidence.

- [ ] **Step 1: Update Fedora installation and login-start instructions**

Document that after installing the optional subpackage, the desktop user must run or rerun:

```sh
voisu service install
```

Document verification:

```sh
systemctl --user is-enabled voisu-overlay.service
systemctl --user is-active voisu-overlay.service
pgrep -a -x voisu-overlay
```

State explicitly that Idle is hidden by design and that `service start|stop|restart` manage only the daemon.

- [ ] **Step 2: Update upgrade and removal instructions**

Document rerunning `voisu service install` after an upgrade or after adding the optional Overlay subpackage. Before RPM removal, retain:

```sh
voisu service uninstall
sudo dnf remove voisu-overlay voisu
systemctl --user daemon-reload
```

Explain that uninstall best-effort disables the optional Overlay service independently and never makes daemon uninstall depend on it.

- [ ] **Step 3: Update release evidence honestly**

Add the new `service_cli` test names to the Overlay isolation and upgrade/removal repository-proof cells. Add package-unit ownership and unit-contract proof. Keep real graphical-login start, KWin surface acceptance, phase rendering, and kill-mid-Recording behavior marked PENDING until Raja observes them on the live host.

- [ ] **Step 4: Append the decision**

Append:

```markdown
## 2026-07-17 — Start the optional Overlay through its own graphical-session user unit
**Why:** The Overlay RPM previously shipped only the healthy binary, so no login path launched `voisu-overlay --supervise`. The optional subpackage now owns an independent `graphical-session.target` user unit, while `voisu service install|uninstall` manages it only when present and treats every Overlay failure as non-fatal. `After=voisu.service` provides ordering without `Wants=` or `Requires=`; daemon start, Recording, Transcript production, and Delivery never depend on presentation. A separate CLI verb was rejected as unnecessary setup friction, and XDG autostart was rejected because it diverges from the existing observable systemd-user lifecycle.
```

- [ ] **Step 5: Check terminology and documentation diff**

Run:

```bash
git diff --check
git diff -- docs/packaging-fedora.md docs/release-evidence.md docs/decisions.md
```

Expected: no whitespace errors; all references use Overlay, Recording, Transcript, and Delivery consistently.

---

### Task 5: Automated verification and live-host handoff

**Files:**
- Verify all modified files.
- Update `docs/release-evidence.md` only after host observations occur.

**Interfaces:**
- Consumes: all previous tasks.
- Produces: green repository suite plus exact evidence commands for the one acceptance layer the sandbox cannot observe.

- [ ] **Step 1: Run targeted and full automated tests**

Run:

```bash
cargo test -p voisu-app --test service_cli
cargo test --workspace
cargo check -p voisu-app --features overlay
git diff --check
```

Expected: all tests and checks PASS. If the full suite fails only because the managed sandbox denies Unix-domain or private D-Bus binds, retain exact output and hand those cases to the host instead of weakening tests.

- [ ] **Step 2: Review the complete diff against the architecture boundary**

Run:

```bash
git diff --stat
git diff -- packaging/voisu-overlay.service packaging/voisu.spec packaging/build-rpm.sh crates/voisu-app/src/service.rs crates/voisu-app/tests/service_cli.rs docs/packaging-fedora.md docs/release-evidence.md docs/decisions.md
rg -n 'voisu-overlay|OVERLAY_UNIT_NAME|OptionalOverlay' crates/voisu-app/src crates/voisu-app/tests packaging
```

Verify manually:

- No daemon binary or actor code spawns, signals, waits on, or imports Overlay lifecycle code.
- No daemon unit has `Wants=` or `Requires=` on the Overlay.
- `service start|stop|restart` remain daemon-only.
- Base Cargo commands do not enable the `overlay` feature.
- Overlay systemctl errors are report warnings, not returned daemon errors.

- [ ] **Step 3: Prepare the host RPM rebuild command**

Because packaging requires a clean committed tree and sudo cannot run in the sandbox, after Raja authorizes a commit, rebuild in Konsole with:

```sh
cd /home/raja/Anuraj-Dev/voisu
packaging/build-rpm.sh |& tee /tmp/voisu-overlay-rpmbuild.log
rpmlint dist/rpm/*.rpm |& tee /tmp/voisu-overlay-rpmlint.log
rpm -qpl dist/rpm/voisu-overlay-0.1.0-*.rpm |& tee /tmp/voisu-overlay-manifest.log
```

Expected manifest includes both `/usr/bin/voisu-overlay` and `/usr/lib/systemd/user/voisu-overlay.service`.

- [ ] **Step 4: Prepare install/enable/login evidence commands**

Raja runs in Konsole:

```sh
cd /home/raja/Anuraj-Dev/voisu
sudo dnf install ./dist/rpm/voisu-0.1.0-*.rpm ./dist/rpm/voisu-overlay-0.1.0-*.rpm |& tee /tmp/voisu-overlay-install.log
voisu service install |& tee /tmp/voisu-overlay-enable.log
systemctl --user status voisu.service voisu-overlay.service --no-pager |& tee /tmp/voisu-overlay-status.log
systemctl --user is-enabled voisu-overlay.service |& tee -a /tmp/voisu-overlay-status.log
pgrep -a -x voisu-overlay |& tee -a /tmp/voisu-overlay-status.log
voisu-overlay --report-backend |& tee /tmp/voisu-overlay-backend.log
```

Then log out and back in and rerun the status, `pgrep`, and backend commands.

- [ ] **Step 5: Perform visual and independence acceptance with Raja observing**

Start a Recording with `voisu toggle`; visually confirm the green Recording capsule, Processing, Success/Failure, and hidden Idle. During another Recording, identify and kill an Overlay process, then capture:

```sh
systemctl --user is-active voisu.service |& tee /tmp/voisu-overlay-isolation.log
pgrep -a -x voisu-overlay |& tee -a /tmp/voisu-overlay-isolation.log
voisu service status |& tee -a /tmp/voisu-overlay-isolation.log
journalctl --user -u voisu-overlay.service -u voisu.service --since '-5 min' --no-pager |& tee -a /tmp/voisu-overlay-isolation.log
```

Expected: `voisu.service` remains active, the Recording reaches Transcript/Delivery normally, and Overlay supervision respawns presentation without daemon intervention.

- [ ] **Step 6: Verify clean uninstall**

Raja runs:

```sh
voisu service uninstall |& tee /tmp/voisu-overlay-uninstall.log
systemctl --user is-enabled voisu.service voisu-overlay.service |& tee -a /tmp/voisu-overlay-uninstall.log
systemctl --user is-active voisu.service voisu-overlay.service |& tee -a /tmp/voisu-overlay-uninstall.log
sudo dnf remove voisu-overlay voisu |& tee -a /tmp/voisu-overlay-uninstall.log
systemctl --user daemon-reload
```

Expected: both services are disabled/stopped before package removal; RPM-owned binaries and units disappear; supported user data remains.

- [ ] **Step 7: Update evidence only from observed results**

After reading the `/tmp/voisu-overlay-*.log` files and Raja's visual confirmation, change the corresponding `docs/release-evidence.md` rows from PENDING to PROVEN with the date and exact observations. Do not infer KWin acceptance from `--report-backend`; only the visible capsule proves compositor acceptance.

- [ ] **Step 8: Run the repository checkpoint workflow**

Invoke `/checkpoint` to update `docs/STATE.md` and the session log with implementation state, automated results, and any host checks still pending.
