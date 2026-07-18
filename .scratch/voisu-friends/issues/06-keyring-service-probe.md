# Task: probe keyring availability from the systemd user service (Fedora KDE)

**Label:** `wayfinder:task` (AFK probe + HITL confirm on host)
**Status:** open
**Blocked by:** —
**Blocks:** 07-setup-wizard-keyring

## Question

Before building keyring storage: empirically test on the live Fedora KDE
install whether Secret Service (KWallet) is reachable and unlocked when
`voisu.service` (systemd user unit) starts at login. Minimal probe: a throwaway
binary/script using the Rust `keyring` crate (or `secret-tool lookup`) run via
a temporary drop-in ExecStartPre, capturing success/failure + timing across a
reboot/login cycle. Record: reachable at service start? unlocked? needs
retry-after-delay? Answer shapes ticket 07's fallback design (lazy retry vs
loud file fallback). Research digest §9 caveat. HITL: Raja reboots/logs in
once.
