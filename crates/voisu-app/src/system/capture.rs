// PipeWire capture: pw-record subprocess, WAV stripping, recording deadlines and errors.
//
// Split out of system.rs as a pure move; module-global items come from `super`.

use super::*;

pub(super) struct ProcessOutcome {
    pub(super) success: bool,
    pub(super) stdout: Vec<u8>,
    pub(super) stderr: Vec<u8>,
}

pub(super) enum ProcessError {
    Unavailable,
    Input,
    TimedOut,
    Wait,
    Output,
}

pub struct PipeWireCapture {
    reaper: ProviderReaper,
    levels: LevelRegistry,
}

impl PipeWireCapture {
    pub fn new(reaper: ProviderReaper, levels: LevelRegistry) -> Self {
        Self { reaper, levels }
    }
}

pub(super) struct CaptureReaderState {
    pub(super) chunks: VecDeque<AudioChunk>,
    pub(super) received_bytes: usize,
    pub(super) eof: bool,
    pub(super) error: Option<String>,
    pub(super) buffer_cap_reached: bool,
}

struct InjectedCaptureReadFailure<R> {
    inner: R,
    bytes_before_failure: usize,
}

impl<R: Read> Read for InjectedCaptureReadFailure<R> {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        if self.bytes_before_failure == 0 {
            return Err(std::io::Error::other(
                "controlled production capture read failure",
            ));
        }
        let maximum = output.len().min(self.bytes_before_failure);
        let read = self.inner.read(&mut output[..maximum])?;
        self.bytes_before_failure = self.bytes_before_failure.saturating_sub(read);
        Ok(read)
    }
}

fn injected_capture_read_failure_after_bytes() -> Option<usize> {
    (std::env::var_os("VOISU_TEST_MODE").as_deref()
        == Some(std::ffi::OsStr::new("system-boundaries")))
    .then(|| {
        std::env::var("VOISU_TEST_CAPTURE_READ_ERROR_AFTER_BYTES")
            .ok()?
            .parse()
            .ok()
    })
    .flatten()
}

