# Fedora release evidence

This is the evidence sheet for the Ticket 13 release candidate. The tested
commit and RPM filenames are filled by the host release run. Headless tests
prove contracts; the Fedora KDE Plasma / Wayland checks below remain explicit
host work where compositor, portal, PipeWire, Secret Service, or cloud
credentials are required.

## Tested artifact

| Item | Evidence |
|---|---|
| Git commit | PROVEN — current installed Overlay-start artifact is local `main` commit `67aa3b6ea86023791bbd0e12999b86e7071d4fd3`; the earlier full live-smoke artifact was `73f57274734cdcbd188b407e277e291546b0f80e` |
| RPM Release | PROVEN — current base + Overlay RPMs are `0.1.0-1.git67aa3b6ea86023791bbd0e12999b86e7071d4fd3.fc43`; DNF installed them with `--allow-downgrade` because commit-qualified Releases are lexical rather than chronological |
| Live desktop smoke | PROVEN 2026-07-17 at `73f5727` — `VOISU_FEDORA_LIVE_SMOKE=1 packaging/fedora-smoke.sh` passed on Fedora KDE Wayland: all six doctor capabilities PASS, a real 8-second Recording reached the cloud providers with real credentials, and the Transcript was submitted through the compositor and preserved on the clipboard; cleanup restored the pre-smoke state. The `67aa3b6` Overlay artifact additionally passed immediate live install/start and visible-capsule checks, but not the full smoke/logout-login sequence |
| Cargo lockfile | PROVEN — `packaging/build-rpm.sh` produced the exact-commit source archive and deterministic vendored `Source1` on the Fedora host; the offline `--locked` build consumed them |
| Standard suite | PROVEN — the exact RPM `%check` ran the full release test suite inside `rpmbuild` on the Fedora host with 0 failures |
| Overlay check | PROVEN — the exact RPM overlay release build and `cargo check --features overlay` ran inside `rpmbuild` on the Fedora host |
| rpmlint | PROVEN — 0 substantive findings after polish; remaining warnings are cosmetic (no man pages, changelog carries the static base version while Release embeds the commit) |

The host `rpmbuild` gate first ran at commit `674b93e`; the final release
artifacts (base, overlay, debuginfo RPMs and the SRPM in `dist/rpm/`) were
rebuilt at `73f5727`. The first live smoke attempts surfaced two real
desktop-only defects, both fixed and covered by tests before the passing run:
`wl-copy`'s clipboard-serving child was misread as a timeout (`f876425`), and
real `pw-record`'s silent nonzero exit on SIGINT failed every graceful stop
(`73f5727`). Categories below not exercised by the live smoke (portal
revocation, upgrade paths, explicit fallback scenarios) remain PENDING.

The Overlay login-start fix was subsequently built and installed from exact
commit `67aa3b6`. `voisu service install` reported that the optional Overlay was
enabled and started; systemd showed both packaged units enabled and active, with
`/usr/bin/voisu-overlay --supervise` plus its child, and the backend reported
`layer-shell` without degradation. Raja visibly observed the capsule during a
real Recording. A logout/login start and kill-mid-Recording isolation check were
not performed, so those narrower claims remain PENDING.

Recent real histories also expose a release-blocking product-quality issue:
transcription accuracy is too low for daily use. Groq did participate in each
examined call (with source Transcripts and completion timings around 400–544 ms)
and was generally more accurate than Deepgram. The reconciled final Transcript
often reintroduced errors from the weaker Deepgram source. Accuracy research
should therefore begin with reconciliation/source-selection behavior rather
than assuming Groq was absent.

## Evidence categories

