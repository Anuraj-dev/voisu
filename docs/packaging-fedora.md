# Fedora RPM release candidate

Voisu is packaged as an RPM for Fedora KDE Plasma on Wayland. The base RPM
contains `/usr/bin/voisu` and `/usr/bin/voisu-daemon` and is GTK-free. The
optional `voisu-overlay` subpackage contains `/usr/bin/voisu-overlay` and adds
GTK4 plus GTK4 Layer Shell runtime dependencies.

The base package declares only the boundaries used by the application:

- `wl-clipboard` for `wl-copy` and `wl-paste`;
- `pipewire-utils` for the spawned `pw-record` tool;
- `wireplumber` for the spawned `wpctl` tool;
- `curl` for cloud provider requests;
- `libsecret` for the `secret-tool` credential boundary;
- an optional `libei` Recommends entry because Voisu loads `libei.so.1` at
  runtime. If it is absent or lacks the required capability, Delivery remains
  available through clipboard preservation.

The package does not add a build-time or hard runtime dependency on
`libei-devel`; direct Delivery is portal-mediated and runtime-loaded. The
Overlay subpackage alone requires `gtk4` and `gtk4-layer-shell`.

## Exact build

Run this on Fedora with `cargo`, `rustc`, `rpmbuild`, `rpm`, the GTK4
development packages, and `systemd-rpm-macros` installed:

```sh
git checkout <tested-commit>
git status --short                 # must be empty
VOISU_COMMIT=$(git rev-parse HEAD) packaging/build-rpm.sh
```

`packaging/build-rpm.sh` runs the standard workspace suite, release workspace
build, and opt-in Overlay check before creating a Cargo.lock-pinned source
archive with `git archive`. It also runs `cargo vendor --locked` from that
exact commit and creates `voisu-vendor-<version>.tar.gz` as `Source1`. During
`%prep`, the RPM unpacks that archive and writes `.cargo/config.toml` with a
`[source.crates-io] replace-with = "vendored-sources"` source. `%build` and
`%check` use `--offline`, so a clean mock build cannot fetch crates from
crates.io. The Cargo.lock plus vendor archive generated from the exact commit
is the reproducibility mechanism.

`rpmbuild` then repeats the standard suite in the archive's `%check` section.
The full commit is included in the RPM Release as `1.git<commit>`, so the
package cannot be mistaken for an artifact from a different tested tree. No
Debian or APT artifacts are produced.

The base and Overlay RPMs are written to `dist/rpm/`. Inspect them before
installation:

```sh
rpm -qip dist/rpm/voisu-*.rpm
rpm -qpl dist/rpm/voisu-*.rpm
```

## Install and login start

Install the base RPM, and install `voisu-overlay` only when GTK feedback is
wanted:

```sh
sudo dnf install ./dist/rpm/voisu-0.1.0-*.rpm
# optional:
sudo dnf install ./dist/rpm/voisu-overlay-0.1.0-*.rpm
```

Set up readiness and credentials as the desktop user. Credentials go through
Secret Service and are not written to the unit or command line:

```sh
voisu doctor
printf '%s\n' "$GROQ_API_KEY" | voisu auth set groq
printf '%s\n' "$DEEPGRAM_API_KEY" | voisu auth set deepgram
voisu auth verify groq
voisu auth verify deepgram
voisu service install
voisu service start                 # immediate start; login start is enabled
voisu service status
```

The packaged user unit is `/usr/lib/systemd/user/voisu.service` and points at
`/usr/bin/voisu-daemon --systemd`. `voisu service install` detects that unit
before considering the Ticket 09 XDG user-data copy. If an old user unit or
daemon copy shadows the package, it disables the old owner, removes only those
Voisu-managed stale files, reloads systemd, and enables the packaged unit.

## Upgrade and removal

After an RPM upgrade, run the user-owned migration command once if the old
Ticket 09 installation exists:

```sh
sudo dnf upgrade ./dist/rpm/voisu-0.1.0-*.rpm
voisu service install
voisu service status
```

The migration never removes Secret Service credentials, supported user state,
or diagnostics. The packaged daemon path remains `/usr/bin/voisu-daemon`; no
checkout or XDG user-data executable is allowed to keep owning the unit.

Removal must first disable the packaged user unit as the desktop user. The
user-owned command is required before `dnf remove`: `%systemd_user_preun` cannot
reliably stop a running per-user unit or remove per-user enablement under
`~/.config`. The RPM removal then removes packaged binaries and the packaged
unit. User data is preserved:

```sh
voisu service uninstall
sudo dnf remove voisu voisu-overlay
systemctl --user daemon-reload
```

`voisu service uninstall` reports that it must run before removing the RPM. It
disables the packaged service and removes only a stale Ticket 09 shadow. It does
not remove RPM-owned files. An explicit purge is a separate, destructive user
action:
remove the Voisu state/configuration directories under `XDG_STATE_HOME`,
`XDG_CONFIG_HOME`, and `XDG_DATA_HOME`, then clear the `voisu-provider` Secret
Service entries for `groq` and `deepgram` with the user's keyring tool.

## Standard and Fedora smoke suites

The exact RPM artifact is checked headlessly and, on Fedora, against the
desktop using:

```sh
packaging/fedora-smoke.sh dist/rpm/voisu-0.1.0-<release>.x86_64.rpm <tested-commit>
VOISU_FEDORA_LIVE_SMOKE=1 packaging/fedora-smoke.sh \
  dist/rpm/voisu-0.1.0-<release>.x86_64.rpm <tested-commit>
```

The first invocation verifies RPM ownership, declared dependency names,
artifact paths, the packaged unit, CLI startup, and packaged-unit migration.
The opt-in invocation additionally runs readiness, starts the packaged user
service, performs a real three-second Recording, stops it, and verifies that a
Transcript is available through `wl-paste`. The orchestrator must complete the
interactive KDE/Wayland checks in `docs/release-evidence.md`, including portal
approval, direct Delivery, clipboard fallback, login start, and upgrade/removal
behavior. These checks are intentionally not run by the default build.

APT/DEB packaging is explicitly out of scope for this Fedora release and is a
subsequent milestone; no Debian artifacts or packaging metadata belong here.