pub(super) fn read_capture_stream(
    mut stdout: impl Read,
    reader_state: Arc<Mutex<CaptureReaderState>>,
    level_ring: Option<Arc<crate::audio_level::LevelRing>>,
    pcm_byte_cap: usize,
) {
    let mut buffer = [0_u8; 640];
    let mut assembler = PcmChunkAssembler::default();
    let mut band_state = BandState::default();
    let mut decoder = SampleDecoder::default();
    loop {
        match stdout.read(&mut buffer) {
            Ok(0) => {
                let mut state = reader_state.lock().unwrap();
                if let Some(tail) = assembler.finish() {
                    state.chunks.push_back(AudioChunk(tail));
                }
                state.eof = true;
                return;
            }
            Ok(read) => {
                if let Some(level_ring) = level_ring.as_ref() {
                    let samples = decoder.decode(&buffer[..read]);
                    // A read that completes no sample pushes no frame: an
                    // all-zero frame would advance the ring sequence and could
                    // evict real peaks.
                    if !samples.is_empty() {
                        level_ring.push(bands(&samples, &mut band_state));
                    }
                }
                let mut state = reader_state.lock().unwrap();
                let retained = read.min(pcm_byte_cap.saturating_sub(state.received_bytes));
                state.received_bytes = state.received_bytes.saturating_add(read);
                if retained > 0 {
                    for chunk in assembler.push(&buffer[..retained]) {
                        state.chunks.push_back(AudioChunk(chunk));
                    }
                }
                if retained < read {
                    state.buffer_cap_reached = true;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => {
                let mut state = reader_state.lock().unwrap();
                // A WAV-container format/boundary problem carries a specific,
                // actionable message; anything else is the generic read
                // failure.
                state.error = Some(match error.kind() {
                    std::io::ErrorKind::InvalidData | std::io::ErrorKind::UnexpectedEof => {
                        error.to_string()
                    }
                    _ => "pw-record audio read failed".to_owned(),
                });
                state.eof = true;
                return;
            }
        }
    }
}

/// The capture mode the host's `pw-record` supports.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PwRecordProbe {
    /// `pw-record` could not be run at all (missing/broken).
    Unavailable,
    /// `--raw` is understood: headerless PCM on stdout (PipeWire >= 1.1, Fedora).
    Raw,
    /// `--raw` is absent (PipeWire 1.0.5, Ubuntu 24.04): `pw-record` wraps the
    /// PCM in a WAV container that must be unwrapped.
    Wav,
}

/// Probe the host `pw-record` once, caching for the process lifetime. `--raw`
/// is a *newer* PipeWire option — absent on 1.0.5, what Ubuntu 24.04 LTS ships
/// — so passing it there makes `pw-record` reject the whole invocation. A
/// version-number comparison was rejected as fragile across distro backports;
/// this parses `pw-record --help` for an exact `--raw` option token.
/// `VOISU_TEST_PW_RECORD_RAW` forces the answer for hermetic tests.
pub(super) fn pw_record_capture_mode() -> PwRecordProbe {
    static MODE: OnceLock<PwRecordProbe> = OnceLock::new();
    *MODE.get_or_init(|| {
        if let Some(forced) = std::env::var_os("VOISU_TEST_PW_RECORD_RAW") {
            match forced.to_string_lossy().trim() {
                "0" | "wav" => return PwRecordProbe::Wav,
                "unavailable" => return PwRecordProbe::Unavailable,
                // `probe` exercises the real `pw-record --help` parse below
                // against a fake pw-record; anything else forces the raw path.
                "probe" => {}
                _ => return PwRecordProbe::Raw,
            }
        }
        // `--help` may print to either stream and exit nonzero on some builds;
        // inspect both regardless of exit status. A spawn failure (Err) means
        // pw-record cannot be run at all.
        match run_restricted("pw-record", &["--help"], None, true) {
            Ok(outcome) => {
                if help_advertises_raw(&outcome.stdout) || help_advertises_raw(&outcome.stderr) {
                    PwRecordProbe::Raw
                } else {
                    PwRecordProbe::Wav
                }
            }
            Err(_) => PwRecordProbe::Unavailable,
        }
    })
}

/// True only when the help text lists `--raw` as an exact option token. Splitting
/// on option separators (whitespace, `,`, `=`) rejects near-matches like
/// `--raw-file` and `--rawmode` that a substring search would accept.
pub(super) fn help_advertises_raw(help: &[u8]) -> bool {
    String::from_utf8_lossy(help)
        .split(|character: char| character.is_whitespace() || character == ',' || character == '=')
        .any(|token| token == "--raw")
}

/// Strips the RIFF/WAVE framing from `pw-record` output when the tool lacks
/// `--raw` and therefore emits a WAV container. It buffers only the leading
/// header while walking the chunk chain to the `data` payload (validating the
/// format on the way), then passes the PCM through unchanged — so the existing
/// chunk reader stays oblivious to the container. A header that never resolves
/// within the retained-stdout ceiling, or whose format is wrong, is surfaced as
/// a read error and becomes a capture boundary error rather than wrong-format
/// audio reaching a provider.
pub(super) struct WavHeaderStripper<R: Read> {
    inner: R,
    scan: Vec<u8>,
    pending: Vec<u8>,
    pending_pos: usize,
    header_done: bool,
}

impl<R: Read> WavHeaderStripper<R> {
    pub(super) fn new(inner: R) -> Self {
        Self {
            inner,
            scan: Vec::new(),
            pending: Vec::new(),
            pending_pos: 0,
            header_done: false,
        }
    }
}

impl<R: Read> Read for WavHeaderStripper<R> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        loop {
            if self.pending_pos < self.pending.len() {
                let available = self.pending.len() - self.pending_pos;
                let take = available.min(out.len());
                out[..take]
                    .copy_from_slice(&self.pending[self.pending_pos..self.pending_pos + take]);
                self.pending_pos += take;
                if self.pending_pos == self.pending.len() {
                    self.pending.clear();
                    self.pending_pos = 0;
                }
                return Ok(take);
            }
            if self.header_done {
                return self.inner.read(out);
            }
            let mut buffer = [0_u8; PCM_CHUNK_BYTES];
            let read = self.inner.read(&mut buffer)?;
            if read == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "pw-record WAV stream ended before its data chunk",
                ));
            }
            self.scan.extend_from_slice(&buffer[..read]);
            match scan_wav_pcm(&self.scan) {
                WavScan::Incomplete => {
                    if self.scan.len() > MAX_RETAINED_STDOUT_BYTES {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "pw-record WAV header did not resolve within the bounded prefix",
                        ));
                    }
                }
                WavScan::Invalid(reason) => {
                    return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, reason));
                }
                WavScan::DataAt(offset) => {
                    self.pending = self.scan.split_off(offset);
                    self.scan = Vec::new();
                    self.pending_pos = 0;
                    self.header_done = true;
                }
            }
        }
    }
}

