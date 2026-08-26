use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde_json::Value;

pub const BEGIN_MARKER: &str = "-- BEGIN VOISU MANAGED TRIGGER";
pub const END_MARKER: &str = "-- END VOISU MANAGED TRIGGER";
pub const VOISU_TOGGLE_COMMAND: &str = "voisu toggle";
pub const VOISU_TRIGGER_DESCRIPTION: &str = "Voisu dictation";
pub const RECOVERY_COMMAND: &str = "voisu setup";
const CAPS_NONE: &str = "caps:none";
const BOTH_CAPSLOCK_CANCEL: &str = "shift:both_capslock_cancel";

/// A key chord that is safe for the daemon to emit after it has verified the
/// corresponding Hyprland binding. This is data only: no Lua or shell text is
/// ever executed by the paste path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PasteShortcut {
    pub binding: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PasteBehavior {
    /// The known Omarchy helper chooses a different application shortcut for a
    /// terminal. The daemon emits the verified binding key and lets Hyprland
    /// perform that focus-sensitive choice.
    OmarchyUniversal {
        normal: PasteShortcut,
        terminal: PasteShortcut,
    },
    /// A literal `o.bind` dispatcher selected by the user. The command remains
    /// owned by Hyprland; Voisu only emits the verified binding key.
    Simple,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedPasteAction {
    pub shortcut: PasteShortcut,
    pub description: String,
    pub behavior: PasteBehavior,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TriggerKey {
    pub label: &'static str,
    pub code: &'static str,
}

pub const CAPS_LOCK: TriggerKey = TriggerKey {
    label: "Caps Lock",
    code: "code:66",
};

pub const RIGHT_ALT: TriggerKey = TriggerKey {
    label: "Right Alt",
    code: "code:108",
};

/// Left Alt remains a documented combination example; Hyprland setup never
/// auto-installs it as the Trigger Key.
pub const LEFT_ALT: TriggerKey = TriggerKey {
    label: "Left Alt",
    code: "code:64",
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LuaBinding {
    pub key: String,
    pub description: String,
    pub command: String,
    managed: bool,
}

impl LuaBinding {
    pub fn is_standalone(&self) -> bool {
        self.is_standalone_for(CAPS_LOCK) || self.is_standalone_for(RIGHT_ALT)
    }

    fn is_standalone_for(&self, candidate: TriggerKey) -> bool {
        self.key.trim() == candidate.code
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BindingConflict {
    pub candidate: TriggerKey,
    pub description: String,
    pub command: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TriggerBindingPlan {
    Install { key: TriggerKey },
    AlreadyInstalled { key: TriggerKey },
    Conflicts { conflicts: Vec<BindingConflict> },
}

#[derive(Debug)]
pub enum TriggerBindingError {
    File {
        action: &'static str,
        path: PathBuf,
        detail: String,
    },
    Conflicts {
        conflicts: Vec<BindingConflict>,
    },
    ReloadFailed {
        detail: String,
        backup_path: PathBuf,
        restore_error: Option<String>,
    },
    VerificationFailed {
        detail: String,
        backup_path: PathBuf,
        restore_error: Option<String>,
    },
}

impl TriggerBindingError {
    pub fn from_conflicts(conflicts: Vec<BindingConflict>) -> Self {
        Self::Conflicts { conflicts }
    }

    pub const fn exit_code(&self) -> u8 {
        4
    }
}

impl fmt::Display for TriggerBindingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::File {
                action,
                path,
                detail,
            } => {
                write!(formatter, "cannot {action} {}: {detail}", path.display())
            }
            Self::Conflicts { conflicts } => {
                writeln!(
                    formatter,
                    "preferred Trigger Key candidates are already bound:"
                )?;
                for conflict in conflicts {
                    writeln!(
                        formatter,
                        "- {} ({}): {} -> {}",
                        conflict.candidate.label,
                        conflict.candidate.code,
                        conflict.description,
                        conflict.command
                    )?;
                }
                write!(
                    formatter,
                    "Recovery: remove or change one exact binding, then rerun `{RECOVERY_COMMAND}`."
                )
            }
            Self::ReloadFailed {
                detail,
                backup_path,
                restore_error,
            } => write_recovery_error(
                formatter,
                "Hyprland reload failed",
                detail,
                backup_path,
                restore_error.as_deref(),
            ),
            Self::VerificationFailed {
                detail,
                backup_path,
                restore_error,
            } => write_recovery_error(
                formatter,
                "Hyprland binding verification failed",
                detail,
                backup_path,
                restore_error.as_deref(),
            ),
        }
    }
}

fn write_recovery_error(
    formatter: &mut fmt::Formatter<'_>,
    headline: &str,
    detail: &str,
    backup_path: &Path,
    restore_error: Option<&str>,
) -> fmt::Result {
    write!(
        formatter,
        "{headline}: {detail}; {}. Recovery: restore {} and run `hyprctl reload`.",
        restore_error
            .map(|error| format!("automatic restore failed: {error}"))
            .unwrap_or_else(|| "the previous configuration was restored".to_owned()),
        backup_path.display()
    )
}

impl std::error::Error for TriggerBindingError {}

pub trait BindingFileSystem {
    fn read_to_string(&self, path: &Path) -> Result<Option<String>, String>;
    fn write_atomic(&self, path: &Path, contents: &str) -> Result<(), String>;
    fn remove_file(&self, path: &Path) -> Result<(), String>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct LocalBindingFileSystem;

impl BindingFileSystem for LocalBindingFileSystem {
    fn read_to_string(&self, path: &Path) -> Result<Option<String>, String> {
        match fs::read_to_string(path) {
            Ok(contents) => Ok(Some(contents)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }

    fn write_atomic(&self, path: &Path, contents: &str) -> Result<(), String> {
        let parent = path
            .parent()
            .ok_or_else(|| "configuration path has no parent".to_owned())?;
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;

        let mode = match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err("refusing to replace a symbolic link".to_owned());
            }
            Ok(metadata) => metadata.mode() & 0o777,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0o600,
            Err(error) => return Err(error.to_string()),
        };

        let mut staged = tempfile::Builder::new()
            .prefix(".voisu-hyprland.")
            .tempfile_in(parent)
            .map_err(|error| error.to_string())?;
        staged
            .write_all(contents.as_bytes())
            .and_then(|()| staged.as_file().sync_all())
            .map_err(|error| error.to_string())?;
        fs::set_permissions(staged.path(), fs::Permissions::from_mode(mode))
            .map_err(|error| error.to_string())?;
        staged
            .persist(path)
            .map_err(|error| error.error.to_string())?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| error.to_string())
    }

    fn remove_file(&self, path: &Path) -> Result<(), String> {
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.to_string()),
        }
    }
}

pub trait HyprlandController {
    fn reload(&mut self) -> Result<(), String>;
    fn binding_is_installed(&mut self, key: &str, command: &str) -> Result<bool, String>;

    /// Returns the compositor's current binding table. A default keeps the
    /// trigger-binding seam source-compatible for callers that only need
    /// installation verification.
    fn live_bindings(&mut self) -> Result<Value, String> {
        Err("live Hyprland bindings are unavailable".to_owned())
    }
}

pub struct LiveHyprlandController;

impl HyprlandController for LiveHyprlandController {
    fn reload(&mut self) -> Result<(), String> {
        crate::system::run_restricted_stdout("hyprctl", &["reload"])
            .map(|_| ())
            .ok_or_else(|| "`hyprctl reload` returned a failure".to_owned())
    }

    fn binding_is_installed(&mut self, key: &str, command: &str) -> Result<bool, String> {
        let payload = crate::system::run_restricted_stdout("hyprctl", &["binds", "-j"])
            .ok_or_else(|| "`hyprctl binds -j` returned a failure".to_owned())?;
        serde_json::from_slice::<Value>(&payload)
            .map(|value| hyprland_binding_is_installed(&value, key, command))
            .map_err(|error| format!("invalid `hyprctl binds -j` response: {error}"))
    }

    fn live_bindings(&mut self) -> Result<Value, String> {
        let payload = crate::system::run_restricted_stdout("hyprctl", &["binds", "-j"])
            .ok_or_else(|| "`hyprctl binds -j` returned a failure".to_owned())?;
        serde_json::from_slice(&payload)
            .map_err(|error| format!("invalid `hyprctl binds -j` response: {error}"))
    }
}

pub fn hyprland_binding_is_installed(payload: &Value, key: &str, command: &str) -> bool {
    payload.as_array().is_some_and(|bindings| {
        bindings.iter().any(|binding| {
            if binding.get("modmask").and_then(Value::as_u64) != Some(0) {
                return false;
            }

            let dispatcher = binding.get("dispatcher").and_then(Value::as_str);
            let argument = binding_argument(binding).map(str::trim);
            let native_exec = dispatcher == Some("exec")
                && binding.get("key").and_then(Value::as_str) == Some(key)
                && argument == Some(command);

            // Current Hyprland Lua bindings are reported as dispatcher="__lua"
            // with an opaque registry id in arg; the physical key and function
            // body are intentionally not exposed by `hyprctl binds -j`. The
            // exact Voisu-managed description is therefore the stable identity
            // available for post-reload verification. Lua config parsing above
            // still proves the candidate key before writing this block.
            let lua_binding = dispatcher == Some("__lua")
                && binding.get("description").and_then(Value::as_str)
                    == Some(VOISU_TRIGGER_DESCRIPTION)
                && argument.is_some_and(|value| value.parse::<u64>().is_ok());

            native_exec || lua_binding
        })
    })
}

fn binding_argument(binding: &Value) -> Option<&str> {
    binding.get("arg").and_then(Value::as_str).or_else(|| {
        binding
            .get("arg")
            .and_then(|arg| arg.get("arg"))
            .and_then(Value::as_str)
    })
}

pub fn parse_lua_bindings(source: &str) -> Vec<LuaBinding> {
    let mut bindings = Vec::new();
    let mut chunk = String::new();
    let mut managed = false;

    for line in source.split_inclusive('\n') {
        match line.trim() {
            BEGIN_MARKER => {
                bindings.extend(parse_lua_chunk(&chunk, managed));
                chunk.clear();
                managed = true;
            }
            END_MARKER => {
                bindings.extend(parse_lua_chunk(&chunk, managed));
                chunk.clear();
                managed = false;
            }
            _ => chunk.push_str(line),
        }
    }
    bindings.extend(parse_lua_chunk(&chunk, managed));
    bindings
}

/// Finds the first paste binding that is proven by both the active Lua source
/// and the compositor's live binding table. The source is deliberately
/// conservative: literal dispatchers are accepted only when their description
/// identifies a paste action, and dynamic Lua functions are accepted only for
/// the exact Omarchy universal-paste helper shape.
pub fn discover_paste_action(
    sources: &[&str],
    live_bindings: &Value,
) -> Option<VerifiedPasteAction> {
    let bindings = sources
        .iter()
        .flat_map(|source| {
            parse_lua_bindings(source)
                .into_iter()
                .map(|binding| (binding, (*source).to_owned()))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let live = live_bindings.as_array()?;

    for binding in live {
        let Some(dispatcher) = binding.get("dispatcher").and_then(Value::as_str) else {
            continue;
        };
        let Some(live_description) = binding.get("description").and_then(Value::as_str) else {
            continue;
        };
        let Some(source) = bindings.iter().find_map(|(candidate, source)| {
            (candidate.description == live_description
                && live_binding_matches_source(binding, candidate))
            .then_some((candidate, source))
        }) else {
            continue;
        };
        let (candidate, source_text) = source;

        if dispatcher == "__lua" && is_omarchy_universal_paste(candidate, source_text) {
            return Some(VerifiedPasteAction {
                shortcut: PasteShortcut {
                    binding: candidate.key.trim().to_owned(),
                },
                description: candidate.description.clone(),
                behavior: PasteBehavior::OmarchyUniversal {
                    normal: PasteShortcut {
                        binding: "CTRL + V".to_owned(),
                    },
                    terminal: PasteShortcut {
                        binding: "SHIFT + Insert".to_owned(),
                    },
                },
            });
        }

        // A literal dispatcher is safe to trigger through the compositor. The
        // command itself is not executed by Voisu; it is only selected after
        // the source and live dispatcher agree. Dynamic functions and unknown
        // dispatchers never reach this branch.
        if dispatcher == "exec"
            && candidate.command != "<Lua dispatcher>"
            && is_paste_description(&candidate.description)
        {
            return Some(VerifiedPasteAction {
                shortcut: PasteShortcut {
                    binding: candidate.key.trim().to_owned(),
                },
                description: candidate.description.clone(),
                behavior: PasteBehavior::Simple,
            });
        }
    }
    None
}

/// Reads the active current-Lua root and the small set of source files that
/// can define the standard Omarchy clipboard helper. Setup can use this with
/// its injected [`BindingFileSystem`] seam; production discovery never scans
/// arbitrary files or evaluates Lua.
pub fn discover_paste_action_from_sources(
    root: &Path,
    files: &dyn BindingFileSystem,
    hyprland: &mut dyn HyprlandController,
) -> Result<Option<VerifiedPasteAction>, String> {
    let root_source = files
        .read_to_string(root)?
        .ok_or_else(|| format!("active Hyprland Lua source is missing: {}", root.display()))?;
    let mut source_storage = vec![root_source];

    // Current Omarchy imports the clipboard bindings through its default
    // module tree rather than copying the helper into the user's file. Only
    // add the known file when the active source actually refers to Omarchy;
    // this keeps unrelated or inactive Lua files out of the decision.
    let root_mentions_omarchy = source_storage[0].contains("omarchy")
        || std::env::var("XDG_CURRENT_DESKTOP")
            .is_ok_and(|desktop| desktop.eq_ignore_ascii_case("omarchy"));
    if root_mentions_omarchy {
        for path in omarchy_clipboard_source_candidates(root) {
            if let Some(source) = files.read_to_string(&path)? {
                source_storage.push(source);
                break;
            }
        }
    }

    let live = hyprland.live_bindings()?;
    let sources = source_storage
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    Ok(discover_paste_action(&sources, &live))
}

/// Production discovery for the daemon. A missing or unreadable source is a
/// safe clipboard-only result; setup/diagnostic callers that need the exact
/// failure should use [`discover_paste_action_from_sources`] directly.
pub fn discover_live_paste_action() -> Option<VerifiedPasteAction> {
    std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE")?;
    let config_root = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join(".config"))
        })?;
    let root = config_root.join("hypr/hyprland.lua");
    let files = LocalBindingFileSystem;
    let mut hyprland = LiveHyprlandController;
    let root_source = files.read_to_string(&root).ok().flatten()?;
    let mut sources = vec![root_source];
    let bindings = root.with_file_name("bindings.lua");
    if let Ok(Some(source)) = files.read_to_string(&bindings) {
        sources.push(source);
    }
    if sources[0].contains("omarchy")
        || std::env::var("XDG_CURRENT_DESKTOP")
            .is_ok_and(|desktop| desktop.eq_ignore_ascii_case("omarchy"))
    {
        for path in omarchy_clipboard_source_candidates(&root) {
            if let Ok(Some(source)) = files.read_to_string(&path) {
                sources.push(source);
                break;
            }
        }
    }
    let live = hyprland.live_bindings().ok()?;
    let source_refs = sources.iter().map(String::as_str).collect::<Vec<_>>();
    discover_paste_action(&source_refs, &live)
}

