// Hand-rolled LibEI FFI: RemoteDesktop portal session and direct input delivery.
//
// Split out of system.rs as a pure move; module-global items come from `super`.

use super::*;

const REMOTE_DESKTOP_INTERFACE: &str = "org.freedesktop.portal.RemoteDesktop";

const KEYBOARD_DEVICE: u32 = 1;

const PERSIST_UNTIL_REVOKED: u32 = 2;

const MAX_RESTORE_TOKEN_BYTES: u64 = 4 * 1024;

static REMOTE_DESKTOP_TOKEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

static RESTORE_TOKEN_TEMP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn restore_token_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("VOISU_REMOTE_DESKTOP_TOKEN_FILE") {
        let path = PathBuf::from(path);
        return path.is_absolute().then_some(path);
    }
    let state_root = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .map(|home| home.join(".local/state"))
        })?;
    Some(state_root.join("voisu/remote-desktop.restore-token"))
}

fn private_restore_token_file(path: &Path) -> Option<File> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.file_type().is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
        || metadata.len() > MAX_RESTORE_TOKEN_BYTES
    {
        return None;
    }
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .ok()
}

fn load_restore_token() -> Option<String> {
    let path = restore_token_path()?;
    let file = private_restore_token_file(&path)?;
    let mut token = String::new();
    file.take(MAX_RESTORE_TOKEN_BYTES + 1)
        .read_to_string(&mut token)
        .ok()?;
    (!token.is_empty() && token.len() as u64 <= MAX_RESTORE_TOKEN_BYTES).then_some(token)
}

fn persist_restore_token(token: &str) -> bool {
    if token.is_empty() || token.len() as u64 > MAX_RESTORE_TOKEN_BYTES {
        return false;
    }
    let Some(path) = restore_token_path() else {
        return false;
    };
    let Some(parent) = path.parent() else {
        return false;
    };
    if fs::create_dir_all(parent).is_err()
        || fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).is_err()
    {
        return false;
    }
    let Ok(parent_metadata) = fs::symlink_metadata(parent) else {
        return false;
    };
    if !parent_metadata.file_type().is_dir()
        || parent_metadata.uid() != unsafe { libc::geteuid() }
        || parent_metadata.mode() & 0o077 != 0
    {
        return false;
    }
    let sequence = RESTORE_TOKEN_TEMP.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temp = parent.join(format!(
        ".remote-desktop.restore-token.{}.{}.tmp",
        std::process::id(),
        sequence
    ));
    let written = (|| -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&temp)?;
        file.write_all(token.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temp, &path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        Ok(())
    })();
    if written.is_err() {
        let _ = fs::remove_file(&temp);
        return false;
    }
    true
}

pub(super) fn clear_restore_token() {
    if let Some(path) = restore_token_path() {
        let _ = fs::remove_file(path);
    }
}

pub struct FedoraRemoteDesktopPortal;

pub(super) struct DisabledRemoteDesktopPortal;

impl RemoteDesktopPortal for DisabledRemoteDesktopPortal {
    fn connect(&mut self) -> BoundaryFuture<'_, Box<dyn DirectDeliverySession>> {
        Box::pin(async {
            Err(BoundaryError::new(
                BoundaryKind::Delivery,
                "direct Delivery disabled for this run",
            ))
        })
    }
}

impl FedoraRemoteDesktopPortal {
    async fn connect_inner(
        force_keyboard: bool,
    ) -> Result<Box<dyn DirectDeliverySession>, BoundaryError> {
        use std::sync::atomic::Ordering;
        use zbus::zvariant::Value;

        let connection = zbus::Connection::session().await.map_err(|_| {
            BoundaryError::new(BoundaryKind::Delivery, "RemoteDesktop portal unavailable")
        })?;
        let portal = zbus::Proxy::new(
            &connection,
            PORTAL_BUS_NAME,
            PORTAL_OBJECT_PATH,
            REMOTE_DESKTOP_INTERFACE,
        )
        .await
        .map_err(|_| {
            BoundaryError::new(BoundaryKind::Delivery, "RemoteDesktop portal unavailable")
        })?;

        let unique = REMOTE_DESKTOP_TOKEN.fetch_add(1, Ordering::Relaxed);
        let prefix = format!("voisu_delivery_{}_{}", std::process::id(), unique);
        let session_token = format!("{prefix}_session");
        let session_path = format!(
            "/org/freedesktop/portal/desktop/session/{}/{session_token}",
            escaped_sender(&connection).map_err(|_| BoundaryError::new(
                BoundaryKind::Delivery,
                "RemoteDesktop portal unavailable",
            ))?
        );
        let create_options: std::collections::HashMap<&str, Value<'_>> =
            std::collections::HashMap::from([
                ("handle_token", Value::from(format!("{prefix}_create"))),
                ("session_handle_token", Value::from(session_token.as_str())),
            ]);
        let create_results = portal_request(
            &connection,
            &portal,
            BoundaryKind::Delivery,
            "CreateSession",
            &(create_options,),
            PORTAL_SESSION_DEADLINE,
        )
        .await
        .map_err(classify_remote_desktop_failure)?;
        let session_path = session_handle_from(&create_results).unwrap_or(session_path);
        let session_object: zbus::zvariant::OwnedObjectPath =
            zbus::zvariant::ObjectPath::try_from(session_path.as_str())
                .map_err(|_| BoundaryError::new(BoundaryKind::Delivery, "permission denied"))?
                .into();
        let session_proxy = zbus::Proxy::new(
            &connection,
            PORTAL_BUS_NAME,
            session_path.clone(),
            PORTAL_SESSION_INTERFACE,
        )
        .await
        .map_err(|_| BoundaryError::new(BoundaryKind::Delivery, "permission denied"))?;
        let closures = session_proxy
            .receive_signal("Closed")
            .await
            .map_err(|_| BoundaryError::new(BoundaryKind::Delivery, "permission denied"))?;

        let restore_token = load_restore_token();
        let mut select_options: std::collections::HashMap<&str, Value<'_>> =
            std::collections::HashMap::from([
                ("handle_token", Value::from(format!("{prefix}_select"))),
                ("types", Value::from(KEYBOARD_DEVICE)),
                ("persist_mode", Value::from(PERSIST_UNTIL_REVOKED)),
            ]);
        if let Some(token) = restore_token.as_deref() {
            select_options.insert("restore_token", Value::from(token));
        }
        if let Err(error) = portal_request(
            &connection,
            &portal,
            BoundaryKind::Delivery,
            "SelectDevices",
            &(session_object.clone(), select_options),
            PORTAL_BIND_DEADLINE,
        )
        .await
        {
            return Err(fail_and_close(&connection, session_path.as_str(), error).await);
        }

        let start_options: std::collections::HashMap<&str, Value<'_>> =
            std::collections::HashMap::from([(
                "handle_token",
                Value::from(format!("{prefix}_start")),
            )]);
        let started = match portal_request(
            &connection,
            &portal,
            BoundaryKind::Delivery,
            "Start",
            &(session_object.clone(), "", start_options),
            PORTAL_BIND_DEADLINE,
        )
        .await
        {
            Ok(results) => results,
            Err(error) => {
                return Err(fail_and_close(&connection, session_path.as_str(), error).await);
            }
        };
        if let Some(token) = started
            .get("restore_token")
            .and_then(|value| value.downcast_ref::<zbus::zvariant::Str<'_>>().ok())
        {
            let _ = persist_restore_token(token.as_str());
        } else if restore_token.is_some() {
            // Restore tokens are single-use. If Start did not rotate the
            // supplied token, retaining it would guarantee a stale retry.
            clear_restore_token();
        }
        let devices = started
            .get("devices")
            .and_then(|value| value.downcast_ref::<u32>().ok())
            .unwrap_or(0);
        if devices & KEYBOARD_DEVICE == 0 {
            close_portal_session(&connection, session_path.as_str()).await;
            return Err(BoundaryError::new(
                BoundaryKind::Delivery,
                "keyboard permission unavailable",
            ));
        }

        let options: std::collections::HashMap<&str, Value<'_>> = std::collections::HashMap::new();
        let reply = portal
            .call_method("ConnectToEIS", &(session_object.clone(), options))
            .await
            .map_err(|_| {
                BoundaryError::new(BoundaryKind::Delivery, "libei connection unavailable")
            })?;
        let fd: zbus::zvariant::OwnedFd = reply.body().deserialize().map_err(|_| {
            BoundaryError::new(BoundaryKind::Delivery, "libei connection unavailable")
        })?;
        let fd: std::os::fd::OwnedFd = fd.into();
        let sender_result = tokio::task::spawn_blocking(move || {
            NativeEiSender::connect_with_keyboard(fd.into_raw_fd(), force_keyboard)
        })
        .await
        .map_err(|_| BoundaryError::new(BoundaryKind::Delivery, "libei connection unavailable"))?;
        let sender = match sender_result {
            Ok(sender) => sender,
            Err(error) => {
                close_portal_session(&connection, session_path.as_str()).await;
                return Err(error);
            }
        };

        Ok(Box::new(FedoraDirectDeliverySession {
            connection,
            session_path: session_object,
            closures,
            sender: Some(sender),
        }) as Box<dyn DirectDeliverySession>)
    }
}

impl RemoteDesktopPortal for FedoraRemoteDesktopPortal {
    fn connect(&mut self) -> BoundaryFuture<'_, Box<dyn DirectDeliverySession>> {
        Box::pin(Self::connect_inner(false))
    }

