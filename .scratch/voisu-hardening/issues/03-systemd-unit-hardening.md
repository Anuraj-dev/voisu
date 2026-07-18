# Add sandboxing directives to the systemd user units

**Label:** `wayfinder:task` (AFK, implementation — parallel-safe)
**Status:** open
**Blocked by:** — (frontier; safe to run even while latency work is in flight —
touches only `packaging/`, zero overlap with latency files)
**Blocks:** —

## Question

`packaging/voisu.service` and `packaging/voisu-overlay.service` carry zero
hardening directives. Add defense-in-depth so a compromised dependency
(curl/rustls/libei) gets a confined process, starting from:
`NoNewPrivileges=yes`, `ProtectSystem=strict` with explicit `ReadWritePaths=`
for `XDG_RUNTIME_DIR`/`XDG_CONFIG_HOME`, `PrivateTmp=yes`,
`RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6` (network must stay — provider
calls), plus whatever of `ProtectKernelTunables`/`ProtectControlGroups`/
`MemoryDenyWriteExecute` survives testing (note: `MemoryDenyWriteExecute` may
break libei/GTK — verify, don't assume).

These are user units: the daemon needs Secret Service (D-Bus), PipeWire,
portals, subprocess spawning (`pw-record`, `curl`, `wl-copy`, `secret-tool`),
and the overlay needs GTK/layer-shell — every directive must be validated
against a real install (`voisu doctor`, a live Recording, overlay startup),
not just unit-file linting. Deliverable: hardened units + a line in
`docs/packaging-fedora.md` noting the directives and why each exception exists.
Evidence: [audit report](../assets/audit-2026-07-18.md), security major #1.
