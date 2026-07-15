# Build reliable cloud-first dictation for Fedora Wayland

**Status:** ready-for-agent  
**Label:** `ready-for-agent`

## Problem Statement

Linux users need fast speech-driven text entry without giving a local speech
model enough CPU and memory to disrupt their normal work. Existing approaches
often depend on one compositor, raw keyboard permissions, manual clipboard
pasting, or UI processes that can take the transcription daemon down with them.
They also provide too little diagnostic evidence to improve real latency and
recognition failures safely.

The first user has Fedora KDE Plasma on Wayland and needs a dependable workflow:
press one approved shortcut, speak, press it again, and receive accurate text in
the currently focused application.

## Solution

Voisu will run a lightweight Rust daemon as a systemd user service. A first
Trigger Key activation begins a Recording and the next activation ends it.
During the Recording, Voisu sends audio concurrently to Deepgram and Groq so
network processing overlaps speech.

After stop, Voisu sends the final audio tail and observes a configurable
Provider Deadline. When two valid Source Transcripts arrive, it reconciles and
validates them. When only one valid provider finishes within the deadline, that
Source Transcript proceeds without waiting indefinitely. Voisu then delivers
one final Transcript to the focused application through compositor-authorized
libei input, retaining the clipboard as a guaranteed fallback.

The daemon works without a graphical process. Once the daemon is proven
reliable, a separate lightweight GTK4 Overlay will display Recording,
processing, success, and failure states through versioned Unix IPC.

## User Stories

1. As a Fedora user, I want Voisu to run in my desktop session, so that dictation is available whenever I need it.
2. As a user, I want to approve a global Trigger Key through my desktop, so that Voisu does not need raw keyboard access.
3. As a user, I want one Trigger Key press to start a Recording, so that I do not need to hold a key while speaking.
4. As a user, I want the next Trigger Key press to stop the Recording, so that the interaction is predictable.
5. As a user, I want a forgotten Recording to stop at a safety limit, so that my microphone and cloud usage cannot run indefinitely.
6. As a user, I want `voisu start` to begin a Recording, so that I can integrate Voisu with desktop-specific shortcuts.
7. As a user, I want `voisu stop` to end and process a Recording, so that automation has an explicit control surface.
8. As a user, I want `voisu toggle` to mirror the Trigger Key, so that simple desktop bindings remain possible.
9. As a user, I want daemon lifecycle commands under `voisu service`, so that service control cannot be confused with Recording control.
10. As a user, I want `voisu status` to report current state, so that I can tell whether Voisu is recording, processing, delivering, or unavailable.
11. As a user, I want Voisu to discover and capture my default PipeWire microphone, so that Fedora audio configuration works naturally.
12. As a user, I want to choose another microphone, so that headsets and external devices work reliably.
13. As a user, I want microphone readiness checked before Recording, so that setup failures are explained before I speak.
14. As a user, I want audio streamed while I speak, so that most provider latency overlaps my Recording.
15. As a user, I want Deepgram and Groq to process the same Recording independently, so that one provider can correct or replace the other.
16. As a user, I want the final audio tail included after stop, so that the last spoken words are not lost.
17. As a user, I want Voisu to avoid waiting indefinitely for a slow provider, so that release-to-text latency stays bounded.
18. As a user, I want one valid provider to be enough when the other fails, so that a partial cloud outage does not block dictation.
19. As a user, I want two materially different Source Transcripts reconciled, so that words and formatting benefit from both providers.
20. As a user, I want near-identical Source Transcripts selected deterministically, so that Voisu does not add unnecessary merge latency.
21. As a user, I want unsafe or obviously corrupted candidate text rejected, so that prompt artifacts and hallucinated content are not inserted.
22. As a user, I want only one final Transcript delivered, so that partial corrections never damage text in my application.
23. As a user, I want the final Transcript inserted into my focused application, so that dictation feels faster than manual clipboard use.
24. As a user, I want Voisu to request desktop permission for automatic Delivery, so that input emulation remains under my control.
25. As a user, I want the Transcript placed on the clipboard when direct Delivery is unavailable, so that my speech is never silently lost.
26. As a user, I want Delivery failure reported clearly, so that I know to paste from the clipboard.
27. As a user, I want API keys stored outside ordinary configuration where the desktop supports secure secret storage, so that credentials are not casually exposed.
28. As a user, I want setup to verify provider authentication, so that invalid keys are found before a real Recording.
29. As a user, I want provider model identifiers configurable, so that supported cloud models can change without recompiling Voisu.
30. As a user, I want my provider deadlines and safety limits configurable within safe bounds, so that Voisu can be tuned from evidence.
31. As a user, I want each Recording represented by one correlation ID, so that every stage of a failure can be traced together.
32. As a user, I want local timing events for capture, chunk upload, provider completion, reconciliation, validation, and Delivery, so that latency can be improved scientifically.
33. As a user, I want Source Transcripts and final Transcripts retained locally for a bounded period, so that recognition and merge errors can be reviewed.
34. As a user, I want raw audio deleted by default, so that ordinary dictation does not create a permanent voice archive.
35. As a user, I want debug audio capture to require explicit activation and expire automatically, so that fixed-input reproduction is available without indefinite retention.
36. As a user, I want diagnostics to stay local unless I export them, so that Voisu does not become an unannounced telemetry collector.
37. As a user, I want a diagnostic export to redact secrets, so that I can share evidence safely.
38. As a user, I want the daemon to survive an Overlay crash, so that visual feedback cannot break dictation.
39. As a user, I want the GTK4 Overlay hidden while idle, so that it consumes no attention and negligible rendering work.
40. As a user, I want the Overlay to show voice activity while Recording, so that I know the microphone is receiving speech.
41. As a user, I want the Overlay to show processing, success, and failure states, so that the asynchronous workflow is understandable.
42. As a user, I want a notification or regular-window fallback when Layer Shell is unavailable, so that unsupported compositors still provide feedback.
43. As a user, I want Voisu to start automatically after login, so that I do not manage the daemon manually every day.
44. As a user, I want a Fedora-native installation and removal path, so that service files and desktop permissions do not become stale.
45. As a developer, I want the standard test suite to avoid microphones, desktop permission dialogs, and paid APIs, so that TDD remains fast and repeatable.
46. As a developer, I want opt-in live Fedora smoke tests, so that adapter contracts are verified against the real desktop and cloud services.
47. As a developer, I want every implementation slice to follow RED, GREEN, and REFACTOR, so that behavior is specified before production code expands.
48. As a maintainer, I want adapted MIT work attributed, so that Voisu remains independent without erasing its influences.

