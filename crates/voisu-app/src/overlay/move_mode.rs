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
//! Every line ends in a newline; one cut short by a hang-up is not a line.
//! The client holding the connection open is what keeps move mode alive: if
//! it goes away (Ctrl-C, a closed TUI), the Overlay cancels and restores.
//!
//! The Overlay never adopts a saved spot on its own say-so: it re-reads the
//! config once the client has written it, so a save that fails leaves the
//! pill where the config says.

use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::OverlayPosition;

/// With no drag, click or key for this long, move mode cancels by itself.
pub const MOVE_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a ✓ waits for a daemon status, sent after it, that shows Voisu
/// idle. Without one by then the move is cancelled, never saved: only a
/// daemon that doesn't answer makes it wait this long.
pub const SAVE_SETTLE: Duration = Duration::from_secs(2);
/// How long a client waits for the Overlay to accept or refuse.
const ACK_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a client waits for the outcome once moving. Far past the idle
/// timeout; it only guards against an Overlay that hangs.
const OUTCOME_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// How long the Overlay waits for a connected client's request line.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
/// The longest request line read; anything longer is not a request.
const MAX_REQUEST: u64 = 64;
/// Connections whose request is still being read, each on its own thread. A
/// client that connects and says nothing holds one for [`REQUEST_TIMEOUT`];
/// past this many, new connections are turned away at once.
const MAX_UNREAD_REQUESTS: usize = 8;

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
    /// ✓ was pressed, but no status in [`SAVE_SETTLE`] showed Voisu idle, so a
    /// Recording may have started; nothing was saved.
    Unconfirmed,
}

