//! Move mode: the user drags the Overlay pill to a new spot and saves it with
//! ✓ (or Enter) or drops it with ✗ (or Esc).
//!
//! # Why a socket of its own
//!
//! The Overlay is a read-only observer of the daemon: the daemon has no
//! command that reaches it, and its systemd sandbox (`ProtectSystem=strict`,
//! writes only under `%t`) cannot write `~/.config`. So the client that asks
//! for move mode — `voisu overlay-position move` or the TUI — talks to the
//! Overlay directly over a Unix socket beside the daemon's, in the private
//! runtime directory, and writes the config itself when the Overlay reports
//! where the pill was dropped. The daemon is not involved.
//!
//! The protocol is one line each way, then one more from the Overlay:
//!
//! ```text
//! client  → move
//! overlay → moving                       (or: error <why>)
//! overlay → save 0.250,0.100             (or: cancel user|timeout|recording)
//! ```
//!
//! The client holding the connection open is what keeps move mode alive: if
//! it goes away (Ctrl-C, a closed TUI), the Overlay cancels and restores.

use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::OverlayPosition;

/// With no drag, click or key for this long, move mode cancels by itself.
pub const MOVE_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a client waits for the Overlay to accept or refuse.
const ACK_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a client waits for the outcome once moving. Far past the idle
/// timeout; it only guards against an Overlay that hangs.
const OUTCOME_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// How long the Overlay waits for a connected client's request line.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
/// The longest request line read; anything longer is not a request.
const MAX_REQUEST: u64 = 64;

const MOVE_REQUEST: &str = "move";

/// The Overlay's move socket: `overlay.sock` beside the daemon's socket in the
/// private runtime directory.
pub fn socket_path() -> Result<PathBuf, String> {
    Ok(voisu_core::socket_path()?.with_file_name("overlay.sock"))
}

/// Why move mode ended without saving.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CancelReason {
    /// ✗ or Esc.
    User,
    /// No input for [`MOVE_IDLE_TIMEOUT`].
    Timeout,
    /// A Recording started; the pill shows it instead.
    Recording,
    /// The client that asked went away.
    RequesterGone,
}

impl CancelReason {
    const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Timeout => "timeout",
            Self::Recording => "recording",
            Self::RequesterGone => "gone",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        [
            Self::User,
            Self::Timeout,
            Self::Recording,
            Self::RequesterGone,
        ]
        .into_iter()
        .find(|reason| reason.as_str() == value)
    }
}

/// A line the Overlay sends to the client.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MoveReply {
    Moving,
    Error(String),
    Save(OverlayPosition),
    Cancel(CancelReason),
}

impl MoveReply {
    /// The reply as one protocol line, newline included.
    #[must_use]
    pub fn line(&self) -> String {
        match self {
            Self::Moving => "moving\n".to_owned(),
            // One line, whatever the message holds.
            Self::Error(message) => format!("error {}\n", message.replace('\n', " ")),
            Self::Save(position) => format!("save {position}\n"),
            Self::Cancel(reason) => format!("cancel {}\n", reason.as_str()),
        }
    }

    #[must_use]
    pub fn parse(line: &str) -> Option<Self> {
        let line = line.strip_suffix('\n').unwrap_or(line);
        if line == "moving" {
            return Some(Self::Moving);
        }
        let (word, rest) = line.split_once(' ')?;
        match word {
            "error" => Some(Self::Error(rest.to_owned())),
            "save" => OverlayPosition::parse(rest).map(Self::Save),
            "cancel" => CancelReason::parse(rest).map(Self::Cancel),
            _ => None,
        }
    }
}

/// How move mode ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MoveEnd {
    Save(OverlayPosition),
    Cancel(CancelReason),
}

impl MoveEnd {
    /// What the client is told.
    #[must_use]
    pub const fn reply(self) -> MoveReply {
        match self {
            Self::Save(position) => MoveReply::Save(position),
            Self::Cancel(reason) => MoveReply::Cancel(reason),
        }
    }
}