impl AudioCapture for PipeWireCapture {
    fn begin(&mut self, _recording_id: u64) -> Result<Box<dyn ActiveCapture>, BoundaryError> {
        let maximum = resolve_recording_maximum(std::env::var("VOISU_RECORDING_DEADLINE_MS").ok());
        let pcm_byte_cap = maximum.pcm_byte_cap;
        // `--raw` yields headerless PCM directly (the Fedora path); without it
        // pw-record wraps the same PCM in a WAV container that WavHeaderStripper
        // unwraps below. The remaining flags are identical on both paths. An
        // Unavailable probe still takes the WAV path — the spawn below fails
        // cleanly if pw-record is truly missing.
        let raw_supported = pw_record_capture_mode() == PwRecordProbe::Raw;
        let mut command = restricted_command("pw-record");
        if raw_supported {
            command.arg("--raw");
        }
        command.args(["--rate", "16000", "--channels", "1", "--format", "s16"]);
        if let Some(target) = std::env::var_os("VOISU_PIPEWIRE_TARGET") {
            command.arg("--target").arg(target);
        }
        command
            .arg("-")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let state = Arc::new(Mutex::new(CaptureReaderState {
            chunks: VecDeque::new(),
            received_bytes: 0,
            eof: false,
            error: None,
            buffer_cap_reached: false,
        }));
        let reader_state = Arc::clone(&state);
        let level_ring = self.levels.current();
        // pw-record MUST be spawned from the reader thread, never from the
        // caller: `guard_external_child` arms PR_SET_PDEATHSIG, and the kernel
        // delivers that signal when the FORKING THREAD exits, not the process.
        // The caller runs on a transient Tokio blocking-pool thread that is
        // reaped after ~10 s idle, which SIGKILLed every Recording longer than
        // that. The reader thread lives until the capture ends, so parent-death
        // delivery degrades to exactly the daemon-death contract intended.
        let (handoff_tx, handoff_rx) =
            std::sync::mpsc::channel::<Result<(Child, std::process::ChildStderr), &'static str>>();
        let reader = thread::spawn(move || {
            let mut child = match command.spawn() {
                Ok(child) => child,
                Err(_) => {
                    let _ = handoff_tx.send(Err("pw-record unavailable"));
                    return;
                }
            };
            let stdout = child.stdout.take();
            let stderr = child.stderr.take();
            let (Some(stdout), Some(stderr)) = (stdout, stderr) else {
                let _ = child.kill();
                let _ = child.wait();
                let _ = handoff_tx.send(Err("pw-record stdout unavailable"));
                return;
            };
            // Headerless PCM (`--raw`) is read straight through; a WAV container
            // (no `--raw`) is unwrapped to its PCM payload first.
            let stdout: Box<dyn Read + Send> = if raw_supported {
                Box::new(stdout)
            } else {
                Box::new(WavHeaderStripper::new(stdout))
            };
            let stdout: Box<dyn Read + Send> = match injected_capture_read_failure_after_bytes() {
                Some(bytes_before_failure) => Box::new(InjectedCaptureReadFailure {
                    inner: stdout,
                    bytes_before_failure,
                }),
                None => stdout,
            };
            if let Err(returned) = handoff_tx.send(Ok((child, stderr))) {
                // begin() is blocked on recv, so this only happens if it
                // panicked; reclaim the child rather than leaking it.
                if let Ok((mut child, _)) = returned.0 {
                    let _ = child.kill();
                    let _ = child.wait();
                }
                return;
            }
            read_capture_stream(stdout, reader_state, level_ring, pcm_byte_cap);
        });
        let (child, mut stderr) = handoff_rx
            .recv()
            .map_err(|_| BoundaryError::new(BoundaryKind::Capture, "pw-record unavailable"))?
            .map_err(|message| BoundaryError::new(BoundaryKind::Capture, message))?;
        let stderr_reader = thread::spawn(move || {
            read_capped(&mut stderr, MAX_RETAINED_STDERR_BYTES).unwrap_or_default()
        });
        Ok(Box::new(PipeWireActiveCapture {
            child: Some(child),
            state,
            reader: Some(reader),
            stderr_reader: Some(stderr_reader),
            cleanup: None,
            reaper: self.reaper.clone(),
            pcm: Vec::new(),
            started: Instant::now(),
            deadline: maximum.deadline,
        }))
    }
}

