//! macOS Delivery: the Transcript goes on the general pasteboard, Cmd+V is
//! posted to the app that was frontmost when the Recording started, and the
//! user's clipboard comes back shortly after.
//!
//! 1. Preflight: the app has quit, or it is Voisu itself.
//! 2. Without Accessibility macOS drops posted keystrokes, so the Transcript
//!    is only copied and the outcome is `Copied`.
//! 3. Bring the target back to the front if something else took it. If
//!    macOS refuses or it takes too long, the Transcript is only copied.
//! 4. Snapshot the pasteboard: every item's types and bytes, in pasteboard
//!    order. It is not restored at all if it holds a type
//!    [`mac_never_restore`] names or more than `SNAPSHOT_BUDGET`.
//! 5. Write the Transcript, marked transient and concealed so clipboard
//!    managers that follow the nspasteboard.org conventions skip it, and
//!    post Cmd+V tagged with `MASK_TAG` so the Trigger Key's tap ignores it.
//! 6. macOS has no read receipt: report the paste as sent, wait
//!    [`mac_restore_due`], then restore while the pasteboard's change count
//!    is still Voisu's.
//!
//! The paste key is the key at the ANSI V position. On a layout that moves
//! V without a "⌘ QWERTY" variant (plain Dvorak) Cmd+V lands elsewhere.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};

use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::ProtocolObject;
use objc2_app_kit::{
    NSApplicationActivationOptions, NSPasteboard, NSPasteboardItem, NSPasteboardTypeString,
    NSPasteboardWriting, NSRunningApplication, NSWorkspace,
};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapLocation,
};
use objc2_foundation::{NSArray, NSData, NSString};

use super::{
    CHORD_HOLD, DeliveryError, DeliveryOutcome, Integrity, Probe, SNAPSHOT_BUDGET,
    mac_never_restore, mac_restore_due, may_restore, preflight,
};
use crate::chord::MASK_TAG;
use crate::mac_keys::keycode;
use crate::platform::permissions;

/// Types that keep the Transcript out of clipboard history apps.
const PRIVACY_TYPES: [&str; 2] = [
    "org.nspasteboard.TransientType",
    "org.nspasteboard.ConcealedType",
];
/// How long the target gets to come back to the front.
const ACTIVATE_WAIT: Duration = Duration::from_millis(500);
const ACTIVATE_POLL: Duration = Duration::from_millis(20);
const RESTORE_POLL: Duration = Duration::from_millis(50);

static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());
/// Deliveries waiting on `ONE_AT_A_TIME`; one waiting shortens the wait of
/// the delivery ahead of it.
static QUEUED: AtomicUsize = AtomicUsize::new(0);

/// One pasteboard item: each type it holds and its bytes.
type Item = Vec<(Retained<NSString>, Retained<NSData>)>;

pub fn foreground() -> isize {
    autoreleasepool(|_| {
        NSWorkspace::sharedWorkspace()
            .frontmostApplication()
            .map_or(0, |app| app.processIdentifier() as isize)
    })
}

pub fn deliver(
    pid: isize,
    text: &str,
    early: &mut dyn FnMut(Result<DeliveryOutcome, DeliveryError>),
) -> Result<DeliveryOutcome, DeliveryError> {
    let _serial = queue();
    autoreleasepool(|_| deliver_serial(pid, text, early))
}

fn deliver_serial(
    pid: isize,
    text: &str,
    early: &mut dyn FnMut(Result<DeliveryOutcome, DeliveryError>),
) -> Result<DeliveryOutcome, DeliveryError> {
    let app = i32::try_from(pid)
        .ok()
        .filter(|&pid| pid > 0)
        .and_then(NSRunningApplication::runningApplicationWithProcessIdentifier)
        .filter(|app| !app.isTerminated());
    let own_pid = NSRunningApplication::currentApplication().processIdentifier();
    let probe = Probe {
        exists: app.is_some(),
        pid: pid as u32,
        own_pid: own_pid as u32,
        target: Integrity::Unknown,
        own: Integrity::Unknown,
    };
    if let Some(outcome) = preflight(probe) {
        return Ok(outcome);
    }
    let Some(app) = app else {
        return Ok(DeliveryOutcome::TargetGone);
    };
    if text.is_empty() {
        return Ok(DeliveryOutcome::Pasted);
    }
    let pasteboard = NSPasteboard::generalPasteboard();
    if !permissions::accessibility_trusted() {
        write(&pasteboard, text)?;
        return Ok(DeliveryOutcome::Copied);
    }
    if !bring_to_front(&app) {
        tracing::info!("target app did not come to the front; copying only");
        write(&pasteboard, text)?;
        return Ok(DeliveryOutcome::Copied);
    }
    let saved = snapshot(&pasteboard);
    let ours = write(&pasteboard, text)?;
    post_paste()?;
    let chord_at = Instant::now();
    early(Ok(DeliveryOutcome::NotConfirmed));
    while !mac_restore_due(chord_at.elapsed(), QUEUED.load(Ordering::SeqCst) > 0) {
        sleep(RESTORE_POLL);
    }
    let now = pasteboard.changeCount();
    // The change count is the pasteboard's sequence number; any write since
    // Voisu's means the user or another app put something new there.
    if let Some(saved) = saved
        && may_restore(true, now as u32, ours as u32)
    {
        restore(&pasteboard, saved);
    }
    Ok(DeliveryOutcome::NotConfirmed)
}