## Implementation Decisions

- Use Rust and Cargo for the daemon, CLI, tests, and later GTK4 process. Cargo
  owns dependency resolution, builds, tests, formatting, linting, and lockfile
  reproducibility.
- Keep the CLI thin. It communicates with the running daemon through a
  versioned Unix socket and does not duplicate Recording orchestration.
- Place the socket and disposable process state under the XDG runtime directory;
  place durable configuration, history, and logs under their appropriate XDG
  base directories.
- Model daemon behavior as explicit states with rejected invalid transitions.
  At minimum the observable lifecycle covers idle, Recording, finalizing,
  reconciling, delivering, and recoverable failure.
- Represent audio capture behind a PipeWire boundary. Normalize the provider
  stream to a documented mono PCM contract while allowing PipeWire to negotiate
  the physical device format.
- Stream Deepgram continuously and submit bounded, overlapping chunks to Groq.
  Chunk numbering and timing make reordering, duplication, missing tails, and
  retry behavior observable.
- Treat provider model identifiers, chunk policy, Provider Deadline, Recording
  Deadline, language, and vocabulary hints as validated configuration.
- Prefer deterministic Source Transcript selection when the providers agree.
  Invoke cloud reconciliation only when the difference is material enough to
  justify its latency and cost.
- Validate every candidate before Delivery. A Quality Failure may trigger one
  bounded repair attempt or selection of a clean Source Transcript; it never
  exposes partial candidate text to the focused application.
- Use the XDG Global Shortcuts portal as the Fedora Trigger Key integration.
  Retain explicit CLI Recording commands for desktop-specific bindings and
  recovery.
- Use the XDG Remote Desktop portal and libei for direct Delivery. Never require
  raw input-device membership or privileged `uinput` access on the standard
  Fedora path.
