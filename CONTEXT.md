# Voisu Dictation

Voisu turns a bounded period of speech into validated text delivered to the
application the user is currently working in.

## Language

**Recording**:
The bounded audio capture that begins with one Trigger Key activation and ends
with the next activation or a safety limit.
_Avoid_: Session, clip, voice packet

**Transcript**:
The final validated text produced from a Recording.
_Avoid_: Output, message, transcription result

**Source Transcript**:
Text returned independently by Groq or Deepgram before reconciliation and
quality validation.
_Avoid_: Raw transcript, provider output

**Merge Result**:
The candidate text produced by reconciling two Source Transcripts.
_Avoid_: Blend, fusion

**Trigger Key**:
The user-approved global shortcut whose first activation starts a Recording and
whose next activation stops it.
_Avoid_: Hold key, push-to-talk key

**Delivery**:
Placing the final Transcript into the focused application, with clipboard
preservation as the fallback.
_Avoid_: Paste, output

**Overlay**:
The optional, separate on-screen status surface that reflects daemon state.
_Avoid_: Popup, HUD

**Recording Deadline**:
The safety limit that automatically stops a forgotten Recording.
_Avoid_: Timeout

**Quality Failure**:
A candidate Transcript that violates a content or consistency guardrail.
_Avoid_: Provider error, bad transcription

**Provider Deadline**:
The bounded post-Recording wait during which Voisu accepts Source Transcripts
from both cloud providers before using the valid result already available.
_Avoid_: API timeout, race timeout

