Name:           voisu
Version:        0.1.0
%{!?voisu_commit:%global voisu_commit unknown}
# Release is computed by the build scripts, ONE spec serving both channels:
#   - dev machine (build-rpm.sh): voisu_release undefined -> 1.git<commit>
#   - COPR channel (build-srpm.sh / make-srpm.sh): voisu_release is a
#     monotonically increasing snapshot release like 0.<count>.<ct>.git<sha>,
#     whose leading 0. sorts before an eventual tagged 1%{?dist} and whose
#     commit-count primary key increases for any descendant commit (immune to
#     committer clock skew), so `dnf upgrade` always sees a newer NEVR.
Release:        %{?voisu_release}%{!?voisu_release:1.git%{?voisu_commit}}%{?dist}
Summary:        Cloud-first Linux dictation for Fedora Wayland
# Voisu is MIT; the statically linked ring crate adds ISC (new code) and
# Apache-2.0 (BoringSSL-derived code). Ring's license texts ship in %%license.
License:        MIT AND Apache-2.0 AND ISC
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

The package is built from the exact tested git commit recorded in the RPM
Release metadata and a Cargo.lock-pinned source archive.

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
# Statically linked ring is ISC AND Apache-2.0; its texts must ship with the RPM.
cp ../voisu-vendor-%{version}/ring/LICENSE LICENSE.ring
cp ../voisu-vendor-%{version}/ring/LICENSE-BoringSSL LICENSE.ring-BoringSSL
cp ../voisu-vendor-%{version}/ring/LICENSE-other-bits LICENSE.ring-other-bits
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
cargo test --offline --release --locked --workspace
cargo check --offline --release --locked -p voisu-app --features overlay

%install
install -D -m 0755 target/release/voisu %{buildroot}%{_bindir}/voisu
install -D -m 0755 target/release/voisu-daemon %{buildroot}%{_bindir}/voisu-daemon
install -D -m 0755 target/release/voisu-overlay %{buildroot}%{_bindir}/voisu-overlay
install -D -m 0644 packaging/voisu.service %{buildroot}%{_userunitdir}/voisu.service
install -D -m 0644 packaging/voisu-overlay.service %{buildroot}%{_userunitdir}/voisu-overlay.service

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
%license LICENSE LICENSE.ring LICENSE.ring-BoringSSL LICENSE.ring-other-bits
%doc README.md
%{_bindir}/voisu
%{_bindir}/voisu-daemon
%{_userunitdir}/voisu.service

%files overlay
%{_bindir}/voisu-overlay
%{_userunitdir}/voisu-overlay.service

%changelog
* Thu Jul 16 2026 Voisu maintainers <voisu@example.invalid> - 0.1.0-1
- Fedora release candidate package; exact commit is recorded in Release.
