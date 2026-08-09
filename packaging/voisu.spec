Name:           voisu
Version:        0.11.0
%{!?voisu_commit:%global voisu_commit unknown}
# Release is computed by the build scripts and baked in as %%global voisu_release
# (see packaging/rpm-lib.sh for the unified policy). ONE spec, all channels:
#   - pre-release builds (build-rpm.sh dev path AND COPR snapshots via
#     build-srpm.sh / make-srpm.sh): 0.<count>.<ct>.git<sha> — the leading 0.
#     keeps every snapshot below any tagged release, and the commit-count primary
#     key increases for any descendant commit (immune to committer clock skew).
#   - tagged releases: a plain integer N from the committed packaging/rpm-release.
# The fallback below is only reached if the spec is built raw without the scripts;
# its leading 0. keeps such an accidental build below every real release.
Release:        %{?voisu_release}%{!?voisu_release:0.0.gitunknown}%{?dist}
Summary:        Cloud-first Linux dictation for Fedora Wayland
# Voisu is MIT; the statically linked ring crate adds ISC (new code),
# Apache-2.0 and BSD-3-Clause (BoringSSL-derived code), plus MIT/Apache-2.0
# (once_cell polyfill) and Apache-2.0 (fiat). Ring's full upstream license tree
# ships in %%license under ring/.
License:        MIT AND Apache-2.0 AND ISC AND BSD-3-Clause
URL:            https://github.com/Anuraj-Dev/voisu
Source0:        %{name}-%{version}.tar.gz
Source1:        voisu-vendor-%{version}.tar.gz

BuildRequires:  cargo
BuildRequires:  rust
BuildRequires:  systemd-rpm-macros
BuildRequires:  dbus-daemon
BuildRequires:  python3
BuildRequires:  curl
# Fedora test subprocess ownership: dbus-daemon provides dbus-daemon,
# python3 provides /usr/bin/python3, and curl provides /usr/bin/curl.
# https://packages.fedoraproject.org/pkgs/dbus/dbus-daemon/
# https://packages.fedoraproject.org/pkgs/python3.14/python3/
# https://packages.fedoraproject.org/pkgs/curl/curl/
BuildRequires:  pkgconfig(gtk4)
BuildRequires:  pkgconfig(gtk4-layer-shell-0)
BuildRequires:  pkgconfig(xkbcommon)

# These are actual external boundaries in system.rs: wl-copy/wl-paste, pw-record,
# wpctl, curl, and secret-tool. Fedora ownership was verified against the
# package file lists: pipewire-utils ships pw-record
# (https://packages.fedoraproject.org/pkgs/pipewire/pipewire-utils/fedora-43-updates.html),
# wireplumber ships wpctl
# (https://packages.fedoraproject.org/pkgs/wireplumber/wireplumber/fedora-43-updates.html),
# and libsecret ships secret-tool
# (https://packages.fedoraproject.org/pkgs/libsecret/libsecret/fedora-43-updates.html).
# secret-tool is required as a FILE dependency: Voisu needs the binary, not the
# library, and `Requires: libsecret` trips rpmlint's explicit-lib-dependency.
# libei is dlopen()'d by SONAME and is therefore an optional runtime capability
# rather than a hard build/link dependency.
Requires:       wl-clipboard
Requires:       pipewire-utils
Requires:       wireplumber
Requires:       curl
Requires:       /usr/bin/secret-tool
Recommends:     libei%{?_isa}
%{?systemd_requires}

%description
Voisu is a cloud-first Linux dictation application for Fedora KDE Plasma on
Wayland. It keeps the daemon and CLI usable without GTK and uses desktop
portals for the Trigger Key and direct Delivery, with clipboard preservation
as the fallback.

The package is built from a Cargo.lock-pinned source archive of one exact git
commit. Pre-release builds carry that commit in their Release string; tagged
releases carry it as the %%global voisu_commit baked into the SRPM's spec.

%package overlay
Summary:        Optional GTK4 Voisu Overlay
Requires:       %{name}%{?_isa} = %{version}-%{release}
Requires:       gtk4%{?_isa}
Requires:       gtk4-layer-shell%{?_isa}
%{?systemd_requires}

%description overlay
Optional observer-only GTK4 Overlay feedback for Voisu. The base package is
GTK-free; installing this package adds the separate Overlay process.