    fn connect_paste(&mut self) -> BoundaryFuture<'_, Box<dyn DirectDeliverySession>> {
        Box::pin(Self::connect_inner(true))
    }
}

pub(super) fn classify_remote_desktop_failure(error: BoundaryError) -> BoundaryError {
    let permanent = error.is_permanent();
    let reason = if error.is_permanent() || error.diagnostic().contains("denied or cancelled") {
        "permission denied"
    } else {
        "RemoteDesktop portal unavailable"
    };
    let classified = BoundaryError::new(BoundaryKind::Delivery, reason);
    if permanent {
        classified.permanent()
    } else {
        classified
    }
}

async fn fail_and_close(
    connection: &zbus::Connection,
    session_path: &str,
    error: BoundaryError,
) -> BoundaryError {
    close_portal_session(connection, session_path).await;
    let error = classify_remote_desktop_failure(error);
    if error.is_permanent() || terminal_remote_desktop_failure(error.diagnostic()) {
        clear_restore_token();
    }
    error
}

struct FedoraDirectDeliverySession {
    connection: zbus::Connection,
    session_path: zbus::zvariant::OwnedObjectPath,
    closures: zbus::proxy::SignalStream<'static>,
    sender: Option<NativeEiSender>,
}

impl DirectDeliverySession for FedoraDirectDeliverySession {
    fn deliver_text(&mut self, text: &str) -> BoundaryFuture<'_, ()> {
        let text = text.to_owned();
        Box::pin(async move {
            use zbus::export::ordered_stream::OrderedStreamExt;
            if matches!(
                tokio::time::timeout(Duration::from_millis(1), self.closures.next()).await,
                Ok(Some(_))
            ) {
                clear_restore_token();
                return Err(BoundaryError::new(
                    BoundaryKind::Delivery,
                    "permission revoked",
                ));
            }
            let mut sender = self
                .sender
                .take()
                .ok_or_else(|| BoundaryError::new(BoundaryKind::Delivery, "libei disconnected"))?;
            let (returned, result) = tokio::task::spawn_blocking(move || {
                let result = sender.deliver(&text);
                (sender, result)
            })
            .await
            .map_err(|_| BoundaryError::new(BoundaryKind::Delivery, "libei disconnected"))?;
            self.sender = Some(returned);
            result
        })
    }

    fn deliver_shortcut(&mut self, shortcut: &str) -> BoundaryFuture<'_, ()> {
        let shortcut = shortcut.to_owned();
        Box::pin(async move {
            use zbus::export::ordered_stream::OrderedStreamExt;
            if matches!(
                tokio::time::timeout(Duration::from_millis(1), self.closures.next()).await,
                Ok(Some(_))
            ) {
                clear_restore_token();
                return Err(BoundaryError::new(
                    BoundaryKind::Delivery,
                    "permission revoked",
                ));
            }
            let mut sender = self
                .sender
                .take()
                .ok_or_else(|| BoundaryError::new(BoundaryKind::Delivery, "libei disconnected"))?;
            let (returned, result) = tokio::task::spawn_blocking(move || {
                let result = sender.deliver_shortcut(&shortcut);
                (sender, result)
            })
            .await
            .map_err(|_| BoundaryError::new(BoundaryKind::Delivery, "libei disconnected"))?;
            self.sender = Some(returned);
            result
        })
    }
}

impl Drop for FedoraDirectDeliverySession {
    fn drop(&mut self) {
        let connection = self.connection.clone();
        let path = self.session_path.to_string();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move { close_portal_session(&connection, &path).await });
        }
    }
}

#[repr(C)]
struct EiContext {
    _private: [u8; 0],
}

#[repr(C)]
struct EiEvent {
    _private: [u8; 0],
}

#[repr(C)]
struct EiSeat {
    _private: [u8; 0],
}

#[repr(C)]
struct EiDevice {
    _private: [u8; 0],
}

#[repr(C)]
struct EiPing {
    _private: [u8; 0],
}

