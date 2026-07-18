use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::time::timeout;
use voisu_core::{BoundaryFuture, FocusProbe, WindowIdentity};

use crate::system::run_restricted_stdout;

const FOCUS_DBUS_DEADLINE: Duration = Duration::from_secs(2);
const KWIN_BUS_NAME: &str = "org.kde.KWin";
const KWIN_SCRIPTING_PATH: &str = "/Scripting";
const KWIN_SCRIPTING_INTERFACE: &str = "org.kde.kwin.Scripting";
const VOISU_FOCUS_BUS_NAME: &str = "org.voisu.Focus1";
const VOISU_FOCUS_PATH: &str = "/org/voisu/Focus1";
const KWIN_SCRIPT_PLUGIN: &str = "voisu-focus-guard";

/// KWin pushes on activation rather than polling. Ten minutes exceeds Voisu's
/// five-minute Recording cap, while still making a silent script failure fail
/// closed instead of trusting an indefinitely old identity. A single-window
/// dwell past the deadline therefore falls back safely to clipboard; the bound
/// limits the fail-open window if the push-only script dies without a freshness
/// signal while KWin keeps running. A KWin restart is detected separately by
/// comparing its unique D-Bus owner on every read.
pub const KWIN_FOCUS_STALE_AFTER: Duration = Duration::from_secs(10 * 60);

pub const KWIN_FOCUS_SCRIPT: &str = r#"function reportActiveWindow(window) {
    if (!window) {
        callDBus("org.voisu.Focus1", "/org/voisu/Focus1", "org.voisu.Focus1", "Update", "", "", "");
        return;
    }
    callDBus(
        "org.voisu.Focus1",
        "/org/voisu/Focus1",
        "org.voisu.Focus1",
        "Update",
        String(window.internalId),
        String(window.pid || ""),
        String(window.resourceClass || "")
    );
}

workspace.windowActivated.connect(reportActiveWindow);
reportActiveWindow(workspace.activeWindow);
"#;

pub type SharedFocusProbe = Arc<tokio::sync::Mutex<Box<dyn FocusProbe>>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FocusBackendKind {
    Kwin,
    Hyprland,
    None,
}

impl FocusBackendKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Kwin => "kwin",
            Self::Hyprland => "hyprland",
            Self::None => "none",
        }
    }
}

#[derive(Default)]
struct KwinFocusStore {
    last_push: Option<(WindowIdentity, Instant)>,
}

impl KwinFocusStore {
    fn push(&mut self, stable_id: &str, process_id: &str, app_id: &str) {
        self.last_push = (!stable_id.is_empty()).then(|| {
            (
                WindowIdentity {
                    stable_id: stable_id.to_owned(),
                    process_id: process_id.parse::<u32>().ok().filter(|pid| *pid != 0),
                    app_id: (!app_id.is_empty()).then(|| app_id.to_owned()),
                },
                Instant::now(),
            )
        });
    }

    fn current_at(&self, now: Instant) -> Option<WindowIdentity> {
        self.last_push
            .as_ref()
            .filter(|(_, received_at)| now.duration_since(*received_at) < KWIN_FOCUS_STALE_AFTER)
            .map(|(identity, _)| identity.clone())
    }
}

struct KwinFocusUpdates {
    store: Arc<Mutex<KwinFocusStore>>,
    expected_owner: String,
}

impl KwinFocusUpdates {
    fn apply_update(
        &self,
        sender: Option<&str>,
        stable_id: &str,
        process_id: &str,
        app_id: &str,
    ) {
        if sender != Some(self.expected_owner.as_str()) {
            return;
        }
        if let Ok(mut store) = self.store.lock() {
            store.push(stable_id, process_id, app_id);
        }
    }
}

#[zbus::interface(name = "org.voisu.Focus1")]
impl KwinFocusUpdates {
    fn update(
        &self,
        stable_id: String,
        process_id: String,
        app_id: String,
        #[zbus(header)] hdr: zbus::message::Header<'_>,
    ) {
        self.apply_update(
            hdr.sender().map(|sender| sender.as_str()),
            &stable_id,
            &process_id,
            &app_id,
        );
    }
}

pub struct KwinFocusProbe {
    connection: zbus::Connection,
    owner: String,
    store: Arc<Mutex<KwinFocusStore>>,
}