/// Default ceiling on a single Recording before the Recording Deadline stops
/// it. Recordings routinely run past two minutes (the provider chunking path
/// exists for exactly those), so the default must be generous; a stuck or
/// forgotten Recording is still bounded. `VOISU_RECORDING_DEADLINE_MS` may
/// shorten it but never lengthen it: [`MAX_RECORDING_DURATION`] is an absolute
/// ceiling, not merely this default.
pub(super) const DEFAULT_RECORDING_DEADLINE: Duration = MAX_RECORDING_DURATION;

/// The one resolved maximum for a Recording: the wall-clock Deadline, and the
/// retained-PCM byte cap derived from it.
pub(super) struct RecordingMaximum {
    pub(super) deadline: Duration,
    pub(super) pcm_byte_cap: usize,
}

/// Parse the raw `VOISU_RECORDING_DEADLINE_MS` value. A parseable, non-zero
/// millisecond count is an override; anything else — absent, unparseable, or
/// zero — is `None`. Shared, so the resolver and the startup notice can never
/// disagree about what counts as an override.
fn parse_recording_deadline_override(raw: Option<String>) -> Option<Duration> {
    raw.and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_millis)
        .filter(|value| !value.is_zero())
}

/// One-shot startup diagnostic for an override longer than the absolute
/// ceiling. The resolver shortens it silently — correct, but an operator who
/// configured twenty minutes and is stopped at ten would otherwise meet a limit
/// they were never told about. `None` when there is nothing to say. This
/// belongs at startup rather than in the resolver, which runs on every
/// Recording and would repeat the line per Recording.
pub fn recording_deadline_override_notice(raw: Option<String>) -> Option<String> {
    let requested = parse_recording_deadline_override(raw)?;
    (requested > MAX_RECORDING_DURATION).then(|| {
        format!(
            "VOISU_RECORDING_DEADLINE_MS={} ms exceeds the {} s maximum Recording length; \
             Recordings stop at {} s",
            requested.as_millis(),
            MAX_RECORDING_DURATION.as_secs(),
            MAX_RECORDING_DURATION.as_secs(),
        )
    })
}

/// The Deadline a capture implementation resolves for ITSELF at `begin()`,
/// through the one resolver every enforcer shares.
///
/// Only a capture may call this, and only to stamp its own
/// [`DeadlineClock`] — the observer path reads that clock back off the capture
/// rather than re-resolving here, so there is never a second answer to compare
/// against the running one.
pub fn capture_deadline() -> Duration {
    resolve_recording_maximum(std::env::var("VOISU_RECORDING_DEADLINE_MS").ok()).deadline
}

/// Resolve the Recording maximum from the raw `VOISU_RECORDING_DEADLINE_MS`
/// value. An override wins up to [`MAX_RECORDING_DURATION`]; absent, zero or
/// unparseable uses [`DEFAULT_RECORDING_DEADLINE`], and an over-long override
/// is clamped to the ceiling. The byte cap is derived from the resolved
/// Deadline so the two enforcers cannot diverge, then floored at
/// [`MIN_RECORDING_BYTES`] — the floor only ever raises the cap, so it can
/// never make the cap fire before the Deadline.
pub(super) fn resolve_recording_maximum(raw: Option<String>) -> RecordingMaximum {
    let deadline = parse_recording_deadline_override(raw)
        .unwrap_or(DEFAULT_RECORDING_DEADLINE)
        // MAX_RECORDING_DURATION is an absolute ceiling, not merely a default.
        // An operator override may only shorten a Recording: retained PCM is
        // held in memory, so an unclamped override (say an hour) would grow the
        // buffer without bound. Clamping the deadline — the single value both
        // enforcers derive from — keeps them from diverging.
        .min(MAX_RECORDING_DURATION);
    let derived_cap = deadline
        .as_millis()
        .saturating_mul(16_000 * 2)
        .checked_div(1_000)
        .unwrap_or_default()
        .min(usize::MAX as u128) as usize;
    // A deadline under 100 ms derives a cap below the minimum deliverable
    // Recording, so the cap alone would turn every hit into TooShortRecording —
    // total loss, the exact defect this ticket exists to remove. The floor costs
    // nothing at any realistic deadline.
    let pcm_byte_cap = derived_cap.max(MIN_RECORDING_BYTES);
    RecordingMaximum {
        deadline,
        pcm_byte_cap,
    }
}

