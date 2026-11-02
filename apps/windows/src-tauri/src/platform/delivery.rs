//! Delivery: types a finished Transcript into the window that was focused when
//! the Recording started, as if the user had pasted it, then puts their
//! clipboard back.
//!
//! The decisions (paste chord per app, modifier release, read timing, restore
//! guard, preflight) are plain functions here, tested on Linux. `native` is the
//! Win32 side:
//!
//! 1. Preflight the target: gone, one of Voisu's own windows, or elevated
//!    (UIPI drops synthetic input into a higher-integrity window without an
//!    error, so this is checked up front and reported instead).
//! 2. Snapshot every restorable clipboard format.
//! 3. Publish `CF_UNICODETEXT` with delayed rendering from a message-only
//!    owner window, plus the formats that keep it out of clipboard history,
//!    cloud clipboard and clipboard monitors.
//! 4. Release held modifiers, then send the paste chord.
//! 5. The target's `WM_RENDERFORMAT` is the receipt that it pasted. Restore
//!    once reads go quiet, but only while Voisu still owns the clipboard and
//!    nothing else has written to it since (`GetClipboardSequenceNumber`).
//!
//! The receipt design (delayed render as proof of read, a quiet period before
//! restoring, the history/cloud exclusion formats) follows Handy's Windows
//! paste path (MIT, <https://github.com/cjpais/Handy>). No code was copied.

use std::fmt;
use std::time::Duration;

use tauri::{AppHandle, Emitter};

use crate::overlay::{RECORDING_STATE, RecordingState, RecordingStatePayload};

/// Restore this long after the target's last clipboard read.
pub const QUIET_AFTER_READ: Duration = Duration::from_millis(200);
/// No read this long after the chord means the paste did not land. Long
/// enough for a cold Word or Electron paste; a read after this would see the
/// restored clipboard instead.
pub const NO_READ_TIMEOUT: Duration = Duration::from_millis(1500);
/// Hard stop for targets that keep re-reading the clipboard.
pub const RESTORE_TIMEOUT: Duration = Duration::from_secs(8);
/// Gap between chord phases. RDP drops a modifier pressed and released in the
/// same instant and types a literal "v".
pub const CHORD_HOLD: Duration = Duration::from_millis(40);
/// Upper bound on the clipboard snapshot; larger formats are not restored.
pub const SNAPSHOT_BUDGET: usize = 64 << 20;
/// "Test paste" waits this long so the user can click into an app.
pub const TEST_PASTE_DELAY: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeliveryOutcome {
    /// The target read the Transcript from the clipboard after the chord.
    Pasted,
    /// The chord went out but the target never read the clipboard.
    NotConfirmed,
    /// The target runs elevated; Windows would silently drop the keystrokes.
    ElevatedTarget,
    /// No focused window was captured, or it has closed since.
    TargetGone,
    /// The captured window is one of Voisu's own.
    OwnWindow,
    /// The target could not be brought back to the foreground.
    FocusLost,
}

impl DeliveryOutcome {
    /// What to tell the user, if anything. `None` means the text landed.
    pub fn notice(self) -> Option<&'static str> {
        match self {
            Self::Pasted => None,
            Self::NotConfirmed => Some("The app didn't take the paste"),
            Self::ElevatedTarget => Some("Can't type into an admin window"),
            Self::TargetGone => Some("The window you were typing in closed"),
            Self::OwnWindow => Some("Click into another app first"),
            Self::FocusLost => Some("Couldn't return to your window"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeliveryError {
    /// Another process held the clipboard open for too long.
    ClipboardBusy,
    Clipboard(&'static str),
    /// `SendInput` injected fewer events than asked.
    Input,
    Unsupported,
}

impl fmt::Display for DeliveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ClipboardBusy => f.write_str("Another app is holding the clipboard"),
            Self::Clipboard(what) => write!(f, "Clipboard error: {what}"),
            Self::Input => f.write_str("Windows blocked the paste keystroke"),
            Self::Unsupported => f.write_str("Pasting works on Windows only"),
        }
    }
}

impl std::error::Error for DeliveryError {}

/// The window that had focus when the Recording started. Plain data so it can
/// cross to the thread that delivers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeliveryTarget {
    hwnd: isize,
}

impl DeliveryTarget {
    pub fn capture() -> Self {
        Self {
            hwnd: native::foreground(),
        }
    }
}

/// Pastes `text` into `target` and restores the clipboard. Blocks for up to
/// `RESTORE_TIMEOUT` and pumps its own messages: call it off the main thread.
/// Concurrent calls run one after another.
pub fn deliver(target: &DeliveryTarget, text: &str) -> Result<DeliveryOutcome, DeliveryError> {
    native::deliver(target.hwnd, text)
}

// ---- Paste chord ----

const VK_SHIFT: u16 = 0x10;
const VK_CONTROL: u16 = 0x11;
const VK_INSERT: u16 = 0x2D;
const VK_V: u16 = 0x56;
const VK_LWIN: u16 = 0x5B;
const VK_RWIN: u16 = 0x5C;
const VK_LSHIFT: u16 = 0xA0;
const VK_RSHIFT: u16 = 0xA1;
const VK_LCONTROL: u16 = 0xA2;
const VK_RCONTROL: u16 = 0xA3;
const VK_LMENU: u16 = 0xA4;
const VK_RMENU: u16 = 0xA5;
/// Unassigned virtual key, tapped to stop a bare Win or Alt release from
/// opening Start or a menu bar.
const VK_MASK: u16 = 0xE8;

