# Use cloud-only dual-provider transcription for the first product

> The cloud-only, no-local-model part is superseded by [0012](0012-consented-local-install-from-development-evidence.md) and [0014](0014-windows-local-mode.md). See the update at the end.

Voisu will stream each Recording to Groq and Deepgram concurrently and will not
run or train a local speech model in the first product. This preserves the
user's limited workstation capacity for normal work while retaining the
quality and fallback benefits of independent cloud providers.

## Reality note (2026-09-05)

The cloud-only, dual-provider, no-local-model decision stands. The streaming
half was refined in implementation: Deepgram streams the live Recording,
while Groq receives full audio at stop (a Recording past the full-audio limit
is transcribed in overlapping 60 s chunks instead). See ADR 0005 and
`crates/voisu-app/src/system/groq.rs`.

## Update (2026-11-26)

Opt-in Local mode ships on Linux and Windows, so the "no local model" decision no
longer stands. Cloud mode still uses Deepgram and Groq. The Groq source now lives
in `crates/voisu-cloud/src/groq.rs`.