%prep
%autosetup -n %{name}-%{version}
tar -xzf %{SOURCE1} -C ..
# Statically linked ring carries ISC, Apache-2.0, BSD-3-Clause and MIT texts; its
# full upstream license tree must ship with the RPM. Preserve ring's UPSTREAM
# names/paths (ring/... with the once_cell polyfill and fiat sub-paths) so the
# cross-references inside ring's own LICENSE manifest resolve. Source of truth is
# the vendored ring crate inside the Source1 vendor tarball.
_ringsrc=../voisu-vendor-%{version}/ring
mkdir -p ring/src/polyfill/once_cell ring/third_party/fiat
cp $_ringsrc/LICENSE                                ring/LICENSE
cp $_ringsrc/LICENSE-BoringSSL                       ring/LICENSE-BoringSSL
cp $_ringsrc/LICENSE-other-bits                      ring/LICENSE-other-bits
cp $_ringsrc/src/polyfill/once_cell/LICENSE-APACHE   ring/src/polyfill/once_cell/LICENSE-APACHE
cp $_ringsrc/src/polyfill/once_cell/LICENSE-MIT      ring/src/polyfill/once_cell/LICENSE-MIT
cp $_ringsrc/third_party/fiat/LICENSE                ring/third_party/fiat/LICENSE
mkdir -p .cargo
cat > .cargo/config.toml <<'EOF'
[source.crates-io]
replace-with = "vendored-sources"

[source.vendored-sources]
directory = "../voisu-vendor-%{version}"
EOF

%build
cargo build --offline --release --locked --workspace
cargo build --offline --release --locked -p voisu-app --features overlay --bin voisu-overlay

%check
# Constrained builders (mock/COPR) do not inherit the caller's environment, so
# export the tmpfs-quota workaround here: /var/tmp is real disk (default /tmp may
# be a size-capped tmpfs) and RUST_TEST_THREADS bounds the test processes that
# each spawn a dbus-daemon/python/curl subprocess. Documented repo gotcha.
export TMPDIR=/var/tmp
export RUST_TEST_THREADS=4
cargo test --offline --release --locked --workspace
cargo check --offline --release --locked -p voisu-app --features overlay

%install
install -D -m 0755 target/release/voisu %{buildroot}%{_bindir}/voisu
install -D -m 0755 target/release/voisu-daemon %{buildroot}%{_bindir}/voisu-daemon
install -D -m 0755 target/release/voisu-overlay %{buildroot}%{_bindir}/voisu-overlay
install -D -m 0644 packaging/voisu.service %{buildroot}%{_userunitdir}/voisu.service
install -D -m 0644 packaging/voisu-overlay.service %{buildroot}%{_userunitdir}/voisu-overlay.service
# Desktop entry makes a resolvable app_id (voisu) available to portal
# backends that support it; the stable session token is the primary fix.
install -D -m 0644 packaging/voisu.desktop %{buildroot}%{_datadir}/applications/voisu.desktop

%post
%systemd_user_post voisu.service

%preun
%systemd_user_preun voisu.service

%postun
%systemd_user_postun voisu.service

%post overlay
%systemd_user_post voisu-overlay.service

%preun overlay
%systemd_user_preun voisu-overlay.service

%postun overlay
%systemd_user_postun voisu-overlay.service

%files
# %%license copies each listed FILE into %%{_licensedir}/%%{name}/ by BASENAME,
# which would flatten ring's tree and collide the three files named LICENSE
# (voisu's own MIT, ring/LICENSE, ring/third_party/fiat/LICENSE). Mark the whole
# ring DIRECTORY instead: rpm installs it recursively, preserving the upstream
# paths so ring's LICENSE-manifest cross-references resolve.
%license LICENSE
%license ring
%doc README.md
%{_bindir}/voisu
%{_bindir}/voisu-daemon
%{_userunitdir}/voisu.service
%{_datadir}/applications/voisu.desktop

%files overlay
%{_bindir}/voisu-overlay
%{_userunitdir}/voisu-overlay.service