impl CancelReason {
    const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Timeout => "timeout",
            Self::Recording => "recording",
            Self::RequesterGone => "gone",
            Self::Unconfirmed => "unconfirmed",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        [
            Self::User,
            Self::Timeout,
            Self::Recording,
            Self::RequesterGone,
            Self::Unconfirmed,
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
    /// One whole line, newline included; a line cut short is not a reply.
    pub fn parse(line: &str) -> Option<Self> {
        let line = line.strip_suffix('\n')?;
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

/// What becomes of a ✓ (or Enter) pressed at `pressed` to save `position`,
/// checked every tick until it settles. A Recording that has already started
/// must win over it, and the Overlay only learns of one from the daemon's
/// status, so in order:
///
/// - a status showing a Recording (`recording`) cancels the move;
/// - so does the client going away, as it would before ✓;
/// - a reply to a status round trip *sent* at or after `pressed` that showed
///   Voisu idle (`idle_as_of`, the send time of the newest such reply) saves;
/// - a failed or late poll is no evidence either way, so the save waits, and
///   past [`SAVE_SETTLE`] without that evidence it is cancelled
///   ([`CancelReason::Unconfirmed`]) rather than saved blind.
///
/// `None` means keep waiting.
#[must_use]
pub fn settle_save(
    position: OverlayPosition,
    pressed: Instant,
    idle_as_of: Option<Instant>,
    recording: bool,
    requester_gone: bool,
    now: Instant,
) -> Option<MoveEnd> {
    let reason = if recording {
        CancelReason::Recording
    } else if requester_gone {
        CancelReason::RequesterGone
    } else if idle_as_of.is_some_and(|idle| idle >= pressed) {
        return Some(MoveEnd::Save(position));
    } else if now.saturating_duration_since(pressed) >= SAVE_SETTLE {
        CancelReason::Unconfirmed
    } else {
        return None;
    };
    Some(MoveEnd::Cancel(reason))
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
    /// `overlay.lock`, held for the listener's life: whoever holds it owns
    /// the socket path. Dropped after the socket file is removed.
    _lock: std::fs::File,
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

    /// Listens on `path`. The lock file beside it decides who owns the path,
    /// never a connect probe (which could block, and which two starting
    /// Overlays could both pass): holding the lock means any socket file
    /// there is stale and is replaced; a held lock means another Overlay is
    /// listening, and its socket is left alone.
    pub fn bind_at(path: &Path, refusal: Option<String>) -> Result<Self, String> {
        let lock_path = path.with_extension("lock");
        let lock = crate::daemon_lock::lock_exclusive(&lock_path)
            .map_err(|error| format!("cannot open {}: {error}", lock_path.display()))?
            .ok_or_else(|| format!("another Overlay is already listening on {}", path.display()))?;
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(format!("cannot replace {}: {error}", path.display())),
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
            _lock: lock,
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

/// Accepts connections and reads each one's request on a thread of its own,
/// so a client that connects and stalls never holds up the next one.
fn accept(
    listener: &UnixListener,
    refusal: &Arc<Mutex<Option<String>>>,
    sender: &mpsc::Sender<MoveRequester>,
) {
    let unread = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        if unread.fetch_add(1, Ordering::AcqRel) >= MAX_UNREAD_REQUESTS {
            unread.fetch_sub(1, Ordering::AcqRel);
            turn_away(&stream, "the Overlay is busy; try again");
            continue;
        }
        let spawned = thread::Builder::new()
            .name("overlay-move-request".to_owned())
            .spawn({
                let (refusal, sender, unread) =
                    (Arc::clone(refusal), sender.clone(), Arc::clone(&unread));
                move || {
                    handle(stream, &refusal, &sender);
                    unread.fetch_sub(1, Ordering::AcqRel);
                }
            });
        if spawned.is_err() {
            // The closure (and with it the stream) is dropped; the client
            // sees the hang-up.
            unread.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

fn handle(
    stream: UnixStream,
    refusal: &Mutex<Option<String>>,
    sender: &mpsc::Sender<MoveRequester>,
) {
    if !is_move_request(&stream) {
        turn_away(&stream, "unknown request");
        return;
    }
    let mut requester = MoveRequester { stream };
    let refused = refusal.lock().ok().and_then(|reason| reason.clone());
    if let Some(reason) = refused {
        requester.reply(&MoveReply::Error(reason));
        return;
    }
    // If the Overlay stopped listening, the dropped stream tells the client.
    if requester.stream.set_nonblocking(true).is_ok() {
        let _ = sender.send(requester);
    }
}

/// Answers `error <why>` without ever blocking the caller.
fn turn_away(stream: &UnixStream, why: &str) {
    if stream.set_nonblocking(true).is_ok() {
        let _ = (&*stream).write_all(MoveReply::Error(why.to_owned()).line().as_bytes());
    }
}

/// Exactly `move` and a newline: a request cut short by a hang-up is none.
fn is_move_request(stream: &UnixStream) -> bool {
    if stream.set_read_timeout(Some(REQUEST_TIMEOUT)).is_err() {
        return false;
    }
    let mut line = String::new();
    BufReader::new(stream.take(MAX_REQUEST))
        .read_line(&mut line)
        .is_ok_and(|_| line.strip_suffix('\n') == Some(MOVE_REQUEST))
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
        ErrorKind::NotFound | ErrorKind::ConnectionRefused => not_running(
            wayland_session(),
            &std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default(),
        ),
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

/// Whether this is a Wayland session, decided as the Overlay decides it
/// (`XDG_SESSION_TYPE` first, then the displays), so an X11 session with a
/// stray `WAYLAND_DISPLAY` isn't told to start a service that won't run.
fn wayland_session() -> bool {
    let var = |name| std::env::var(name).ok();
    voisu_core::resolve_session(
        var("WAYLAND_DISPLAY").as_deref(),
        var("DISPLAY").as_deref(),
        var("XDG_SESSION_TYPE").as_deref(),
    )
    .session
        == voisu_core::SessionKind::Wayland
}

/// What to say when no Overlay answers. Only a layer-shell Wayland desktop
/// runs the movable GTK Overlay: on GNOME the Overlay is the Shell extension
/// (the GTK service is meant to be off), and without Wayland the service never
/// starts. Telling those sessions to start the service would not help.
fn not_running(wayland: bool, current_desktop: &str) -> String {
    let gnome = current_desktop
        .split(':')
        .any(|desktop| desktop.eq_ignore_ascii_case("gnome"));
    if gnome {
        "On GNOME the Overlay is the Voisu GNOME Shell extension, which can't be moved.".to_owned()
    } else if !wayland {
        "Moving the Overlay needs a Wayland desktop with layer-shell \
         (KDE Plasma, Hyprland); this session isn't Wayland."
            .to_owned()
    } else {
        "The Overlay isn't running. Start it \
         with `systemctl --user start voisu-overlay`, then try again."
            .to_owned()
    }
}

fn read_reply(reader: &mut BufReader<UnixStream>, timeout: Duration) -> Result<MoveReply, String> {
    reader
        .get_ref()
        .set_read_timeout(Some(timeout))
        .map_err(|error| format!("Couldn't reach the Overlay: {error}"))?;
    let mut line = String::new();
    match reader.read_line(&mut line) {
        // A hang-up mid-line is no reply: `save 0.250,0` must never save.
        Ok(_) if !line.ends_with('\n') => {
            Err("The Overlay stopped before the move finished.".to_owned())
        }
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
            MoveReply::Cancel(CancelReason::Unconfirmed),
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
        for junk in [
            "",
            "save",
            "save 2,2",
            "cancel later",
            "moved",
            "Moving",
            "moving",
            "save 0.250,0.100",
        ] {
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
    fn a_save_needs_an_idle_status_sent_after_it_so_a_started_recording_wins() {
        let spot = at(0.25, 0.1);
        let pressed = Instant::now();
        let before = pressed - Duration::from_millis(150);
        let after = pressed + Duration::from_millis(10);
        let settle =
            |idle, recording, gone, now| settle_save(spot, pressed, idle, recording, gone, now);
        // The newest idle status was sent before ✓ (or there is none, or the
        // last poll failed and left it so): a Recording may have started
        // since, so wait.
        assert_eq!(settle(Some(before), false, false, after), None);
        assert_eq!(settle(None, false, false, after), None);
        // A status sent after ✓ showed Voisu idle: save.
        assert_eq!(
            settle(Some(after), false, false, after),
            Some(MoveEnd::Save(spot))
        );
        assert_eq!(
            settle(Some(pressed), false, false, pressed),
            Some(MoveEnd::Save(spot))
        );
        // A Recording seen wins over the save, even over idle evidence that
        // came first.
        assert_eq!(
            settle(Some(after), true, false, after),
            Some(MoveEnd::Cancel(CancelReason::Recording))
        );
        // A client that left while the save waited ends move mode.
        assert_eq!(
            settle(None, false, true, after),
            Some(MoveEnd::Cancel(CancelReason::RequesterGone))
        );
        // A daemon that never answers can't hold the save forever, and
        // without evidence of idle it is cancelled, never saved blind.
        let late = pressed + SAVE_SETTLE;
        assert_eq!(
            settle(Some(before), false, false, late),
            Some(MoveEnd::Cancel(CancelReason::Unconfirmed))
        );
        assert_eq!(
            settle(None, false, false, late),
            Some(MoveEnd::Cancel(CancelReason::Unconfirmed))
        );
        // Evidence that arrived in time still saves, however late the tick.
        assert_eq!(
            settle(Some(after), false, false, late),
            Some(MoveEnd::Save(spot))
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
        let session = not_running(
            wayland_session(),
            &std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default(),
        );
        assert_eq!(error, session);

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
    fn only_a_layer_shell_wayland_session_is_told_to_start_the_service() {
        let kde = not_running(true, "KDE");
        assert!(
            kde.contains("systemctl --user start voisu-overlay"),
            "{kde}"
        );
        assert_eq!(not_running(true, "Hyprland"), kde);
        for gnome in [
            not_running(true, "ubuntu:GNOME"),
            not_running(false, "GNOME"),
        ] {
            assert!(gnome.contains("GNOME Shell extension"), "{gnome}");
            assert!(!gnome.contains("systemctl"), "{gnome}");
        }
        let x11 = not_running(false, "KDE");
        assert!(x11.contains("isn't Wayland"), "{x11}");
        assert!(!x11.contains("systemctl"), "{x11}");
    }

    #[test]
    fn a_reply_cut_short_by_a_hang_up_is_never_saved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("overlay.sock");
        let fake = UnixListener::bind(&path).unwrap();
        for (moving, outcome) in [("moving\n", "save 0.250,0"), ("moving", "")] {
            let overlay = thread::spawn({
                let fake = fake.try_clone().unwrap();
                move || {
                    let (mut stream, _) = fake.accept().unwrap();
                    let mut line = String::new();
                    BufReader::new(&stream).read_line(&mut line).unwrap();
                    stream.write_all(moving.as_bytes()).unwrap();
                    stream.write_all(outcome.as_bytes()).unwrap();
                    // Hangs up mid-line.
                }
            });
            let error = move_overlay_at(&path, |_| panic!("a cut-short line must not save"), || {})
                .unwrap_err();
            assert!(error.contains("stopped before"), "{moving:?}: {error}");
            overlay.join().unwrap();
        }
    }

    #[test]
    fn a_request_cut_short_or_peers_that_stall_never_hold_up_a_real_client() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("overlay.sock");
        let listener = MoveListener::bind_at(&path, None).unwrap();
        // `move` with no newline, then a hang-up of the write side, is no
        // request.
        let cut = UnixStream::connect(&path).unwrap();
        (&cut).write_all(b"move").unwrap();
        cut.shutdown(std::net::Shutdown::Write).unwrap();
        let mut answer = String::new();
        BufReader::new(&cut).read_line(&mut answer).unwrap();
        assert_eq!(answer, "error unknown request\n");
        assert!(listener.take().is_none());
        // Peers that connect and say nothing each wait out their own
        // timeout; a real client behind them is answered at once.
        let stalled: Vec<_> = (0..3)
            .map(|_| UnixStream::connect(&path).unwrap())
            .collect();
        let started = Instant::now();
        let mut client = UnixStream::connect(&path).unwrap();
        client.write_all(b"move\n").unwrap();
        let _requester = next_requester(&listener);
        assert!(
            started.elapsed() < REQUEST_TIMEOUT,
            "stalled peers held up a client for {:?}",
            started.elapsed()
        );
        drop(stalled);
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
        let live = MoveListener::bind_at(&path, None).unwrap();
        let error = MoveListener::bind_at(&path, None).err().unwrap();
        assert!(error.contains("already listening"), "{error}");
        // The loser touched nothing: the live socket still answers.
        let mut client = UnixStream::connect(&path).unwrap();
        client.write_all(b"move\n").unwrap();
        let _requester = next_requester(&live);
        // Once the live one is gone, the path is free again.
        drop(live);
        drop(MoveListener::bind_at(&path, None).unwrap());
    }

    #[test]
    fn racing_binders_on_a_stale_socket_leave_exactly_one_live_listener() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("overlay.sock");
        drop(UnixListener::bind(&path).unwrap());
        let start = Arc::new(std::sync::Barrier::new(4));
        let racers: Vec<_> = (0..4)
            .map(|_| {
                let (path, start) = (path.clone(), Arc::clone(&start));
                thread::spawn(move || {
                    start.wait();
                    MoveListener::bind_at(&path, None).ok()
                })
            })
            .collect();
        let winners: Vec<_> = racers
            .into_iter()
            .filter_map(|racer| racer.join().unwrap())
            .collect();
        assert_eq!(winners.len(), 1, "exactly one binder wins");
        let mut client = UnixStream::connect(&path).unwrap();
        client.write_all(b"move\n").unwrap();
        let _requester = next_requester(&winners[0]);
    }
}