pub fn copy(text: &str) -> Result<(), DeliveryError> {
    let _serial = queue();
    autoreleasepool(|_| write(&NSPasteboard::generalPasteboard(), text).map(drop))
}

/// Waits behind any delivery in progress, counted as queued meanwhile.
fn queue() -> std::sync::MutexGuard<'static, ()> {
    QUEUED.fetch_add(1, Ordering::SeqCst);
    let serial = ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    QUEUED.fetch_sub(1, Ordering::SeqCst);
    serial
}

/// Whether the target is frontmost, activating it first if it is not. False
/// when macOS refuses or it is still behind after `ACTIVATE_WAIT`.
fn bring_to_front(app: &NSRunningApplication) -> bool {
    let pid = app.processIdentifier() as isize;
    if foreground() == pid {
        return true;
    }
    if !app.activateWithOptions(NSApplicationActivationOptions::empty()) {
        return false;
    }
    let deadline = Instant::now() + ACTIVATE_WAIT;
    while Instant::now() < deadline {
        sleep(ACTIVATE_POLL);
        if foreground() == pid {
            return true;
        }
    }
    false
}

/// Every item's types and bytes, in pasteboard order, or `None` when the
/// clipboard must not be restored: it holds a type [`mac_never_restore`]
/// names, or more than `SNAPSHOT_BUDGET`. Types are checked before any data
/// is read, so a file promise is never fulfilled; data an app provides
/// lazily is still produced by the read.
fn snapshot(pasteboard: &NSPasteboard) -> Option<Vec<Item>> {
    let Some(items) = pasteboard.pasteboardItems() else {
        return Some(Vec::new());
    };
    let types: Vec<_> = items.iter().map(|item| item.types()).collect();
    if types
        .iter()
        .flat_map(|kinds| kinds.iter())
        .any(|kind| mac_never_restore(&kind.to_string()))
    {
        tracing::debug!("clipboard is private or promised; it is not restored");
        return None;
    }
    let mut budget = SNAPSHOT_BUDGET;
    let mut saved = Vec::with_capacity(items.len());
    for (item, kinds) in items.iter().zip(&types) {
        let mut entry = Item::new();
        for kind in kinds.iter() {
            let Some(data) = item.dataForType(&kind) else {
                continue;
            };
            let Some(left) = budget.checked_sub(data.len()) else {
                tracing::warn!("clipboard too large to restore");
                return None;
            };
            budget = left;
            entry.push((kind, data));
        }
        saved.push(entry);
    }
    Some(saved)
}

fn restore(pasteboard: &NSPasteboard, saved: Vec<Item>) {
    pasteboard.clearContents();
    let items: Vec<Retained<ProtocolObject<dyn NSPasteboardWriting>>> = saved
        .into_iter()
        .filter(|item| !item.is_empty())
        .map(|item| {
            let restored = NSPasteboardItem::new();
            for (kind, data) in &item {
                restored.setData_forType(data, kind);
            }
            ProtocolObject::from_retained(restored)
        })
        .collect();
    if !items.is_empty() && !pasteboard.writeObjects(&NSArray::from_retained_slice(&items)) {
        tracing::warn!("clipboard restore failed");
    }
}

/// Replaces the pasteboard with `text` and returns its new change count.
fn write(pasteboard: &NSPasteboard, text: &str) -> Result<isize, DeliveryError> {
    pasteboard.clearContents();
    // SAFETY: a framework constant.
    let string_type = unsafe { NSPasteboardTypeString };
    if !pasteboard.setString_forType(&NSString::from_str(text), string_type) {
        return Err(DeliveryError::Clipboard("could not copy the text"));
    }
    let empty = NSData::new();
    for kind in PRIVACY_TYPES {
        pasteboard.setData_forType(Some(&empty), &NSString::from_str(kind));
    }
    Ok(pasteboard.changeCount())
}

/// Posts Cmd+V as the hardware would, tagged as Voisu's own.
fn post_paste() -> Result<(), DeliveryError> {
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState);
    let key = |down| {
        let event = CGEvent::new_keyboard_event(source.as_deref(), keycode::ANSI_V, down)?;
        // Only Command, whatever else is still held.
        CGEvent::set_flags(Some(&event), CGEventFlags::MaskCommand);
        CGEvent::set_integer_value_field(
            Some(&event),
            CGEventField::EventSourceUserData,
            MASK_TAG as i64,
        );
        Some(event)
    };
    let (Some(down), Some(up)) = (key(true), key(false)) else {
        return Err(DeliveryError::Input);
    };
    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&down));
    sleep(CHORD_HOLD);
    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&up));
    Ok(())
}