impl FocusProbe for KwinFocusProbe {
    fn current(&mut self) -> BoundaryFuture<'_, Option<WindowIdentity>> {
        Box::pin(async move {
            let owner = kwin_owner(&self.connection).await;
            if owner.as_deref() != Some(self.owner.as_str()) {
                return Ok(None);
            }
            Ok(self
                .store
                .lock()
                .ok()
                .and_then(|store| store.current_at(Instant::now())))
        })
    }
}

pub struct HyprlandFocusProbe;

impl FocusProbe for HyprlandFocusProbe {
    fn current(&mut self) -> BoundaryFuture<'_, Option<WindowIdentity>> {
        Box::pin(async move {
            let payload = tokio::task::spawn_blocking(|| {
                run_restricted_stdout("hyprctl", &["activewindow", "-j"])
            })
            .await
            .ok()
            .flatten();
            Ok(payload.as_deref().and_then(parse_hyprland_window))
        })
    }
}

pub struct NullFocusProbe;

impl FocusProbe for NullFocusProbe {
    fn current(&mut self) -> BoundaryFuture<'_, Option<WindowIdentity>> {
        Box::pin(async { Ok(None) })
    }
}

fn parse_hyprland_window(payload: &[u8]) -> Option<WindowIdentity> {
    let window: serde_json::Value = serde_json::from_slice(payload).ok()?;
    let stable_id = window
        .get("address")
        .and_then(serde_json::Value::as_str)
        .filter(|address| !address.is_empty())?
        .to_owned();
    let process_id = window
        .get("pid")
        .and_then(serde_json::Value::as_u64)
        .and_then(|pid| u32::try_from(pid).ok());
    let app_id = window
        .get("class")
        .and_then(serde_json::Value::as_str)
        .filter(|class| !class.is_empty())
        .map(str::to_owned);
    Some(WindowIdentity {
        stable_id,
        process_id,
        app_id,
    })
}

pub async fn detect_focus_backend() -> FocusBackendKind {
    if let Ok(controlled) = std::env::var("VOISU_TEST_FOCUS_BACKEND") {
        return match controlled.as_str() {
            "kwin" => FocusBackendKind::Kwin,
            "hyprland" => FocusBackendKind::Hyprland,
            _ => FocusBackendKind::None,
        };
    }
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
        return FocusBackendKind::Hyprland;
    }
    let Ok(connection) = timeout(FOCUS_DBUS_DEADLINE, zbus::Connection::session()).await else {
        return FocusBackendKind::None;
    };
    let Ok(connection) = connection else {
        return FocusBackendKind::None;
    };
    if kwin_owner(&connection).await.is_some() {
        FocusBackendKind::Kwin
    } else {
        FocusBackendKind::None
    }
}

pub async fn initialize_focus_probe(runtime_dir: &Path) -> (FocusBackendKind, SharedFocusProbe) {
    match detect_focus_backend().await {
        FocusBackendKind::Hyprland => (
            FocusBackendKind::Hyprland,
            Arc::new(tokio::sync::Mutex::new(Box::new(HyprlandFocusProbe))),
        ),
        FocusBackendKind::Kwin => match initialize_kwin_probe(runtime_dir).await {
            Some(probe) => (
                FocusBackendKind::Kwin,
                Arc::new(tokio::sync::Mutex::new(Box::new(probe))),
            ),
            None => (
                FocusBackendKind::None,
                Arc::new(tokio::sync::Mutex::new(Box::new(NullFocusProbe))),
            ),
        },
        FocusBackendKind::None => (
            FocusBackendKind::None,
            Arc::new(tokio::sync::Mutex::new(Box::new(NullFocusProbe))),
        ),
    }
}

