# Fedora RPM release candidate

Voisu is packaged as an RPM for Fedora KDE Plasma on Wayland. The base RPM
contains `/usr/bin/voisu` and `/usr/bin/voisu-daemon` and is GTK-free. The
optional `voisu-overlay` subpackage contains `/usr/bin/voisu-overlay`, its
independent systemd user unit, and GTK4 plus GTK4 Layer Shell runtime
dependencies.

The base package declares only the boundaries used by the application:

- `wl-clipboard` for `wl-copy` and `wl-paste`;
- `pipewire-utils` for the spawned `pw-record` tool;
- `wireplumber` for the spawned `wpctl` tool;
- `curl` for cloud provider requests;
- `/usr/bin/secret-tool` (owned by `libsecret`) for the credential boundary,
  declared as a file dependency because Voisu needs the binary, not the library;
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
archive with `git archive`. It then extracts that exact-commit archive and runs
`cargo vendor --locked` from the extraction — never the working tree — so the
vendored `Source1` is a pure function of the commit. The vendor tarball is
written deterministically (`tar --sort=name --owner=0 --group=0 --numeric-owner
--mtime="@<commit-epoch>" --mode='u+rw,go=rX'` piped through `gzip -n`, so no
ordering, ownership, mtime, permission-bit, or gzip-header variation survives).
The self-test then runs an independent `cargo vendor --locked` of the same commit
into a separate directory, archives it the same way, and aborts unless the two
tarballs are byte-identical — proving cargo vendor output stability, not merely
tar/gzip determinism. The same commit therefore yields a byte-identical
`voisu-vendor-<version>.tar.gz`. During `%prep`, the RPM
unpacks that archive and writes `.cargo/config.toml` with a
`[source.crates-io] replace-with = "vendored-sources"` source. `%build` and
`%check` use `--offline`, so a clean mock build cannot fetch crates from
crates.io. The Cargo.lock plus this reproducible vendor archive generated from
the exact commit is the reproducibility mechanism.

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
voisu service start                 # immediate daemon start; login start is enabled
voisu service status
```

When the optional subpackage is installed, `voisu service install` also enables
and immediately starts `/usr/lib/systemd/user/voisu-overlay.service`, after
confirming systemd's effective fragment and `ExecStart` still resolve to the
packaged Overlay rather than a user-owned shadow. Run or rerun the command after
adding `voisu-overlay`; no separate Overlay setup verb is required. Verify the
independent login-start path with:

```sh
systemctl --user is-enabled voisu-overlay.service
systemctl --user is-active voisu-overlay.service
pgrep -a -x voisu-overlay
```

The Overlay is hidden at Idle by design. `voisu service start`, `stop`, and
`restart` continue to manage only the daemon; Overlay management failure is
reported as a warning and never fails daemon installation or uninstallation.

The packaged daemon user unit is `/usr/lib/systemd/user/voisu.service` and points at
`/usr/bin/voisu-daemon --systemd`. `voisu service install` asks systemd for the
unit it would actually run — `systemctl --user show voisu.service -p
FragmentPath -p ExecStart` — so an administrator override under
`/etc/systemd/user` or a drop-in is honored rather than a stale static file, and
it validates the effective `ExecStart` binary. A unit resolved under XDG config
is the Ticket 09 user unit, not a package, and is never migrated as one. Only
when systemctl cannot answer does it fall back to a static search of
`/etc/systemd/user` before `/usr/lib/systemd/user`. If an old user unit or daemon
copy shadows a valid packaged unit, it disables the old owner, removes only those
Voisu-managed stale files, reloads systemd, and enables the packaged unit; if the
effective packaged `ExecStart` binary is missing or untrusted, it clearly falls
back to the Ticket 09 user-data path instead.

The optional Overlay unit runs `/usr/bin/voisu-overlay --supervise`, is owned by
`graphical-session.target`, and is ordered after `voisu.service` without
`Wants=` or `Requires=`. It remains an observer-only process: daemon startup,
Recording, Transcript production, and Delivery never depend on it.

### Trigger Key re-prompts on every start (leaked shortcut sections)

A daemon built before the stable-session-token fix made KDE's Global Shortcuts
portal re-prompt for a Trigger Key on every start and leak one dead section per
start into `~/.config/kglobalshortcutsrc`. Upgrading stops new leaks; prune the
accumulated ones by hand (the daemon never edits your config):

```sh
# Inspect what leaked:
grep -nE '^\[token_voisu_session_|voisu-toggle' ~/.config/kglobalshortcutsrc