#[repr(C)]
struct EiKeymap {
    _private: [u8; 0],
}

type SeatBindCapabilities = unsafe extern "C" fn(*mut EiSeat, ...);

// libei protocol constants, verbatim from libei.h (verified against the
// libei-devel 1.5.0 header shipped for the host runtime):
//   enum ei_event_type { EI_EVENT_CONNECT = 1, EI_EVENT_DISCONNECT,
//     EI_EVENT_SEAT_ADDED, EI_EVENT_SEAT_REMOVED, EI_EVENT_DEVICE_ADDED,
//     EI_EVENT_DEVICE_REMOVED, EI_EVENT_DEVICE_PAUSED, EI_EVENT_DEVICE_RESUMED,
//     ..., EI_EVENT_PONG = 90, ... }
//   enum ei_device_capability { EI_DEVICE_CAP_POINTER = (1 << 0), ...,
//     EI_DEVICE_CAP_BUTTON = (1 << 5) }
const EI_EVENT_DISCONNECT: libc::c_int = 2;

const EI_EVENT_SEAT_ADDED: libc::c_int = 3;

const EI_EVENT_SEAT_REMOVED: libc::c_int = 4;

const EI_EVENT_DEVICE_ADDED: libc::c_int = 5;

const EI_EVENT_DEVICE_REMOVED: libc::c_int = 6;

const EI_EVENT_DEVICE_PAUSED: libc::c_int = 7;

const EI_EVENT_DEVICE_RESUMED: libc::c_int = 8;

const EI_EVENT_PONG: libc::c_int = 90;

// The text capability follows the header's bitmask progression
// (EI_DEVICE_CAP_BUTTON = 1 << 5 is the last capability in 1.5); it ships with
// the libei release that provides ei_device_text_utf8_with_length, which
// EiApi::load requires before this constant is ever used.
const EI_CAP_KEYBOARD: libc::c_int = 1 << 2;

const EI_CAP_TEXT: libc::c_int = 1 << 6;

const EI_EVENT_KEYBOARD_MODIFIERS: libc::c_int = 9;

/// Binds the text capability on a seat through the variadic
/// `ei_seat_bind_capabilities`. The header requires the capability list to be
/// "terminated by ``NULL``" — a pointer-width sentinel. Terminating with an
/// integer (e.g. `-1_i32`) is undefined behavior on ABIs where int and pointer
/// widths differ, so the sentinel is passed as an explicit null pointer.
fn bind_capability(api: &EiApi, seat: *mut EiSeat, capability: libc::c_int) {
    // SAFETY: `seat` is a live pointer obtained from the event currently being
    // processed; the variadic call passes one capability (promoted to int, as
    // C callers do) followed by the documented NULL terminator.
    unsafe { (api.seat_bind_capabilities)(seat, capability, std::ptr::null_mut::<libc::c_void>()) };
}

/// What the connect loop should do in response to one libei event, decided by
/// the pure [`EiDeviceLink`] state machine so the protocol handling is
/// testable without a native EIS server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum EiLinkDirective {
    /// Bind the text capability on the seat carried by this event.
    BindCapability,
    /// Adopt the device carried by this event as the delivery device.
    AdoptDevice,
    /// Nothing to do for this event.
    Continue,
    /// The link is unusable; fail with this reason.
    Fail(&'static str),
}

/// One libei event as seen by the pure state machines. `ours` marks whether
/// the event's device (or ping) is the one this sender adopted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum EiLinkEvent {
    SeatAddedWithText,
    SeatAddedWithKeyboard,
    SeatRemoved { ours: bool },
    DeviceAddedWithText,
    DeviceAddedWithKeyboard,
    DeviceResumed { ours: bool },
    DevicePaused { ours: bool },
    DeviceRemoved { ours: bool },
    Disconnect,
    Pong { ours: bool },
    KeyboardGroup { ours: bool, group: u32 },
    Other,
}

/// The device readiness state machine for a libei sender link.
///
/// libei semantics (libei.h): EI_EVENT_DEVICE_ADDED only announces the device;
/// events sent before EI_EVENT_DEVICE_RESUMED ("The client may send events")
/// are not permitted, and after EI_EVENT_DEVICE_PAUSED "any events sent from
/// this device will be discarded until the next resume". A removed device or
/// seat, or a disconnect, invalidates the link entirely.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct EiDeviceLink {
    adopted: bool,
    resumed: bool,
    keyboard_group: u32,
}

impl EiDeviceLink {
    pub(super) fn observe(&mut self, event: EiLinkEvent) -> EiLinkDirective {
        match event {
            EiLinkEvent::SeatAddedWithText | EiLinkEvent::SeatAddedWithKeyboard => {
                EiLinkDirective::BindCapability
            }
            EiLinkEvent::DeviceAddedWithText | EiLinkEvent::DeviceAddedWithKeyboard
                if !self.adopted =>
            {
                self.adopted = true;
                EiLinkDirective::AdoptDevice
            }
            EiLinkEvent::DeviceResumed { ours: true } => {
                self.resumed = true;
                EiLinkDirective::Continue
            }
            EiLinkEvent::DevicePaused { ours: true } => {
                self.resumed = false;
                EiLinkDirective::Continue
            }
            EiLinkEvent::DeviceRemoved { ours: true } | EiLinkEvent::SeatRemoved { ours: true } => {
                self.adopted = false;
                self.resumed = false;
                EiLinkDirective::Fail("libei disconnected")
            }
            EiLinkEvent::Disconnect => {
                self.adopted = false;
                self.resumed = false;
                EiLinkDirective::Fail("libei disconnected")
            }
            EiLinkEvent::KeyboardGroup { ours: true, group } => {
                self.keyboard_group = group;
                EiLinkDirective::Continue
            }
            _ => EiLinkDirective::Continue,
        }
    }