/// A key pressed while moving.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MoveKey {
    Enter,
    Escape,
    Other,
}

/// The state of one move: where the pill started, where it is now, and when
/// the user last did anything. The GTK side feeds it drags, keys and ticks and
/// acts on the [`MoveEnd`] it returns; ✗, Esc and every cancel leave the saved
/// position as it was.
#[derive(Clone, Copy, Debug)]
pub struct MoveMode {
    original: OverlayPosition,
    current: OverlayPosition,
    last_input: Instant,
}

impl MoveMode {
    #[must_use]
    pub const fn start(original: OverlayPosition, now: Instant) -> Self {
        Self {
            original,
            current: original,
            last_input: now,
        }
    }

    /// Where the pill was when move mode began; a cancel puts it back here.
    #[must_use]
    pub const fn original(&self) -> OverlayPosition {
        self.original
    }

    /// Where the pill is now.
    #[must_use]
    pub const fn current(&self) -> OverlayPosition {
        self.current
    }

    /// The pill was dragged to `position`.
    pub fn drag_to(&mut self, position: OverlayPosition, now: Instant) {
        self.current = position;
        self.last_input = now;
    }

    /// Any other pointer or key activity, which keeps move mode alive.
    pub fn touch(&mut self, now: Instant) {
        self.last_input = now;
    }

    /// ✓ was clicked.
    #[must_use]
    pub const fn confirm(&self) -> MoveEnd {
        MoveEnd::Save(self.current)
    }

    /// ✗ was clicked.
    #[must_use]
    pub const fn cancel(&self) -> MoveEnd {
        MoveEnd::Cancel(CancelReason::User)
    }

    /// Enter saves, Esc cancels, other keys only count as activity.
    pub fn key(&mut self, key: MoveKey, now: Instant) -> Option<MoveEnd> {
        match key {
            MoveKey::Enter => Some(self.confirm()),
            MoveKey::Escape => Some(self.cancel()),
            MoveKey::Other => {
                self.touch(now);
                None
            }
        }
    }

    /// Checked every Overlay tick: a Recording, a vanished client or a long
    /// idle ends move mode without saving.
    #[must_use]
    pub fn tick(&self, now: Instant, recording: bool, requester_gone: bool) -> Option<MoveEnd> {
        let reason = if recording {
            CancelReason::Recording
        } else if requester_gone {
            CancelReason::RequesterGone
        } else if now.saturating_duration_since(self.last_input) >= MOVE_IDLE_TIMEOUT {
            CancelReason::Timeout
        } else {
            return None;
        };
        Some(MoveEnd::Cancel(reason))
    }
}

/// The Overlay's end of the move socket. A thread accepts connections and
/// reads each request off the GTK thread; the GTK tick [`take`](Self::take)s
/// accepted requests without blocking.
pub struct MoveListener {
    requests: mpsc::Receiver<MoveRequester>,
    refusal: Arc<Mutex<Option<String>>>,
    path: PathBuf,
    /// The socket file's identity, so dropping removes only our own file.
    inode: Option<(u64, u64)>,
}

impl MoveListener {
    /// Listens on [`socket_path`], creating the private runtime directory.
    pub fn bind() -> Result<Self, String> {
        Self::bind_with(None)
    }

    /// Listens but answers every request with `error <reason>`, for a
    /// desktop where the pill cannot be shown or dragged. A client then gets a
    /// clear error instead of waiting for nothing.
    pub fn bind_refusing(reason: &str) -> Result<Self, String> {
        Self::bind_with(Some(reason.to_owned()))
    }

    fn bind_with(refusal: Option<String>) -> Result<Self, String> {
        let path = socket_path()?;
        let parent = path
            .parent()
            .ok_or_else(|| "the Overlay socket has no parent directory".to_owned())?;
        crate::daemon_lock::create_private_runtime_dirs(parent)?;
        Self::bind_at(&path, refusal)
    }

