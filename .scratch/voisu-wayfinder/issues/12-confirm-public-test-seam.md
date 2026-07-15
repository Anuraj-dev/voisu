# Confirm the public TDD seam

**Label:** `wayfinder:grilling`  
**Status:** closed  
**Claimed by:** Raja and Codex

## Question

Should most daemon behavior be tested through the real CLI and versioned Unix
IPC while only operating-system and cloud boundaries are replaced by test
adapters?

## Resolution

Use one high public seam: start the real daemon and drive behavior through the
public CLI and versioned Unix IPC. Substitute only operating-system and cloud
boundaries in the standard suite, cover each boundary with contract tests, and
reserve live microphone, portal, and provider checks for opt-in Fedora smoke
tests.