| Category | Repository proof | Fedora host evidence |
|---|---|---|
| Process ownership | `service_manager_guards_its_systemctl_child_with_parent_death_signal`, `managed_service_lifecycle_reports_systemd_ownership_and_daemon_ipc`, `a_manual_daemon_is_reported_and_service_start_does_not_create_a_crash_loop`, `packaged_install_migrates_a_stale_user_service_without_shadowing_the_package` | PARTIAL PROVEN 2026-07-17 — after installing `67aa3b6`, both units loaded from `/usr/lib/systemd/user`, were enabled/active, and had the expected daemon and Overlay supervisor/child PIDs. Login, upgrade, and removal observations remain PENDING |
| Portal behavior | `production_portal_rotates_persistent_permission_and_connects_libei`, `portal_denial_and_unavailable_input_capability_fall_back_explicitly`, `permission_denial_is_terminal_for_the_daemon_lifetime` | PENDING — record Global Shortcuts approval, Remote Desktop approval, restore-token reuse, revocation, and compositor behavior on KDE Wayland |
| Provider fallback | `deepgram_source_transcript_delivers_when_groq_fails`, `provider_disconnect_malformed_response_and_quota_error_fall_back_and_recover`, `provider_deadline_returns_the_valid_source_already_available` | PENDING — run a real Recording with both providers, then repeat with one provider unavailable and retain the resulting clipboard Transcript |
| Latency spans and bounded work | `stop_response_budget_strictly_exceeds_all_daemon_processing_deadlines`, `status_is_responsive_and_processing_is_observable_during_provider_work`, `provider_deadline_awaits_the_losing_stream_abort_before_returning` | PENDING — export one diagnostic record and attach the correlation ID, phase timings, Provider Deadline, Delivery timing, and cleanup timing |
| Log redaction | `boundary_errors_separate_redacted_public_text_from_local_diagnostics`, `export_scrubs_secret_values_from_transcripts_and_reasons`, `capture_finalization_failure_is_redacted_and_the_next_recording_succeeds`, `non_loopback_plaintext_groq_endpoint_is_rejected_without_disclosing_secrets` | PENDING — inspect `journalctl --user -u voisu.service` and an export for API-key, authorization, token, and endpoint-userinfo absence |
| Overlay isolation | `red_bounded_overlay_restarts_stop_without_a_daemon_control_path`, `next_recording_clears_terminal_feedback_and_is_not_lifecycle_coupled`, `missing_display_uses_a_persistent_journal_observer_instead_of_a_noop_notification`, `a_realized_surface_keeps_its_backend_and_only_genuine_absence_falls_back`, `packaged_install_enables_and_starts_the_optional_overlay_service`, `packaged_overlay_is_not_managed_when_a_user_unit_shadows_it`, `overlay_enable_failure_does_not_fail_daemon_service_install` | PARTIAL PROVEN 2026-07-17 — optional RPM installed, packaged unit enabled/active, supervisor + child running, layer-shell backend healthy, and Raja observed the real KWin capsule. Real logout/login startup and kill-mid-Recording independence remain PENDING |
| Package contents and dependencies | `packaging/fedora-smoke.sh` checks RPM file ownership and dependency declarations; `packaging/voisu.spec` keeps the base GTK-free while the optional subpackage owns `/usr/bin/voisu-overlay` and `/usr/lib/systemd/user/voisu-overlay.service`; `packaging/build-rpm.sh` binds both units into the exact-commit source archive | PROVEN for `67aa3b6` — exact-commit RPM rebuild succeeded; Overlay RPM contains `/usr/bin/voisu-overlay` and `/usr/lib/systemd/user/voisu-overlay.service`; scriptlets reference the Overlay unit; `rpmlint` reported 0 errors |
| Upgrade and removal | `install_is_idempotent_atomic_and_free_of_stale_session_or_checkout_values`, `uninstall_disables_service_removes_installed_files_and_leaves_no_runtime_socket`, `packaged_uninstall_disables_only_the_service_and_preserves_packaged_unit_and_user_data`, `packaged_uninstall_disables_and_stops_the_optional_overlay_service`, `overlay_disable_failure_does_not_fail_daemon_service_uninstall`, `effective_execstart_override_binary_missing_falls_back_to_ticket_09_user_data`, `effective_execstart_override_selects_packaged_when_the_static_daemon_is_absent`, `an_xdg_user_unit_with_no_packaged_file_is_never_treated_as_packaged`, `packaged_unit_with_a_non_loaded_load_state_falls_back_to_ticket_09_user_data`, `packaged_unit_with_a_missing_later_execstart_command_falls_back_to_ticket_09` | PENDING — as the desktop user run `voisu service uninstall` before `dnf remove`; then verify credentials, supported state, and diagnostics survive upgrade/removal, both packaged binaries/units disappear, and both user units are disabled |

## Host run checklist

- [x] Fresh RPM install has only the declared runtime dependencies and both
      base binaries at `/usr/bin` (smoke harness, 2026-07-17).
- [x] `voisu doctor`, both credential setup commands, and both auth checks pass
      (all six doctor capabilities PASS; `auth set`/`auth verify` succeeded for
      groq and deepgram against the real APIs, 2026-07-17).
- [x] `voisu service install`, `voisu service status`, and the packaged
      `ExecStart` point at `/usr/bin/voisu-daemon` (smoke harness; login start
      remains to be observed across a real login).
- [ ] After installing `voisu-overlay` and rerunning `voisu service install`,
      `voisu-overlay --supervise` starts immediately and across a real graphical
      login; KWin renders Recording → Processing → Success/Failure and Idle is
      hidden; killing the Overlay during a Recording does not affect the daemon,
      Transcript production, or Delivery. Immediate start, packaged ownership,
      supervisor/child processes, layer-shell backend, and a visible Recording
      capsule are PROVEN at `67aa3b6`; logout/login and kill isolation remain.
- [x] A real Recording reaches the cloud providers and direct Delivery is
      observed in the focused application (Transcript submitted through the
      compositor and preserved on the clipboard, 2026-07-17).
- [ ] Clipboard preservation remains available when direct Delivery is denied,
      unavailable, or the optional `libei` capability is absent.
- [ ] Upgrade removes any old XDG user-data daemon/unit ownership while
      preserving credentials, supported state, and diagnostics.
- [ ] As the desktop user, `voisu service uninstall` runs before `dnf remove`;
      removal then disables the service and removes RPM artifacts while
      preserving user data; an explicit purge is tested separately, if approved.
- [x] `VOISU_FEDORA_LIVE_SMOKE=1 packaging/fedora-smoke.sh ...` passes
      (2026-07-17, against the exact `73f5727` RPM; cleanup restored the
      pre-smoke state).

APT/DEB packaging is not part of this evidence sheet or the Fedora release
candidate.
