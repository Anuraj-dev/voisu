# 10 — Recover cleanly from real workflow failures

**Parent:** [Build reliable cloud-first dictation for Fedora Wayland](../../voisu-spec/issues/01-fedora-cloud-dictation.md)

**What to build:** A hardened daemon whose next Recording remains usable after
failures in every external boundary and after abrupt process interruption.

**Blocked by:** 06 — Inspect and expire correlated local diagnostics; 07 — Toggle Recording through the Global Shortcuts portal; 08 — Deliver text through libei with clipboard fallback; 09 — Own the daemon through a systemd user service.

**Status:** implemented on `ticket-10-recovery` at `86b2225` and `d6bd6b0`; workspace gate green,
review and PR pending

- [x] Microphone disappearance and reconnection leave the next Recording usable.
- [x] Provider disconnect, malformed response, quota error, and deadline expiry follow documented fallback behavior.
- [x] Portal revocation and restart leave CLI control and clipboard Delivery usable.
- [x] CLI termination cannot terminate the daemon or abandon an invalid state.
- [x] Daemon interruption cleans stale runtime ownership and restarts into an observable safe state.
- [x] Repeated failure cannot create duplicate Delivery, leaked provider work, or an unbounded restart loop.
- [x] Opt-in Fedora smoke tests exercise real microphone, providers, portals, systemd, and the next-Recording recovery invariant.
