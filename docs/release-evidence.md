# Fedora release evidence

This is the evidence sheet for the Ticket 13 release candidate. The tested
commit and RPM filenames are filled by the host release run. Headless tests
prove contracts; the Fedora KDE Plasma / Wayland checks below remain explicit
host work where compositor, portal, PipeWire, Secret Service, or cloud
credentials are required.

## Tested artifact

| Item | Evidence |
|---|---|
| Git commit | PENDING — record the full commit from `git rev-parse HEAD` |
| RPM Release | PENDING — record `rpm -qp --qf '%{NAME}-%{VERSION}-%{RELEASE}.%{ARCH}\n'` |
| Cargo lockfile | PROVEN by `packaging/build-rpm.sh` archive checks and `--locked` builds |
| Standard suite | PROVEN by `cargo test --locked --workspace` and the spec `%check` |
| Overlay check | PROVEN by `cargo check --locked -p voisu-app --features overlay` and the spec `%check` |

## Evidence categories

| Category | Repository proof | Fedora host evidence |
|---|---|---|
| Process ownership | `service_manager_guards_its_systemctl_child_with_parent_death_signal`, `managed_service_lifecycle_reports_systemd_ownership_and_daemon_ipc`, `a_manual_daemon_is_reported_and_service_start_does_not_create_a_crash_loop`, `packaged_install_migrates_a_stale_user_service_without_shadowing_the_package` | PENDING — capture `systemctl --user status voisu.service`, MainPID, `ExecStart`, and `voisu service status` after install, login, upgrade, and removal |
| Portal behavior | `production_portal_rotates_persistent_permission_and_connects_libei`, `portal_denial_and_unavailable_input_capability_fall_back_explicitly`, `permission_denial_is_terminal_for_the_daemon_lifetime` | PENDING — record Global Shortcuts approval, Remote Desktop approval, restore-token reuse, revocation, and compositor behavior on KDE Wayland |
| Provider fallback | `deepgram_source_transcript_delivers_when_groq_fails`, `provider_disconnect_malformed_response_and_quota_error_fall_back_and_recover`, `provider_deadline_returns_the_valid_source_already_available` | PENDING — run a real Recording with both providers, then repeat with one provider unavailable and retain the resulting clipboard Transcript |
| Latency spans and bounded work | `stop_response_budget_strictly_exceeds_all_daemon_processing_deadlines`, `status_is_responsive_and_processing_is_observable_during_provider_work`, `provider_deadline_awaits_the_losing_stream_abort_before_returning` | PENDING — export one diagnostic record and attach the correlation ID, phase timings, Provider Deadline, Delivery timing, and cleanup timing |
| Log redaction | `boundary_errors_separate_redacted_public_text_from_local_diagnostics`, `export_scrubs_secret_values_from_transcripts_and_reasons`, `capture_finalization_failure_is_redacted_and_the_next_recording_succeeds`, `non_loopback_plaintext_groq_endpoint_is_rejected_without_disclosing_secrets` | PENDING — inspect `journalctl --user -u voisu.service` and an export for API-key, authorization, token, and endpoint-userinfo absence |
| Overlay isolation | `red_bounded_overlay_restarts_stop_without_a_daemon_control_path`, `next_recording_clears_terminal_feedback_and_is_not_lifecycle_coupled`, `missing_display_uses_a_persistent_journal_observer_instead_of_a_noop_notification`, `a_realized_surface_keeps_its_backend_and_only_genuine_absence_falls_back` | PENDING — install `voisu-overlay` only when wanted, verify Overlay exit/restart never changes daemon ownership or interrupts a Recording, and capture the selected backend/degradation |
| Package contents and dependencies | `packaging/fedora-smoke.sh` checks RPM file ownership and dependency declarations; `packaging/voisu.spec` separates the GTK-free base from `voisu-overlay` | PENDING — run the smoke harness against the exact RPM and record `rpm -qpl`, `rpm -q --requires`, and `rpm -q --recommends` |
| Upgrade and removal | `install_is_idempotent_atomic_and_free_of_stale_session_or_checkout_values`, `uninstall_disables_service_removes_installed_files_and_leaves_no_runtime_socket`, `packaged_uninstall_disables_only_the_service_and_preserves_packaged_unit_and_user_data` | PENDING — verify credentials, supported state, and diagnostics survive upgrade/removal; verify packaged binaries/unit disappear and the user unit is disabled |

## Host run checklist

- [ ] Fresh RPM install has only the declared runtime dependencies and both
      base binaries at `/usr/bin`.
- [ ] `voisu doctor`, both credential setup commands, and both auth checks pass.
- [ ] `voisu service install`, login start, `voisu service status`, and the
      packaged `ExecStart` point at `/usr/bin/voisu-daemon`.
- [ ] A real Recording reaches both cloud providers and direct Delivery is
      observed in the focused application.
- [ ] Clipboard preservation remains available when direct Delivery is denied,
      unavailable, or the optional `libei` capability is absent.
- [ ] Upgrade removes any old XDG user-data daemon/unit ownership while
      preserving credentials, supported state, and diagnostics.
- [ ] Removal disables the service and removes RPM artifacts while preserving
      user data; an explicit purge is tested separately, if approved.
- [ ] `VOISU_FEDORA_LIVE_SMOKE=1 packaging/fedora-smoke.sh ...` passes.

APT/DEB packaging is not part of this evidence sheet or the Fedora release
candidate.
