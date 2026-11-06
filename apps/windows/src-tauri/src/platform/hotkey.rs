//! The Trigger Key: a low-level keyboard hook on its own thread.
//!
//! The callback stays O(1) and never blocks, logs or allocates, because
//! Windows silently removes a hook that exceeds `LowLevelHooksTimeout`. It
//! classifies the key, feeds [`ChordTracker`] and `try_send`s any signal to
//! the gesture thread. The Start-menu mask key is posted back to this
//! thread's message loop and injected after the callback returns, so the
//! hook never re-enters itself. A Trigger Key chosen in the settings window
//! arrives the same way, as a message [`set_chord`] posts.
//!
//! The hook is reinstalled on session unlock, on resume from sleep, and by a
//! health check when input arrived but the callback went quiet, which is how
//! a silently removed hook shows.

use std::cell::RefCell;
use std::io;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::thread;

use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::RemoteDesktop::{
    NOTIFY_FOR_THIS_SESSION, WTSRegisterSessionNotification,
};
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetKeyNameTextW, GetKeyboardLayout, GetLastInputInfo, HKL, INPUT, INPUT_0,
    INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP, LASTINPUTINFO,
    MAPVK_VK_TO_CHAR, MAPVK_VSC_TO_VK_EX, MapVirtualKeyExW, SendInput, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetForegroundWindow,
    GetMessageW, GetWindowThreadProcessId, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT, LLKHF_EXTENDED,
    LLKHF_INJECTED, MSG, PBT_APMRESUMEAUTOMATIC, PostThreadMessageW, RegisterClassW, SetTimer,
    SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL, WINDOW_EX_STYLE, WM_APP, WM_KEYDOWN,
    WM_POWERBROADCAST, WM_SYSKEYDOWN, WM_TIMER, WM_WTSSESSION_CHANGE, WNDCLASSW, WS_OVERLAPPED,
    WTS_SESSION_UNLOCK,
};
use windows::core::w;

use crate::chord::{ChordTracker, Key, MASK_TAG, MASK_VK, ModMask, Modifier, Side, VK_PACKET};
use crate::trigger::Trigger;
use crate::trigger_key::{TriggerChord, scan_code};

const VK_SHIFT: i32 = 0x10;
const VK_CONTROL: i32 = 0x11;
const VK_MENU: i32 = 0x12;
const VK_ESCAPE: u32 = 0x1B;
const VK_LWIN: u32 = 0x5B;
const VK_RWIN: u32 = 0x5C;
const VK_LSHIFT: u32 = 0xA0;
const VK_RSHIFT: u32 = 0xA1;
const VK_LCONTROL: u32 = 0xA2;
const VK_RCONTROL: u32 = 0xA3;
const VK_PACKET_CODE: u32 = VK_PACKET as u32;
const VK_LMENU: u32 = 0xA4;
const VK_RMENU: u32 = 0xA5;
/// AltGr arrives as a synthesized LCtrl whose scan code has this bit set.
const ALTGR_SCAN_BIT: u32 = 0x200;

/// Posted by the callback: inject the mask key now.
const WM_INJECT_MASK: u32 = WM_APP + 1;
/// Posted by [`set_chord`]: `wparam` is the packed [`TriggerChord`].
const WM_SET_CHORD: u32 = WM_APP + 2;
const HEALTH_TIMER: usize = 1;
const HEALTH_INTERVAL_MS: u32 = 5_000;
/// Input newer than the last callback by this much means the hook is gone.
const SILENT_HOOK_MS: u32 = 30_000;

