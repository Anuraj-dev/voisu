# Task: probe keyring availability from the systemd user service (Fedora KDE)

**Label:** `wayfinder:task` (AFK probe + HITL confirm on host)
**Status:** closed (2026-07-19)

## Resolution

Probed on the live fresh-login boot of 2026-07-19 (boot 18:48, voisu.service start 18:50:58).
**Reachable at service start: YES** — `ksecretd` (PAM-launched) owned `org.freedesktop.secrets`
from 18:50:29, 29 s before the daemon started. **Unlocked: YES** — `pam_kwallet_init` auto-unlocked
kdewallet at login (18:50:32); default collection `Locked=false`. **Retry-after-delay: not needed
on the evidence** — no observed race window; store→lookup→delete round trip 45–48 ms, identical
from a `systemd-run --user` (service-like, no-terminal) context, zero prompts. Note: the provider
is `ksecretd`, NOT `kwalletd6` (which activates separately and later — red herring).

Ticket 07 design consequence: keyring primary with a short bounded lazy retry
(immediate + ~250 ms/1 s/3 s ≈ 4.25 s budget); persistent failure after that means a real
problem (locked wallet, no provider) → LOUD file fallback, distinguishing
"activatable-but-unowned" from "owned-but-locked".

Evidence: `assets/06-keyring-service-probe.md`. At-service-start state is journal-inferred for
this boot; a prepared ExecStartPre probe kit (`assets/06-keyring-probe/`, INSTALL.md) captures
it directly on Raja's next reboot — queued HITL, non-gating.
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