    /// Listens on `path`. A socket file nobody answers on is stale and
    /// replaced; one a live Overlay answers on is left alone.
    pub fn bind_at(path: &Path, refusal: Option<String>) -> Result<Self, String> {
        if path.symlink_metadata().is_ok() {
            if UnixStream::connect(path).is_ok() {
                return Err(format!(
                    "another Overlay is already listening on {}",
                    path.display()
                ));
            }
            std::fs::remove_file(path)
                .map_err(|error| format!("cannot replace {}: {error}", path.display()))?;
        }
        let listener = UnixListener::bind(path)
            .map_err(|error| format!("cannot listen on {}: {error}", path.display()))?;
        let (sender, requests) = mpsc::channel();
        let refusal = Arc::new(Mutex::new(refusal));
        thread::Builder::new()
            .name("overlay-move".to_owned())
            .spawn({
                let refusal = Arc::clone(&refusal);
                move || accept(&listener, &refusal, &sender)
            })
            .map_err(|error| format!("cannot start the move listener: {error}"))?;
        Ok(Self {
            requests,
            refusal,
            path: path.to_owned(),
            inode: inode(path),
        })
    }

    /// From now on, answers every request with `error <reason>`: the pill
    /// could not be shown after all.
    pub fn refuse(&self, reason: &str) {
        if let Ok(mut refusal) = self.refusal.lock() {
            *refusal = Some(reason.to_owned());
        }
    }

    /// The next client asking for move mode, if one is waiting.
    #[must_use]
    pub fn take(&self) -> Option<MoveRequester> {
        self.requests.try_recv().ok()
    }
}

fn inode(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let metadata = path.symlink_metadata().ok()?;
    Some((metadata.dev(), metadata.ino()))
}

/// Removing our socket file turns clients away at once ("isn't running") and
/// frees the path for the next listener. A file another Overlay has since
/// bound there is left alone.
impl Drop for MoveListener {
    fn drop(&mut self) {
        if self.inode.is_some() && inode(&self.path) == self.inode {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn accept(
    listener: &UnixListener,
    refusal: &Mutex<Option<String>>,
    sender: &mpsc::Sender<MoveRequester>,
) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        if !is_move_request(&stream) {
            let _ = (&stream).write_all(
                MoveReply::Error("unknown request".to_owned())
                    .line()
                    .as_bytes(),
            );
            continue;
        }
        let mut requester = MoveRequester { stream };
        let refused = refusal.lock().ok().and_then(|reason| reason.clone());
        if let Some(reason) = refused {
            requester.reply(&MoveReply::Error(reason));
            continue;
        }
        if requester.stream.set_nonblocking(true).is_err() || sender.send(requester).is_err() {
            // The Overlay stopped listening; the dropped stream tells the
            // client.
            return;
        }
    }
}

fn is_move_request(stream: &UnixStream) -> bool {
    if stream.set_read_timeout(Some(REQUEST_TIMEOUT)).is_err() {
        return false;
    }
    let mut line = String::new();
    BufReader::new(stream.take(MAX_REQUEST))
        .read_line(&mut line)
        .is_ok_and(|_| line.trim_end_matches('\n') == MOVE_REQUEST)
}

/// The client that asked for move mode, held for as long as it lasts.
pub struct MoveRequester {
    stream: UnixStream,
}

impl MoveRequester {
    /// Sends `reply`; false when the client is no longer there.
    pub fn reply(&mut self, reply: &MoveReply) -> bool {
        self.stream.write_all(reply.line().as_bytes()).is_ok()
    }

    /// True once the client has hung up. Never blocks.
    #[must_use]
    pub fn gone(&self) -> bool {
        let mut byte = [0_u8; 1];
        match (&self.stream).read(&mut byte) {
            Ok(0) => true,
            // Stray bytes after the request are ignored.
            Ok(_) => false,
            Err(error) => !matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted),
        }
    }
}