/// Tick of the most recent callback, read by the health check.
static LAST_CALLBACK: AtomicU32 = AtomicU32::new(0);
/// The hook thread, for [`set_chord`]; 0 until installed.
static HOOK_THREAD: AtomicU32 = AtomicU32::new(0);
/// Set when a reinstall fails, leaving only the hook that was presumed dead;
/// cleared when a later reinstall succeeds or that hook delivers a key after
/// all.
static HOOK_LOST: AtomicBool = AtomicBool::new(false);
/// What [`HEALTH_CHANGED`] was last told [`is_running`] returns.
static REPORTED_OK: AtomicBool = AtomicBool::new(true);
/// Set once by [`install`]. A static rather than [`HookState`], so calling it
/// never holds the `STATE` borrow the hook callback needs.
static HEALTH_CHANGED: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();

struct HookState {
    tracker: ChordTracker,
    trigger: Trigger,
    hook: Option<HHOOK>,
    thread_id: u32,
}

thread_local! {
    /// Only the hook thread touches this: the callback, the window procedure
    /// and the message loop all run there.
    static STATE: RefCell<Option<HookState>> = const { RefCell::new(None) };
}

/// Starts the hook thread for `chord` and returns once the hook is
/// installed. `health_changed` runs on the hook thread, outside the hook
/// callback, whenever [`is_running`] flips.
pub fn install(
    trigger: Trigger,
    chord: TriggerChord,
    health_changed: impl Fn() + Send + Sync + 'static,
) -> io::Result<()> {
    let _ = HEALTH_CHANGED.set(Box::new(health_changed));
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("voisu-hotkey".into())
        .spawn(move || run(trigger, chord, ready_tx))?;
    ready_rx
        .recv()
        .map_err(|_| io::Error::other("hotkey thread exited"))?
}

/// Whether the hook is installed and listening, for the window's "needs a
/// restart" line.
pub fn is_running() -> bool {
    HOOK_THREAD.load(Ordering::Acquire) != 0 && !HOOK_LOST.load(Ordering::Acquire)
}

/// Switches the hook to a new chord, effective from the next key event.
/// False when the hook is not running, so nothing would change.
#[must_use]
pub fn set_chord(chord: TriggerChord) -> bool {
    let thread = HOOK_THREAD.load(Ordering::Acquire);
    let packed = WPARAM(usize::from(chord.pack()));
    // SAFETY: posts a plain message to the hook thread's queue.
    thread != 0 && unsafe { PostThreadMessageW(thread, WM_SET_CHORD, packed, LPARAM(0)) }.is_ok()
}

fn run(trigger: Trigger, chord: TriggerChord, ready: SyncSender<io::Result<()>>) {
    // SAFETY: plain Win32 calls on this thread; every pointer passed is to a
    // live local.
    unsafe {
        STATE.with(|cell| {
            *cell.borrow_mut() = Some(HookState {
                tracker: ChordTracker::new(chord),
                trigger,
                hook: None,
                thread_id: GetCurrentThreadId(),
            });
        });
        if let Err(error) = reinstall() {
            let _ = ready.send(Err(error));
            return;
        }
        // A hidden top-level window: message-only windows miss the
        // WM_POWERBROADCAST broadcast.
        match watcher_window() {
            Ok(hwnd) => {
                if let Err(error) = WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION) {
                    tracing::warn!(%error, "session notifications unavailable");
                }
                SetTimer(Some(hwnd), HEALTH_TIMER, HEALTH_INTERVAL_MS, None);
            }
            Err(error) => tracing::warn!(%error, "hotkey watcher window failed"),
        }
        HOOK_THREAD.store(GetCurrentThreadId(), Ordering::Release);
        tracing::info!(%chord, "Trigger Key hook installed");
        let _ = ready.send(Ok(()));

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if msg.hwnd.is_invalid() {
                match msg.message {
                    WM_INJECT_MASK => inject_mask(),
                    WM_SET_CHORD => {
                        let chord = TriggerChord::unpack(msg.wParam.0 as u16);
                        with_state(|state| switch_chord(state, chord));
                        tracing::info!(%chord, "Trigger Key changed");
                    }
                    _ => {
                        DispatchMessageW(&msg);
                    }
                }
                continue;
            }
            DispatchMessageW(&msg);
        }
    }
}

