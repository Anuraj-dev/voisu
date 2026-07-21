Name:           voisu
Version:        0.1.0
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
# Desktop entry gives the portal a stable app_id (voisu) so KDE's Global
# Shortcuts portal resolves the same persistent binding across restarts.
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
* Thu Jul 16 2026 Voisu maintainers <voisu@example.invalid> - 0.1.0-1
- Fedora release candidate package; exact commit is recorded in Release.