/// How a move ended, seen from the client.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MoveOutcome {
    /// Saved to the config at this position.
    Saved(OverlayPosition),
    Cancelled(CancelReason),
}

/// Asks the running Overlay for move mode, calls `on_moving` once the pill is
/// draggable, waits for ✓ or ✗ and saves a ✓ to the config.
pub fn move_overlay(on_moving: impl FnOnce()) -> Result<MoveOutcome, String> {
    move_overlay_at(
        &socket_path()?,
        |position| crate::config::set_overlay_position(position).map(drop),
        on_moving,
    )
}

/// [`move_overlay`] against `socket`, saving through `save`.
pub fn move_overlay_at(
    socket: &Path,
    save: impl FnOnce(OverlayPosition) -> Result<(), String>,
    on_moving: impl FnOnce(),
) -> Result<MoveOutcome, String> {
    let mut stream = UnixStream::connect(socket).map_err(|error| match error.kind() {
        ErrorKind::NotFound | ErrorKind::ConnectionRefused => {
            "The Overlay isn't running. Start it \
             with `systemctl --user start voisu-overlay`, then try again."
                .to_owned()
        }
        _ => format!("Couldn't reach the Overlay: {error}"),
    })?;
    stream
        .write_all(format!("{MOVE_REQUEST}\n").as_bytes())
        .map_err(|error| format!("Couldn't reach the Overlay: {error}"))?;
    let mut reader = BufReader::new(stream);
    match read_reply(&mut reader, ACK_TIMEOUT)? {
        MoveReply::Moving => on_moving(),
        MoveReply::Error(message) => return Err(message),
        other => return Err(format!("The Overlay answered out of turn: {other:?}")),
    }
    match read_reply(&mut reader, OUTCOME_TIMEOUT)? {
        MoveReply::Save(position) => {
            save(position)
                .map_err(|error| format!("Couldn't save the Overlay position: {error}"))?;
            Ok(MoveOutcome::Saved(position))
        }
        MoveReply::Cancel(reason) => Ok(MoveOutcome::Cancelled(reason)),
        MoveReply::Error(message) => Err(message),
        MoveReply::Moving => Err("The Overlay answered out of turn: Moving".to_owned()),
    }
}