fn with_state(f: impl FnOnce(&mut HookState)) {
    STATE.with(|cell| {
        if let Some(state) = cell.borrow_mut().as_mut() {
            f(state);
        }
    });
}

/// Moves the tracker to `chord`, ending a held chord's Recording.
fn switch_chord(state: &mut HookState, chord: TriggerChord) {
    if let Some(signal) = state.tracker.set_chord(chord) {
        state.trigger.signal(signal);
    }
}

/// The foreground app's keyboard layout, the one the user types with:
/// Windows keeps a layout per thread, and Voisu's own may differ.
fn foreground_layout() -> HKL {
    // SAFETY: plain lookups. Without a foreground window the thread is 0,
    // which means the calling thread's layout.
    unsafe { GetKeyboardLayout(GetWindowThreadProcessId(GetForegroundWindow(), None)) }
}

/// What the key at `vk`'s US position types on `layout`, unshifted, and
/// whether it is a dead key there.
fn layout_char(vk: u8, layout: HKL) -> Option<(char, bool)> {
    let scan = scan_code(vk)?;
    let extended = if scan & 0x100 != 0 { 0xE000 } else { 0 };
    let scan = u32::from(scan & 0xFF) | extended;
    // SAFETY: pure keyboard-layout lookups.
    let mapped = unsafe {
        let local = MapVirtualKeyExW(scan, MAPVK_VSC_TO_VK_EX, Some(layout));
        if local == 0 {
            return None;
        }
        MapVirtualKeyExW(local, MAPVK_VK_TO_CHAR, Some(layout))
    };
    let ch = char::from_u32(mapped & 0xFFFF).filter(|&c| c != '\0')?;
    Some((ch, mapped & 0x8000_0000 != 0))
}

/// The foreground layout's name for the key at `vk`'s US position, such as
/// "Ö" or "^" for the key a US layout calls `. The character it types wins
/// over the key's name, which for a dead key is a word ("ZIRKUMFLEX").
pub fn key_label(vk: u8) -> Option<String> {
    let typed = layout_char(vk, foreground_layout())
        .map(|(ch, _)| ch)
        .filter(|ch| !ch.is_whitespace() && !ch.is_control());
    if let Some(ch) = typed {
        return Some(ch.to_uppercase().collect());
    }
    let scan = scan_code(vk)?;
    let lparam = (i32::from(scan & 0xFF) << 16) | (i32::from(scan & 0x100 != 0) << 24);
    let mut name = [0u16; 64];
    // SAFETY: `name` is a live buffer whose length the call is given.
    let len = unsafe { GetKeyNameTextW(lparam, &mut name) };
    let name = String::from_utf16_lossy(&name[..usize::try_from(len).ok()?]);
    let name = name.trim();
    // Single characters come back as typed ("ö"); show them as on the key.
    (!name.is_empty()).then(|| {
        if name.chars().count() == 1 {
            name.to_uppercase()
        } else {
            name.to_owned()
        }
    })
}

/// Whether the key at `vk`'s US position is a dead key on the foreground
/// layout, so pressing it outside the chord starts an accent instead of
/// typing.
pub fn is_dead_key(vk: u8) -> bool {
    layout_char(vk, foreground_layout()).is_some_and(|(_, dead)| dead)
}

/// Replaces the hook and forgets tracked keys, whose key-ups may have been
/// missed while the hook was gone.
fn reinstall() -> io::Result<()> {
    // SAFETY: installs a hook whose procedure lives for the whole process;
    // the old handle came from an earlier successful install.
    unsafe {
        let module = GetModuleHandleW(None).map_err(io::Error::other)?;
        let hook = SetWindowsHookExW(
            WH_KEYBOARD_LL,
            Some(keyboard_proc),
            Some(HINSTANCE(module.0)),
            0,
        )
        .map_err(io::Error::other)?;
        let old = STATE.with(|cell| {
            let mut state = cell.borrow_mut();
            let state = state.as_mut()?;
            // No carried key here: its key-up may be among those missed.
            if let Some(signal) = state.tracker.reset() {
                state.trigger.signal(signal);
            }
            state.hook.replace(hook)
        });
        if let Some(old) = old {
            let _ = UnhookWindowsHookEx(old);
        }
        LAST_CALLBACK.store(GetTickCount(), Ordering::Relaxed);
    }
    Ok(())
}