async fn initialize_kwin_probe(runtime_dir: &Path) -> Option<KwinFocusProbe> {
    let store = Arc::new(Mutex::new(KwinFocusStore::default()));
    let builder = zbus::connection::Builder::session().ok()?;
    let builder = builder.name(VOISU_FOCUS_BUS_NAME).ok()?;
    let connection = timeout(FOCUS_DBUS_DEADLINE, builder.build())
        .await
        .ok()?
        .ok()?;
    let owner = kwin_owner(&connection).await?;
    connection
        .object_server()
        .at(
            VOISU_FOCUS_PATH,
            KwinFocusUpdates {
                store: Arc::clone(&store),
                expected_owner: owner.clone(),
            },
        )
        .await
        .ok()?;

    let script_path = runtime_dir.join(format!("kwin-focus-{}.js", std::process::id()));
    let mut script = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&script_path)
        .ok()?;
    let result = async {
        std::io::Write::write_all(&mut script, KWIN_FOCUS_SCRIPT.as_bytes()).ok()?;
        drop(script);

        let scripting = zbus::Proxy::new(
            &connection,
            KWIN_BUS_NAME,
            KWIN_SCRIPTING_PATH,
            KWIN_SCRIPTING_INTERFACE,
        )
        .await
        .ok()?;
        let path = script_path.to_string_lossy().into_owned();
        let loaded = timeout(
            FOCUS_DBUS_DEADLINE,
            scripting.call_method("loadScript", &(path, KWIN_SCRIPT_PLUGIN)),
        )
        .await
        .ok()?
        .ok()?;
        let script_id: i32 = loaded.body().deserialize().ok()?;
        if script_id < 0 {
            return None;
        }
        timeout(
            FOCUS_DBUS_DEADLINE,
            scripting.call_method("start", &()),
        )
        .await
        .ok()?
        .ok()?;

        Some(KwinFocusProbe {
            connection,
            owner,
            store,
        })
    }
    .await;
    let _ = std::fs::remove_file(script_path);
    result
}

async fn kwin_owner(connection: &zbus::Connection) -> Option<String> {
    let proxy = zbus::Proxy::new(
        connection,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    )
    .await
    .ok()?;
    timeout(
        FOCUS_DBUS_DEADLINE,
        proxy.call::<_, _, String>("GetNameOwner", &(KWIN_BUS_NAME,)),
    )
    .await
    .ok()?
    .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hyprctl_active_window_json_uses_address_as_the_only_stable_identity() {
        let identity = parse_hyprland_window(
            br#"{"address":"0x55a1","class":"org.example.Editor","pid":4242,"title":"draft.txt"}"#,
        )
        .expect("valid active-window payload");

        assert_eq!(identity.stable_id, "0x55a1");
        assert_eq!(identity.process_id, Some(4242));
        assert_eq!(identity.app_id.as_deref(), Some("org.example.Editor"));
    }

    #[test]
    fn hyprctl_payload_without_an_address_fails_closed() {
        assert!(parse_hyprland_window(br#"{"class":"org.example.Editor","pid":4242}"#).is_none());
    }

    #[test]
    fn kwin_push_uses_internal_id_and_normalizes_optional_diagnostics() {
        let mut store = KwinFocusStore::default();

        store.push("{ae0b-42}", "4242", "org.kde.kate");

        assert_eq!(
            store.current_at(Instant::now()),
            Some(WindowIdentity {
                stable_id: "{ae0b-42}".to_owned(),
                process_id: Some(4242),
                app_id: Some("org.kde.kate".to_owned()),
            })
        );

        store.push("{ae0b-43}", "not-a-pid", "");
        assert_eq!(
            store.current_at(Instant::now()),
            Some(WindowIdentity {
                stable_id: "{ae0b-43}".to_owned(),
                process_id: None,
                app_id: None,
            })
        );
    }

    #[test]
    fn kwin_updates_ignore_missing_or_mismatched_senders() {
        let store = Arc::new(Mutex::new(KwinFocusStore::default()));
        let updates = KwinFocusUpdates {
            store: Arc::clone(&store),
            expected_owner: ":1.42".to_owned(),
        };

        updates.apply_update(Some(":1.99"), "{wrong}", "99", "wrong.app");
        updates.apply_update(None, "{missing}", "98", "missing.app");
        assert_eq!(store.lock().unwrap().current_at(Instant::now()), None);

        updates.apply_update(Some(":1.42"), "{right}", "4242", "org.kde.kate");
        assert_eq!(
            store.lock().unwrap().current_at(Instant::now()),
            Some(WindowIdentity {
                stable_id: "{right}".to_owned(),
                process_id: Some(4242),
                app_id: Some("org.kde.kate".to_owned()),
            })
        );
    }

    #[test]
    fn kwin_empty_internal_id_clears_focus_and_stale_pushes_fail_closed() {
        let mut store = KwinFocusStore::default();
        store.push("{ae0b-42}", "0", "");
        store.last_push = store
            .last_push
            .take()
            .map(|(identity, _)| (identity, Instant::now() - KWIN_FOCUS_STALE_AFTER));

        assert_eq!(store.current_at(Instant::now()), None);

        store.push("", "0", "");
        assert_eq!(store.current_at(Instant::now()), None);
    }
}