    /// The device may emulate events only when it is adopted AND resumed.
    pub(super) fn ready(&self) -> bool {
        self.adopted && self.resumed
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EiDeliveryMode {
    Text,
    KeyboardPaste,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct KeyboardPasteKeys {
    pub(super) control: u32,
    pub(super) paste: u32,
}

pub(super) fn resolve_keyboard_paste_keys(
    keymap_text: String,
    group: u32,
) -> Result<KeyboardPasteKeys, BoundaryError> {
    use xkbcommon::xkb;

    let context = xkb::Context::new(xkb::CONTEXT_NO_ENVIRONMENT_NAMES);
    let keymap = xkb::Keymap::new_from_string(
        &context,
        keymap_text,
        xkb::KEYMAP_FORMAT_TEXT_V1,
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    )
    .ok_or_else(|| {
        BoundaryError::new(BoundaryKind::Delivery, "active keyboard layout unavailable")
    })?;
    let find = |target: xkb::Keysym| -> Option<u32> {
        (keymap.min_keycode().raw()..=keymap.max_keycode().raw()).find_map(|raw| {
            let key = xkb::Keycode::new(raw);
            (keymap.key_get_syms_by_level(key, group, 0) == [target])
                .then(|| raw.checked_sub(8))
                .flatten()
        })
    };
    let control = find(xkb::Keysym::from(xkb::keysyms::KEY_Control_L))
        .or_else(|| find(xkb::Keysym::from(xkb::keysyms::KEY_Control_R)))
        .ok_or_else(|| {
            BoundaryError::new(BoundaryKind::Delivery, "active keyboard layout unavailable")
        })?;
    let paste = find(xkb::Keysym::from(xkb::keysyms::KEY_v)).ok_or_else(|| {
        BoundaryError::new(BoundaryKind::Delivery, "active keyboard layout unavailable")
    })?;
    Ok(KeyboardPasteKeys { control, paste })
}

pub(super) fn resolve_shortcut_keycodes(
    keymap_text: String,
    group: u32,
    shortcut: &str,
) -> Result<Vec<u32>, BoundaryError> {
    use xkbcommon::xkb;

    let context = xkb::Context::new(xkb::CONTEXT_NO_ENVIRONMENT_NAMES);
    let keymap = xkb::Keymap::new_from_string(
        &context,
        keymap_text,
        xkb::KEYMAP_FORMAT_TEXT_V1,
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    )
    .ok_or_else(|| {
        BoundaryError::new(BoundaryKind::Delivery, "active keyboard layout unavailable")
    })?;
    let find = |keysym: xkb::Keysym| -> Option<u32> {
        (keymap.min_keycode().raw()..=keymap.max_keycode().raw()).find_map(|raw| {
            let key = xkb::Keycode::new(raw);
            (keymap.key_get_syms_by_level(key, group, 0) == [keysym])
                .then(|| raw.checked_sub(8))
                .flatten()
        })
    };
    let tokens = shortcut
        .split('+')
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>();
    let key = tokens.last().ok_or_else(|| {
        BoundaryError::new(
            BoundaryKind::Delivery,
            "verified Paste Action shortcut is empty",
        )
    })?;
    let mut codes = Vec::with_capacity(tokens.len());
    for modifier in &tokens[..tokens.len().saturating_sub(1)] {
        let keysym = match modifier.to_ascii_uppercase().as_str() {
            "CTRL" | "CONTROL" => xkb::Keysym::from(xkb::keysyms::KEY_Control_L),
            "SHIFT" => xkb::Keysym::from(xkb::keysyms::KEY_Shift_L),
            "ALT" => xkb::Keysym::from(xkb::keysyms::KEY_Alt_L),
            "SUPER" | "META" | "WIN" | "MOD4" => xkb::Keysym::from(xkb::keysyms::KEY_Super_L),
            _ => {
                return Err(BoundaryError::new(
                    BoundaryKind::Delivery,
                    "verified Paste Action contains an unknown modifier",
                ));
            }
        };
        codes.push(find(keysym).ok_or_else(|| {
            BoundaryError::new(BoundaryKind::Delivery, "active keyboard layout unavailable")
        })?);
    }
    let keycode = if let Some(raw) = key.strip_prefix("code:") {
        raw.parse::<u32>().ok().and_then(|raw| raw.checked_sub(8))
    } else {
        let name = if key.len() == 1 && key.as_bytes()[0].is_ascii_alphabetic() {
            key.to_ascii_lowercase()
        } else {
            (*key).to_owned()
        };
        let keysym = xkb::keysym_from_name(&name, xkb::KEYSYM_CASE_INSENSITIVE);
        (keysym != xkb::Keysym::from(xkb::keysyms::KEY_NoSymbol))
            .then(|| find(keysym))
            .flatten()
    };
    codes.push(keycode.ok_or_else(|| {
        BoundaryError::new(BoundaryKind::Delivery, "active keyboard layout unavailable")
    })?);
    Ok(codes)
}

pub(super) fn libei_text_buffer(text: &str) -> Result<CString, BoundaryError> {
    CString::new(text).map_err(|_| {
        BoundaryError::new(
            BoundaryKind::Delivery,
            "Transcript contains an unsupported NUL character",
        )
    })
}

/// Confirmation state for one delivery roundtrip.
///
/// libei semantics (libei.h, ei_ping): "If the client is disconnected before
/// the roundtrip is complete, libei will emulate a @ref EI_EVENT_PONG event
/// before @ref EI_EVENT_DISCONNECT." A matching PONG therefore proves nothing
/// on its own — the already-queued events behind it must be drained, and a
/// queued DISCONNECT (or loss of the delivery device) converts the synthetic
/// PONG into a failure. Only a matching PONG followed by an exhausted event
/// queue confirms the delivery.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct EiDeliveryConfirmation {
    pong_matched: bool,
    failure: Option<&'static str>,
}

impl EiDeliveryConfirmation {
    pub(super) fn observe(&mut self, event: EiLinkEvent) {
        if self.failure.is_some() {
            return;
        }
        match event {
            EiLinkEvent::Pong { ours: true } => self.pong_matched = true,
            EiLinkEvent::Disconnect => {
                self.failure = Some("libei disconnected during compositor submission");
            }
            EiLinkEvent::DeviceRemoved { ours: true } | EiLinkEvent::SeatRemoved { ours: true } => {
                self.failure = Some("libei disconnected");
            }
            EiLinkEvent::DevicePaused { ours: true } if !self.pong_matched => {
                // The pause discards not-yet-processed events; a pong that has
                // not arrived yet cannot vouch for events sent before it.
                self.failure = Some("compositor rejected libei submission");
            }
            _ => {}
        }
    }

    /// The verdict once the currently queued events are exhausted: a failure
    /// wins over a matched pong (the synthetic-PONG-before-DISCONNECT case);
    /// a matched pong with a clean queue confirms; otherwise keep waiting.
    pub(super) fn verdict(&self) -> Option<Result<(), &'static str>> {
        if let Some(reason) = self.failure {
            return Some(Err(reason));
        }
        if self.pong_matched {
            return Some(Ok(()));
        }
        None
    }
}

struct EiApi {
    library: *mut libc::c_void,
    new_sender: unsafe extern "C" fn(*mut libc::c_void) -> *mut EiContext,
    configure_name: unsafe extern "C" fn(*mut EiContext, *const libc::c_char),
    setup_backend_fd: unsafe extern "C" fn(*mut EiContext, libc::c_int) -> libc::c_int,
    get_fd: unsafe extern "C" fn(*mut EiContext) -> libc::c_int,
    dispatch: unsafe extern "C" fn(*mut EiContext),
    get_event: unsafe extern "C" fn(*mut EiContext) -> *mut EiEvent,
    event_type: unsafe extern "C" fn(*mut EiEvent) -> libc::c_int,
    event_device: unsafe extern "C" fn(*mut EiEvent) -> *mut EiDevice,
    event_seat: unsafe extern "C" fn(*mut EiEvent) -> *mut EiSeat,
    event_unref: unsafe extern "C" fn(*mut EiEvent) -> *mut EiEvent,
    seat_has_capability: unsafe extern "C" fn(*mut EiSeat, libc::c_int) -> bool,
    seat_bind_capabilities: SeatBindCapabilities,
    device_has_capability: unsafe extern "C" fn(*mut EiDevice, libc::c_int) -> bool,
    device_keyboard_get_keymap: unsafe extern "C" fn(*mut EiDevice) -> *mut EiKeymap,
    keymap_get_fd: unsafe extern "C" fn(*mut EiKeymap) -> libc::c_int,
    keymap_get_size: unsafe extern "C" fn(*mut EiKeymap) -> usize,
    device_ref: unsafe extern "C" fn(*mut EiDevice) -> *mut EiDevice,
    device_unref: unsafe extern "C" fn(*mut EiDevice) -> *mut EiDevice,
    start_emulating: unsafe extern "C" fn(*mut EiDevice, u32),
    keyboard_key: unsafe extern "C" fn(*mut EiDevice, u32, bool),
    text_utf8: Option<unsafe extern "C" fn(*mut EiDevice, *const libc::c_char, usize)>,
    frame: unsafe extern "C" fn(*mut EiDevice, u64),
    stop_emulating: unsafe extern "C" fn(*mut EiDevice),
    now: unsafe extern "C" fn(*mut EiContext) -> u64,
    new_ping: unsafe extern "C" fn(*mut EiContext) -> *mut EiPing,
    ping: unsafe extern "C" fn(*mut EiPing),
    ping_get_id: unsafe extern "C" fn(*mut EiPing) -> u64,
    ping_unref: unsafe extern "C" fn(*mut EiPing) -> *mut EiPing,
    event_pong_get_ping: unsafe extern "C" fn(*mut EiEvent) -> *mut EiPing,
    event_keyboard_get_xkb_group: unsafe extern "C" fn(*mut EiEvent) -> u32,
    disconnect: unsafe extern "C" fn(*mut EiContext),
    context_unref: unsafe extern "C" fn(*mut EiContext) -> *mut EiContext,
}

// The loaded libei objects are owned exclusively by one spawn_blocking task at
// a time. No pointer is ever accessed concurrently.
unsafe impl Send for EiApi {}

impl EiApi {
    fn load() -> Result<Self, BoundaryError> {
        unsafe fn symbol<T: Copy>(
            library: *mut libc::c_void,
            name: &'static [u8],
        ) -> Result<T, BoundaryError> {
            // SAFETY: every name is NUL-terminated and each T below is the exact
            // C ABI function-pointer type documented by libei.
            let pointer = unsafe { libc::dlsym(library, name.as_ptr().cast()) };
            if pointer.is_null() {
                return Err(BoundaryError::new(
                    BoundaryKind::Delivery,
                    "libei capability unavailable",
                ));
            }
            // SAFETY: function pointers and dlsym pointers have pointer size on
            // the supported Fedora target; T is Copy and contains no references.
            Ok(unsafe { std::mem::transmute_copy(&pointer) })
        }

        unsafe fn optional_symbol<T: Copy>(
            library: *mut libc::c_void,
            name: &'static [u8],
        ) -> Option<T> {
            let pointer = unsafe { libc::dlsym(library, name.as_ptr().cast()) };
            (!pointer.is_null()).then(|| unsafe { std::mem::transmute_copy(&pointer) })
        }

        // Load by SONAME so the build does not require libei-devel or a linker
        // symlink; Fedora's portal stack provides the runtime library.
        let library = unsafe { libc::dlopen(c"libei.so.1".as_ptr(), libc::RTLD_NOW) };
        if library.is_null() {
            return Err(BoundaryError::new(
                BoundaryKind::Delivery,
                "libei connection unavailable",
            ));
        }
        let loaded = unsafe {
            (|| {
                Ok(Self {
                    library,
                    new_sender: symbol(library, b"ei_new_sender\0")?,
                    configure_name: symbol(library, b"ei_configure_name\0")?,
                    setup_backend_fd: symbol(library, b"ei_setup_backend_fd\0")?,
                    get_fd: symbol(library, b"ei_get_fd\0")?,
                    dispatch: symbol(library, b"ei_dispatch\0")?,
                    get_event: symbol(library, b"ei_get_event\0")?,
                    event_type: symbol(library, b"ei_event_get_type\0")?,
                    event_device: symbol(library, b"ei_event_get_device\0")?,
                    event_seat: symbol(library, b"ei_event_get_seat\0")?,
                    event_unref: symbol(library, b"ei_event_unref\0")?,
                    seat_has_capability: symbol(library, b"ei_seat_has_capability\0")?,
                    seat_bind_capabilities: symbol(library, b"ei_seat_bind_capabilities\0")?,
                    device_has_capability: symbol(library, b"ei_device_has_capability\0")?,
                    device_keyboard_get_keymap: symbol(
                        library,
                        b"ei_device_keyboard_get_keymap\0",
                    )?,
                    keymap_get_fd: symbol(library, b"ei_keymap_get_fd\0")?,
                    keymap_get_size: symbol(library, b"ei_keymap_get_size\0")?,
                    device_ref: symbol(library, b"ei_device_ref\0")?,
                    device_unref: symbol(library, b"ei_device_unref\0")?,
                    start_emulating: symbol(library, b"ei_device_start_emulating\0")?,
                    keyboard_key: symbol(library, b"ei_device_keyboard_key\0")?,
                    text_utf8: optional_symbol(library, b"ei_device_text_utf8_with_length\0"),
                    frame: symbol(library, b"ei_device_frame\0")?,
                    stop_emulating: symbol(library, b"ei_device_stop_emulating\0")?,
                    now: symbol(library, b"ei_now\0")?,
                    new_ping: symbol(library, b"ei_new_ping\0")?,
                    ping: symbol(library, b"ei_ping\0")?,
                    ping_get_id: symbol(library, b"ei_ping_get_id\0")?,
                    ping_unref: symbol(library, b"ei_ping_unref\0")?,
                    event_pong_get_ping: symbol(library, b"ei_event_pong_get_ping\0")?,
                    event_keyboard_get_xkb_group: symbol(
                        library,
                        b"ei_event_keyboard_get_xkb_group\0",
                    )?,
                    disconnect: symbol(library, b"ei_disconnect\0")?,
                    context_unref: symbol(library, b"ei_unref\0")?,
                })
            })()
        };
        if loaded.is_err() {
            unsafe { libc::dlclose(library) };
        }
        loaded
    }
}

impl Drop for EiApi {
    fn drop(&mut self) {
        unsafe { libc::dlclose(self.library) };
    }
}

fn keyboard_keymap_text(api: &EiApi, device: *mut EiDevice) -> Result<String, BoundaryError> {
    let keymap = unsafe { (api.device_keyboard_get_keymap)(device) };
    if keymap.is_null() {
        return Err(BoundaryError::new(
            BoundaryKind::Delivery,
            "active keyboard layout unavailable",
        ));
    }
    let size = unsafe { (api.keymap_get_size)(keymap) };
    if size == 0 || size > 1024 * 1024 {
        return Err(BoundaryError::new(
            BoundaryKind::Delivery,
            "active keyboard layout unavailable",
        ));
    }
    let fd = unsafe { (api.keymap_get_fd)(keymap) };
    let owned_fd = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
    if owned_fd < 0 {
        return Err(BoundaryError::new(
            BoundaryKind::Delivery,
            "active keyboard layout unavailable",
        ));
    }
    // The dup owns the descriptor so every exit closes it.
    let file = unsafe { File::from_raw_fd(owned_fd) };
    read_keymap_fd(file.as_raw_fd(), size)
}

/// Reads a compiled XKB keymap of `size` bytes from an EIS keymap descriptor.
///
/// The keymap descriptor is a shared open file description whose offset is NOT
/// guaranteed to sit at the start: a compositor that populates the backing
/// memfd with `write()` leaves the offset at the end, and `F_DUPFD_CLOEXEC`
/// shares that offset rather than resetting it — as does any earlier read of
/// the same keymap. Reading through the ordinary file cursor therefore yielded
/// zero bytes, and libxkbcommon rejected the resulting empty string
/// (`[XKB-822] Failed to parse input xkb string`), stranding every Delivery on
/// the clipboard fallback. `pread` reads from absolute offset 0, so it neither
/// depends on nor mutates the shared offset. This mirrors the mmap-based
/// consumption the wl_keyboard/EIS keymap convention expects.
pub(super) fn read_keymap_fd(fd: libc::c_int, size: usize) -> Result<String, BoundaryError> {
    let unavailable =
        || BoundaryError::new(BoundaryKind::Delivery, "active keyboard layout unavailable");
    let mut bytes = vec![0u8; size];
    let mut filled = 0usize;
    while filled < size {
        // SAFETY: `fd` is live for the call and the destination is an owned
        // buffer with at least `size - filled` bytes remaining at `filled`.
        let read = unsafe {
            libc::pread(
                fd,
                bytes[filled..].as_mut_ptr().cast(),
                size - filled,
                filled as libc::off_t,
            )
        };
        if read < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(unavailable());
        }
        if read == 0 {
            // Short descriptor: the advertised size overstates the payload.
            break;
        }
        filled += read as usize;
    }
    bytes.truncate(filled);
    // The convention advertises the keymap size including its terminating NUL,
    // which the compiler must not see.
    if bytes.last() == Some(&0) {
        bytes.pop();
    }
    String::from_utf8(bytes).map_err(|_| unavailable())
}