fn reinstall_logged(reason: &'static str) {
    let result = reinstall();
    HOOK_LOST.store(result.is_err(), Ordering::Release);
    match result {
        Ok(()) => tracing::info!(reason, "Trigger Key hook reinstalled"),
        Err(error) => tracing::error!(%error, reason, "Trigger Key hook reinstall failed"),
    }
    report_health();
}

/// Tells the window when [`is_running`] changed since it was last told.
/// Never called from the hook callback, which must not block.
fn report_health() {
    let ok = is_running();
    if REPORTED_OK.swap(ok, Ordering::Relaxed) != ok
        && let Some(health_changed) = HEALTH_CHANGED.get()
    {
        health_changed();
    }
}

fn watcher_window() -> windows::core::Result<HWND> {
    // SAFETY: the class and window names are static strings and the class
    // struct outlives the registration call.
    unsafe {
        let module = GetModuleHandleW(None)?;
        let class = WNDCLASSW {
            lpfnWndProc: Some(watcher_proc),
            hInstance: HINSTANCE(module.0),
            lpszClassName: w!("VoisuHotkeyWatcher"),
            ..Default::default()
        };
        RegisterClassW(&class);
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("VoisuHotkeyWatcher"),
            w!("Voisu"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(HINSTANCE(module.0)),
            None,
        )
    }
}

unsafe extern "system" fn watcher_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_WTSSESSION_CHANGE if wparam.0 as u32 == WTS_SESSION_UNLOCK => {
            reinstall_logged("session unlocked");
        }
        WM_POWERBROADCAST if wparam.0 as u32 == PBT_APMRESUMEAUTOMATIC => {
            reinstall_logged("resumed");
        }
        WM_TIMER if wparam.0 == HEALTH_TIMER => {
            if hook_went_silent() {
                reinstall_logged("hook stopped receiving input");
            }
            // Picks up a false alarm the callback cleared.
            report_health();
        }
        _ => {}
    }
    // SAFETY: forwards this window's own message unchanged.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// True when the user produced input well after the callback last ran.
/// Mouse-only use also trips it; a spare reinstall is harmless.
fn hook_went_silent() -> bool {
    let mut info = LASTINPUTINFO {
        cbSize: size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    // SAFETY: `info` is a live, correctly sized LASTINPUTINFO.
    if !unsafe { GetLastInputInfo(&mut info) }.as_bool() {
        return false;
    }
    let last_callback = LAST_CALLBACK.load(Ordering::Relaxed);
    let input_after = info.dwTime.wrapping_sub(last_callback);
    // Wrapping tick arithmetic: "after" only within half the range.
    input_after > SILENT_HOOK_MS && input_after < u32::MAX / 2
}

fn inject_mask() {
    let key = |flags: KEYBD_EVENT_FLAGS| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(u16::from(MASK_VK)),
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: MASK_TAG,
            },
        },
    };
    let inputs = [key(KEYBD_EVENT_FLAGS(0)), key(KEYEVENTF_KEYUP)];
    // SAFETY: a fixed array of fully initialised keyboard inputs.
    unsafe { SendInput(&inputs, size_of::<INPUT>() as i32) };
}

