# Task: one-time packaging accounts + keys (HITL checklist)

**Label:** `wayfinder:task` (HITL — Raja does these, agent hands a precise checklist)
**Status:** closed (2026-07-20, live HITL session, GH #41)

## Resolution

All four areas done in one guided session (Raja live, Fable 5 medium driving). Every claim below
was verified against command output or API responses, not self-report.

**Where each credential lives (names/locations only — no values anywhere, per policy):**

| Credential | Lives at |
|---|---|
| GPG private key (apt signing) | Local GPG keyring; offline backup `Raja SSD/voisu-signing-key-secret.asc` (passphrase-encrypted, verified valid); GH secret `GPG_PRIVATE_KEY` |
| GPG public key | `packaging/apt/voisu-archive-keyring.asc` (in repo, round-trip verified) |
| GPG passphrase | Raja's password manager only. DECISION: passphrase enabled; CI signs via loopback pinentry using GH secret `GPG_PASSPHRASE` |
| AUR SSH private key | `~/.ssh/keys/aur_voisu` (+ `~/.ssh/config` host entry); GH secret `AUR_SSH_PRIVATE_KEY`. DECISION: no passphrase — dedicated scope-limited deploy key for CI auto-push |
| COPR webhook URL (embeds token) | GH secret `COPR_WEBHOOK_URL` |
| FAS + AUR passwords | Raja's password manager |

**Accounts/URLs tickets 10–13 depend on:**

- GPG key: `4149EE3868B36B6007592966D08BCFDC34125B28`, ed25519 [SC], uid `Voisu Package Signing <rajasaikia1644@gmail.com>`, expires 2028-07-18 (extendable anytime).
- FAS: `anuraj-dev` (accounts.fedoraproject.org).
- COPR: https://copr.fedorainfracloud.org/coprs/anuraj-dev/voisu/ — project ID 246563, chroots fedora-43/44 x86_64 (F42 no longer offered), `enable_net: false` (matches vendored-crates plan). Friends enable: `dnf copr enable anuraj-dev/voisu`. A first accidental capital-V project was deleted and recreated lowercase (COPR names are case-sensitive, no rename).
- AUR: `anuraj-dev` (https://aur.archlinux.org/account/anuraj-dev) — SSH auth verified live (`Welcome to AUR, anuraj-dev!`). `voisu` + `voisu-bin` verified free (RPC info, 0 results); real claim = ticket 11's first push (placeholders violate AUR rules).

**Deviations from the ticket text (verified, not assumed):**

- **AUR 2FA does not exist upstream** (fact-checked 2026-07-20: post-Atomic-Arch mandatory-2FA is proposed, not implemented; the attack vector was orphan adoption, not account cracking). Compensating controls: unique PM-stored password, backup email set, email hidden. QUEUED HITL: enable TOTP the moment aurweb ships it.
- **Cloudsmith token skipped:** ticket 13 has NOT chosen Pages vs Cloudsmith (decision is in-ticket); token is conditional on that choice.
- **COPR auto-rebuild flag is per-package** — no package exists yet; ticket 12 flips it. URL captured now.

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