fn omarchy_clipboard_source_candidates(root: &Path) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(path) = std::env::var_os("OMARCHY_PATH") {
        candidates.push(PathBuf::from(path).join("default/hypr/bindings/clipboard.lua"));
    }
    if let Some(home) = root
        .ancestors()
        .find(|path| path.file_name().is_some_and(|name| name == "hypr"))
        .and_then(Path::parent)
    {
        candidates.push(home.join("default/hypr/bindings/clipboard.lua"));
    }
    candidates.push(PathBuf::from(
        "/usr/share/omarchy/default/hypr/bindings/clipboard.lua",
    ));
    candidates
}

fn live_binding_matches_source(live: &Value, source: &LuaBinding) -> bool {
    let Some((source_modmask, source_key)) = shortcut_parts(&source.key) else {
        return false;
    };
    let live_key = live.get("key").and_then(Value::as_str).unwrap_or_default();
    let live_modmask = live
        .get("modmask")
        .and_then(Value::as_u64)
        .unwrap_or(u64::MAX);
    if source_modmask != live_modmask || normalize_key(live_key) != normalize_key(&source_key) {
        return false;
    }
    match live.get("dispatcher").and_then(Value::as_str) {
        Some("exec") => binding_argument(live)
            .map(str::trim)
            .is_some_and(|argument| argument == source.command.trim()),
        Some("__lua") => source.command == "<Lua dispatcher>",
        _ => false,
    }
}