/// Every modifier that can still be down from the hotkey. AltGr reports as
/// RAlt plus a synthetic LCtrl; both are covered.
pub const MODIFIERS: [u16; 8] = [
    VK_LSHIFT,
    VK_RSHIFT,
    VK_LCONTROL,
    VK_RCONTROL,
    VK_LMENU,
    VK_RMENU,
    VK_LWIN,
    VK_RWIN,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PasteChord {
    CtrlV,
    /// Terminals where Ctrl+V reaches the shell as a literal ^V.
    CtrlShiftV,
    /// Consoles whose default paste binding is Shift+Insert.
    ShiftInsert,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Key {
    pub vk: u16,
    pub up: bool,
}

impl Key {
    fn down(vk: u16) -> Self {
        Self { vk, up: false }
    }

    fn up(vk: u16) -> Self {
        Self { vk, up: true }
    }

    /// Keys whose scan code needs the E0 prefix. Without it, Insert arrives
    /// as numpad 0 and types "0" under NumLock.
    pub fn extended(self) -> bool {
        matches!(
            self.vk,
            VK_INSERT | VK_RCONTROL | VK_RMENU | VK_LWIN | VK_RWIN
        )
    }
}

/// Picks the chord from the target's window class and executable name.
pub fn paste_chord(class_name: &str, exe_path: &str) -> PasteChord {
    match class_name {
        // Windows Terminal.
        "CASCADIA_HOSTING_WINDOW_CLASS" => return PasteChord::CtrlShiftV,
        // conhost, PuTTY, mintty (Git Bash), ConEmu.
        "ConsoleWindowClass" | "PuTTY" | "mintty" | "VirtualConsoleClass" => {
            return PasteChord::ShiftInsert;
        }
        _ => {}
    }
    let exe = exe_path
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    match exe.as_str() {
        "windowsterminal.exe" | "alacritty.exe" | "wezterm-gui.exe" => PasteChord::CtrlShiftV,
        "putty.exe" | "mintty.exe" | "conemu64.exe" | "conemu.exe" => PasteChord::ShiftInsert,
        _ => PasteChord::CtrlV,
    }
}

/// Key-ups for every held modifier, so the chord is not read as, say,
/// Ctrl+Alt+V. Releasing a bare Win or Alt opens Start or a menu bar, so an
/// unassigned key taps first while they are still down.
pub fn release_held(held: &[u16]) -> Vec<Key> {
    let mut keys = Vec::new();
    if held
        .iter()
        .any(|vk| matches!(*vk, VK_LWIN | VK_RWIN | VK_LMENU | VK_RMENU))
    {
        keys.push(Key::down(VK_MASK));
        keys.push(Key::up(VK_MASK));
    }
    keys.extend(held.iter().map(|vk| Key::up(*vk)));
    keys
}

/// The chord as three phases sent `CHORD_HOLD` apart: modifiers down, key
/// tap, modifiers up.
pub fn chord_phases(chord: PasteChord) -> [Vec<Key>; 3] {
    let (modifiers, key): (&[u16], u16) = match chord {
        PasteChord::CtrlV => (&[VK_CONTROL], VK_V),
        PasteChord::CtrlShiftV => (&[VK_CONTROL, VK_SHIFT], VK_V),
        PasteChord::ShiftInsert => (&[VK_SHIFT], VK_INSERT),
    };
    [
        modifiers.iter().map(|vk| Key::down(*vk)).collect(),
        vec![Key::down(key), Key::up(key)],
        modifiers.iter().rev().map(|vk| Key::up(*vk)).collect(),
    ]
}

// ---- Clipboard ----

/// `CF_UNICODETEXT` payload: CRLF line endings (what edit controls expect),
/// no interior NUL (it would cut the text short), NUL-terminated UTF-16.
pub fn clipboard_text(text: &str) -> Vec<u16> {
    let mut normalised = String::with_capacity(text.len() + 8);
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\0' => {}
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                normalised.push_str("\r\n");
            }
            '\n' => normalised.push_str("\r\n"),
            c => normalised.push(c),
        }
    }
    let mut wide: Vec<u16> = normalised.encode_utf16().collect();
    wide.push(0);
    wide
}

/// Formats whose data is a plain `HGLOBAL` and can be copied byte for byte.
/// GDI-handle formats (bitmap, palette, metafiles), owner-display and the
/// private/GDI-object ranges hold handles, not bytes. A bitmap still survives
/// through its `CF_DIB`/`CF_DIBV5` forms, which Windows synthesises.
pub fn snapshot_format(format: u32) -> bool {
    !matches!(
        format,
        2 | 3 | 9 | 14 | 0x80 | 0x82 | 0x83 | 0x8E | 0x200..=0x3FF
    )
}

/// Restore only while Voisu still owns the clipboard and its sequence number
/// is the one Voisu last left. Anything else means another app wrote to it.
pub fn may_restore(still_owner: bool, sequence_now: u32, sequence_ours: u32) -> bool {
    still_owner && sequence_now == sequence_ours
}

