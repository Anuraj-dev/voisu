# Task: one-time packaging accounts + keys (HITL checklist)

**Label:** `wayfinder:task` (HITL — Raja does these, agent hands a precise checklist)
**Status:** open
**Blocked by:** 04-guarded-delivery-mode, 05-dictionary-cli-hotreload, 07-setup-wizard-keyring, 08-gnome-plain-window-fallback (phase gate: features merge before packaging starts)
**Blocks:** 10-cargo-deb-package, 11-aur-packages, 12-copr-channel, 13-apt-repo-channel

## Question

Raja has never shipped distro packages — this ticket is the guided one-time
setup, as a checklist with exact commands/URLs:

1. **GPG signing key** for the apt repo: generate (ed25519), export public key,
   back up private key offline; store passphrase decision.
2. **Fedora account (FAS) + COPR project** `voisu`: enable webhook auto-rebuild,
   note the webhook URL for the CI ticket.
3. **AUR account** with SSH key + **2FA enabled** (Atomic Arch takeover
   campaign 2026-06 makes 2FA non-negotiable), claim `voisu` + `voisu-bin`
   names.
4. **GitHub repo secrets**: AUR SSH key, GPG private key, any Cloudsmith token
   (if ticket 13 chooses Cloudsmith over Pages).

Resolution records where each credential lives (keyring/secrets) and the
account/project URLs later tickets depend on.