fn shortcut_parts(binding: &str) -> Option<(u64, String)> {
    let pieces = binding
        .split('+')
        .map(str::trim)
        .filter(|piece| !piece.is_empty())
        .collect::<Vec<_>>();
    let key = pieces.last()?.to_owned();
    let mut modmask = 0;
    for modifier in &pieces[..pieces.len().saturating_sub(1)] {
        modmask |= match modifier.to_ascii_uppercase().as_str() {
            "SHIFT" => 1,
            "CAPS" | "CAPSLOCK" => 2,
            "CTRL" | "CONTROL" => 4,
            "ALT" => 8,
            "SUPER" | "META" | "WIN" | "MOD4" => 64,
            _ => return None,
        };
    }
    Some((modmask, key.to_owned()))
}

fn normalize_key(key: &str) -> String {
    key.trim().to_ascii_uppercase()
}

fn is_paste_description(description: &str) -> bool {
    let description = description.to_ascii_lowercase();
    description.contains("paste")
        || (description.contains("clipboard") && description.contains("insert"))
}

fn is_omarchy_universal_paste(binding: &LuaBinding, source: &str) -> bool {
    if binding.command != "<Lua dispatcher>"
        || binding.description != "Universal paste"
        || !binding.key.contains('+')
    {
        return false;
    }
    let compact = source
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>();
    [
        "universal_clipboard_shortcut(default_mods,default_key,terminal_mods,terminal_key)",
        "ifactive_window_is_terminal()then",
        "send_shortcut_once(terminal_mods,terminal_key)()",
        "send_shortcut_once(default_mods,default_key)()",
        "o.bind(\"SUPER+V\",\"Universalpaste\",universal_clipboard_shortcut(\"CTRL\",\"V\",\"SHIFT\",\"Insert\"))",
    ]
    .iter()
    .all(|marker| compact.contains(marker))
        && (compact.contains("functionuniversal_clipboard_shortcut(")
            || compact.contains("localfunctionuniversal_clipboard_shortcut("))
}

pub fn plan_trigger_binding(source: &str, prefer_caps_lock: bool) -> TriggerBindingPlan {
    plan_trigger_bindings(&[source], prefer_caps_lock)
}

/// Plans the Trigger Key from every occupancy source. Sibling files such as
/// `bindings.lua` must be included so an exact unmanaged Caps Lock bind there
/// is not installed again in `hyprland.lua`.
pub fn plan_trigger_bindings(sources: &[&str], prefer_caps_lock: bool) -> TriggerBindingPlan {
    let bindings = sources
        .iter()
        .flat_map(|source| parse_lua_bindings(source))
        .collect::<Vec<_>>();
    for candidate in [CAPS_LOCK, RIGHT_ALT] {
        if bindings
            .iter()
            .any(|binding| binding.managed && binding.is_standalone_for(candidate))
        {
            return TriggerBindingPlan::AlreadyInstalled { key: candidate };
        }
    }

    let unmanaged = |candidate: TriggerKey| {
        bindings
            .iter()
            .find(|binding| !binding.managed && binding.is_standalone_for(candidate))
    };

    if prefer_caps_lock && unmanaged(CAPS_LOCK).is_none() {
        return TriggerBindingPlan::Install { key: CAPS_LOCK };
    }
    if unmanaged(RIGHT_ALT).is_none() {
        return TriggerBindingPlan::Install { key: RIGHT_ALT };
    }

    let mut conflicts = Vec::new();
    for candidate in [CAPS_LOCK, RIGHT_ALT] {
        if let Some(binding) = unmanaged(candidate) {
            conflicts.push(BindingConflict {
                candidate,
                description: binding.description.clone(),
                command: binding.command.clone(),
            });
        }
    }
    TriggerBindingPlan::Conflicts { conflicts }
}

pub fn backup_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("hyprland.lua"));
    let mut backup_name = OsString::from(".");
    backup_name.push(file_name);
    backup_name.push(".voisu-backup");
    path.with_file_name(backup_name)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TriggerBindingInstallReport {
    pub key: TriggerKey,
    pub changed: bool,
    pub backup_path: PathBuf,
}