fn classify(event: &KBDLLHOOKSTRUCT) -> Key {
    match event.vkCode {
        VK_LCONTROL => Key::Mod(Modifier::Ctrl, Side::Left),
        VK_RCONTROL => Key::Mod(Modifier::Ctrl, Side::Right),
        VK_LMENU => Key::Mod(Modifier::Alt, Side::Left),
        VK_RMENU => Key::Mod(Modifier::Alt, Side::Right),
        VK_LSHIFT => Key::Mod(Modifier::Shift, Side::Left),
        VK_RSHIFT => Key::Mod(Modifier::Shift, Side::Right),
        VK_LWIN => Key::Mod(Modifier::Win, Side::Left),
        VK_RWIN => Key::Mod(Modifier::Win, Side::Right),
        VK_ESCAPE => Key::Escape,
        // Unicode text: the scan code field holds a UTF-16 unit.
        VK_PACKET_CODE => Key::Other(VK_PACKET, 0),
        vk => {
            let scan = match event.scanCode & 0xFF {
                0 => 0,
                code if event.flags.contains(LLKHF_EXTENDED) => code as u16 | 0x100,
                code => code as u16,
            };
            Key::Other(vk as u8, scan)
        }
    }
}

fn held(vk: i32) -> bool {
    // SAFETY: reads the asynchronous key state; no pointers involved.
    unsafe { GetAsyncKeyState(vk) < 0 }
}

/// The OS's view of the modifiers, read only on a modifier or chord-key
/// press.
fn os_modifiers() -> ModMask {
    let mut mask = ModMask::default();
    for (modifier, down) in [
        (Modifier::Ctrl, held(VK_CONTROL)),
        (Modifier::Alt, held(VK_MENU)),
        (Modifier::Shift, held(VK_SHIFT)),
        (Modifier::Win, held(VK_LWIN as i32) || held(VK_RWIN as i32)),
    ] {
        if down {
            mask = mask.with(modifier);
        }
    }
    mask
}

fn post_mask(state: &HookState) {
    // SAFETY: posts to this thread's own queue; never blocks.
    let _ = unsafe { PostThreadMessageW(state.thread_id, WM_INJECT_MASK, WPARAM(0), LPARAM(0)) };
}

/// Returns true to swallow the event.
fn on_key(event: &KBDLLHOOKSTRUCT, message: u32) -> bool {
    // AltGr's synthetic LCtrl never forms or breaks the chord.
    if event.vkCode == VK_LCONTROL && event.scanCode & ALTGR_SCAN_BIT != 0 {
        return false;
    }
    let down = message == WM_KEYDOWN || message == WM_SYSKEYDOWN;
    let injected = event.flags.contains(LLKHF_INJECTED);
    let key = match classify(event) {
        key if injected => key.injected(),
        key => key,
    };
    let tagged = event.dwExtraInfo == MASK_TAG;
    STATE.with(|cell| {
        // A failed borrow means re-entry; let the event through untouched.
        let Ok(mut state) = cell.try_borrow_mut() else {
            return false;
        };
        let Some(state) = state.as_mut() else {
            return false;
        };
        // Injected keys never form or break the chord, except an untagged
        // chord key such as a remapper's F24.
        if injected && !state.tracker.admits_injected(key, tagged) {
            return false;
        }
        let armed = state.trigger.armed();
        let verdict = state.tracker.handle(key, down, os_modifiers, armed);
        if let Some(signal) = verdict.signal {
            state.trigger.signal(signal);
        }
        if verdict.inject_mask {
            post_mask(state);
        }
        !verdict.pass
    })
}

unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: for HC_ACTION, lparam points at the event's KBDLLHOOKSTRUCT;
    // every other code is forwarded untouched.
    unsafe {
        if code == HC_ACTION as i32 {
            LAST_CALLBACK.store(GetTickCount(), Ordering::Relaxed);
            // The hook a failed reinstall left behind still works after all.
            if HOOK_LOST.load(Ordering::Relaxed) {
                HOOK_LOST.store(false, Ordering::Relaxed);
            }
            let event = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
            if on_key(event, wparam.0 as u32) {
                return LRESULT(1);
            }
        }
        CallNextHookEx(None, code, wparam, lparam)
    }
}