struct NativeEiSender {
    api: EiApi,
    context: *mut EiContext,
    device: *mut EiDevice,
    link: EiDeviceLink,
    mode: EiDeliveryMode,
    sequence: u32,
}

unsafe impl Send for NativeEiSender {}

/// RAII cleanup for a libei context (and an adopted device reference) while a
/// connection attempt is in flight: every early exit — poll failure, protocol
/// failure, deadline — disconnects and releases the native objects. Disarmed
/// exactly once, on successful handoff into [`NativeEiSender`].
struct EiConnectGuard<'a> {
    api: &'a EiApi,
    context: *mut EiContext,
    device: *mut EiDevice,
    armed: bool,
}

impl Drop for EiConnectGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // SAFETY: the guard exclusively owns these pointers until disarmed;
        // device (when adopted) holds the reference taken via device_ref.
        unsafe {
            if !self.device.is_null() {
                (self.api.device_unref)(self.device);
            }
            (self.api.disconnect)(self.context);
            (self.api.context_unref)(self.context);
        }
    }
}

impl NativeEiSender {
    fn connect_with_keyboard(fd: libc::c_int, force_keyboard: bool) -> Result<Self, BoundaryError> {
        let api = match EiApi::load() {
            Ok(api) => api,
            Err(error) => {
                unsafe { libc::close(fd) };
                return Err(error);
            }
        };
        let context = unsafe { (api.new_sender)(std::ptr::null_mut()) };
        if context.is_null() {
            unsafe { libc::close(fd) };
            return Err(BoundaryError::new(
                BoundaryKind::Delivery,
                "libei connection unavailable",
            ));
        }
        unsafe { (api.configure_name)(context, c"Voisu Delivery".as_ptr()) };
        if unsafe { (api.setup_backend_fd)(context, fd) } != 0 {
            unsafe { (api.context_unref)(context) };
            return Err(BoundaryError::new(
                BoundaryKind::Delivery,
                "libei connection unavailable",
            ));
        }

        let mut guard = EiConnectGuard {
            api: &api,
            context,
            device: std::ptr::null_mut(),
            armed: true,
        };
        let mut link = EiDeviceLink::default();
        let mut mode = None;
        let deadline = Instant::now() + LIBEI_DELIVERY_DEADLINE;
        // The device may emulate only after EI_EVENT_DEVICE_RESUMED; an added
        // device alone is not ready (libei.h).
        while Instant::now() < deadline && !link.ready() {
            poll_libei(guard.api, context, deadline)?;
            loop {
                let event = unsafe { (guard.api.get_event)(context) };
                if event.is_null() {
                    break;
                }
                let event_type = unsafe { (guard.api.event_type)(event) };
                // Pointers are read before the event is released.
                let mut event_seat: *mut EiSeat = std::ptr::null_mut();
                let mut event_device: *mut EiDevice = std::ptr::null_mut();
                let link_event = match event_type {
                    EI_EVENT_SEAT_ADDED => {
                        event_seat = unsafe { (guard.api.event_seat)(event) };
                        if !force_keyboard
                            && guard.api.text_utf8.is_some()
                            && !event_seat.is_null()
                            && unsafe { (guard.api.seat_has_capability)(event_seat, EI_CAP_TEXT) }
                        {
                            EiLinkEvent::SeatAddedWithText
                        } else if !event_seat.is_null()
                            && unsafe {
                                (guard.api.seat_has_capability)(event_seat, EI_CAP_KEYBOARD)
                            }
                        {
                            EiLinkEvent::SeatAddedWithKeyboard
                        } else {
                            EiLinkEvent::Other
                        }
                    }
                    EI_EVENT_SEAT_REMOVED => EiLinkEvent::SeatRemoved {
                        // Conservative: once a device is adopted, any seat
                        // removal invalidates the link and falls back.
                        ours: !guard.device.is_null(),
                    },
                    EI_EVENT_DEVICE_ADDED => {
                        event_device = unsafe { (guard.api.event_device)(event) };
                        if !force_keyboard
                            && guard.api.text_utf8.is_some()
                            && !event_device.is_null()
                            && unsafe {
                                (guard.api.device_has_capability)(event_device, EI_CAP_TEXT)
                            }
                        {
                            EiLinkEvent::DeviceAddedWithText
                        } else if !event_device.is_null()
                            && unsafe {
                                (guard.api.device_has_capability)(event_device, EI_CAP_KEYBOARD)
                            }
                        {
                            EiLinkEvent::DeviceAddedWithKeyboard
                        } else {
                            EiLinkEvent::Other
                        }
                    }
                    EI_EVENT_DEVICE_RESUMED => EiLinkEvent::DeviceResumed {
                        ours: Self::event_is_for_device(&api, event, guard.device),
                    },
                    EI_EVENT_DEVICE_PAUSED => EiLinkEvent::DevicePaused {
                        ours: Self::event_is_for_device(&api, event, guard.device),
                    },
                    EI_EVENT_DEVICE_REMOVED => EiLinkEvent::DeviceRemoved {
                        ours: Self::event_is_for_device(&api, event, guard.device),
                    },
                    EI_EVENT_KEYBOARD_MODIFIERS => EiLinkEvent::KeyboardGroup {
                        ours: Self::event_is_for_device(&api, event, guard.device),
                        group: unsafe { (guard.api.event_keyboard_get_xkb_group)(event) },
                    },
                    EI_EVENT_DISCONNECT => EiLinkEvent::Disconnect,
                    _ => EiLinkEvent::Other,
                };
                let directive = link.observe(link_event);
                match directive {
                    EiLinkDirective::BindCapability => {
                        let capability = if link_event == EiLinkEvent::SeatAddedWithText {
                            EI_CAP_TEXT
                        } else {
                            EI_CAP_KEYBOARD
                        };
                        bind_capability(&api, event_seat, capability);
                    }
                    EiLinkDirective::AdoptDevice => {
                        guard.device = unsafe { (guard.api.device_ref)(event_device) };
                        if guard.device.is_null() {
                            unsafe { (guard.api.event_unref)(event) };
                            return Err(BoundaryError::new(
                                BoundaryKind::Delivery,
                                "libei connection unavailable",
                            ));
                        }
                        mode = Some(if link_event == EiLinkEvent::DeviceAddedWithText {
                            EiDeliveryMode::Text
                        } else {
                            EiDeliveryMode::KeyboardPaste
                        });
                    }
                    EiLinkDirective::Continue => {}
                    EiLinkDirective::Fail(_) => {}
                }
                unsafe { (guard.api.event_unref)(event) };
                if let EiLinkDirective::Fail(reason) = directive {
                    return Err(BoundaryError::new(BoundaryKind::Delivery, reason));
                }
            }
        }
        if !link.ready() {
            return Err(BoundaryError::new(
                BoundaryKind::Delivery,
                "text or keyboard capability unavailable",
            ));
        }
        let mode = mode.ok_or_else(|| {
            BoundaryError::new(
                BoundaryKind::Delivery,
                "text or keyboard capability unavailable",
            )
        })?;
        guard.armed = false;
        let device = guard.device;
        drop(guard);
        Ok(Self {
            api,
            context,
            device,
            link,
            mode,
            sequence: 0,
        })
    }