pub fn install_trigger_binding(
    path: &Path,
    files: &dyn BindingFileSystem,
    hyprland: &mut dyn HyprlandController,
    prefer_caps_lock: bool,
) -> Result<TriggerBindingInstallReport, TriggerBindingError> {
    let original = files
        .read_to_string(path)
        .map_err(|detail| TriggerBindingError::File {
            action: "read",
            path: path.to_owned(),
            detail,
        })?;
    let source = original.as_deref().unwrap_or_default();
    let sibling_path = path.with_file_name("bindings.lua");
    let sibling =
        files
            .read_to_string(&sibling_path)
            .map_err(|detail| TriggerBindingError::File {
                action: "read",
                path: sibling_path,
                detail,
            })?;
    let occupancy = occupancy_sources(source, sibling.as_deref());
    let backup = backup_path(path);
    let plan = plan_trigger_bindings(&occupancy, prefer_caps_lock);
    let input_path = path.with_file_name("input.lua");
    let original_input =
        files
            .read_to_string(&input_path)
            .map_err(|detail| TriggerBindingError::File {
                action: "read",
                path: input_path,
                detail,
            })?;

    let key = match plan {
        TriggerBindingPlan::AlreadyInstalled { key } => key,
        TriggerBindingPlan::Conflicts { conflicts } => {
            return Err(TriggerBindingError::from_conflicts(conflicts));
        }
        TriggerBindingPlan::Install { key } => key,
    };

    let updated = desired_hyprland_source(source, key, original_input.as_deref().unwrap_or(""));
    if updated == source {
        verify_installed_binding(hyprland, key, &backup)?;
        return Ok(TriggerBindingInstallReport {
            key,
            changed: false,
            backup_path: backup,
        });
    }

    files
        .write_atomic(&backup, source)
        .map_err(|detail| TriggerBindingError::File {
            action: "save a recoverable backup",
            path: backup.clone(),
            detail,
        })?;

    if let Err(detail) = files.write_atomic(path, &updated) {
        let restore_error = restore_original(files, path, original.as_deref()).err();
        return Err(TriggerBindingError::File {
            action: "install the Hyprland binding",
            path: path.to_owned(),
            detail: restore_error
                .map(|restore| format!("{detail}; automatic restore failed: {restore}"))
                .unwrap_or(detail),
        });
    }

    if let Err(error) = reload_and_verify(hyprland, key, path, files, original.as_deref(), &backup) {
        return Err(error);
    }

    Ok(TriggerBindingInstallReport {
        key,
        changed: true,
        backup_path: backup,
    })
}

fn reload_and_verify(
    hyprland: &mut dyn HyprlandController,
    key: TriggerKey,
    path: &Path,
    files: &dyn BindingFileSystem,
    original: Option<&str>,
    backup: &Path,
) -> Result<(), TriggerBindingError> {
    if let Err(detail) = hyprland.reload() {
        let restore_error = restore_original(files, path, original).err();
        return Err(TriggerBindingError::ReloadFailed {
            detail,
            backup_path: backup.to_owned(),
            restore_error,
        });
    }
    match hyprland.binding_is_installed(key.code, VOISU_TOGGLE_COMMAND) {
        Ok(true) => Ok(()),
        Ok(false) => {
            let restore_error = restore_original(files, path, original).err();
            Err(TriggerBindingError::VerificationFailed {
                detail: format!(
                    "Hyprland did not report {} ({}) running `{VOISU_TOGGLE_COMMAND}`",
                    key.label, key.code
                ),
                backup_path: backup.to_owned(),
                restore_error,
            })
        }
        Err(detail) => {
            let restore_error = restore_original(files, path, original).err();
            Err(TriggerBindingError::VerificationFailed {
                detail,
                backup_path: backup.to_owned(),
                restore_error,
            })
        }
    }
}

fn verify_installed_binding(
    hyprland: &mut dyn HyprlandController,
    key: TriggerKey,
    backup: &Path,
) -> Result<(), TriggerBindingError> {
    let verified = hyprland
        .binding_is_installed(key.code, VOISU_TOGGLE_COMMAND)
        .map_err(|detail| TriggerBindingError::VerificationFailed {
            detail,
            backup_path: backup.to_owned(),
            restore_error: None,
        })?;
    if verified {
        Ok(())
    } else {
        Err(TriggerBindingError::VerificationFailed {
            detail: format!(
                "{} ({}) is managed by Voisu but is not reported by Hyprland",
                key.label, key.code
            ),
            backup_path: backup.to_owned(),
            restore_error: None,
        })
    }
}

fn occupancy_sources<'a>(hyprland: &'a str, sibling: Option<&'a str>) -> Vec<&'a str> {
    match sibling {
        Some(extra) => vec![hyprland, extra],
        None => vec![hyprland],
    }
}

fn restore_original(
    files: &dyn BindingFileSystem,
    path: &Path,
    original: Option<&str>,
) -> Result<(), String> {
    match original {
        Some(contents) => files.write_atomic(path, contents),
        None => files.remove_file(path),
    }
}

fn desired_hyprland_source(source: &str, key: TriggerKey, input_source: &str) -> String {
    let kb_options = (key == CAPS_LOCK)
        .then(|| merge_caps_lock_kb_options(last_kb_options(&[input_source, source])));
    append_managed_binding(source, key, kb_options.as_deref())
}

fn append_managed_binding(source: &str, key: TriggerKey, kb_options: Option<&str>) -> String {
    let mut content = remove_managed_blocks(source);
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    if !content.is_empty() && !content.ends_with("\n\n") {
        content.push('\n');
    }
    content.push_str(BEGIN_MARKER);
    content.push('\n');
    if let Some(kb_options) = kb_options {
        // Stock Hyprland Lua has no Omarchy package.path, so a sibling
        // require("hypr.input") is a hard error. Last-wins hl.config here
        // applies Caps Lock kb_options without loading another file.
        content.push_str("-- Caps Lock is the Trigger Key; disable lock on that key.\n");
        content.push_str("hl.config({\n");
        content.push_str("  input = {\n");
        content.push_str(&format!("    kb_options = \"{kb_options}\",\n"));
        content.push_str("  },\n");
        content.push_str("})\n");
    }
    content.push_str(&format!(
        "o.bind(\"{}\", \"{VOISU_TRIGGER_DESCRIPTION}\", \"{VOISU_TOGGLE_COMMAND}\")\n",
        key.code
    ));
    content.push_str(END_MARKER);
    content.push('\n');
    content
}

fn last_kb_options(sources: &[&str]) -> Option<String> {
    sources
        .iter()
        .rev()
        .find_map(|source| kb_options_values(source).into_iter().next_back())
}