%changelog
* Sun Aug 09 2026 Voisu maintainers <voisu@example.invalid> - 0.11.0-1
- feat(config): add Writing Mode CLI and persisted Smart/Literal (#113) (#116)
- docs(research): approve Smart Writing implementation specification (#103) (#112)
- docs(research): approve Smart Writing edit safety (#111)
- docs(research): define final-only transform pipeline (#110)
- docs(research): approve Smart Writing behavior corpus (#109)
- docs(research): add Smart Writing approval corpus (#108)

* Sat Aug 08 2026 Voisu maintainers <voisu@example.invalid> - 0.10.3-1
- fix(reconciliation): migrate default model to Qwen (#107)
- docs(research): record Groq reconciliation benchmark (#106)
- test(service): capture lifecycle flake evidence (#104)

* Sat Aug 08 2026 Voisu maintainers <voisu@example.invalid> - 0.10.2-1
- fix(deps): update event-listener for RUSTSEC-2026-0221 (#105)

* Tue Jul 28 2026 Voisu maintainers <voisu@example.invalid> - 0.10.1-1
- fix(overlay): drop poll_tick's latch guards before the warning arm commits

* Tue Jul 28 2026 Voisu maintainers <voisu@example.invalid> - 0.10.0-1
- refactor(overlay): group the per-tick inputs instead of growing arg lists
- fix(overlay): settle an accepted limit warning instead of leaving it open
- fix(daemon): race the controlled chunk delay against the Deadline clock
- fix(overlay): commit a limit warning only once the notifier accepts it
- fix(daemon): make the controlled capture enforce the clock it reports
- fix(overlay): route both notification paths through the truthful body
- fix(overlay): say what is true, and say only one thing per notifier tick
- fix(overlay): key the limit-warning latch to the Recording it warned about
- fix(daemon): report headroom against the capture's own Deadline clock
- test(daemon): prove status carries the Recording headroom over the wire
- feat(overlay): warn the user before the Recording limit cuts them off
- feat(daemon): report the remaining Recording headroom on the observer path
- feat(overlay): derive the approaching-limit warning stages from the ceiling
- test(overlay): pin the approaching-limit warnings to the derived ceiling

* Tue Jul 28 2026 Voisu maintainers <voisu@example.invalid> - 0.9.0-1
- style(daemon): satisfy int_plus_one on the legacy frame assertion
- style(diagnostics): pin the count bound at compile time
- test(diagnostics): decouple the retention test from selection policy
- fix(diagnostics): never present unknown history as durable truth
- fix(diagnostics): harden history durability and compatibility
- fix(journal): carry truncated_by through the rebase onto v0.7.0
- fix(diagnostics): never let a diagnostics precondition block startup
- perf(diagnostics): append records instead of rewriting the ring
- fix(diagnostics): retain history in the durable state directory
- fix(cli): reinstate the diagnostic response ceiling
- fix(journal): stop a diagnostic forging a structured record
- fix(ipc): negotiate diagnostic paging per request
- test(service): follow protocol socket version
- test(ipc): follow protocol version constant
- fix(diagnostics): close review gaps
- test(diagnostics): harden observability regressions
- fix(service): try-restart optional overlay
- fix(diagnostics): format startup failure timings
- fix(diagnostics): align server write deadline
- fix(diagnostics): separate journal timing records

* Tue Jul 28 2026 Voisu maintainers <voisu@example.invalid> - 0.8.0-1
- test(core): pin the outro narrowing and the wordless invariant through the contraction arm
- refactor(core): delete the lexical-difference override; the Groq default always wins
- fix(core): a wordless source transcript is never delivered while a sibling heard words
- test(core): RED - a wordless source transcript is delivered over heard words
- fix(core): narrow the outro anchor back to final-sentence-start or text-end
- fix(core): catch unpunctuated outros and stop refusing all-stopword dictations
- fix(core): asymmetric misheard spans must be vouched for by a dictionary term
- fix(core): anchor hallucinated outros to the final sentence and true up two guard comments
- fix(core): refuse all-stopword repairs, record delivered contractions, honor the longer-source rule 
- fix(core): decide lexically different near-identical pairs by a single misheard-span rule
- style(core): wrap the dictionary-term lookup chain
- test(core): pin that a misheard dictionary term cannot smuggle out an adjacent negation
- fix(core): anchor the meta-reasoning trigger and restore a floor repair cannot lose a dictation to
- feat(core): widen near-identical selection to lexically different Source Transcripts
- fix(core): scope the contraction floor to the merge and stop the merge arbitrating its own rejection
- test(core): prevent formatting-driven padding wins
- fix(core): cap sentence boundary credit
- fix(core): normalize linguistic contractions
- test(core): enforce formatting comparator fixtures
- fix(core): filter contraction fallback sources

* Mon Jul 27 2026 Voisu maintainers <voisu@example.invalid> - 0.7.0-1
- style(capture): keep the one-line notice assertions as rustfmt writes them
- refactor(daemon): rename publish_trigger_outcome to publish_terminal_outcome
- test(capture): name what the byte-cap floor actually pins
- feat(daemon): tell the operator when a Recording maximum override is clamped
- test(daemon): restore the recovery headroom the deepgram pause consumed
- test(capture): order the deadline-retention test instead of racing it
- fix(daemon): log self-terminated Recordings origin-neutrally
- feat(history): mark truncated Recordings and name the cap that fired
- fix(capture): clamp the Recording maximum and pin deadline retention
- fix(capture): retain partial reads at byte cap
- test(capture): keep fatal cleanup regressions
- test(capture): align recoverable deadline regressions
- test(daemon): pass outcome publication intent
- test(capture): inject production read failure
- test(capture): exercise production cap drain
- fix(delivery): warn on truncated clipboard fallback
- fix(daemon): publish self-termination outcome
- fix(capture): unify configured recording ceilings
- fix(capture): retain audio at recording deadline
- test(daemon): retain self-termination outcome

* Thu Jul 23 2026 Voisu maintainers <voisu@example.invalid> - 0.6.0-1
- fix(overlay): capsule fills the window, skip idle interpolation, lock 44-bar math
- feat(overlay): 44-bar meter with taller drawable

* Thu Jul 23 2026 Voisu maintainers <voisu@example.invalid> - 0.5.0-1
- fix(overlay): keep accessible description reachable when the label is hidden
- docs(overlay): update phase_glyph doc to graphics-first behavior
- fix(overlay): Processing has no glyph, graphics-only capsule fills full width, dedupe fallback no-sp
- test(overlay): satisfy clippy manual_range_contains in falloff assertion
- feat(overlay): text-free capsule — white waveform, light sweep, check, red edge, amber no-speech
- feat(overlay): pure drawing math for falloff, resting floor, light sweep
- feat(overlay): no-speech notification latch, fires on all windowed paths
- feat(overlay): add NoSpeech phase and text-free capsule labels

* Wed Jul 22 2026 Voisu maintainers <voisu@example.invalid> - 0.4.1-1
- fix(tests): serialize restart in the fake systemctl stub

* Wed Jul 22 2026 Voisu maintainers <voisu@example.invalid> - 0.4.0-1
- fix(overlay): address waveform review round 1
- feat(overlay): replace the glyph meter with a live audio bar meter

* Wed Jul 22 2026 Voisu maintainers <voisu@example.invalid> - 0.3.0-1
- fix: converge round-2 review — CI hermeticity and fallback/notifier fixes
- fix: address code-review findings on x11-cross-distro
- feat: detect display session at runtime and pick working tools

* Wed Jul 22 2026 Voisu maintainers <voisu@example.invalid> - 0.2.0-1
- fix(setup): address masked-echo review findings
- feat(setup): masked echo and confirmation for interactive key entry
- chore: move agent-workflow scaffolding out of the public tree
- docs: log benchmark rows 200-201 (version-bump dispatches)
- ci: remove literal expression syntax from version-bump run comment
- ci: address version-bump review — drop --offline, marker-anchored range, %%%% escaping
- ci: add per-merge automatic version-bump workflow

* Tue Jul 21 2026 Voisu maintainers <voisu@example.invalid> - 0.1.1-1
- Stable Global Shortcuts session identity: the Trigger Key binds once and
  survives daemon restarts instead of re-prompting every start (PR #76).
- Ship a desktop entry so portals can resolve a stable app id (PR #76).
- Provision config/state directories via systemd so a fresh home can start
  the service (fixes status=226/NAMESPACE on first run, PR #77).
- voisu doctor: probe the GlobalShortcuts portal interface and surface a
  failed service unit with a journalctl pointer (PR #77).

* Thu Jul 16 2026 Voisu maintainers <voisu@example.invalid> - 0.1.0-1
- Fedora release candidate package; exact commit is recorded in Release.
