# Keyring service probe — Secret Service reachability at login (ticket 06)

_Sonnet 5 diagnostics agent, 2026-07-19. Read-only live-machine probe on the
current boot (booted 18:48:14 IST, this session). Fedora KDE Plasma 6,
Wayland. Saved verbatim by the driver._

## Timeline (this boot), deltas relative to voisu.service's first start

`voisu.service` actually started **twice** this boot: first at **18:50:58**
(matching the ticket's stated time), then systemd stopped and restarted it at
18:51:52 (10s after the first start, `voisu-daemon` logged `Trigger Key
binding is unavailable: portal CreateSession response deadline elapsed` —
likely cause of the restart, unrelated to keyring). All deltas below are
relative to **T0 = 18:50:58**, the first/login-time start.

| Δ | Time | Event | Source |
|---|---|---|---|
| −2m44s | 18:48:14 | System boot | `uptime -s` |
| −29s | 18:50:29 | `ksecretd` process starts (`--pam-login 13 14`) | `ps -o lstart -p 2827` |
| −28s | 18:50:30 | Session 9 login timestamp; `sddm-helper` logs `pam_kwallet5: final socket path` | `loginctl show-session`, journal |
| −26s | 18:50:32 | `pam_kwallet_init`: PAM-credential wallet unlock helper runs | journal |
| −1s | 18:50:57 | `ksecretd` attempts portal registration (already running, not starting) | journal |
| **0** | **18:50:58** | **`voisu.service` started (first, login-time instance)** | `journalctl -u voisu.service` |
| +0s | 18:50:58 | `xdg-desktop-portal.service` started | journal |
| +10s | 18:51:08 | `voisu-daemon`: portal `CreateSession` deadline elapsed | journal |
| +54s | 18:51:52 | `voisu.service` stopped and restarted (currently-active instance) | journal, `systemctl show` |
| +26m23s | 19:17:21 | `dbus-…-org.kde.kwalletd6@0.service` D-Bus-activated | journal (see note below) |

**Note on kwalletd6:** this is a *separate, later-activated* service — it does
**not** own `org.freedesktop.secrets` (see next section) and started 26+
minutes after login, almost certainly triggered by an unrelated GUI action
(e.g. opening KWallet Manager), not by anything keyring-related at boot. It
is a red herring for "is the secret service up at login" — the real provider
was already running.

## Provider identity (live, right now)

- `org.freedesktop.secrets` is currently owned by **`:1.42` → PID 2827 →
  `/usr/bin/ksecretd --pam-login 13 14`** (confirmed via `busctl --user
  status org.freedesktop.secrets`), i.e. **KWallet's PAM-login secret-service
  daemon**, not `kwalletd6`.
- `ksecretd` also owns `org.kde.ksecretd`, `org.kde.secretservicecompat`, and
  `org.freedesktop.impl.portal.desktop.kwallet` on this bus — it is the
  full KWallet6 Secret Service implementation, launched at login via PAM
  (`--pam-login`), not D-Bus-activated on first use.
- `org.freedesktop.secrets` is *also* listed as `(activatable)` in
  `ListActivatableNames`, so even if `ksecretd` weren't already running, a
  cold D-Bus call would auto-start it — but on this boot it didn't need to,
  since PAM started it proactively at login, ~29s before voisu.service's own
  first start.

## Locked/unlocked + round-trip timings

| Check | Result | Wall time |
|---|---|---|
| Default collection (`ReadAlias default`) | `/org/freedesktop/secrets/collection/kdewallet` (present) | 6ms |
| `Locked` property on default collection | **`false` (unlocked)** | 5ms |
| `secret-tool store` (label `voisu-probe`) | success | 25ms |
| `secret-tool lookup` | value matched exactly | 12ms |
| `secret-tool clear` (delete) | success | 11ms |
| Post-delete lookup (verify cleanup) | empty / exit 1, confirming deletion | 11ms |
| **Total round trip (store→lookup→delete)** | | **~48ms** |

The probe secret was created and deleted during this session; post-delete
lookup confirms nothing was left in the keyring.

### Service-context replay (`systemd-run --user --wait --collect --pipe`)

Ran successfully, no prompt, no denial — this mimics `voisu.service`'s own
non-interactive, no-terminal execution environment:

```
epoch_ms_start=1784470870533
GetNameOwner org.freedesktop.secrets -> ":1.42"
Locked -> false
store/lookup/clear -> success ("svc-context-probe" round-tripped)
epoch_ms_end=1784470870578
```

Full round trip in the service-mimicking context: **45ms**, `Service
runtime: 66ms` per `systemd-run`'s own accounting. **This directly answers
the ticket's core worry** (would the service's restricted, non-interactive
context behave differently from an interactive shell) — it does not; no
polkit/portal prompt appeared.

## Ticket answers

1. **Reachable at voisu.service start?** — **Yes.** `ksecretd` (the
   `org.freedesktop.secrets` owner) started at 18:50:29, 29s before
   voisu.service's first start at 18:50:58, and was already registered on
   the session bus.
2. **Unlocked?** — **Yes.** `Locked` on the default (`kdewallet`) collection
   reads `false` right now, and the boot timeline shows `pam_kwallet_init`
   (PAM-credential auto-unlock) ran at 18:50:32 — 26s before voisu.service's
   first start and well before it. No prompt was needed for any probe call
   in this session, including the `systemd-run` no-terminal replay.
3. **Needs retry-after-delay?** — **Not on this boot's evidence.** The
   secrets service is PAM-launched and pre-unlocked before voisu.service
   even starts, so there is no observed window where a first attempt would
   fail and a retry would succeed. This is an *inferred* answer (see caveat
   below) — the login-time race is favorable here because PAM's
   `pam_kwallet` unlock happens synchronously during login, ahead of
   graphical session services.

## Recommendation for ticket 07

Build the keyring path as the primary store with **a bounded lazy-retry, not
a loud file fallback as the default reaction to a single failure.** Evidence:
round trips measured at 11–48ms both interactively and in a
`systemd-run`-mimicked service context, and the provider (`ksecretd`) is
PAM-unlocked ~29s before voisu.service's own start on this machine — so a
first-attempt failure is more likely to indicate a real problem (locked
wallet the user declined to unlock, KWallet disabled, non-KDE session with no
provider) than a startup race. Suggested budget: **1 immediate attempt, then
up to 3 retries with short backoff (e.g. 250ms/1s/3s, total ≈4.25s)** to
absorb any edge-case slow activation; if still failing after that, fall back
to the loud file warning rather than retrying silently forever or blocking
service startup. Treat "activatable but unowned" and "owned but Locked=true"
as distinct failure modes in the retry logic — the first is worth a short
retry (activation in flight), the second may need a user-facing "wallet is
locked, unlock it" nudge instead of silent retries.

## Caveat

The reachable/unlocked findings for "at voisu.service start" are **inferred
from journal timestamps on this single boot**, not measured from inside
voisu.service's own start sequence — no probe ran as an actual
`ExecStartPre` of voisu.service this session (that would require a reboot,
which was out of scope for this read-only pass). The `systemd-run` replay
strengthens the inference (same non-interactive D-Bus environment, same
result) but is not a substitute for an in-situ measurement across a real
reboot. Prepared capture files are ready in
`.scratch/voisu-friends/assets/06-keyring-probe/` for Raja to install ONLY
when he wants to confirm this empirically across a real reboot/login cycle
(see `INSTALL.md` in that directory for the 4-command install + cleanup).