// ---- Read timing ----

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Watch {
    pub since_chord: Duration,
    /// When the target last read the clipboard, measured from the chord.
    /// Reads before the chord (clipboard managers) never count.
    pub last_read: Option<Duration>,
    /// Another app took the clipboard.
    pub ownership_lost: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Wait,
    Restore {
        pasted: bool,
    },
    /// Leave the clipboard alone: it is no longer Voisu's.
    Abandon {
        pasted: bool,
    },
}

pub fn evaluate(watch: Watch) -> Verdict {
    let pasted = watch.last_read.is_some();
    if watch.ownership_lost {
        return Verdict::Abandon { pasted };
    }
    if watch.since_chord >= RESTORE_TIMEOUT {
        return Verdict::Restore { pasted };
    }
    match watch.last_read {
        Some(read) if watch.since_chord.saturating_sub(read) >= QUIET_AFTER_READ => {
            Verdict::Restore { pasted: true }
        }
        None if watch.since_chord >= NO_READ_TIMEOUT => Verdict::Restore { pasted: false },
        _ => Verdict::Wait,
    }
}

// ---- Preflight ----

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Integrity {
    /// Mandatory-label RID: 0x2000 medium, 0x3000 high (elevated), ...
    Level(u32),
    /// The target's process or token refused a query: higher integrity.
    Denied,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Probe {
    pub exists: bool,
    pub pid: u32,
    pub own_pid: u32,
    pub target: Integrity,
    pub own: Integrity,
}

/// `None` means go ahead and paste.
pub fn preflight(probe: Probe) -> Option<DeliveryOutcome> {
    if !probe.exists {
        return Some(DeliveryOutcome::TargetGone);
    }
    if probe.pid == probe.own_pid {
        return Some(DeliveryOutcome::OwnWindow);
    }
    let blocked = match (probe.target, probe.own) {
        (Integrity::Denied, _) => true,
        (Integrity::Level(target), Integrity::Level(own)) => target > own,
        _ => false,
    };
    blocked.then_some(DeliveryOutcome::ElevatedTarget)
}

// ---- Tray diagnostic ----

/// Hinglish, Devanagari, emoji (outside the BMP) and more than 300 chars, so
/// one paste exercises the long-text and Unicode paths.
pub fn test_sample() -> String {
    let mut text = String::from(
        "Voisu test paste: namaste दोस्तों 🙏, aaj ka plan kya hai? Meeting 4 baje, \
         phir chai ☕. Emoji check 🎙️✨👍🏽. ",
    );
    let mut n = 1;
    while text.chars().count() < 300 {
        text.push_str(&format!("line {n} theek hai. "));
        n += 1;
    }
    text.push_str("End.");
    text
}

/// Tray "Test paste": wait so the user can click into an app, then paste the
/// sample through the real Delivery path.
pub fn test_paste(app: &AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(TEST_PASTE_DELAY);
        let target = DeliveryTarget::capture();
        report(&app, &deliver(&target, &test_sample()));
    });
}

/// Logs the result and shows anything short of a paste on the Overlay.
pub fn report(app: &AppHandle, result: &Result<DeliveryOutcome, DeliveryError>) {
    let message = match result {
        Ok(outcome) => {
            tracing::info!(?outcome, "delivery finished");
            outcome.notice().map(str::to_owned)
        }
        Err(error) => {
            tracing::warn!(%error, "delivery failed");
            Some(error.to_string())
        }
    };
    if let Some(message) = message {
        let payload = RecordingStatePayload {
            state: RecordingState::Error,
            message: Some(message),
        };
        if let Err(error) = app.emit(RECORDING_STATE, payload) {
            tracing::warn!(%error, "delivery notice not shown");
        }
    }
}

#[cfg(not(windows))]
mod native {
    use super::{DeliveryError, DeliveryOutcome};

    pub fn foreground() -> isize {
        0
    }

    pub fn deliver(_hwnd: isize, _text: &str) -> Result<DeliveryOutcome, DeliveryError> {
        Err(DeliveryError::Unsupported)
    }
}

#[cfg(windows)]
mod native {
    use std::cell::RefCell;
    use std::ffi::c_void;
    use std::sync::{Mutex, Once};
    use std::thread::sleep;
    use std::time::{Duration, Instant};

    use windows::Win32::Foundation::{
        CloseHandle, ERROR_ACCESS_DENIED, GlobalFree, HANDLE, HGLOBAL, HINSTANCE, HWND, LPARAM,
        LRESULT, WPARAM,
    };
    use windows::Win32::Security::{
        GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TOKEN_MANDATORY_LABEL,
        TOKEN_QUERY, TokenIntegrityLevel,
    };
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData, GetClipboardOwner,
        GetClipboardSequenceNumber, IsClipboardFormatAvailable, OpenClipboard,
        RegisterClipboardFormatW, SetClipboardData,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Memory::{
        GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
    };
    use windows::Win32::System::Threading::{
        GetCurrentProcess, GetCurrentProcessId, OpenProcess, OpenProcessToken, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT,
        KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, MAPVK_VK_TO_VSC, MapVirtualKeyW, SendInput,
        VIRTUAL_KEY,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClassNameW,
        GetForegroundWindow, GetWindowThreadProcessId, HWND_MESSAGE, IsWindow, MSG,
        MsgWaitForMultipleObjects, PM_REMOVE, PeekMessageW, QS_ALLINPUT, RegisterClassW,
        SetForegroundWindow, TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE, WM_DESTROYCLIPBOARD,
        WM_RENDERALLFORMATS, WM_RENDERFORMAT, WNDCLASSW,
    };
    use windows::core::{HRESULT, PCWSTR, PWSTR, w};