- Write the final Transcript to the clipboard before or as part of direct
  Delivery so a failed insertion has a recoverable user path.
- Prefer the desktop Secret Service for cloud credentials. Permit controlled
  environment-based credentials for development and headless diagnostics, but
  never write secrets into logs or diagnostic exports.
- Emit structured local events sharing one correlation ID. Source Transcripts
  and final Transcripts use bounded configurable retention. Raw audio is off by
  default, explicitly enabled for debugging, and automatically expires.
- Run as a systemd user service on Fedora. Service installation, upgrade, and
  removal must be idempotent and must not capture stale display-session values.
- Keep the GTK4 Overlay in a separately supervised process. It consumes a
  stable, versioned state stream and cannot own Recording or provider state.
- Use GTK4 Layer Shell where the compositor advertises support. Provide a
  regular GTK or notification fallback elsewhere.
- Keep the Overlay as a compact, system-font voice capsule that is hidden while
  idle and stops animation work when not visible. Final visual tokens require a
  later prototype and `DESIGN.md` approval.
- Fedora KDE Wayland is the release gate. APT/DEB packaging and broad desktop
  certification begin only after the Fedora milestone succeeds.

## Testing Decisions

- The primary acceptance seam is the running daemon's public CLI and versioned
  Unix IPC. Tests issue user commands and observe public state, Delivery, and
  structured events rather than calling internal functions.
- The standard acceptance harness uses the real daemon orchestration with test
  implementations only at external boundaries: audio capture, cloud transports,
  desktop portals, clipboard, clock, and filesystem locations.
- Each behavior is built as one vertical RED -> GREEN cycle. Tests are not
  written as a large horizontal batch ahead of implementation.
- State-transition tests verify observable rejection and recovery behavior, not
  private enum layout or function calls.
- Provider tests use local HTTP/WebSocket servers to exercise request framing,
  streaming, deadlines, reordering, retries, and malformed responses without
  spending cloud credits.
- Audio tests use deterministic PCM/WAV fixtures through the public capture
  contract. Real PipeWire devices are reserved for an opt-in Fedora smoke suite.
- Portal tests use a controlled D-Bus service implementing the required public
  interfaces. They cover permission granted, denied, revoked, unavailable, and
  restored behavior.
- Delivery tests observe the public Delivery result and fallback clipboard,
  including Unicode text and applications that reject emulated input.
- Diagnostic tests verify correlation, redaction, retention, expiry, and export
  behavior through CLI output and files in isolated XDG directories.
- Crash tests terminate provider connections, the CLI, and the later Overlay to
  prove the daemon returns to a usable state without corrupting the next
  Recording.
- The opt-in live Fedora suite verifies a real microphone, KDE portals, Groq,
  Deepgram, libei Delivery, systemd ownership, login restart, and the exact
  installed build.
- Performance measurements report Recording stop to Delivery latency and its
  component spans. Initial thresholds are baselines to tune from real usage,
  not guessed claims that silently change behavior.
- Prior art comes from HyprVox's end-to-end toggle flow, provider fallbacks,
  transcript-quality fixtures, overlay isolation, and replay diagnostics, but
  Voisu tests are written against Voisu's public language and interfaces.

## Out of Scope

- Running a local speech model.
- Training or fine-tuning a speech model.
- Live partial text insertion.
- A privileged raw-keyboard or `/dev/uinput` helper for the standard path.
- Automatic upload of telemetry, Source Transcripts, or audio.
- Building the GTK4 Overlay before the daemon is accepted as reliable.
- Full GNOME, Hyprland, Sway, COSMIC, X11, RPM-family, and DEB-family
  certification in the first Fedora milestone.
- Mobile, Windows, and macOS clients.
- A general-purpose voice assistant, command execution, or conversational agent.

## Further Notes

- `voisu start`, `voisu stop`, and `voisu toggle` control a Recording.
  `voisu service start|stop|restart|status` controls the daemon.
- The repository must include attribution for any deliberately adapted
  MIT-licensed HyprVox logic.
- Overlay design follows the project design system and requires rendered
  screenshot critique before it can be considered complete.
- Package and model versions will be locked when their implementation tickets
  begin, based on then-current Fedora and provider support.

