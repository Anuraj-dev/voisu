# Stream concurrently with a bounded quality wait

Deepgram receives continuous audio frames while Groq receives overlapping
bounded chunks during the same Recording. After stop, Voisu waits only until a
configurable Provider Deadline: it reconciles two valid Source Transcripts when
available and otherwise delivers the valid Source Transcript already present,
preventing one slow provider from dominating release-to-text latency.

## Reality note (2026-09-05)

Groq does not receive chunks throughout the Recording. Audio is buffered
locally; a Recording at or below the full-audio limit is sent to Groq as one
request after stop, and only a Recording that crosses that limit mid-Recording
pre-streams overlapping 60 s windows. The bounded Provider Deadline and the
reconcile-or-deliver-valid behavior stand as decided. Ground truth:
`crates/voisu-app/src/system/groq.rs` (`send_audio`, `plan_finalize_chunks`,
`complete`).