    use super::{
        CHORD_HOLD, DeliveryError, DeliveryOutcome, Integrity, Key, MODIFIERS, PasteChord, Probe,
        SNAPSHOT_BUDGET, Verdict, Watch, chord_phases, clipboard_text, evaluate, may_restore,
        paste_chord, preflight, release_held, snapshot_format,
    };

    const CF_UNICODETEXT: u32 = 13;
    const OWNER_CLASS: PCWSTR = w!("VoisuPasteOwner");
    /// Formats that keep the Transcript out of clipboard monitors, Win+V
    /// history and cloud clipboard. Each carries a zero DWORD.
    const PRIVACY_FORMATS: [PCWSTR; 3] = [
        w!("ExcludeClipboardContentFromMonitorProcessing"),
        w!("CanIncludeInClipboardHistory"),
        w!("CanUploadToCloudClipboard"),
    ];

    static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

    /// State the owner window's procedure shares with `paste`, on the
    /// delivering thread. Never borrowed across a Win32 call: clipboard calls
    /// can re-enter the window procedure.
    struct Published {
        text: Vec<u16>,
        reads: Vec<Instant>,
        sequence: u32,
        lost: bool,
    }

    thread_local! {
        static PUBLISHED: RefCell<Option<Published>> = const { RefCell::new(None) };
    }

    fn with_published<R>(f: impl FnOnce(&mut Published) -> R) -> Option<R> {
        PUBLISHED.with(|cell| cell.borrow_mut().as_mut().map(f))
    }

    pub fn foreground() -> isize {
        unsafe { GetForegroundWindow() }.0 as isize
    }

    pub fn deliver(hwnd: isize, text: &str) -> Result<DeliveryOutcome, DeliveryError> {
        let _serial = ONE_AT_A_TIME
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let target = HWND(hwnd as *mut c_void);
        let probe = probe(target);
        if let Some(outcome) = preflight(probe) {
            return Ok(outcome);
        }
        if text.is_empty() {
            return Ok(DeliveryOutcome::Pasted);
        }
        let chord = paste_chord(&class_name(target), &exe_path(probe.pid));
        if !bring_to_front(target) {
            return Ok(DeliveryOutcome::FocusLost);
        }
        let owner = owner_window()?;
        let result = paste(owner, target, text, chord);
        // Destroying the owner renders any still-delayed text, so state goes
        // only after it.
        unsafe {
            let _ = DestroyWindow(owner);
        }
        PUBLISHED.with(|cell| cell.borrow_mut().take());
        result
    }

    fn paste(
        owner: HWND,
        target: HWND,
        text: &str,
        chord: PasteChord,
    ) -> Result<DeliveryOutcome, DeliveryError> {
        open(owner)?;
        let saved = snapshot();
        close();

        PUBLISHED.with(|cell| {
            *cell.borrow_mut() = Some(Published {
                text: clipboard_text(text),
                reads: Vec::new(),
                sequence: 0,
                lost: false,
            });
        });
        if let Err(error) = publish(owner) {
            restore(owner, &saved, None);
            return Err(error);
        }
        let sequence = unsafe { GetClipboardSequenceNumber() };
        with_published(|state| state.sequence = sequence);

        if unsafe { GetForegroundWindow() } != target {
            restore_guarded(owner, &saved);
            return Ok(DeliveryOutcome::FocusLost);
        }
        if let Err(error) = send_chord(chord) {
            restore_guarded(owner, &saved);
            return Err(error);
        }

        let chord_at = Instant::now();
        let verdict = loop {
            pump();
            let (last_read, lost) = with_published(|state| {
                let last = state
                    .reads
                    .iter()
                    .rev()
                    .find(|read| **read >= chord_at)
                    .map(|read| read.duration_since(chord_at));
                (last, state.lost)
            })
            .unwrap_or((None, true));
            match evaluate(Watch {
                since_chord: chord_at.elapsed(),
                last_read,
                ownership_lost: lost,
            }) {
                Verdict::Wait => unsafe {
                    MsgWaitForMultipleObjects(None, false, 25, QS_ALLINPUT);
                },
                verdict => break verdict,
            }
        };
        let pasted = match verdict {
            Verdict::Restore { pasted } => {
                restore_guarded(owner, &saved);
                pasted
            }
            Verdict::Abandon { pasted } => pasted,
            Verdict::Wait => unreachable!(),
        };
        Ok(if pasted {
            DeliveryOutcome::Pasted
        } else {
            DeliveryOutcome::NotConfirmed
        })
    }

    // ---- Target ----