    /// Whether this event's device is the adopted delivery device (pointer
    /// identity; libei keeps one struct per device for the context lifetime).
    fn event_is_for_device(api: &EiApi, event: *mut EiEvent, device: *mut EiDevice) -> bool {
        if device.is_null() {
            return false;
        }
        (unsafe { (api.event_device)(event) }) == device
    }

    /// Translates one already-fetched event for the delivery loops; pointers
    /// are read before the caller releases the event.
    fn classify_delivery_event(
        &self,
        event: *mut EiEvent,
        event_type: libc::c_int,
        expected_ping: u64,
    ) -> EiLinkEvent {
        match event_type {
            EI_EVENT_PONG => {
                let ping = unsafe { (self.api.event_pong_get_ping)(event) };
                let ours =
                    !ping.is_null() && unsafe { (self.api.ping_get_id)(ping) } == expected_ping;
                EiLinkEvent::Pong { ours }
            }
            EI_EVENT_DISCONNECT => EiLinkEvent::Disconnect,
            EI_EVENT_DEVICE_RESUMED => EiLinkEvent::DeviceResumed {
                ours: Self::event_is_for_device(&self.api, event, self.device),
            },
            EI_EVENT_DEVICE_PAUSED => EiLinkEvent::DevicePaused {
                ours: Self::event_is_for_device(&self.api, event, self.device),
            },
            EI_EVENT_DEVICE_REMOVED => EiLinkEvent::DeviceRemoved {
                ours: Self::event_is_for_device(&self.api, event, self.device),
            },
            EI_EVENT_KEYBOARD_MODIFIERS => EiLinkEvent::KeyboardGroup {
                ours: Self::event_is_for_device(&self.api, event, self.device),
                group: unsafe { (self.api.event_keyboard_get_xkb_group)(event) },
            },
            EI_EVENT_SEAT_REMOVED => EiLinkEvent::SeatRemoved { ours: true },
            _ => EiLinkEvent::Other,
        }
    }