pub(super) struct PipeWireActiveCapture {
    pub(super) child: Option<Child>,
    pub(super) state: Arc<Mutex<CaptureReaderState>>,
    pub(super) reader: Option<thread::JoinHandle<()>>,
    pub(super) stderr_reader: Option<thread::JoinHandle<Vec<u8>>>,
    pub(super) cleanup: Option<tokio::task::JoinHandle<Result<Vec<u8>, BoundaryError>>>,
    pub(super) reaper: ProviderReaper,
    pub(super) pcm: Vec<u8>,
    pub(super) started: Instant,
    pub(super) deadline: Duration,
}

impl PipeWireActiveCapture {
    fn drain_chunks(&mut self) {
        let mut state = self.state.lock().unwrap();
        while let Some(chunk) = state.chunks.pop_front() {
            self.pcm.extend_from_slice(&chunk.0);
        }
    }

    async fn stop_child(&mut self, graceful: bool) -> Result<Vec<u8>, BoundaryError> {
        if self.cleanup.is_none() {
            let child = self.child.take().ok_or_else(|| {
                BoundaryError::new(BoundaryKind::Capture, "pw-record already finalized")
            })?;
            let reader = self.reader.take();
            let stderr_reader = self.stderr_reader.take();
            self.cleanup = Some(tokio::task::spawn_blocking(move || {
                stop_child_blocking(child, reader, stderr_reader, graceful)
            }));
        }
        let result = self
            .cleanup
            .as_mut()
            .expect("capture cleanup is present")
            .await;
        self.cleanup.take();
        result.map_err(|_| {
            BoundaryError::new(BoundaryKind::Capture, "pw-record cleanup task failed")
        })?
    }

    fn validate_audio(&self) -> Result<(), BoundaryError> {
        if self.pcm.is_empty() {
            return Err(BoundaryError::new(
                BoundaryKind::EmptyRecording,
                "pw-record returned no audio frames",
            ));
        }
        if self.pcm.len() < MIN_RECORDING_BYTES {
            return Err(BoundaryError::new(
                BoundaryKind::TooShortRecording,
                format!("Recording contained {} PCM bytes", self.pcm.len()),
            ));
        }
        let audible = self
            .pcm
            .as_chunks::<2>()
            .0
            .iter()
            .any(|sample| i16::from_le_bytes([sample[0], sample[1]]).unsigned_abs() > 32);
        if !audible {
            return Err(BoundaryError::new(
                BoundaryKind::SilentRecording,
                "Recording peak amplitude did not exceed the silence floor",
            ));
        }
        Ok(())
    }
}

pub(super) fn stop_child_blocking(
    mut child: Child,
    reader: Option<thread::JoinHandle<()>>,
    stderr_reader: Option<thread::JoinHandle<Vec<u8>>>,
    graceful: bool,
) -> Result<Vec<u8>, BoundaryError> {
    // A tool that already exited before the stop failed on its own; only a
    // process that was still capturing when interrupted may exit nonzero.
    let exited_before_stop = matches!(child.try_wait(), Ok(Some(_)));
    if graceful {
        if let Ok(pid) = child.id().try_into() {
            unsafe {
                libc::kill(pid, libc::SIGINT);
            }
        }
    } else {
        let _ = child.kill();
    }
    let stopped = Instant::now();
    let status = wait_for_child(&mut child, stopped, PROCESS_DEADLINE, None);
    let reader = reader.map(|handle| bounded_join(handle, stopped, &mut child, PROCESS_DEADLINE));
    let stderr =
        stderr_reader.map(|handle| bounded_join(handle, stopped, &mut child, PROCESS_DEADLINE));
    if !matches!(reader, None | Some(Ok(()))) {
        return Err(BoundaryError::new(
            BoundaryKind::Capture,
            "pw-record audio drain failed",
        ));
    }
    let stderr = match stderr {
        Some(Ok(bytes)) => bytes,
        None => Vec::new(),
        Some(Err(_)) => {
            return Err(BoundaryError::new(
                BoundaryKind::Capture,
                "pw-record diagnostic drain failed",
            ));
        }
    };
    let status = status.map_err(|error| capture_process_error(error, &stderr))?;
    let expected_signal = if graceful {
        libc::SIGINT
    } else {
        libc::SIGKILL
    };
    // Real pw-record catches SIGINT and exits nonzero with no diagnostics
    // rather than dying by the signal; that silent nonzero exit is its
    // normal interrupted shape, not a failure. Anything with diagnostics,
    // or that had already died before the interrupt, stays rejected.
    let interrupted_cleanly = graceful && !exited_before_stop && stderr.is_empty();
    if !status.success() && status.signal() != Some(expected_signal) && !interrupted_cleanly {
        return Err(BoundaryError::new(
            BoundaryKind::Capture,
            process_diagnostic("pw-record failed", &stderr),
        ));
    }
    Ok(stderr)
}