fn kb_options_values(source: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut rest = source;
    while let Some(index) = rest.find("kb_options") {
        rest = &rest[index + "kb_options".len()..];
        let trimmed = rest.trim_start();
        let Some(trimmed) = trimmed.strip_prefix('=') else {
            continue;
        };
        let trimmed = trimmed.trim_start();
        let quote = match trimmed.chars().next() {
            Some('"') => '"',
            Some('\'') => '\'',
            _ => continue,
        };
        let inner = &trimmed[1..];
        if let Some(end) = inner.find(quote) {
            values.push(inner[..end].to_owned());
        }
    }
    values
}

fn merge_caps_lock_kb_options(existing: Option<String>) -> String {
    let mut kept = Vec::new();
    if let Some(existing) = existing {
        for option in existing.split(',') {
            let option = option.trim();
            if option.is_empty()
                || option == "compose:caps"
                || option.starts_with("caps:")
                || option == BOTH_CAPSLOCK_CANCEL
                || option == "shift:both_capslock"
            {
                continue;
            }
            if !kept.iter().any(|kept: &String| kept == option) {
                kept.push(option.to_owned());
            }
        }
    }
    let mut result = vec![CAPS_NONE.to_owned(), BOTH_CAPSLOCK_CANCEL.to_owned()];
    result.extend(kept);
    result.join(",")
}

fn remove_managed_blocks(source: &str) -> String {
    remove_marked_block(source, BEGIN_MARKER, END_MARKER)
}

