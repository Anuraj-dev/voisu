# Use cloud-only dual-provider transcription for the first product

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

