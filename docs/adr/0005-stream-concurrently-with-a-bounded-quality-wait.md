# Stream concurrently with a bounded quality wait

Deepgram receives continuous audio frames while Groq receives overlapping
bounded chunks during the same Recording. After stop, Voisu waits only until a
configurable Provider Deadline: it reconciles two valid Source Transcripts when
available and otherwise delivers the valid Source Transcript already present,
preventing one slow provider from dominating release-to-text latency.