fn remove_marked_block(source: &str, begin: &str, end: &str) -> String {
    let mut result = String::new();
    let mut managed = false;
    for line in source.split_inclusive('\n') {
        match line.trim() {
            marker if marker == begin => managed = true,
            marker if marker == end && managed => managed = false,
            _ if !managed => result.push_str(line),
            _ => {}
        }
    }
    result
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum LuaToken {
    Identifier(String),
    String(String),
    Symbol(char),
}

fn parse_lua_chunk(source: &str, managed: bool) -> Vec<LuaBinding> {
    let tokens = lex_lua(source);
    let mut bindings = Vec::new();
    let mut index = 0;
    while index + 9 < tokens.len() {
        let is_bind_call = matches!(
            (&tokens[index], &tokens[index + 1], &tokens[index + 2], &tokens[index + 3]),
            (
                LuaToken::Identifier(object),
                LuaToken::Symbol('.'),
                LuaToken::Identifier(method),
                LuaToken::Symbol('(')
            ) if object == "o" && method == "bind"
        );
        if !is_bind_call {
            index += 1;
            continue;
        }
        let Some((key, description, command)) = parse_bind_arguments(&tokens[index + 4..]) else {
            index += 1;
            continue;
        };
        bindings.push(LuaBinding {
            key,
            description,
            command,
            managed,
        });
        index += 4;
    }
    bindings
}

fn parse_bind_arguments(tokens: &[LuaToken]) -> Option<(String, String, String)> {
    let mut index = 0;
    let read_string = |index: &mut usize| -> Option<String> {
        let value = match tokens.get(*index)? {
            LuaToken::String(value) => value.clone(),
            _ => return None,
        };
        *index += 1;
        Some(value)
    };
    let key = read_string(&mut index)?;
    if !matches!(tokens.get(index), Some(LuaToken::Symbol(','))) {
        return None;
    }
    index += 1;
    let description = read_string(&mut index)?;
    if !matches!(tokens.get(index), Some(LuaToken::Symbol(','))) {
        return None;
    }
    index += 1;
    let command = match tokens.get(index)? {
        LuaToken::String(value) => value.clone(),
        LuaToken::Symbol(')') => return None,
        _ => "<Lua dispatcher>".to_owned(),
    };
    Some((key, description, command))
}

fn lex_lua(source: &str) -> Vec<LuaToken> {
    let mut chars = source.chars().peekable();
    let mut tokens = Vec::new();
    while let Some(character) = chars.next() {
        if character == '-' && chars.peek() == Some(&'-') {
            chars.next();
            if chars.peek() == Some(&'[') {
                chars.next();
                if chars.peek() == Some(&'[') {
                    chars.next();
                    let mut previous = None;
                    for character in chars.by_ref() {
                        if previous == Some(']') && character == ']' {
                            break;
                        }
                        previous = Some(character);
                    }
                    continue;
                }
            }
            for character in chars.by_ref() {
                if character == '\n' {
                    break;
                }
            }
            continue;
        }
        if character == '"' || character == '\'' {
            let quote = character;
            let mut value = String::new();
            while let Some(character) = chars.next() {
                match character {
                    '\\' => match chars.next() {
                        Some('n') => value.push('\n'),
                        Some('r') => value.push('\r'),
                        Some('t') => value.push('\t'),
                        Some(escaped) => value.push(escaped),
                        None => break,
                    },
                    character if character == quote => break,
                    character => value.push(character),
                }
            }
            tokens.push(LuaToken::String(value));
            continue;
        }
        if character.is_ascii_alphabetic() || character == '_' {
            let mut identifier = String::from(character);
            while let Some(next) = chars.peek().copied() {
                if next.is_ascii_alphanumeric() || next == '_' {
                    identifier.push(next);
                    chars.next();
                } else {
                    break;
                }
            }
            tokens.push(LuaToken::Identifier(identifier));
            continue;
        }
        if matches!(character, '.' | '(' | ')' | ',' | '{' | '}') {
            tokens.push(LuaToken::Symbol(character));
        }
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;

    struct FakeHyprland {
        reload_succeeds: bool,
        verification_succeeds: bool,
    }

    impl HyprlandController for FakeHyprland {
        fn reload(&mut self) -> Result<(), String> {
            self.reload_succeeds
                .then_some(())
                .ok_or_else(|| "reload failed".to_owned())
        }

        fn binding_is_installed(&mut self, _key: &str, _command: &str) -> Result<bool, String> {
            Ok(self.verification_succeeds)
        }
    }

    fn config_dir() -> (TempDir, std::path::PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("hyprland.lua");
        (directory, path)
    }

    fn managed_block(source: &str) -> &str {
        let start = source.find(BEGIN_MARKER).expect("managed begin marker");
        let end = source.find(END_MARKER).expect("managed end marker") + END_MARKER.len();
        &source[start..end]
    }

    fn assert_caps_lock_managed_block(source: &str) {
        let block = managed_block(source);
        assert!(
            !block.contains("require("),
            "managed trigger block must not require a sibling file: {block}"
        );
        assert!(
            !block.contains("require(\"hypr.input\")") && !block.contains("require(\"input\")"),
            "managed trigger block must not load input.lua: {block}"
        );
        assert!(
            block.contains("o.bind(\"code:66\", \"Voisu dictation\", \"voisu toggle\")"),
            "{block}"
        );
        let last = last_kb_options(&[block]).expect("managed kb_options");
        assert!(
            last.split(',').map(str::trim).any(|part| part == CAPS_NONE),
            "last-wins kb_options must include {CAPS_NONE}: {last}"
        );
        assert!(
            last.split(',').map(str::trim).any(|part| part == BOTH_CAPSLOCK_CANCEL),
            "last-wins kb_options must keep {BOTH_CAPSLOCK_CANCEL}: {last}"
        );
    }

    fn assert_right_alt_managed_block(source: &str) {
        let block = managed_block(source);
        assert!(
            block.contains("o.bind(\"code:108\", \"Voisu dictation\", \"voisu toggle\")"),
            "{block}"
        );
        assert!(
            !block.contains("kb_options"),
            "Right Alt must not inject kb_options: {block}"
        );
        assert!(
            !block.contains("require("),
            "Right Alt must not inject a require: {block}"
        );
    }

    #[test]
    fn lua_parser_keeps_combinations_out_of_standalone_conflicts() {
        let source = r#"
-- o.bind("code:64", "comment", "not a binding")
o.bind("ALT, code:64", "Alt Tab", "workspace next")
o.bind(
    "code:108",
    "Open terminal",
    "kitty"
)
"#;

        let bindings = parse_lua_bindings(source);

        assert_eq!(bindings.len(), 2);
        assert!(!bindings[0].is_standalone());
        assert_eq!(bindings[0].description, "Alt Tab");
        assert!(bindings[1].is_standalone());
        assert_eq!(bindings[1].key, "code:108");
        assert_eq!(bindings[1].command, "kitty");
    }

    #[test]
    fn caps_lock_conflict_falls_back_to_right_alt() {
        let source = r#"
o.bind("code:66", "Launch terminal", "kitty")
o.bind("ALT, code:108", "Modified right alt", "workspace next")
"#;

        assert_eq!(
            plan_trigger_binding(source, true),
            TriggerBindingPlan::Install { key: RIGHT_ALT }
        );
    }

    #[test]
    fn declining_caps_lock_installs_right_alt() {
        assert_eq!(
            plan_trigger_binding("", false),
            TriggerBindingPlan::Install { key: RIGHT_ALT }
        );
    }

    #[test]
    fn unmanaged_caps_lock_in_sibling_bindings_falls_back_to_right_alt() {
        let (_directory, path) = config_dir();
        fs::write(&path, "").unwrap();
        let bindings = path.with_file_name("bindings.lua");
        let original_bindings = "o.bind(\"code:66\", \"Launch terminal\", \"kitty\")\n";
        fs::write(&bindings, original_bindings).unwrap();
        let mut hyprland = FakeHyprland {
            reload_succeeds: true,
            verification_succeeds: true,
        };

        let report =
            install_trigger_binding(&path, &LocalBindingFileSystem, &mut hyprland, true).unwrap();
        let hyprland_lua = fs::read_to_string(&path).unwrap();

        assert_eq!(report.key, RIGHT_ALT);
        assert!(!hyprland_lua.contains("code:66"));
        assert_right_alt_managed_block(&hyprland_lua);
        assert_eq!(fs::read_to_string(&bindings).unwrap(), original_bindings);
        assert!(!path.with_file_name("input.lua").exists());
    }

    #[test]
    fn exact_bindings_with_lua_dispatchers_are_still_conflicts() {
        let source = r#"
o.bind("code:66", "Terminal", { omarchy = "terminal" })
o.bind("code:108", "Launcher", hl.dsp.exec_cmd("launcher"))
"#;

        let TriggerBindingPlan::Conflicts { conflicts } = plan_trigger_binding(source, true) else {
            panic!("non-string dispatchers must not leave exact keys available");
        };
        assert_eq!(conflicts.len(), 2);
        assert_eq!(conflicts[0].description, "Terminal");
        assert_eq!(conflicts[0].command, "<Lua dispatcher>");
        assert_eq!(conflicts[1].description, "Launcher");
        assert_eq!(conflicts[1].command, "<Lua dispatcher>");
    }

    #[test]
    fn hyprland_verification_requires_an_exact_unmodified_exec_binding() {
        let installed = serde_json::json!([
            {
                "key": "code:64",
                "modmask": 0,
                "dispatcher": "exec",
                "arg": "voisu toggle"
            }
        ]);
        assert!(hyprland_binding_is_installed(
            &installed,
            LEFT_ALT.code,
            VOISU_TOGGLE_COMMAND
        ));

        let combination = serde_json::json!([
            {
                "key": "code:64",
                "modmask": 64,
                "dispatcher": "exec",
                "arg": "voisu toggle"
            }
        ]);
        assert!(!hyprland_binding_is_installed(
            &combination,
            LEFT_ALT.code,
            VOISU_TOGGLE_COMMAND
        ));

        let lua_binding = serde_json::json!([
            {
                "key": "",
                "modmask": 0,
                "description": VOISU_TRIGGER_DESCRIPTION,
                "dispatcher": "__lua",
                "arg": "64"
            }
        ]);
        assert!(hyprland_binding_is_installed(
            &lua_binding,
            LEFT_ALT.code,
            VOISU_TOGGLE_COMMAND
        ));

        let unrelated_lua_binding = serde_json::json!([
            {
                "key": "",
                "modmask": 0,
                "description": "Other binding",
                "dispatcher": "__lua",
                "arg": "64"
            }
        ]);
        assert!(!hyprland_binding_is_installed(
            &unrelated_lua_binding,
            LEFT_ALT.code,
            VOISU_TOGGLE_COMMAND
        ));
    }

    #[test]
    fn both_exact_conflicts_describe_bindings_and_recovery() {
        let source = r#"
o.bind("code:66", "Launch terminal", "kitty")
o.bind("code:108", "Lock screen", "loginctl lock-session")
"#;

        let plan = plan_trigger_binding(source, true);
        let TriggerBindingPlan::Conflicts { conflicts } = plan else {
            panic!("both exact bindings must conflict");
        };
        assert_eq!(conflicts.len(), 2);
        assert_eq!(conflicts[0].description, "Launch terminal");
        assert_eq!(conflicts[1].description, "Lock screen");
        let error = TriggerBindingError::from_conflicts(conflicts);
        assert_eq!(error.exit_code(), 4);
        let message = error.to_string();
        assert!(message.contains("Launch terminal"));
        assert!(message.contains("Lock screen"));
        assert!(message.contains("voisu setup"));
    }

    #[test]
    fn install_writes_one_atomic_managed_block_and_keeps_backup() {
        let (_directory, path) = config_dir();
        let original =
            "-- user bindings\no.bind(\"ALT, code:64\", \"Alt Tab\", \"workspace next\")\n";
        fs::write(&path, original).unwrap();
        let mut hyprland = FakeHyprland {
            reload_succeeds: true,
            verification_succeeds: true,
        };

        let report =
            install_trigger_binding(&path, &LocalBindingFileSystem, &mut hyprland, true).unwrap();

        assert_eq!(report.key, CAPS_LOCK);
        assert!(report.changed);
        let updated = fs::read_to_string(&path).unwrap();
        assert_eq!(updated.matches(BEGIN_MARKER).count(), 1);
        assert_caps_lock_managed_block(&updated);
        assert!(updated.contains("-- user bindings"));
        assert_eq!(fs::read_to_string(&report.backup_path).unwrap(), original);
        assert!(!path.with_file_name("input.lua").exists());
        assert!(fs::read_dir(path.parent().unwrap())
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".voisu-hyprland.")));
    }

    #[test]
    fn stock_hyprland_without_input_require_gets_bind_and_kb_options() {
        let (_directory, path) = config_dir();
        fs::write(
            &path,
            concat!(
                "-- stock Hyprland Lua has no Omarchy bootstrap\n",
                "o.bind(\"SUPER, Q\", \"Quit\", \"hyprctl dispatch exit\")\n"
            ),
        )
        .unwrap();
        let mut hyprland = FakeHyprland {
            reload_succeeds: true,
            verification_succeeds: true,
        };

        let report =
            install_trigger_binding(&path, &LocalBindingFileSystem, &mut hyprland, true).unwrap();
        let updated = fs::read_to_string(&path).unwrap();

        assert_eq!(report.key, CAPS_LOCK);
        assert!(updated.contains("o.bind(\"SUPER, Q\", \"Quit\", \"hyprctl dispatch exit\")"));
        assert!(!updated.contains("require("));
        assert_caps_lock_managed_block(&updated);
        assert!(!path.with_file_name("input.lua").exists());
    }

    #[test]
    fn fallback_preserves_the_exact_caps_lock_binding() {
        let (_directory, path) = config_dir();
        let original = "o.bind(\"code:66\", \"Terminal\", \"kitty\")\n";
        fs::write(&path, original).unwrap();
        let mut hyprland = FakeHyprland {
            reload_succeeds: true,
            verification_succeeds: true,
        };

        let report =
            install_trigger_binding(&path, &LocalBindingFileSystem, &mut hyprland, true).unwrap();
        let updated = fs::read_to_string(&path).unwrap();

        assert_eq!(report.key, RIGHT_ALT);
        assert!(updated.contains(original));
        assert_right_alt_managed_block(&updated);
        assert!(!path.with_file_name("input.lua").exists());
    }

    #[test]
    fn rejected_caps_lock_installs_right_alt_without_input_rewrite() {
        let (_directory, path) = config_dir();
        fs::write(&path, "-- user bindings\n").unwrap();
        let mut hyprland = FakeHyprland {
            reload_succeeds: true,
            verification_succeeds: true,
        };

        let report =
            install_trigger_binding(&path, &LocalBindingFileSystem, &mut hyprland, false).unwrap();

        assert_eq!(report.key, RIGHT_ALT);
        let updated = fs::read_to_string(&path).unwrap();
        assert_right_alt_managed_block(&updated);
        assert!(!updated.contains("hl.config"));
        assert!(!path.with_file_name("input.lua").exists());
    }

    #[test]
    fn both_conflicts_leave_the_file_and_compositor_untouched() {
        let (_directory, path) = config_dir();
        let original = concat!(
            "o.bind(\"code:66\", \"Terminal\", \"kitty\")\n",
            "o.bind(\"code:108\", \"Launcher\", \"launcher\")\n"
        );
        fs::write(&path, original).unwrap();
        let mut hyprland = FakeHyprland {
            reload_succeeds: false,
            verification_succeeds: false,
        };

        let error = install_trigger_binding(&path, &LocalBindingFileSystem, &mut hyprland, true)
            .expect_err("both conflicts must stop before any external action");

        assert_eq!(error.exit_code(), 4);
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        assert!(!backup_path(&path).exists());
    }

    #[test]
    fn rerunning_does_not_duplicate_the_managed_binding() {
        let (_directory, path) = config_dir();
        fs::write(&path, "-- user bindings\n").unwrap();
        let mut first = FakeHyprland {
            reload_succeeds: true,
            verification_succeeds: true,
        };
        install_trigger_binding(&path, &LocalBindingFileSystem, &mut first, true).unwrap();
        let before = fs::read_to_string(&path).unwrap();
        let mut second = FakeHyprland {
            reload_succeeds: false,
            verification_succeeds: true,
        };

        let report =
            install_trigger_binding(&path, &LocalBindingFileSystem, &mut second, false).unwrap();

        assert_eq!(report.key, CAPS_LOCK);
        assert!(!report.changed);
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
        assert_eq!(before.matches(BEGIN_MARKER).count(), 1);
        assert_caps_lock_managed_block(&before);
    }

    #[test]
    fn reload_failure_restores_the_previous_file() {
        let (_directory, path) = config_dir();
        let original = "-- original\n";
        fs::write(&path, original).unwrap();
        let input_path = path.with_file_name("input.lua");
        let original_input = "require = nil -- user input.lua must stay untouched\n";
        fs::write(&input_path, original_input).unwrap();
        let mut hyprland = FakeHyprland {
            reload_succeeds: false,
            verification_succeeds: true,
        };

        let error = install_trigger_binding(&path, &LocalBindingFileSystem, &mut hyprland, true)
            .expect_err("reload failure must fail setup");

        assert!(error.to_string().contains("reload"));
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        assert_eq!(
            fs::read_to_string(backup_path(Path::new(&path))).unwrap(),
            original
        );
        assert_eq!(fs::read_to_string(&input_path).unwrap(), original_input);
    }

    #[test]
    fn verification_failure_restores_the_previous_file() {
        let (_directory, path) = config_dir();
        let original = "-- original\n";
        fs::write(&path, original).unwrap();
        let mut hyprland = FakeHyprland {
            reload_succeeds: true,
            verification_succeeds: false,
        };

        let error = install_trigger_binding(&path, &LocalBindingFileSystem, &mut hyprland, true)
            .expect_err("verification failure must fail setup");

        assert!(error.to_string().contains("verification"));
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        assert!(!path.with_file_name("input.lua").exists());
    }

    #[test]
    fn already_installed_right_alt_is_kept_without_asking_caps_lock() {
        let (_directory, path) = config_dir();
        fs::write(
            &path,
            concat!(
                "-- BEGIN VOISU MANAGED TRIGGER\n",
                "o.bind(\"code:108\", \"Voisu dictation\", \"voisu toggle\")\n",
                "-- END VOISU MANAGED TRIGGER\n"
            ),
        )
        .unwrap();
        let before = fs::read_to_string(&path).unwrap();
        let mut hyprland = FakeHyprland {
            reload_succeeds: false,
            verification_succeeds: true,
        };

        let report =
            install_trigger_binding(&path, &LocalBindingFileSystem, &mut hyprland, true).unwrap();

        assert_eq!(report.key, RIGHT_ALT);
        assert!(!report.changed);
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
        assert_right_alt_managed_block(&before);
        assert!(!path.with_file_name("input.lua").exists());
    }

    #[test]
    fn caps_lock_input_merges_existing_kb_options() {
        let (_directory, path) = config_dir();
        fs::write(&path, "-- user bindings\n").unwrap();
        fs::write(
            path.with_file_name("input.lua"),
            "hl.config({\n  input = {\n    kb_options = \"compose:caps,grp:alts_toggle\",\n  },\n})\n",
        )
        .unwrap();
        let mut hyprland = FakeHyprland {
            reload_succeeds: true,
            verification_succeeds: true,
        };

        let original_input = fs::read_to_string(path.with_file_name("input.lua")).unwrap();
        install_trigger_binding(&path, &LocalBindingFileSystem, &mut hyprland, true).unwrap();
        let hyprland_lua = fs::read_to_string(&path).unwrap();
        let input = fs::read_to_string(path.with_file_name("input.lua")).unwrap();
        let last = last_kb_options(&[&input, &hyprland_lua]).expect("managed kb_options");

        assert_eq!(input, original_input);
        assert_caps_lock_managed_block(&hyprland_lua);
        assert_eq!(
            last,
            format!("{CAPS_NONE},{BOTH_CAPSLOCK_CANCEL},grp:alts_toggle")
        );
        assert!(
            input.contains("compose:caps"),
            "the original assignment stays; last-wins managed kb_options overrides it"
        );
    }

    #[test]
    fn existing_caps_lock_input_is_not_rewritten() {
        let (_directory, path) = config_dir();
        fs::write(&path, "-- user bindings\n").unwrap();
        let input_path = path.with_file_name("input.lua");
        let original_input =
            "hl.config({\n  input = {\n    kb_options = \"caps:none,shift:both_capslock_cancel\",\n  },\n})\n";
        fs::write(&input_path, original_input).unwrap();
        let mut hyprland = FakeHyprland {
            reload_succeeds: true,
            verification_succeeds: true,
        };

        install_trigger_binding(&path, &LocalBindingFileSystem, &mut hyprland, true).unwrap();
        let hyprland_lua = fs::read_to_string(&path).unwrap();

        assert_eq!(fs::read_to_string(&input_path).unwrap(), original_input);
        assert_caps_lock_managed_block(&hyprland_lua);
    }

    #[test]
    fn existing_user_input_require_is_left_outside_the_managed_block() {
        let (_directory, path) = config_dir();
        fs::write(
            &path,
            concat!(
                "require(\"hypr.input\")\n",
                "o.bind(\"SUPER, Return\", \"Terminal\", \"kitty\")\n"
            ),
        )
        .unwrap();
        let mut hyprland = FakeHyprland {
            reload_succeeds: true,
            verification_succeeds: true,
        };

        install_trigger_binding(&path, &LocalBindingFileSystem, &mut hyprland, true).unwrap();
        let updated = fs::read_to_string(&path).unwrap();

        assert!(updated.contains("require(\"hypr.input\")"));
        assert!(!managed_block(&updated).contains("require("));
        assert_caps_lock_managed_block(&updated);
        assert!(!path.with_file_name("input.lua").exists());
    }

    #[test]
    fn already_installed_caps_lock_drops_managed_input_require() {
        let (_directory, path) = config_dir();
        fs::write(
            &path,
            concat!(
                "-- BEGIN VOISU MANAGED TRIGGER\n",
                "require(\"hypr.input\")\n",
                "o.bind(\"code:66\", \"Voisu dictation\", \"voisu toggle\")\n",
                "-- END VOISU MANAGED TRIGGER\n"
            ),
        )
        .unwrap();
        let mut hyprland = FakeHyprland {
            reload_succeeds: true,
            verification_succeeds: true,
        };

        let report =
            install_trigger_binding(&path, &LocalBindingFileSystem, &mut hyprland, true).unwrap();
        let updated = fs::read_to_string(&path).unwrap();

        assert_eq!(report.key, CAPS_LOCK);
        assert!(report.changed);
        assert_caps_lock_managed_block(&updated);
        assert!(!updated.contains("require("));
        assert!(!path.with_file_name("input.lua").exists());
    }

    const OMARCHY_PASTE_SOURCE: &str = r#"
local function send_shortcut_once(mods, key)
  return function()
    hl.dispatch(hl.dsp.send_key_state({ mods = mods, key = key, state = "down" }))
    hl.timer(function()
      hl.dispatch(hl.dsp.send_key_state({ mods = mods, key = key, state = "up" }))
    end, { timeout = 50, type = "oneshot" })
  end
end
local function active_window_is_terminal()
  return true
end
local function universal_clipboard_shortcut(default_mods, default_key, terminal_mods, terminal_key)
  return function()
    if active_window_is_terminal() then
      send_shortcut_once(terminal_mods, terminal_key)()
    else
      send_shortcut_once(default_mods, default_key)()
    end
  end
end
o.bind("SUPER + V", "Universal paste", universal_clipboard_shortcut("CTRL", "V", "SHIFT", "Insert"))
"#;

    #[test]
    fn omarchy_universal_paste_requires_live_dynamic_binding_and_keeps_both_paths() {
        let live = serde_json::json!([{
            "key": "V",
            "modmask": 64,
            "description": "Universal paste",
            "dispatcher": "__lua",
            "arg": "91"
        }]);

        let action = discover_paste_action(&[OMARCHY_PASTE_SOURCE], &live)
            .expect("known Omarchy helper should be verified");
        assert_eq!(action.shortcut.binding, "SUPER + V");
        assert_eq!(
            action.behavior,
            PasteBehavior::OmarchyUniversal {
                normal: PasteShortcut {
                    binding: "CTRL + V".to_owned()
                },
                terminal: PasteShortcut {
                    binding: "SHIFT + Insert".to_owned()
                },
            }
        );
    }

    #[test]
    fn a_different_literal_paste_binding_is_verified_from_live_hyprland() {
        let source = r#"o.bind("CTRL + SHIFT + P", "Paste transcript", "hyprctl dispatch sendshortcut CTRL V")"#;
        let live = serde_json::json!([{
            "key": "P",
            "modmask": 5,
            "description": "Paste transcript",
            "dispatcher": "exec",
            "arg": "hyprctl dispatch sendshortcut CTRL V"
        }]);

        let action = discover_paste_action(&[source], &live).expect("literal binding is verified");
        assert_eq!(action.shortcut.binding, "CTRL + SHIFT + P");
        assert_eq!(action.behavior, PasteBehavior::Simple);
    }

    #[test]
    fn a_literal_paste_binding_with_a_different_live_command_fails_closed() {
        let source = r#"o.bind("CTRL + SHIFT + P", "Paste transcript", "safe-paste")"#;
        let live = serde_json::json!([{
            "key": "P",
            "modmask": 5,
            "description": "Paste transcript",
            "dispatcher": "exec",
            "arg": "different-command"
        }]);

        assert!(discover_paste_action(&[source], &live).is_none());
    }

    #[test]
    fn unknown_dynamic_paste_function_fails_closed() {
        let source = r#"
local function my_paste()
  os.execute("dangerous-command")
end
o.bind("SUPER + P", "Paste transcript", my_paste)
"#;
        let live = serde_json::json!([{
            "key": "P",
            "modmask": 64,
            "description": "Paste transcript",
            "dispatcher": "__lua",
            "arg": "92"
        }]);

        assert!(discover_paste_action(&[source], &live).is_none());
    }
}