fn read_reply(reader: &mut BufReader<UnixStream>, timeout: Duration) -> Result<MoveReply, String> {
    reader
        .get_ref()
        .set_read_timeout(Some(timeout))
        .map_err(|error| format!("Couldn't reach the Overlay: {error}"))?;
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) => Err("The Overlay stopped before the move finished.".to_owned()),
        Ok(_) => MoveReply::parse(&line)
            .ok_or_else(|| format!("The Overlay sent something unexpected: {}", line.trim_end())),
        Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => Err(
            "The Overlay didn't answer. Try `systemctl --user restart voisu-overlay`.".to_owned(),
        ),
        Err(error) => Err(format!("Couldn't reach the Overlay: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn at(x: f64, y: f64) -> OverlayPosition {
        OverlayPosition::from_fractions(x, y).unwrap()
    }

    fn next_requester(listener: &MoveListener) -> MoveRequester {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(requester) = listener.take() {
                return requester;
            }
            assert!(Instant::now() < deadline, "no move request arrived");
            thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn replies_round_trip_as_single_lines_and_junk_is_rejected() {
        for reply in [
            MoveReply::Moving,
            MoveReply::Save(at(0.25, 0.1)),
            MoveReply::Cancel(CancelReason::User),
            MoveReply::Cancel(CancelReason::Timeout),
            MoveReply::Cancel(CancelReason::Recording),
            MoveReply::Cancel(CancelReason::RequesterGone),
            MoveReply::Error("no layer shell".to_owned()),
        ] {
            assert_eq!(
                MoveReply::parse(&reply.line()),
                Some(reply.clone()),
                "{reply:?}"
            );
        }
        assert_eq!(MoveReply::Save(at(0.25, 0.1)).line(), "save 0.250,0.100\n");
        assert_eq!(MoveReply::Error("a\nb".to_owned()).line(), "error a b\n");
        for junk in ["", "save", "save 2,2", "cancel later", "moved", "Moving"] {
            assert_eq!(MoveReply::parse(junk), None, "{junk:?}");
        }
    }

    #[test]
    fn enter_or_check_saves_the_dragged_spot_and_esc_or_cross_cancels() {
        let now = Instant::now();
        let mut mode = MoveMode::start(OverlayPosition::DEFAULT, now);
        assert_eq!(mode.confirm(), MoveEnd::Save(OverlayPosition::DEFAULT));
        mode.drag_to(at(0.25, 0.1), now);
        assert_eq!(mode.current(), at(0.25, 0.1));
        assert_eq!(mode.original(), OverlayPosition::DEFAULT);
        assert_eq!(mode.confirm(), MoveEnd::Save(at(0.25, 0.1)));
        assert_eq!(
            mode.key(MoveKey::Enter, now),
            Some(MoveEnd::Save(at(0.25, 0.1)))
        );
        assert_eq!(mode.cancel(), MoveEnd::Cancel(CancelReason::User));
        assert_eq!(
            mode.key(MoveKey::Escape, now),
            Some(MoveEnd::Cancel(CancelReason::User))
        );
        assert_eq!(mode.key(MoveKey::Other, now), None);
    }

    #[test]
    fn idle_recording_or_a_vanished_client_cancel_and_input_keeps_it_alive() {
        let start = Instant::now();
        let mut mode = MoveMode::start(OverlayPosition::DEFAULT, start);
        let almost = start + MOVE_IDLE_TIMEOUT - Duration::from_millis(1);
        assert_eq!(mode.tick(almost, false, false), None);
        assert_eq!(
            mode.tick(start + MOVE_IDLE_TIMEOUT, false, false),
            Some(MoveEnd::Cancel(CancelReason::Timeout))
        );
        // A drag, or any other input, restarts the idle clock.
        mode.drag_to(at(0.3, 0.3), almost);
        assert_eq!(mode.tick(start + MOVE_IDLE_TIMEOUT, false, false), None);
        mode.touch(almost + Duration::from_secs(50));
        assert_eq!(
            mode.tick(almost + Duration::from_secs(100), false, false),
            None
        );
        let _ = mode.key(MoveKey::Other, almost + Duration::from_secs(100));
        assert_eq!(
            mode.tick(almost + Duration::from_secs(150), false, false),
            None
        );
        assert_eq!(
            mode.tick(start, true, true),
            Some(MoveEnd::Cancel(CancelReason::Recording)),
            "a Recording wins: the pill must show it"
        );
        assert_eq!(
            mode.tick(start, false, true),
            Some(MoveEnd::Cancel(CancelReason::RequesterGone))
        );
        assert_eq!(
            MoveEnd::Cancel(CancelReason::User).reply(),
            MoveReply::Cancel(CancelReason::User)
        );
    }

    #[test]
    fn a_client_saves_what_the_overlay_reports_after_it_starts_moving() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("overlay.sock");
        let listener = MoveListener::bind_at(&path, None).unwrap();
        let overlay = thread::spawn(move || {
            let mut requester = next_requester(&listener);
            assert!(requester.reply(&MoveReply::Moving));
            assert!(!requester.gone(), "the client is still waiting");
            assert!(requester.reply(&MoveReply::Save(at(0.25, 0.1))));
            listener
        });
        let saved = Arc::new(Mutex::new(None));
        let moving = Arc::new(Mutex::new(false));
        let outcome = move_overlay_at(
            &path,
            |position| {
                *saved.lock().unwrap() = Some(position);
                Ok(())
            },
            || *moving.lock().unwrap() = true,
        );
        assert_eq!(outcome, Ok(MoveOutcome::Saved(at(0.25, 0.1))));
        assert_eq!(*saved.lock().unwrap(), Some(at(0.25, 0.1)));
        assert!(*moving.lock().unwrap());
        let listener = overlay.join().unwrap();

        // A cancel saves nothing.
        let overlay = thread::spawn(move || {
            let mut requester = next_requester(&listener);
            requester.reply(&MoveReply::Moving);
            requester.reply(&MoveReply::Cancel(CancelReason::Timeout));
        });
        let outcome = move_overlay_at(&path, |_| panic!("a cancel must not save"), || {});
        assert_eq!(outcome, Ok(MoveOutcome::Cancelled(CancelReason::Timeout)));
        overlay.join().unwrap();
    }

    #[test]
    fn a_failed_save_is_an_error_not_a_saved_outcome() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("overlay.sock");
        let listener = MoveListener::bind_at(&path, None).unwrap();
        let overlay = thread::spawn(move || {
            let mut requester = next_requester(&listener);
            requester.reply(&MoveReply::Moving);
            requester.reply(&MoveReply::Save(at(0.5, 0.5)));
        });
        let outcome = move_overlay_at(&path, |_| Err("read-only".to_owned()), || {});
        assert_eq!(
            outcome,
            Err("Couldn't save the Overlay position: read-only".to_owned())
        );
        overlay.join().unwrap();
    }

    #[test]
    fn a_refusing_or_missing_overlay_is_a_clear_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("overlay.sock");
        let error = move_overlay_at(&path, |_| Ok(()), || panic!("not moving")).unwrap_err();
        assert!(error.contains("isn't running"), "{error}");
        assert!(
            error.contains("systemctl --user start voisu-overlay"),
            "{error}"
        );

        let listener =
            MoveListener::bind_at(&path, Some("no layer shell here".to_owned())).unwrap();
        assert_eq!(
            move_overlay_at(&path, |_| Ok(()), || panic!("not moving")),
            Err("no layer shell here".to_owned())
        );
        drop(listener);

        // A listener that starts accepting can turn to refusing later, when
        // the pill's surface fails.
        let listener = MoveListener::bind_at(&path, None).unwrap();
        listener.refuse("the pill's surface failed");
        assert_eq!(
            move_overlay_at(&path, |_| Ok(()), || panic!("not moving")),
            Err("the pill's surface failed".to_owned())
        );
        assert!(listener.take().is_none());
    }

    #[test]
    fn the_overlay_sees_a_client_hang_up_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("overlay.sock");
        let listener = MoveListener::bind_at(&path, None).unwrap();
        let mut client = UnixStream::connect(&path).unwrap();
        client.write_all(b"move\n").unwrap();
        let mut requester = next_requester(&listener);
        assert!(requester.reply(&MoveReply::Moving));
        assert!(!requester.gone());
        drop(client);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !requester.gone() {
            assert!(Instant::now() < deadline, "hang-up never seen");
            thread::sleep(Duration::from_millis(2));
        }
        // Anything but `move` is turned away and never reaches the Overlay.
        let mut stranger = UnixStream::connect(&path).unwrap();
        stranger.write_all(b"hello\n").unwrap();
        let mut answer = String::new();
        BufReader::new(stranger).read_line(&mut answer).unwrap();
        assert_eq!(answer, "error unknown request\n");
        assert!(listener.take().is_none());
    }

    #[test]
    fn a_stale_socket_is_replaced_but_a_live_overlay_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("overlay.sock");
        drop(UnixListener::bind(&path).unwrap());
        assert!(path.exists(), "a dead listener leaves its file behind");
        let _live = MoveListener::bind_at(&path, None).unwrap();
        let error = MoveListener::bind_at(&path, None).err().unwrap();
        assert!(error.contains("already listening"), "{error}");
    }
}
