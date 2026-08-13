# Qwen formatter — flagged host rollout (2026-08-13)

**Status:** process documentation. The Qwen small-edit formatter stays **off**
by default. This file does not enable it, and it does not record host
pass/fail counts.
**Date:** 2026-08-13
**Code:** `VOISU_ENABLE_QWEN_FORMAT` (`ENABLE_QWEN_FORMAT_ENV` in
`crates/voisu-app/src/config.rs`); daemon reads `qwen_format_enabled()` for
`small_edit_contract`.

This document is the Ticket 8 host-rollout procedure. Silence/outro
classification and validate-before-format (Tickets 1 and 2) can ship while
this formatter remains off. Those fixes do not depend on this flag.

## Default: formatter off

A packaged install ships `VOISU_ENABLE_DPR=1` on the user unit so local
organize (spoken marks, quotes, first/second lists) runs. It does **not**
set `VOISU_ENABLE_QWEN_FORMAT`. A stock `~/.config/voisu/config.toml` also
leaves Qwen formatting **off**.

| Condition | Formatter |
|---|---|
| Env unset / missing | off |
| `VOISU_ENABLE_QWEN_FORMAT=0` / `false` / empty / any other value | off |
| `VOISU_ENABLE_QWEN_FORMAT=1` or `true` **and** `VOISU_ENABLE_DPR=1` or `true` | on (test host only) |

There is no `config.toml` key for this gate. Do not add one. Do not set the
flag in the package, the packaged unit, or a shipped drop-in.

With the formatter off, Developer Prompt Rendering (when `VOISU_ENABLE_DPR`
is on) stays on the existing #139 derivation path and the 1.5 s
utterance-end clock. The five-second ValidationCompleted formatting clock
applies only while the Qwen flag is on.

## Independent flags

The two rollout env vars are independent. Both must be explicit `1` or
`true` before the formatter runs.

| Flag | What it enables |
|---|---|
| `VOISU_ENABLE_DPR` | Flagged Developer Prompt Rendering pipeline |
| `VOISU_ENABLE_QWEN_FORMAT` | Small-edit Qwen formatting contract |

Turning DPR on does **not** turn Qwen formatting on. Turning Qwen formatting
on without DPR does not take the formatting path: the daemon only constructs
`small_edit_contract` on the DPR English-eligible branch.

Parser: only `1` and `true` (ASCII, trimmed, case-insensitive) enable a flag.
`0`, `false`, empty, `yes`, and every other string stay off.

## Enable on one test host

Do this only on Raja's machine after a test RPM from the stacked commit is
installed. Do not enable in packaging or for other hosts.

Both flags are required:

```sh
VOISU_ENABLE_DPR=1
VOISU_ENABLE_QWEN_FORMAT=1
```

### Fedora systemd user drop-in

The packaged daemon unit is `/usr/lib/systemd/user/voisu.service`. It starts
`/usr/bin/voisu-daemon --systemd`. It does **not** set either flag. Use a
user drop-in so the package stays flag-off and rollback is a file delete.

```sh
mkdir -p ~/.config/systemd/user/voisu.service.d
cat > ~/.config/systemd/user/voisu.service.d/qwen-format.conf <<'EOF'
[Service]
Environment=VOISU_ENABLE_DPR=1
Environment=VOISU_ENABLE_QWEN_FORMAT=1
EOF
systemctl --user daemon-reload
systemctl --user restart voisu.service
```

Confirm the effective environment after restart:

```sh
systemctl --user show-environment
systemctl --user show voisu.service -p Environment
```

`voisu doctor` / `voisu service status` should still report a healthy
user-session daemon. Credentials stay in Secret Service; do not put API keys
in the drop-in.

## Instant rollback

No rebuild. Unset the Qwen flag or set it to anything other than `1`/`true`,
then restart the user daemon. The derivation path and the 1.5 s utterance-end
clock return on the next Recording.

```sh
# Preferred: remove only the Qwen line, or delete the drop-in entirely.
rm -f ~/.config/systemd/user/voisu.service.d/qwen-format.conf
# Or keep the file and force the off parser:
# Environment=VOISU_ENABLE_QWEN_FORMAT=0
systemctl --user daemon-reload
systemctl --user restart voisu.service
```

Leaving `VOISU_ENABLE_DPR=1` in place is fine. Ticket 1/2 behaviour stays.
Only the formatter rolls back.

## Host evidence process (do not invent results here)

Record pass/fail counts in a **later** dated notes file under `docs/` after
the session. This file is the procedure only.

### 1. Build a test RPM from the stacked commit

Follow `docs/packaging-fedora.md` on Fedora with `cargo`, `rustc`,
`rpmbuild`, `rpm`, the GTK4 development packages, and `systemd-rpm-macros`:

```sh
git checkout <stacked-commit>
git status --short                 # must be empty
VOISU_COMMIT=$(git rev-parse HEAD) packaging/build-rpm.sh
```

Inspect `dist/rpm/` before install (`rpm -qip`, `rpm -qpl`). The Release
string carries `git<commit>` so the artifact cannot be mistaken for another
tree.

### 2. Install on Raja's machine

```sh
sudo dnf install ./dist/rpm/voisu-*.rpm
# optional Overlay only when GTK feedback is wanted:
# sudo dnf install ./dist/rpm/voisu-overlay-*.rpm
voisu doctor
voisu service install
voisu service start
```

Then apply the test-host drop-in above and restart `voisu.service`. Do not
commit that drop-in. Do not ship it in the RPM.

### 3. Run 30–50 real English dictations

Speak into the live daemon the way a user would. Cover at least:

- silence (and any “thank you for watching” / outro hallucination)
- structured prompts
- lists
- fillers and spoken corrections
- ordinary chat
- quotes
- names, URLs, and numbers
- at least one timeout or provider-error observation

Count each Recording as pass or fail against the abort criteria below. Note
the commit, RPM Release, whether both flags were on, and the clock in use
(ValidationCompleted / 5 s while Qwen is on).

### 4. Write results later

A later dated notes file (for example
`docs/research/qwen-format-host-evidence-YYYY-MM-DD.md`) should hold the
pass/fail table. Do not back-fill numbers into this 2026-08-13 process doc.

## Abort / keep-off criteria

Keep the formatter **off** (or roll it back immediately) if the host session
shows any of:

- any outro Delivery (including silence + “Thank you for watching”)
- any protected-fact mutation (names, commands, paths, URLs, numbers, dates,
  times, negations, quoted interiors)
- prompt artifacts in Delivered text
- repeated format stalls longer than 5 s

A single timeout or invalid-edit fallback to the local baseline is expected
and is not by itself an abort, as long as Delivery still happens once before
the 5 s ceiling and the baseline does not mutate protected facts. Repeated
>5 s stalls are an abort.

## Packaging invariant

Do **not** default this flag on in:

- `packaging/voisu.service` or `packaging/voisu-overlay.service`
- `packaging/voisu.spec` / RPM scripts
- AUR / apt / desktop files
- a shipped `config.toml` or config-directory template

The only supported on-switch is an operator-local env (`1`/`true`) on a test
host, typically the systemd user drop-in above.