    fn probe(target: HWND) -> Probe {
        let exists = !target.is_invalid() && unsafe { IsWindow(Some(target)) }.as_bool();
        let mut pid = 0u32;
        if exists {
            unsafe { GetWindowThreadProcessId(target, Some(&mut pid as *mut u32)) };
        }
        Probe {
            exists,
            pid,
            own_pid: unsafe { GetCurrentProcessId() },
            target: process_integrity(pid),
            own: token_integrity(unsafe { GetCurrentProcess() }),
        }
    }

    fn denied(error: &windows::core::Error) -> bool {
        error.code() == HRESULT::from_win32(ERROR_ACCESS_DENIED.0)
    }

    fn process_integrity(pid: u32) -> Integrity {
        if pid == 0 {
            return Integrity::Unknown;
        }
        let process = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
            Ok(process) => process,
            Err(error) if denied(&error) => return Integrity::Denied,
            Err(_) => return Integrity::Unknown,
        };
        let integrity = token_integrity(process);
        unsafe {
            let _ = CloseHandle(process);
        }
        integrity
    }

    fn token_integrity(process: HANDLE) -> Integrity {
        let mut token = HANDLE::default();
        if let Err(error) = unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } {
            return if denied(&error) {
                Integrity::Denied
            } else {
                Integrity::Unknown
            };
        }
        // u64 keeps the buffer aligned for TOKEN_MANDATORY_LABEL.
        let mut buffer = [0u64; 16];
        let mut needed = 0u32;
        let read = unsafe {
            GetTokenInformation(
                token,
                TokenIntegrityLevel,
                Some(buffer.as_mut_ptr().cast()),
                std::mem::size_of_val(&buffer) as u32,
                &mut needed,
            )
        };
        let integrity = if read.is_ok() {
            unsafe {
                let label = &*(buffer.as_ptr() as *const TOKEN_MANDATORY_LABEL);
                let sid = label.Label.Sid;
                let count = *GetSidSubAuthorityCount(sid);
                if count == 0 {
                    Integrity::Unknown
                } else {
                    Integrity::Level(*GetSidSubAuthority(sid, u32::from(count) - 1))
                }
            }
        } else {
            Integrity::Unknown
        };
        unsafe {
            let _ = CloseHandle(token);
        }
        integrity
    }

    fn class_name(target: HWND) -> String {
        let mut buffer = [0u16; 256];
        let len = unsafe { GetClassNameW(target, &mut buffer) };
        String::from_utf16_lossy(&buffer[..len.max(0) as usize])
    }

    fn exe_path(pid: u32) -> String {
        let Ok(process) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) })
        else {
            return String::new();
        };
        let mut buffer = [0u16; 1024];
        let mut len = buffer.len() as u32;
        let path = unsafe {
            QueryFullProcessImageNameW(
                process,
                PROCESS_NAME_WIN32,
                PWSTR(buffer.as_mut_ptr()),
                &mut len,
            )
        }
        .map(|()| String::from_utf16_lossy(&buffer[..len as usize]))
        .unwrap_or_default();
        unsafe {
            let _ = CloseHandle(process);
        }
        path
    }

    /// The target is usually still in front (the Overlay never activates).
    /// If the user clicked away, ask for it back; Windows may refuse.
    fn bring_to_front(target: HWND) -> bool {
        if unsafe { GetForegroundWindow() } == target {
            return true;
        }
        unsafe {
            let _ = SetForegroundWindow(target);
        }
        for _ in 0..10 {
            if unsafe { GetForegroundWindow() } == target {
                return true;
            }
            sleep(Duration::from_millis(20));
        }
        false
    }

    // ---- Input ----

    fn input(key: Key) -> INPUT {
        let mut flags = KEYBD_EVENT_FLAGS(0);
        if key.up {
            flags |= KEYEVENTF_KEYUP;
        }
        if key.extended() {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(key.vk),
                    // Scan codes too: RDP and some games read only those.
                    wScan: unsafe { MapVirtualKeyW(u32::from(key.vk), MAPVK_VK_TO_VSC) } as u16,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    fn send(keys: &[Key]) -> Result<(), DeliveryError> {
        if keys.is_empty() {
            return Ok(());
        }
        let inputs: Vec<INPUT> = keys.iter().copied().map(input).collect();
        let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
        if sent as usize == inputs.len() {
            Ok(())
        } else {
            Err(DeliveryError::Input)
        }
    }

    fn send_chord(chord: PasteChord) -> Result<(), DeliveryError> {
        let held: Vec<u16> = MODIFIERS
            .iter()
            .copied()
            .filter(|vk| unsafe { GetAsyncKeyState(i32::from(*vk)) } < 0)
            .collect();
        send(&release_held(&held))?;
        let [down, tap, up] = chord_phases(chord);
        send(&down)?;
        sleep(CHORD_HOLD);
        let tapped = send(&tap);
        sleep(CHORD_HOLD);
        // Modifiers go up even if the tap failed, so none stays stuck.
        let released = send(&up);
        tapped.and(released)
    }

    // ---- Clipboard ----

    fn open(owner: HWND) -> Result<(), DeliveryError> {
        for _ in 0..20 {
            if unsafe { OpenClipboard(Some(owner)) }.is_ok() {
                return Ok(());
            }
            sleep(Duration::from_millis(25));
        }
        Err(DeliveryError::ClipboardBusy)
    }

    fn close() {
        unsafe {
            let _ = CloseClipboard();
        }
    }

    fn owns_clipboard(owner: HWND) -> bool {
        matches!(unsafe { GetClipboardOwner() }, Ok(current) if current == owner)
    }

    fn global(bytes: &[u8]) -> Option<HGLOBAL> {
        unsafe {
            let memory = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1)).ok()?;
            let target = GlobalLock(memory);
            if target.is_null() {
                let _ = GlobalFree(Some(memory));
                return None;
            }
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), target.cast::<u8>(), bytes.len());
            let _ = GlobalUnlock(memory);
            Some(memory)
        }
    }

    /// Hands `bytes` to the clipboard (open, or inside `WM_RENDERFORMAT`).
    fn set(format: u32, bytes: &[u8]) -> bool {
        let Some(memory) = global(bytes) else {
            return false;
        };
        if unsafe { SetClipboardData(format, Some(HANDLE(memory.0))) }.is_ok() {
            true
        } else {
            unsafe {
                let _ = GlobalFree(Some(memory));
            }
            false
        }
    }

    type Snapshot = Vec<(u32, Vec<u8>)>;

    /// Every restorable format, in clipboard order. The clipboard is open.
    fn snapshot() -> Snapshot {
        let mut saved = Vec::new();
        let mut total = 0usize;
        let mut format = 0u32;
        loop {
            format = unsafe { EnumClipboardFormats(format) };
            if format == 0 {
                break;
            }
            if !snapshot_format(format) {
                continue;
            }
            let Ok(handle) = (unsafe { GetClipboardData(format) }) else {
                continue;
            };
            let memory = HGLOBAL(handle.0);
            let size = unsafe { GlobalSize(memory) };
            if size == 0 {
                continue;
            }
            if total + size > SNAPSHOT_BUDGET {
                tracing::warn!(format, size, "clipboard format too large to restore");
                continue;
            }
            let source = unsafe { GlobalLock(memory) };
            if source.is_null() {
                continue;
            }
            let bytes = unsafe { std::slice::from_raw_parts(source.cast::<u8>(), size) }.to_vec();
            unsafe {
                let _ = GlobalUnlock(memory);
            }
            total += size;
            saved.push((format, bytes));
        }
        saved
    }

    /// Owns the clipboard with the text as a delayed render, so the target's
    /// read arrives as `WM_RENDERFORMAT`.
    fn publish(owner: HWND) -> Result<(), DeliveryError> {
        open(owner)?;
        let result = publish_open();
        close();
        result
    }

    fn publish_open() -> Result<(), DeliveryError> {
        unsafe { EmptyClipboard() }
            .map_err(|_| DeliveryError::Clipboard("could not clear the clipboard"))?;
        // A NULL handle registers a delayed render; the call's return value
        // is NULL on success too, so check availability instead.
        unsafe {
            let _ = SetClipboardData(CF_UNICODETEXT, None);
        }
        if unsafe { IsClipboardFormatAvailable(CF_UNICODETEXT) }.is_err() {
            return Err(DeliveryError::Clipboard("could not offer the text"));
        }
        for name in PRIVACY_FORMATS {
            let format = unsafe { RegisterClipboardFormatW(name) };
            if format == 0 || !set(format, &0u32.to_ne_bytes()) {
                return Err(DeliveryError::Clipboard(
                    "could not hide the text from history",
                ));
            }
        }
        Ok(())
    }

    fn restore_guarded(owner: HWND, saved: &Snapshot) {
        let expected = with_published(|state| state.sequence);
        restore(owner, saved, expected);
    }

    /// Puts the snapshot back. With `expected`, only if the clipboard is still
    /// exactly what Voisu left; without, only if Voisu still owns it.
    fn restore(owner: HWND, saved: &Snapshot, expected: Option<u32>) {
        if open(owner).is_err() {
            tracing::warn!("clipboard busy; previous clipboard not restored");
            return;
        }
        let ours = owns_clipboard(owner);
        let allowed = match expected {
            Some(sequence) => may_restore(ours, unsafe { GetClipboardSequenceNumber() }, sequence),
            None => ours,
        };
        if allowed {
            if unsafe { EmptyClipboard() }.is_ok() {
                for (format, bytes) in saved {
                    if !set(*format, bytes) {
                        tracing::warn!(format, "clipboard format not restored");
                    }
                }
            }
        } else {
            tracing::info!("clipboard changed since the paste; left as is");
        }
        close();
    }

    // ---- Owner window ----

    fn owner_window() -> Result<HWND, DeliveryError> {
        static REGISTER: Once = Once::new();
        let module = unsafe { GetModuleHandleW(None) }
            .map_err(|_| DeliveryError::Clipboard("module handle unavailable"))?;
        let instance = HINSTANCE(module.0);
        REGISTER.call_once(|| {
            let class = WNDCLASSW {
                lpfnWndProc: Some(owner_proc),
                hInstance: instance,
                lpszClassName: OWNER_CLASS,
                ..Default::default()
            };
            unsafe { RegisterClassW(&class) };
        });
        unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                OWNER_CLASS,
                PCWSTR::null(),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                Some(instance),
                None,
            )
        }
        .map_err(|_| DeliveryError::Clipboard("could not create the clipboard owner"))
    }

    fn pump() {
        let mut message = MSG::default();
        unsafe {
            while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&message);
                let _ = DispatchMessageW(&message);
            }
        }
    }

    /// Writes the text in answer to a render request. `read` marks a real
    /// request from an app, as opposed to the owner window closing.
    fn render(read: bool) {
        let Some(text) = with_published(|state| state.text.clone()) else {
            return;
        };
        let bytes: Vec<u8> = text.iter().flat_map(|unit| unit.to_ne_bytes()).collect();
        set(CF_UNICODETEXT, &bytes);
        let sequence = unsafe { GetClipboardSequenceNumber() };
        with_published(|state| {
            state.sequence = sequence;
            if read {
                state.reads.push(Instant::now());
            }
        });
    }

    unsafe extern "system" fn owner_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match message {
            WM_RENDERFORMAT => {
                if wparam.0 as u32 == CF_UNICODETEXT {
                    render(true);
                }
                LRESULT(0)
            }
            WM_RENDERALLFORMATS => {
                if unsafe { OpenClipboard(Some(hwnd)) }.is_ok() {
                    if owns_clipboard(hwnd) {
                        render(false);
                    }
                    close();
                }
                LRESULT(0)
            }
            WM_DESTROYCLIPBOARD => {
                with_published(|state| state.lost = true);
                LRESULT(0)
            }
            _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn terminals_get_their_paste_chord() {
        assert_eq!(
            paste_chord("CASCADIA_HOSTING_WINDOW_CLASS", ""),
            PasteChord::CtrlShiftV
        );
        assert_eq!(
            paste_chord("ConsoleWindowClass", r"C:\Windows\System32\conhost.exe"),
            PasteChord::ShiftInsert
        );
        assert_eq!(
            paste_chord("Window Class", r"C:\Program Files\Alacritty\ALACRITTY.EXE"),
            PasteChord::CtrlShiftV
        );
        assert_eq!(
            paste_chord("PuTTY", r"C:\Program Files\PuTTY\putty.exe"),
            PasteChord::ShiftInsert
        );
    }

    #[test]
    fn ordinary_apps_get_ctrl_v() {
        for (class, exe) in [
            ("Notepad", r"C:\Windows\notepad.exe"),
            ("OpusApp", r"C:\Program Files\Microsoft Office\WINWORD.EXE"),
            (
                "Chrome_WidgetWin_1",
                r"C:\Program Files\Google\Chrome\chrome.exe",
            ),
            (
                "Chrome_WidgetWin_1",
                r"C:\Users\r\AppData\Local\Programs\Microsoft VS Code\Code.exe",
            ),
            ("", ""),
        ] {
            assert_eq!(paste_chord(class, exe), PasteChord::CtrlV, "{class} {exe}");
        }
    }

    #[test]
    fn chords_press_modifiers_around_the_key() {
        let [down, tap, up] = chord_phases(PasteChord::CtrlShiftV);
        assert_eq!(down, vec![Key::down(VK_CONTROL), Key::down(VK_SHIFT)]);
        assert_eq!(tap, vec![Key::down(VK_V), Key::up(VK_V)]);
        assert_eq!(up, vec![Key::up(VK_SHIFT), Key::up(VK_CONTROL)]);
    }

    #[test]
    fn shift_insert_uses_the_extended_insert_key() {
        let [_, tap, _] = chord_phases(PasteChord::ShiftInsert);
        assert!(tap.iter().all(|key| key.vk == VK_INSERT && key.extended()));
        assert!(!Key::down(VK_V).extended());
    }

    #[test]
    fn nothing_held_releases_nothing() {
        assert!(release_held(&[]).is_empty());
    }

    #[test]
    fn held_ctrl_and_shift_are_released_without_a_mask() {
        assert_eq!(
            release_held(&[VK_LCONTROL, VK_RSHIFT]),
            vec![Key::up(VK_LCONTROL), Key::up(VK_RSHIFT)]
        );
    }

    #[test]
    fn held_win_or_alt_is_masked_before_release() {
        for vk in [VK_LWIN, VK_RWIN, VK_LMENU, VK_RMENU] {
            let keys = release_held(&[vk]);
            assert_eq!(
                keys,
                vec![Key::down(VK_MASK), Key::up(VK_MASK), Key::up(vk)]
            );
        }
    }

    #[test]
    fn altgr_releases_both_halves() {
        let keys = release_held(&[VK_LCONTROL, VK_RMENU]);
        assert!(keys.contains(&Key::up(VK_LCONTROL)));
        assert!(keys.contains(&Key::up(VK_RMENU)));
    }

    fn decode(wide: &[u16]) -> String {
        assert_eq!(wide.last(), Some(&0), "NUL-terminated");
        String::from_utf16(&wide[..wide.len() - 1]).expect("valid UTF-16")
    }

    #[test]
    fn hinglish_and_emoji_survive_the_clipboard_encoding() {
        let text = "namaste दोस्तों 🙏 chai ☕ 👍🏽";
        assert_eq!(decode(&clipboard_text(text)), text);
    }

    #[test]
    fn long_text_is_kept_whole() {
        let text = "ek do teen ".repeat(100);
        assert!(text.chars().count() > 200);
        assert_eq!(decode(&clipboard_text(&text)), text);
    }

    #[test]
    fn line_endings_become_crlf_and_nul_is_dropped() {
        assert_eq!(
            decode(&clipboard_text("a\nb\r\nc\rd\0e")),
            "a\r\nb\r\nc\r\nde"
        );
    }

    #[test]
    fn test_sample_covers_long_text_hinglish_and_emoji() {
        let sample = test_sample();
        assert!(sample.chars().count() >= 300);
        assert!(
            sample
                .chars()
                .any(|c| ('\u{0900}'..='\u{097F}').contains(&c))
        );
        assert!(sample.chars().any(|c| u32::from(c) > 0xFFFF));
        assert_eq!(decode(&clipboard_text(&sample)), sample);
    }

    #[test]
    fn handle_formats_are_not_snapshotted() {
        for format in [
            2, 3, 9, 14, 0x80, 0x82, 0x83, 0x8E, 0x200, 0x2FF, 0x300, 0x3FF,
        ] {
            assert!(!snapshot_format(format), "{format:#x}");
        }
        // CF_TEXT, CF_DIB, CF_UNICODETEXT, CF_HDROP, CF_LOCALE, CF_DIBV5, a
        // registered format such as "HTML Format".
        for format in [1, 8, 13, 15, 16, 17, 0xC0A1] {
            assert!(snapshot_format(format), "{format:#x}");
        }
    }

    #[test]
    fn restore_needs_ownership_and_an_unchanged_sequence() {
        assert!(may_restore(true, 7, 7));
        assert!(!may_restore(true, 8, 7));
        assert!(!may_restore(false, 7, 7));
    }

    fn watch(since_chord: u64, last_read: Option<u64>) -> Watch {
        Watch {
            since_chord: ms(since_chord),
            last_read: last_read.map(ms),
            ownership_lost: false,
        }
    }

    #[test]
    fn waits_for_the_read_then_a_quiet_period() {
        assert_eq!(evaluate(watch(50, None)), Verdict::Wait);
        assert_eq!(evaluate(watch(150, Some(100))), Verdict::Wait);
        assert_eq!(
            evaluate(watch(300, Some(100))),
            Verdict::Restore { pasted: true }
        );
    }

    #[test]
    fn a_later_read_restarts_the_quiet_period() {
        assert_eq!(evaluate(watch(350, Some(250))), Verdict::Wait);
    }

    #[test]
    fn no_read_gives_up_and_restores() {
        assert_eq!(evaluate(watch(1499, None)), Verdict::Wait);
        assert_eq!(
            evaluate(watch(1500, None)),
            Verdict::Restore { pasted: false }
        );
    }

    #[test]
    fn endless_reads_still_restore_at_the_timeout() {
        assert_eq!(
            evaluate(watch(8000, Some(7990))),
            Verdict::Restore { pasted: true }
        );
    }

    #[test]
    fn losing_the_clipboard_abandons_the_restore() {
        let mut lost = watch(100, Some(50));
        lost.ownership_lost = true;
        assert_eq!(evaluate(lost), Verdict::Abandon { pasted: true });
        lost.last_read = None;
        assert_eq!(evaluate(lost), Verdict::Abandon { pasted: false });
    }

    fn probe() -> Probe {
        Probe {
            exists: true,
            pid: 42,
            own_pid: 7,
            target: Integrity::Level(0x2000),
            own: Integrity::Level(0x2000),
        }
    }

    #[test]
    fn same_integrity_target_is_pasted_into() {
        assert_eq!(preflight(probe()), None);
        assert_eq!(
            preflight(Probe {
                target: Integrity::Unknown,
                ..probe()
            }),
            None
        );
    }

    #[test]
    fn missing_target_is_reported() {
        assert_eq!(
            preflight(Probe {
                exists: false,
                ..probe()
            }),
            Some(DeliveryOutcome::TargetGone)
        );
    }

    #[test]
    fn voisu_never_pastes_into_itself() {
        assert_eq!(
            preflight(Probe { pid: 7, ..probe() }),
            Some(DeliveryOutcome::OwnWindow)
        );
    }

    #[test]
    fn elevated_target_is_reported_not_attempted() {
        assert_eq!(
            preflight(Probe {
                target: Integrity::Level(0x3000),
                ..probe()
            }),
            Some(DeliveryOutcome::ElevatedTarget)
        );
        assert_eq!(
            preflight(Probe {
                target: Integrity::Denied,
                ..probe()
            }),
            Some(DeliveryOutcome::ElevatedTarget)
        );
    }

    #[test]
    fn elevated_voisu_can_paste_into_normal_apps() {
        assert_eq!(
            preflight(Probe {
                own: Integrity::Level(0x3000),
                ..probe()
            }),
            None
        );
    }

    #[test]
    fn every_outcome_but_pasted_has_a_notice() {
        assert_eq!(DeliveryOutcome::Pasted.notice(), None);
        for outcome in [
            DeliveryOutcome::NotConfirmed,
            DeliveryOutcome::ElevatedTarget,
            DeliveryOutcome::TargetGone,
            DeliveryOutcome::OwnWindow,
            DeliveryOutcome::FocusLost,
        ] {
            assert!(outcome.notice().is_some(), "{outcome:?}");
        }
    }
}