    /// Absorbs every event already queued on the context without blocking,
    /// updating the device link. Called before emitting so a pause, removal,
    /// revocation, or disconnect that arrived between deliveries is honored.
    fn absorb_pending_state(&mut self) -> Result<(), BoundaryError> {
        unsafe { (self.api.dispatch)(self.context) };
        loop {
            let event = unsafe { (self.api.get_event)(self.context) };
            if event.is_null() {
                return Ok(());
            }
            let event_type = unsafe { (self.api.event_type)(event) };
            let link_event = self.classify_delivery_event(event, event_type, 0);
            let directive = self.link.observe(link_event);
            unsafe { (self.api.event_unref)(event) };
            if let EiLinkDirective::Fail(reason) = directive {
                return Err(BoundaryError::new(BoundaryKind::Delivery, reason));
            }
        }
    }

    fn deliver_shortcut(&mut self, shortcut: &str) -> Result<(), BoundaryError> {
        self.absorb_pending_state()?;
        if !self.link.ready() || self.mode != EiDeliveryMode::KeyboardPaste {
            return Err(BoundaryError::new(
                BoundaryKind::Delivery,
                "keyboard shortcut submission unavailable",
            ));
        }
        let keycodes = resolve_shortcut_keycodes(
            keyboard_keymap_text(&self.api, self.device)?,
            self.link.keyboard_group,
            shortcut,
        )?;
        self.sequence = self.sequence.wrapping_add(1).max(1);
        unsafe {
            (self.api.start_emulating)(self.device, self.sequence);
            for key in &keycodes {
                (self.api.keyboard_key)(self.device, *key, true);
                (self.api.frame)(self.device, (self.api.now)(self.context));
            }
            for key in keycodes.iter().rev() {
                (self.api.keyboard_key)(self.device, *key, false);
                (self.api.frame)(self.device, (self.api.now)(self.context));
            }
            (self.api.stop_emulating)(self.device);
        }
        let ping = unsafe { (self.api.new_ping)(self.context) };
        if ping.is_null() {
            return Err(BoundaryError::new(
                BoundaryKind::Delivery,
                "compositor rejected Paste Action",
            ));
        }
        let expected_ping = unsafe { (self.api.ping_get_id)(ping) };
        unsafe {
            (self.api.ping)(ping);
            (self.api.ping_unref)(ping);
        }
        let mut confirmation = EiDeliveryConfirmation::default();
        let deadline = Instant::now() + LIBEI_DELIVERY_DEADLINE;
        while Instant::now() < deadline {
            poll_libei(&self.api, self.context, deadline)?;
            loop {
                let event = unsafe { (self.api.get_event)(self.context) };
                if event.is_null() {
                    break;
                }
                let event_type = unsafe { (self.api.event_type)(event) };
                let link_event = self.classify_delivery_event(event, event_type, expected_ping);
                let _ = self.link.observe(link_event);
                confirmation.observe(link_event);
                unsafe { (self.api.event_unref)(event) };
            }
            if let Some(verdict) = confirmation.verdict() {
                return verdict
                    .map_err(|reason| BoundaryError::new(BoundaryKind::Delivery, reason));
            }
        }
        Err(BoundaryError::new(
            BoundaryKind::Delivery,
            "compositor rejected Paste Action",
        ))
    }

