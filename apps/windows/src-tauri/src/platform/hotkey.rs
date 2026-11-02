//! The Ctrl+Win Trigger Key: a low-level keyboard hook on its own thread.
//!
//! The callback stays O(1) and never blocks, logs or allocates, because
//! Windows silently removes a hook that exceeds `LowLevelHooksTimeout`. It
//! classifies the key, feeds [`ChordTracker`] and `try_send`s any signal to
//! the gesture thread. The Start-menu mask key is posted back to this
//! thread's message loop and injected after the callback returns, so the
//! hook never re-enters itself.
//!
//! The hook is reinstalled on session unlock, on resume from sleep, and by a
//! health check when input arrived but the callback went quiet, which is how
//! a silently removed hook shows.

use std::cell::RefCell;
use std::io;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::thread;

use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::RemoteDesktop::{
    NOTIFY_FOR_THIS_SESSION, WTSRegisterSessionNotification,
};
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetLastInputInfo, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS,
    KEYBDINPUT, KEYEVENTF_KEYUP, LASTINPUTINFO, SendInput, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, HC_ACTION,
    HHOOK, KBDLLHOOKSTRUCT, LLKHF_INJECTED, MSG, PBT_APMRESUMEAUTOMATIC, PostThreadMessageW,
    RegisterClassW, SetTimer, SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL,
    WINDOW_EX_STYLE, WM_APP, WM_KEYDOWN, WM_POWERBROADCAST, WM_SYSKEYDOWN, WM_TIMER,
    WM_WTSSESSION_CHANGE, WNDCLASSW, WS_OVERLAPPED, WTS_SESSION_UNLOCK,
};
use windows::core::w;

use crate::chord::{ChordTracker, Key, Side};
use crate::trigger::Trigger;

const VK_CONTROL: i32 = 0x11;
const VK_ESCAPE: u32 = 0x1B;
const VK_LWIN: u32 = 0x5B;
const VK_RWIN: u32 = 0x5C;
const VK_LCONTROL: u32 = 0xA2;
const VK_RCONTROL: u32 = 0xA3;
/// Unassigned virtual key injected while Win is held, so releasing Win does
/// not open Start. The same key AutoHotkey and PowerToys use.
const VK_MASK: u16 = 0xE8;
/// AltGr arrives as a synthesized LCtrl whose scan code has this bit set.
const ALTGR_SCAN_BIT: u32 = 0x200;
/// Tags Voisu's own injected keys.
const MASK_TAG: usize = 0x564F_4953; // "VOIS"

/// Posted by the callback: inject the mask key now.
const WM_INJECT_MASK: u32 = WM_APP + 1;
const HEALTH_TIMER: usize = 1;
const HEALTH_INTERVAL_MS: u32 = 5_000;
/// Input newer than the last callback by this much means the hook is gone.
const SILENT_HOOK_MS: u32 = 30_000;

/// Tick of the most recent callback, read by the health check.
static LAST_CALLBACK: AtomicU32 = AtomicU32::new(0);

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

/// Starts the hook thread and returns once the hook is installed.
pub fn install(trigger: Trigger) -> io::Result<()> {
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("voisu-hotkey".into())
        .spawn(move || run(trigger, ready_tx))?;
    ready_rx
        .recv()
        .map_err(|_| io::Error::other("hotkey thread exited"))?
}

fn run(trigger: Trigger, ready: mpsc::SyncSender<io::Result<()>>) {
    // SAFETY: plain Win32 calls on this thread; every pointer passed is to a
    // live local.
    unsafe {
        STATE.with(|cell| {
            *cell.borrow_mut() = Some(HookState {
                tracker: ChordTracker::default(),
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
        tracing::info!("Ctrl+Win hook installed");
        let _ = ready.send(Ok(()));

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if msg.hwnd.is_invalid() && msg.message == WM_INJECT_MASK {
                inject_mask();
                continue;
            }
            DispatchMessageW(&msg);
        }
    }
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
            state.tracker.reset();
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
    match reinstall() {
        Ok(()) => tracing::info!(reason, "Ctrl+Win hook reinstalled"),
        Err(error) => tracing::error!(%error, reason, "Ctrl+Win hook reinstall failed"),
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
        WM_TIMER if wparam.0 == HEALTH_TIMER && hook_went_silent() => {
            reinstall_logged("hook stopped receiving input");
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
                wVk: VIRTUAL_KEY(VK_MASK),
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

fn classify(vk: u32) -> Key {
    match vk {
        VK_LCONTROL => Key::Ctrl(Side::Left),
        VK_RCONTROL => Key::Ctrl(Side::Right),
        VK_LWIN => Key::Win(Side::Left),
        VK_RWIN => Key::Win(Side::Right),
        VK_ESCAPE => Key::Escape,
        _ => Key::Other,
    }
}

fn held(vk: i32) -> bool {
    // SAFETY: reads the asynchronous key state; no pointers involved.
    unsafe { GetAsyncKeyState(vk) < 0 }
}

/// Returns true to swallow the event.
fn on_key(event: &KBDLLHOOKSTRUCT, message: u32) -> bool {
    // Injected keys (Voisu's own mask key included) and AltGr's synthetic
    // LCtrl never form or break the chord.
    if event.flags.contains(LLKHF_INJECTED)
        || (event.vkCode == VK_LCONTROL && event.scanCode & ALTGR_SCAN_BIT != 0)
    {
        return false;
    }
    let down = message == WM_KEYDOWN || message == WM_SYSKEYDOWN;
    let key = classify(event.vkCode);
    let other_down = match key {
        Key::Ctrl(_) => held(VK_LWIN as i32) || held(VK_RWIN as i32),
        Key::Win(_) => held(VK_CONTROL),
        Key::Escape | Key::Other => false,
    };
    STATE.with(|cell| {
        // A failed borrow means re-entry; let the event through untouched.
        let Ok(mut state) = cell.try_borrow_mut() else {
            return false;
        };
        let Some(state) = state.as_mut() else {
            return false;
        };
        let verdict = state
            .tracker
            .handle(key, down, other_down, state.trigger.armed());
        if let Some(signal) = verdict.signal {
            state.trigger.signal(signal);
        }
        if verdict.inject_mask {
            // SAFETY: posts to this thread's own queue; never blocks.
            let _ = unsafe {
                PostThreadMessageW(state.thread_id, WM_INJECT_MASK, WPARAM(0), LPARAM(0))
            };
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
            let event = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
            if on_key(event, wparam.0 as u32) {
                return LRESULT(1);
            }
        }
        CallNextHookEx(None, code, wparam, lparam)
    }
}