# With the daemon stopped, delete every [token_voisu_session_*] section and any
# stray `voisu-toggle=...` line left under a terminal section (e.g. [Alacritty]),
# then re-bind the Trigger Key from a fresh daemon start:
systemctl --user stop voisu.service
# edit ~/.config/kglobalshortcutsrc, remove those sections/lines, save
systemctl --user start voisu.service
```

### Unit sandboxing

Both user units carry systemd hardening directives (`NoNewPrivileges`,
`ProtectSystem=strict`, `PrivateTmp`, `RestrictAddressFamilies`, kernel/cgroup
protections, `SystemCallArchitectures=native`) so a compromised dependency gets
a confined process. The deliberate exceptions:

- Daemon `ReadWritePaths=%t %h/.config/voisu %h/.local/state/voisu` — control
  socket and capture scratch, config/dictionary, history/diagnostics.
- Daemon keeps `AF_INET`/`AF_INET6` (provider HTTPS/WSS via curl) and
  `AF_NETLINK` (glibc `getaddrinfo` interface enumeration); the overlay is
  `AF_UNIX`-only (daemon socket, Wayland, D-Bus).
- `MemoryDenyWriteExecute=yes` is set on the daemon (no JIT anywhere in its
  process tree) but omitted on the overlay: GTK/GL shader pipelines and libei
  may map writable+executable pages.

Every directive must be re-validated against a real install (`voisu doctor`, a
live Recording, overlay startup) whenever the dependency surface changes.

## Upgrade and removal

After an RPM upgrade, after adding the optional Overlay subpackage, or when an
old Ticket 09 installation exists, rerun the user-owned setup command:

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
sudo dnf remove voisu-overlay voisu
systemctl --user daemon-reload
```

`voisu service uninstall` reports that it must run before removing the RPM. It
best-effort disables and stops the optional Overlay unit when present, then
disables the packaged daemon service and removes only a stale Ticket 09 shadow.
An Overlay failure remains a warning and does not block daemon uninstall. The
command does not remove RPM-owned files. An explicit purge is a separate,
destructive user action:
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
artifact paths, the packaged unit, CLI startup, and packaged-unit migration. It
binds to the exact supplied RPM by comparing the full `rpm -qp --dump` manifest
(path, size, mtime, digest, mode, owner, group for every file) against the
installed `rpm -q --dump`, and refuses when a same-NEVRA package is already
installed with a different payload — `dnf` will not replace a same-NEVRA package,
so this stops the smoke from silently exercising the wrong artifact. It snapshots
the user-service state that `voisu service install` mutates (enablement including
enabled-runtime, active state, and any Ticket 09 XDG shadow it migrates away) and
restores it in a cleanup trap that runs on success and on failure. Restoration is
judged on the end state rather than on individual step exit codes: after
restoring, the harness compares systemd's reported enablement and active state
against the pre-smoke snapshot whenever a unit exists again after restoration —
a pre-existing package install or a restored Ticket 09 shadow, so an active
Ticket 09 service is restarted even on the fresh-install path (and, for a fresh
install, verifies the smoke-installed RPM is removed, the packaged unit is not
left enabled, and the smoke-started service is stopped); any mismatch is
printed and forces a non-zero exit even when the smoke otherwise passed, and
enablement states that cannot be faithfully reproduced are reported instead of
silently downgraded.
RPM-owned files are never modified. The opt-in invocation additionally runs readiness, starts the packaged
user service, performs a real three-second Recording, stops it, and verifies that
a Transcript is available through `wl-paste`. The release process additionally requires completing the
interactive KDE/Wayland release-evidence checks, including portal
approval, direct Delivery, clipboard fallback, login start, and upgrade/removal
behavior. These checks are intentionally not run by the default build.

APT/DEB packaging is explicitly out of scope for this Fedora release and is a
subsequent milestone; no Debian artifacts or packaging metadata belong here.