    fn deliver(&mut self, text: &str) -> Result<(), BoundaryError> {
        self.absorb_pending_state()?;
        if !self.link.ready() {
            return Err(BoundaryError::new(
                BoundaryKind::Delivery,
                "libei disconnected",
            ));
        }
        let text = match self.mode {
            EiDeliveryMode::Text => Some(libei_text_buffer(text)?),
            EiDeliveryMode::KeyboardPaste => None,
        };
        let paste_keys = match self.mode {
            EiDeliveryMode::Text => None,
            EiDeliveryMode::KeyboardPaste => Some(resolve_keyboard_paste_keys(
                keyboard_keymap_text(&self.api, self.device)?,
                self.link.keyboard_group,
            )?),
        };
        self.sequence = self.sequence.wrapping_add(1).max(1);
        unsafe {
            (self.api.start_emulating)(self.device, self.sequence);
            match (self.mode, text.as_ref(), paste_keys) {
                (EiDeliveryMode::Text, Some(text), _) => {
                    let text_utf8 = self
                        .api
                        .text_utf8
                        .expect("TEXT mode requires the TEXT symbol");
                    text_utf8(self.device, text.as_ptr(), text.as_bytes().len());
                    (self.api.frame)(self.device, (self.api.now)(self.context));
                }
                (EiDeliveryMode::KeyboardPaste, _, Some(keys)) => {
                    // The Transcript is already on the clipboard. Submit the
                    // focused application's paste shortcut using the active
                    // EIS keymap, with one frame per key transition as libei
                    // requires.
                    for (key, pressed) in [
                        (keys.control, true),
                        (keys.paste, true),
                        (keys.paste, false),
                        (keys.control, false),
                    ] {
                        (self.api.keyboard_key)(self.device, key, pressed);
                        (self.api.frame)(self.device, (self.api.now)(self.context));
                    }
                }
                _ => unreachable!("Delivery mode inputs are complete"),
            }
            (self.api.stop_emulating)(self.device);
        }
        let ping = unsafe { (self.api.new_ping)(self.context) };
        if ping.is_null() {
            return Err(BoundaryError::new(
                BoundaryKind::Delivery,
                "compositor rejected libei submission",
            ));
        }
        let expected_ping = unsafe { (self.api.ping_get_id)(ping) };
        unsafe {
            (self.api.ping)(ping);
            (self.api.ping_unref)(ping);
        }
        let mut confirmation = EiDeliveryConfirmation::default();
        let deadline = Instant::now() + LIBEI_DELIVERY_DEADLINE;
        while Instant::now() < deadline {
            poll_libei(&self.api, self.context, deadline)?;
            loop {
                let event = unsafe { (self.api.get_event)(self.context) };
                if event.is_null() {
                    break;
                }
                let event_type = unsafe { (self.api.event_type)(event) };
                let link_event = self.classify_delivery_event(event, event_type, expected_ping);
                // Both machines observe: the link so pauses/removals persist
                // beyond this delivery, the confirmation for the verdict.
                let _ = self.link.observe(link_event);
                confirmation.observe(link_event);
                unsafe { (self.api.event_unref)(event) };
            }
            // Only judge once the queued events are exhausted: libei emits a
            // synthetic PONG before EI_EVENT_DISCONNECT, so a matched pong
            // must not win against a disconnect queued right behind it.
            if let Some(verdict) = confirmation.verdict() {
                return verdict
                    .map_err(|reason| BoundaryError::new(BoundaryKind::Delivery, reason));
            }
        }
        Err(BoundaryError::new(
            BoundaryKind::Delivery,
            "compositor rejected libei submission",
        ))
    }
}

fn poll_libei(
    api: &EiApi,
    context: *mut EiContext,
    deadline: Instant,
) -> Result<(), BoundaryError> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    let millis = remaining.as_millis().clamp(1, 100) as libc::c_int;
    let mut pollfd = libc::pollfd {
        fd: unsafe { (api.get_fd)(context) },
        events: libc::POLLIN,
        revents: 0,
    };
    let result = unsafe { libc::poll(&mut pollfd, 1, millis) };
    if result < 0 {
        return Err(BoundaryError::new(
            BoundaryKind::Delivery,
            "libei disconnected",
        ));
    }
    unsafe { (api.dispatch)(context) };
    Ok(())
}

impl Drop for NativeEiSender {
    fn drop(&mut self) {
        unsafe {
            (self.api.device_unref)(self.device);
            (self.api.disconnect)(self.context);
            (self.api.context_unref)(self.context);
        }
    }
}