impl ActiveCapture for PipeWireActiveCapture {
    fn deadline_clock(&self) -> DeadlineClock {
        // The same pair `next_chunk` enforces against, one field read apart —
        // there is no second resolution and no second clock to disagree.
        DeadlineClock {
            started: self.started,
            deadline: self.deadline,
        }
    }

    fn next_chunk(&mut self) -> BoundaryFuture<'_, Option<AudioChunk>> {
        Box::pin(async move {
            loop {
                if self.started.elapsed() >= self.deadline {
                    return Err(BoundaryError::new(
                        BoundaryKind::RecordingDeadline,
                        "configured Recording Deadline elapsed",
                    ));
                }
                let next = {
                    let mut state = self.state.lock().unwrap();
                    if let Some(error) = state.error.clone() {
                        return Err(BoundaryError::new(BoundaryKind::Capture, error));
                    }
                    (
                        state.chunks.pop_front(),
                        state.eof || state.buffer_cap_reached,
                    )
                };
                match next {
                    (Some(chunk), _) => {
                        self.pcm.extend_from_slice(&chunk.0);
                        return Ok(Some(chunk));
                    }
                    (None, true) => return Ok(None),
                    (None, false) => tokio::time::sleep(PROCESS_POLL).await,
                }
            }
        })
    }

    fn finish(&mut self) -> BoundaryFuture<'_, CapturedAudio> {
        Box::pin(async move {
            self.stop_child(true).await?;
            self.drain_chunks();
            if let Some(error) = self.state.lock().unwrap().error.clone() {
                return Err(BoundaryError::new(BoundaryKind::Capture, error));
            }
            self.validate_audio()?;
            let pcm = std::mem::take(&mut self.pcm);
            if self.state.lock().unwrap().buffer_cap_reached {
                Ok(CapturedAudio::truncated(pcm, CaptureLimit::Buffer))
            } else {
                Ok(CapturedAudio::new(pcm))
            }
        })
    }

    fn abort(mut self: Box<Self>) -> BoundaryFuture<'static, ()> {
        Box::pin(async move {
            self.stop_child(false).await?;
            Ok(())
        })
    }
}

impl Drop for PipeWireActiveCapture {
    fn drop(&mut self) {
        if let Some(cleanup) = self.cleanup.take() {
            // An outer abort deadline may drop stop_child after it transferred
            // pw-record and both reader handles to spawn_blocking. Retain that
            // task in the actor-owned supervisor; every workflow drains it before
            // its acknowledgement permits the next Recording.
            self.reaper.adopt_capture(cleanup);
        } else if let Some(child) = self.child.take() {
            // stop_child never ran: capture_pump panicked or was cancelled while
            // still owning a live pw-record. Killing under reap_briefly's 250 ms
            // and then dropping the child and both reader handles would let a
            // slow-exiting child — or a descendant holding the pipe — outlive
            // Drop while the reaper looks empty, so supervise_recording could
            // permit Idle mid-cleanup. Hand the raw child and reader handles to
            // the reaper's bounded kill/reap instead; every Idle-permitting path
            // drains it before its acknowledgement releases the next Recording.
            self.reaper.adopt_capture_blocking(
                child,
                self.reader.take(),
                self.stderr_reader.take(),
            );
        }
    }
}

fn capture_process_error(error: ProcessError, stderr: &[u8]) -> BoundaryError {
    let detail = match error {
        ProcessError::Unavailable => "pw-record unavailable".to_owned(),
        ProcessError::TimedOut => "pw-record cleanup deadline elapsed".to_owned(),
        ProcessError::Input | ProcessError::Wait | ProcessError::Output => {
            process_diagnostic("pw-record execution failed", stderr)
        }
    };
    BoundaryError::new(BoundaryKind::Capture, detail)
}

fn process_diagnostic(prefix: &str, stderr: &[u8]) -> String {
    let detail = String::from_utf8_lossy(stderr);
    let detail = detail.trim();
    if detail.is_empty() {
        prefix.to_owned()
    } else {
        format!("{prefix}: {detail}")
    }
}
