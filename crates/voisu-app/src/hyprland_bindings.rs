use std::collections::HashSet;
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
    pub live_binding_identity: String,
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

const PREFERRED_TRIGGER_KEYS: [TriggerKey; 2] = [CAPS_LOCK, RIGHT_ALT];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LuaBinding {
    pub key: String,
    pub description: String,
    pub command: String,
    dispatcher: Option<String>,
    dispatcher_arguments: Option<Vec<String>>,
    managed: bool,
}

impl LuaBinding {
    pub fn is_standalone(&self) -> bool {
        PREFERRED_TRIGGER_KEYS
            .iter()
            .any(|candidate| self.is_standalone_for(*candidate))
    }

    fn is_standalone_for(&self, candidate: TriggerKey) -> bool {
        let key = self.key.trim();
        if key.eq_ignore_ascii_case(candidate.code) {
            return true;
        }
        match candidate.code {
            "code:66" => {
                key.eq_ignore_ascii_case("Caps_Lock") || key.eq_ignore_ascii_case("CapsLock")
            }
            "code:108" => key.eq_ignore_ascii_case("Alt_R"),
            "code:64" => key.eq_ignore_ascii_case("Alt_L"),
            _ => false,
        }
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
    Unparseable { detail: String },
}

/// Occupancy used by the Hyprland Trigger Key prompt. Bindings come from the
/// same import walk plus sibling `bindings.lua` that install uses; `compose:caps`
/// comes from the last `kb_options` in execution order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TriggerOccupancy {
    pub managed_key: Option<TriggerKey>,
    pub unmanaged_caps_lock: Option<(String, String)>,
    pub compose_caps: bool,
}

pub fn inspect_trigger_occupancy(
    path: &Path,
    source: &str,
    files: &dyn BindingFileSystem,
) -> Result<TriggerOccupancy, String> {
    let bindings = load_lua_bindings_with_occupancy(path, source, files)?;
    let mut options = collect_reachable_kb_options(path, source, files)?;
    if options.is_empty() {
        let input_path = path.with_file_name("input.lua");
        if let Some(input) = files.read_to_string(&input_path)? {
            options.extend(kb_options_values(&input));
        }
    }
    Ok(occupancy_from_bindings(
        &bindings,
        options.last().map(String::as_str),
    ))
}

fn occupancy_from_bindings(
    bindings: &[LuaBinding],
    last_kb_options: Option<&str>,
) -> TriggerOccupancy {
    let managed_key = PREFERRED_TRIGGER_KEYS.into_iter().find(|&candidate| {
        let managed = bindings
            .iter()
            .any(|binding| binding.managed && binding.is_standalone_for(candidate));
        let unmanaged = bindings
            .iter()
            .any(|binding| !binding.managed && binding.is_standalone_for(candidate));
        managed && !unmanaged
    });
    let unmanaged_caps_lock = bindings.iter().find_map(|binding| {
        (!binding.managed && binding.is_standalone_for(CAPS_LOCK))
            .then(|| (binding.description.clone(), binding.command.clone()))
    });
    TriggerOccupancy {
        managed_key,
        unmanaged_caps_lock,
        compose_caps: last_kb_options.is_some_and(kb_options_contains_compose_caps),
    }
}

fn kb_options_contains_compose_caps(value: &str) -> bool {
    value
        .split(',')
        .map(str::trim)
        .any(|part| part == "compose:caps")
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
    Unparseable {
        detail: String,
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
            Self::Unparseable { detail } => write!(
                formatter,
                "cannot safely inspect the Hyprland Lua bindings: {detail}. Recovery: fix the binding expression and rerun `{RECOVERY_COMMAND}`."
            ),
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
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::IsADirectory
                ) =>
            {
                Ok(None)
            }
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
        let payload = crate::system::run_hyprctl_binds_json()?;
        serde_json::from_slice::<Value>(&payload)
            .map(|value| hyprland_binding_is_installed(&value, key, command))
            .map_err(|error| format!("invalid `hyprctl binds -j` response: {error}"))
    }

    fn live_bindings(&mut self) -> Result<Value, String> {
        let payload = crate::system::run_hyprctl_binds_json()?;
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

fn live_binding_identity(binding: &Value, dispatcher: &str) -> Option<String> {
    let argument = binding_argument(binding)?.trim();
    match dispatcher {
        "__lua" => argument.parse::<u64>().ok().map(|_| argument.to_owned()),
        "exec" => Some(argument.to_owned()),
        _ => None,
    }
}

pub fn parse_lua_bindings(source: &str) -> Result<Vec<LuaBinding>, String> {
    let mut bindings = Vec::new();
    for (chunk, managed) in split_lua_chunks(source)? {
        bindings.extend(parse_lua_chunk(&chunk, managed)?);
    }
    Ok(bindings)
}

fn split_lua_chunks(source: &str) -> Result<Vec<(String, bool)>, String> {
    let mut chunks = Vec::new();
    let mut chunk = String::new();
    let mut managed = false;
    let mut state = LuaSourceState::default();

    for line in source.split_inclusive('\n') {
        match state.marker_for_line(line) {
            Some(LuaMarker::Begin) if managed => {
                return Err("nested Voisu managed trigger begin marker".to_owned());
            }
            Some(LuaMarker::Begin) => {
                chunks.push((std::mem::take(&mut chunk), false));
                managed = true;
            }
            Some(LuaMarker::End) if !managed => {
                return Err(
                    "Voisu managed trigger end marker has no matching begin marker".to_owned(),
                );
            }
            Some(LuaMarker::End) => {
                chunks.push((std::mem::take(&mut chunk), true));
                managed = false;
            }
            None => chunk.push_str(line),
        }
        state.consume(line);
    }

    if managed {
        return Err("Voisu managed trigger begin marker has no matching end marker".to_owned());
    }
    chunks.push((chunk, false));
    Ok(chunks)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LuaMarker {
    Begin,
    End,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
enum LuaSourceMode {
    #[default]
    Code,
    LineComment,
    Quoted(char),
    LongBracket(String),
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct LuaSourceState {
    mode: LuaSourceMode,
}

impl LuaSourceState {
    fn marker_for_line(&self, line: &str) -> Option<LuaMarker> {
        if self.mode != LuaSourceMode::Code {
            return None;
        }
        match line.trim() {
            BEGIN_MARKER => Some(LuaMarker::Begin),
            END_MARKER => Some(LuaMarker::End),
            _ => None,
        }
    }

    fn consume(&mut self, line: &str) {
        let chars: Vec<char> = line.chars().collect();
        let mut index = 0;
        while index < chars.len() {
            match &self.mode {
                LuaSourceMode::Code => {
                    if chars[index] == '-' && chars.get(index + 1) == Some(&'-') {
                        index += 2;
                        if let Some((closing, consumed)) = long_bracket_open(&chars, index) {
                            self.mode = LuaSourceMode::LongBracket(closing);
                            index = consumed;
                        } else {
                            self.mode = LuaSourceMode::LineComment;
                        }
                    } else if matches!(chars[index], '\'' | '"') {
                        self.mode = LuaSourceMode::Quoted(chars[index]);
                        index += 1;
                    } else if let Some((closing, consumed)) = long_bracket_open(&chars, index) {
                        self.mode = LuaSourceMode::LongBracket(closing);
                        index = consumed;
                    } else {
                        index += 1;
                    }
                }
                LuaSourceMode::LineComment => {
                    if chars[index] == '\n' {
                        self.mode = LuaSourceMode::Code;
                    }
                    index += 1;
                }
                LuaSourceMode::Quoted(quote) => {
                    if chars[index] == '\\' {
                        index = (index + 2).min(chars.len());
                    } else {
                        if chars[index] == *quote {
                            self.mode = LuaSourceMode::Code;
                        }
                        index += 1;
                    }
                }
                LuaSourceMode::LongBracket(closing) => {
                    if chars[index..].starts_with(&closing.chars().collect::<Vec<_>>()) {
                        index += closing.chars().count();
                        self.mode = LuaSourceMode::Code;
                    } else {
                        index += 1;
                    }
                }
            }
        }
        if self.mode == LuaSourceMode::LineComment {
            self.mode = LuaSourceMode::Code;
        }
    }
}

fn long_bracket_open(chars: &[char], start: usize) -> Option<(String, usize)> {
    if chars.get(start) != Some(&'[') {
        return None;
    }
    let mut index = start + 1;
    while chars.get(index) == Some(&'=') {
        index += 1;
    }
    if chars.get(index) != Some(&'[') {
        return None;
    }
    let equals = index - start - 1;
    Some((format!("]{}]", "=".repeat(equals)), index + 1))
}

/// Finds the first paste binding that is proven by both the active Lua source
/// and the compositor's live binding table. The source is deliberately
/// conservative: string-literal third arguments are accepted only when their
/// description identifies a paste action, and dynamic Lua functions are
/// accepted only for the exact Omarchy universal-paste helper shape.
pub fn discover_paste_action(
    sources: &[&str],
    live_bindings: &Value,
) -> Option<VerifiedPasteAction> {
    let bindings = sources
        .iter()
        .flat_map(|source| {
            parse_lua_bindings(source)
                .ok()
                .into_iter()
                .flatten()
                .map(|binding| (binding, (*source).to_owned()))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let live = live_bindings.as_array()?;

    for binding in live {
        let Some(dispatcher) = binding.get("dispatcher").and_then(Value::as_str) else {
            continue;
        };
        let Some(live_binding_identity) = live_binding_identity(binding, dispatcher) else {
            continue;
        };
        let Some(live_description) = binding.get("description").and_then(Value::as_str) else {
            continue;
        };
        // One live row can share a description with several source binds. Try
        // each match so an earlier unverified candidate cannot hide a later
        // helper or string command.
        for (candidate, source_text) in &bindings {
            if candidate.description != live_description
                || !live_binding_matches_source(binding, candidate)
            {
                continue;
            }

            if dispatcher == "__lua" && is_omarchy_universal_paste(candidate, source_text) {
                return Some(VerifiedPasteAction {
                    shortcut: PasteShortcut {
                        binding: candidate.key.trim().to_owned(),
                    },
                    description: candidate.description.clone(),
                    live_binding_identity,
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

            // String o.bind commands are live __lua on current Hyprland, not
            // exec. The command stays with Hyprland; Voisu only emits the chord.
            // Identifier and function third arguments stay unverified except
            // the Omarchy helper above.
            if matches!(dispatcher, "exec" | "__lua")
                && is_string_literal_command(candidate)
                && is_paste_description(&candidate.description)
            {
                return Some(VerifiedPasteAction {
                    shortcut: PasteShortcut {
                        binding: candidate.key.trim().to_owned(),
                    },
                    description: candidate.description.clone(),
                    live_binding_identity,
                    behavior: PasteBehavior::Simple,
                });
            }
        }
    }
    None
}

/// Reads the active current-Lua root and every Lua file reachable from it
/// through the same import walker used for trigger-binding inspection. The
/// known Omarchy clipboard helper is added only when the active root refers
/// to Omarchy; sibling files that are not imported are ignored. Setup can
/// use this with its injected [`BindingFileSystem`] seam; production
/// discovery never scans arbitrary files or evaluates Lua.
pub fn discover_paste_action_from_sources(
    root: &Path,
    files: &dyn BindingFileSystem,
    hyprland: &mut dyn HyprlandController,
) -> Result<Option<VerifiedPasteAction>, String> {
    let root_source = files
        .read_to_string(root)?
        .ok_or_else(|| format!("active Hyprland Lua source is missing: {}", root.display()))?;
    let (mut source_storage, mut visited) =
        collect_reachable_lua_sources(root, &root_source, files)?;

    // Current Omarchy imports the clipboard bindings through its default
    // module tree rather than copying the helper into the user's file. Only
    // add the known file when the active source actually refers to Omarchy;
    // this keeps unrelated or inactive Lua files out of the decision.
    let root_mentions_omarchy = root_source.contains("omarchy")
        || std::env::var("XDG_CURRENT_DESKTOP")
            .is_ok_and(|desktop| desktop_has_label(&desktop, "omarchy"));
    if root_mentions_omarchy {
        for path in omarchy_clipboard_source_candidates(root) {
            let path = normalize_path(&path);
            if !visited.insert(path.clone()) {
                continue;
            }
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
    discover_paste_action_from_sources(&root, &files, &mut hyprland)
        .ok()
        .flatten()
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
    let dispatcher = live.get("dispatcher").and_then(Value::as_str);
    let live_key = live.get("key").and_then(Value::as_str).unwrap_or_default();
    // Current Hyprland reports Lua binds with an empty key and often
    // modmask 0. The physical chord is recovered from the matching source
    // after description, dispatcher, and helper-body checks succeed.
    let opaque_lua_key = dispatcher == Some("__lua") && live_key.trim().is_empty();
    if !opaque_lua_key {
        let live_modmask = live
            .get("modmask")
            .and_then(Value::as_u64)
            .unwrap_or(u64::MAX);
        if source_modmask != live_modmask || normalize_key(live_key) != normalize_key(&source_key) {
            return false;
        }
    }
    match dispatcher {
        Some("exec") => {
            is_string_literal_command(source)
                && binding_argument(live)
                    .map(str::trim)
                    .is_some_and(|argument| argument == source.command.trim())
        }
        Some("__lua") => {
            live_binding_identity(live, "__lua").is_some()
                && (is_string_literal_command(source)
                    || (source.command == "<Lua dispatcher>"
                        && source.dispatcher.as_deref().is_some()))
        }
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
            // Caps is a Hyprland modmask bit, but paste emission has no Caps
            // keycode. Treat it as unverified rather than discovering a chord
            // that later fails with "unknown modifier".
            "CAPS" | "CAPSLOCK" => return None,
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

fn desktop_has_label(desktop: &str, wanted: &str) -> bool {
    desktop
        .split([':', ';'])
        .map(str::trim)
        .any(|label| label.eq_ignore_ascii_case(wanted))
}

fn is_paste_description(description: &str) -> bool {
    let description = description.to_ascii_lowercase();
    description.contains("paste")
        || (description.contains("clipboard") && description.contains("insert"))
}

fn is_string_literal_command(binding: &LuaBinding) -> bool {
    binding.command != "<Lua dispatcher>" && binding.dispatcher.is_none()
}

fn is_omarchy_universal_paste(binding: &LuaBinding, source: &str) -> bool {
    if binding.command != "<Lua dispatcher>"
        || binding.dispatcher.as_deref() != Some("universal_clipboard_shortcut")
        || binding.description != "Universal paste"
        || !binding.key.contains('+')
        || !binding
            .dispatcher_arguments
            .as_ref()
            .is_some_and(|arguments| {
                arguments
                    .iter()
                    .map(String::as_str)
                    .eq(["CTRL", "V", "SHIFT", "Insert"])
            })
    {
        return false;
    }
    let Ok(tokens) = lex_lua(source) else {
        return false;
    };
    function_body_matches(
        &tokens,
        "universal_clipboard_shortcut",
        &[
            "default_mods",
            "default_key",
            "terminal_mods",
            "terminal_key",
        ],
        r#"
return function()
  if active_window_is_terminal() then
    send_shortcut_once(terminal_mods, terminal_key)()
  else
    send_shortcut_once(default_mods, default_key)()
  end
end
"#,
    ) && function_body_matches(
        &tokens,
        "send_shortcut_once",
        &["mods", "key"],
        r#"
return function()
  hl.dispatch(hl.dsp.send_key_state({ mods = mods, key = key, state = "down" }))
  hl.timer(function()
    hl.dispatch(hl.dsp.send_key_state({ mods = mods, key = key, state = "up" }))
  end, { timeout = 50, type = "oneshot" })
end
"#,
    ) && function_body_matches(
        &tokens,
        "active_window_is_terminal",
        &[],
        r#"
local window = hl.get_active_window()
if not window then
  return false
end
for _, tag in ipairs(window.tags or {}) do
  if tag:gsub("%*$", "") == "terminal" then
    return true
  end
end
return false
"#,
    )
}

fn function_body_matches(
    tokens: &[LuaToken],
    name: &str,
    expected_parameters: &[&str],
    expected_body: &str,
) -> bool {
    let Some(body) = named_function_body(tokens, name, expected_parameters) else {
        return false;
    };
    lex_lua(expected_body).is_ok_and(|expected| expected.as_slice() == body)
}

fn named_function_body<'a>(
    tokens: &'a [LuaToken],
    name: &str,
    expected_parameters: &[&str],
) -> Option<&'a [LuaToken]> {
    for function_index in 0..tokens.len().saturating_sub(3) {
        if !matches!(
            (&tokens[function_index], &tokens[function_index + 1]),
            (LuaToken::Identifier(keyword), LuaToken::Identifier(function_name))
                if keyword == "function" && function_name == name
        ) {
            continue;
        }
        if !matches!(tokens.get(function_index + 2), Some(LuaToken::Symbol('('))) {
            continue;
        }
        let (body_start, parameters) = parse_function_parameters(tokens, function_index + 2)?;
        if parameters != expected_parameters {
            continue;
        }
        let body_end = matching_function_end(tokens, body_start)?;
        return Some(&tokens[body_start..body_end]);
    }
    None
}

fn parse_function_parameters(tokens: &[LuaToken], open_index: usize) -> Option<(usize, Vec<&str>)> {
    let mut index = open_index + 1;
    let mut parameters = Vec::new();
    loop {
        match tokens.get(index)? {
            LuaToken::Identifier(parameter) => {
                parameters.push(parameter.as_str());
                index += 1;
                if !matches!(tokens.get(index), Some(LuaToken::Symbol(','))) {
                    if !matches!(tokens.get(index), Some(LuaToken::Symbol(')'))) {
                        return None;
                    }
                    return Some((index + 1, parameters));
                }
                index += 1;
            }
            LuaToken::Symbol(')') => return Some((index + 1, parameters)),
            _ => return None,
        }
    }
}

fn matching_function_end(tokens: &[LuaToken], body_start: usize) -> Option<usize> {
    let mut blocks = vec![LuaBlock::Function];
    let mut awaiting_loop_do = false;
    for (index, token) in tokens.iter().enumerate().skip(body_start) {
        let LuaToken::Identifier(keyword) = token else {
            continue;
        };
        match keyword.as_str() {
            "function" => {
                blocks.push(LuaBlock::Function);
                awaiting_loop_do = false;
            }
            "if" => {
                blocks.push(LuaBlock::Conditional);
                awaiting_loop_do = false;
            }
            "for" | "while" => {
                blocks.push(LuaBlock::Loop);
                awaiting_loop_do = true;
            }
            "do" if awaiting_loop_do => awaiting_loop_do = false,
            "do" => {
                blocks.push(LuaBlock::Do);
                awaiting_loop_do = false;
            }
            "repeat" => {
                blocks.push(LuaBlock::Repeat);
                awaiting_loop_do = false;
            }
            "until" if matches!(blocks.last(), Some(LuaBlock::Repeat)) => {
                blocks.pop();
            }
            "until" => return None,
            "end" => {
                blocks.pop()?;
                if blocks.is_empty() {
                    return Some(index);
                }
                awaiting_loop_do = false;
            }
            _ => {}
        }
    }
    None
}

enum LuaBlock {
    Function,
    Conditional,
    Loop,
    Do,
    Repeat,
}

pub fn plan_trigger_binding(source: &str, prefer_caps_lock: bool) -> TriggerBindingPlan {
    plan_trigger_bindings(&[source], prefer_caps_lock)
}

/// Plans the Trigger Key from every occupancy source. Sibling files such as
/// `bindings.lua` must be included so an exact unmanaged Caps Lock bind there
/// is not installed again in `hyprland.lua`.
pub fn plan_trigger_bindings(sources: &[&str], prefer_caps_lock: bool) -> TriggerBindingPlan {
    let mut bindings = Vec::new();
    for source in sources {
        match parse_lua_bindings(source) {
            Ok(parsed) => bindings.extend(parsed),
            Err(detail) => return TriggerBindingPlan::Unparseable { detail },
        }
    }
    plan_trigger_binding_from_bindings(&bindings, prefer_caps_lock)
}

struct PlannedTriggerBinding {
    bindings: Vec<LuaBinding>,
    plan: TriggerBindingPlan,
}

fn plan_trigger_binding_with_imports(
    path: &Path,
    source: &str,
    files: &dyn BindingFileSystem,
    prefer_caps_lock: bool,
) -> PlannedTriggerBinding {
    match load_lua_bindings_with_occupancy(path, source, files) {
        Ok(bindings) => PlannedTriggerBinding {
            plan: plan_trigger_binding_from_bindings(&bindings, prefer_caps_lock),
            bindings,
        },
        Err(detail) => PlannedTriggerBinding {
            bindings: Vec::new(),
            plan: TriggerBindingPlan::Unparseable { detail },
        },
    }
}

fn plan_trigger_binding_from_bindings(
    bindings: &[LuaBinding],
    prefer_caps_lock: bool,
) -> TriggerBindingPlan {
    if let Some(binding) = bindings.iter().find(|binding| {
        binding.managed
            && binding.is_standalone()
            && (binding.description != VOISU_TRIGGER_DESCRIPTION
                || binding.command != VOISU_TOGGLE_COMMAND)
    }) {
        let candidate = PREFERRED_TRIGGER_KEYS
            .into_iter()
            .find(|candidate| binding.is_standalone_for(*candidate))
            .expect("standalone managed binding has a preferred candidate");
        return TriggerBindingPlan::Unparseable {
            detail: format!(
                "the managed {} binding does not match Voisu's expected description or command",
                candidate.label
            ),
        };
    }

    let unmanaged = |candidate: TriggerKey| {
        bindings
            .iter()
            .find(|binding| !binding.managed && binding.is_standalone_for(candidate))
    };

    for candidate in PREFERRED_TRIGGER_KEYS {
        let managed = bindings
            .iter()
            .any(|binding| binding.managed && binding.is_standalone_for(candidate));
        if managed && unmanaged(candidate).is_none() {
            return TriggerBindingPlan::AlreadyInstalled { key: candidate };
        }
    }

    if prefer_caps_lock {
        match unmanaged(CAPS_LOCK) {
            None => return TriggerBindingPlan::Install { key: CAPS_LOCK },
            Some(binding) if binding.command == VOISU_TOGGLE_COMMAND => {
                return TriggerBindingPlan::AlreadyInstalled { key: CAPS_LOCK };
            }
            Some(_) => {}
        }
    }
    if unmanaged(RIGHT_ALT).is_none() {
        return TriggerBindingPlan::Install { key: RIGHT_ALT };
    }

    let mut conflicts = Vec::new();
    for candidate in PREFERRED_TRIGGER_KEYS {
        if let Some(binding) = unmanaged(candidate) {
            conflicts.push(BindingConflict {
                candidate,
                description: binding.description.clone(),
                command: binding.command.clone(),
            });
        }
    }
    if !conflicts.is_empty() {
        return TriggerBindingPlan::Conflicts { conflicts };
    }

    unreachable!("a conflict-free binding plan always finds an available candidate")
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LuaImport {
    loader: &'static str,
    path: String,
}

fn load_lua_bindings_with_occupancy(
    root_path: &Path,
    root_source: &str,
    files: &dyn BindingFileSystem,
) -> Result<Vec<LuaBinding>, String> {
    let mut visited = HashSet::new();
    let mut bindings = Vec::new();
    for_each_lua_source(
        root_path,
        root_source,
        files,
        &mut visited,
        &mut |_path, source| {
            bindings.extend(parse_lua_bindings(source)?);
            Ok(())
        },
    )?;
    let sibling_path = normalize_path(&root_path.with_file_name("bindings.lua"));
    if !visited.contains(&sibling_path) {
        if let Some(sibling) = files.read_to_string(&sibling_path)? {
            bindings.extend(parse_lua_bindings(&sibling)?);
        }
    }
    Ok(bindings)
}

fn collect_reachable_lua_sources(
    root_path: &Path,
    root_source: &str,
    files: &dyn BindingFileSystem,
) -> Result<(Vec<String>, HashSet<PathBuf>), String> {
    let mut visited = HashSet::new();
    let mut sources = Vec::new();
    for_each_lua_source(
        root_path,
        root_source,
        files,
        &mut visited,
        &mut |_path, source| {
            sources.push(source.to_owned());
            Ok(())
        },
    )?;
    Ok((sources, visited))
}

fn collect_reachable_kb_options(
    root_path: &Path,
    root_source: &str,
    files: &dyn BindingFileSystem,
) -> Result<Vec<String>, String> {
    let mut active = HashSet::new();
    let mut required = HashSet::new();
    let mut options = Vec::new();
    collect_kb_options_in_execution_order(
        root_path,
        root_source,
        files,
        &mut active,
        &mut required,
        &mut options,
    )?;
    Ok(options)
}

fn collect_kb_options_in_execution_order(
    path: &Path,
    source: &str,
    files: &dyn BindingFileSystem,
    active: &mut HashSet<PathBuf>,
    required: &mut HashSet<PathBuf>,
    options: &mut Vec<String>,
) -> Result<(), String> {
    let path = normalize_path(path);
    if !active.insert(path.clone()) {
        return Ok(());
    }

    let tokens = lex_lua(source)?;
    let mut index = 0;
    while index < tokens.len() {
        if let (
            Some(LuaToken::Identifier(name)),
            Some(LuaToken::Symbol('=')),
            Some(LuaToken::String(value)),
        ) = (tokens.get(index), tokens.get(index + 1), tokens.get(index + 2))
            && name == "kb_options"
        {
            options.push(value.clone());
            index += 3;
            continue;
        }

        let Some((import, consumed)) = parse_lua_import_at(&tokens, index)? else {
            index += 1;
            continue;
        };
        let Some(imported_path) = resolve_lua_import(&path, &import, files)? else {
            index += consumed;
            continue;
        };
        if import.loader == "require" && !required.insert(imported_path.clone()) {
            index += consumed;
            continue;
        }
        let imported_source = files
            .read_to_string(&imported_path)
            .map_err(|detail| {
                format!(
                    "cannot read imported Lua file {}: {detail}",
                    imported_path.display()
                )
            })?
            .ok_or_else(|| {
                format!(
                    "imported Lua file {} does not exist",
                    imported_path.display()
                )
            })?;
        collect_kb_options_in_execution_order(
            &imported_path,
            &imported_source,
            files,
            active,
            required,
            options,
        )?;
        index += consumed;
    }
    active.remove(&path);
    Ok(())
}

fn for_each_lua_source(
    path: &Path,
    source: &str,
    files: &dyn BindingFileSystem,
    visited: &mut HashSet<PathBuf>,
    visit: &mut dyn FnMut(&Path, &str) -> Result<(), String>,
) -> Result<(), String> {
    let path = normalize_path(path);
    if !visited.insert(path.clone()) {
        return Ok(());
    }

    visit(&path, source)?;
    for import in parse_lua_imports(source)? {
        let Some(imported_path) = resolve_lua_import(&path, &import, files)? else {
            continue;
        };
        let imported_source = files
            .read_to_string(&imported_path)
            .map_err(|detail| {
                format!(
                    "cannot read imported Lua file {}: {detail}",
                    imported_path.display()
                )
            })?
            .ok_or_else(|| {
                format!(
                    "imported Lua file {} does not exist",
                    imported_path.display()
                )
            })?;
        for_each_lua_source(&imported_path, &imported_source, files, visited, visit)?;
    }
    Ok(())
}

fn parse_lua_import_at(
    tokens: &[LuaToken],
    index: usize,
) -> Result<Option<(LuaImport, usize)>, String> {
    let Some(LuaToken::Identifier(loader)) = tokens.get(index) else {
        return Ok(None);
    };
    if !matches!(
        loader.as_str(),
        "dofile" | "loadfile" | "require" | "source"
    ) {
        return Ok(None);
    }
    let (path, consumed) = match tokens.get(index + 1) {
        Some(LuaToken::Symbol('(')) => match tokens.get(index + 2) {
            Some(LuaToken::String(path)) => (path, 3),
            _ if loader.as_str() == "require" => {
                return Err(format!(
                    "the Lua import {loader} must use a string literal path"
                ));
            }
            // Non-literal dofile/loadfile/source cannot be resolved; skip this
            // token so later literal requires are still walked.
            _ => return Ok(None),
        },
        Some(LuaToken::String(path)) => (path, 2),
        _ => return Ok(None),
    };
    if path.is_empty() {
        return Err(format!("the Lua import {loader} has an empty path"));
    }
    Ok(Some((
        LuaImport {
            loader: match loader.as_str() {
                "dofile" => "dofile",
                "loadfile" => "loadfile",
                "require" => "require",
                "source" => "source",
                _ => unreachable!(),
            },
            path: path.clone(),
        },
        consumed,
    )))
}

fn parse_lua_imports(source: &str) -> Result<Vec<LuaImport>, String> {
    let tokens = lex_lua(source)?;
    let mut imports = Vec::new();
    let mut index = 0;
    while index < tokens.len() {
        let Some((import, consumed)) = parse_lua_import_at(&tokens, index)? else {
            index += 1;
            continue;
        };
        imports.push(import);
        index += consumed;
    }
    Ok(imports)
}

fn resolve_lua_import(
    importing_path: &Path,
    import: &LuaImport,
    files: &dyn BindingFileSystem,
) -> Result<Option<PathBuf>, String> {
    let import_path = Path::new(&import.path);
    let parent = importing_path.parent().unwrap_or_else(|| Path::new("."));
    let mut candidates = vec![if import_path.is_absolute() {
        import_path.to_owned()
    } else {
        parent.join(import_path)
    }];

    // `Path::extension` treats `hypr.input` as a file with extension `input`.
    // Lua require names use dots as module separators, so convert unless the
    // argument already names a `.lua` file.
    if import.loader == "require" && !import.path.ends_with(".lua") {
        let module_path = import.path.replace('.', "/");
        candidates.push(parent.join(format!("{module_path}.lua")));
        candidates.push(parent.join(&module_path).join("init.lua"));
    }

    for candidate in &candidates {
        if files
            .read_to_string(candidate)
            .map_err(|detail| {
                format!(
                    "cannot inspect imported Lua file {}: {detail}",
                    candidate.display()
                )
            })?
            .is_some()
        {
            return Ok(Some(normalize_path(candidate)));
        }
    }

    if import.loader == "require" {
        Ok(None)
    } else {
        Err(format!(
            "Lua import {} refers to missing file {}",
            import.loader,
            candidates
                .first()
                .map_or_else(|| import.path.clone(), |path| path.display().to_string())
        ))
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            component => normalized.push(component.as_os_str()),
        }
    }
    normalized
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
    let backup = backup_path(path);
    let PlannedTriggerBinding { bindings, plan } =
        plan_trigger_binding_with_imports(path, source, files, prefer_caps_lock);
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
        TriggerBindingPlan::Conflicts { conflicts } => {
            return Err(TriggerBindingError::from_conflicts(conflicts));
        }
        TriggerBindingPlan::Unparseable { detail } => {
            return Err(TriggerBindingError::Unparseable { detail });
        }
        TriggerBindingPlan::AlreadyInstalled { key } => {
            let managed = bindings
                .iter()
                .any(|binding| binding.managed && binding.is_standalone_for(key));
            if !managed {
                verify_already_installed(hyprland, key, &backup)?;
                return Ok(TriggerBindingInstallReport {
                    key,
                    changed: false,
                    backup_path: backup,
                });
            }
            key
        }
        TriggerBindingPlan::Install { key } => key,
    };
    let ordered_kb_options = if key == CAPS_LOCK {
        match collect_reachable_kb_options(path, source, files) {
            Ok(options) => options,
            Err(detail) => return Err(TriggerBindingError::Unparseable { detail }),
        }
    } else {
        Vec::new()
    };

    let updated = match desired_hyprland_source(
        source,
        key,
        original_input.as_deref().unwrap_or(""),
        &ordered_kb_options,
    ) {
        Ok(updated) => updated,
        Err(detail) => return Err(TriggerBindingError::Unparseable { detail }),
    };
    if updated == source {
        verify_already_installed(hyprland, key, &backup)?;
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

    if let Err(detail) = hyprland.reload() {
        let restore_error = restore_original_and_reload(files, path, original.as_deref(), hyprland);
        return Err(TriggerBindingError::ReloadFailed {
            detail,
            backup_path: backup,
            restore_error,
        });
    }

    match hyprland.binding_is_installed(key.code, VOISU_TOGGLE_COMMAND) {
        Ok(true) => Ok(TriggerBindingInstallReport {
            key,
            changed: true,
            backup_path: backup,
        }),
        Ok(false) => {
            let restore_error =
                restore_original_and_reload(files, path, original.as_deref(), hyprland);
            Err(TriggerBindingError::VerificationFailed {
                detail: format!(
                    "Hyprland did not report {} ({}) running `{VOISU_TOGGLE_COMMAND}`",
                    key.label, key.code
                ),
                backup_path: backup,
                restore_error,
            })
        }
        Err(detail) => {
            let restore_error =
                restore_original_and_reload(files, path, original.as_deref(), hyprland);
            Err(TriggerBindingError::VerificationFailed {
                detail,
                backup_path: backup,
                restore_error,
            })
        }
    }
}

fn verify_already_installed(
    hyprland: &mut dyn HyprlandController,
    key: TriggerKey,
    backup: &Path,
) -> Result<(), TriggerBindingError> {
    if let Err(detail) = hyprland.reload() {
        return Err(TriggerBindingError::ReloadFailed {
            detail,
            backup_path: backup.to_owned(),
            restore_error: None,
        });
    }
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

fn restore_original_and_reload(
    files: &dyn BindingFileSystem,
    path: &Path,
    original: Option<&str>,
    hyprland: &mut dyn HyprlandController,
) -> Option<String> {
    if let Err(detail) = restore_original(files, path, original) {
        return Some(detail);
    }
    hyprland
        .reload()
        .err()
        .map(|detail| format!("reloading the restored configuration failed: {detail}"))
}

fn desired_hyprland_source(
    source: &str,
    key: TriggerKey,
    input_source: &str,
    ordered_kb_options: &[String],
) -> Result<String, String> {
    let kb_options = if key == CAPS_LOCK {
        // input.lua is a special sibling that may be loaded independently of
        // the active root. The remaining entries follow the root's execution
        // order, including each imported Lua file at its call site.
        let existing = ordered_kb_options
            .last()
            .cloned()
            .or_else(|| kb_options_values(input_source).into_iter().next_back());
        Some(merge_caps_lock_kb_options(existing))
    } else {
        None
    };
    append_managed_binding(source, key, kb_options.as_deref())
}

fn append_managed_binding(
    source: &str,
    key: TriggerKey,
    kb_options: Option<&str>,
) -> Result<String, String> {
    let mut content = remove_managed_blocks(source)?;
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
    Ok(content)
}

#[cfg(test)]
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

fn remove_managed_blocks(source: &str) -> Result<String, String> {
    Ok(split_lua_chunks(source)?
        .into_iter()
        .filter(|(_, managed)| !managed)
        .map(|(chunk, _)| chunk)
        .collect())
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum LuaToken {
    Identifier(String),
    String(String),
    Number(String),
    Symbol(char),
}

type ParsedBindArguments = (String, String, String, Option<String>, Option<Vec<String>>);

fn parse_lua_chunk(source: &str, managed: bool) -> Result<Vec<LuaBinding>, String> {
    let tokens = lex_lua(source)?;
    let mut bindings = Vec::new();
    let mut index = 0;
    while index + 3 < tokens.len() {
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
        match parse_bind_arguments(&tokens[index + 4..]) {
            Ok(Some((key, description, command, dispatcher, dispatcher_arguments))) => {
                bindings.push(LuaBinding {
                    key,
                    description,
                    command,
                    dispatcher,
                    dispatcher_arguments,
                    managed,
                });
                index += 4;
            }
            // Computed chords (`"SUPER + " .. key`) and Lua callbacks on
            // combination keys are not Caps Lock occupancy. Skip the call.
            Ok(None) => index = matching_parenthesis(&tokens, index + 3)?,
            Err(error) => match tokens.get(index + 4) {
                Some(LuaToken::String(key)) if !occupies_preferred_trigger_key(key) => {
                    index = matching_parenthesis(&tokens, index + 3)?;
                }
                _ => return Err(error),
            },
        }
    }
    Ok(bindings)
}

fn occupies_preferred_trigger_key(key: &str) -> bool {
    PREFERRED_TRIGGER_KEYS.iter().any(|candidate| {
        LuaBinding {
            key: key.to_owned(),
            description: String::new(),
            command: String::new(),
            dispatcher: None,
            dispatcher_arguments: None,
            managed: false,
        }
        .is_standalone_for(*candidate)
    })
}

fn parse_bind_arguments(tokens: &[LuaToken]) -> Result<Option<ParsedBindArguments>, String> {
    let mut index = 0;
    let read_string = |index: &mut usize, label: &str| -> Result<String, String> {
        let value = match tokens.get(*index) {
            Some(LuaToken::String(value)) => value.clone(),
            Some(_) => return Err(format!("the {label} must be a string literal")),
            None => return Err(format!("the {label} is missing")),
        };
        *index += 1;
        Ok(value)
    };
    let key = read_string(&mut index, "binding key")?;
    if !matches!(tokens.get(index), Some(LuaToken::Symbol(','))) {
        if occupies_preferred_trigger_key(&key) {
            return Err("the binding key must be followed by a comma".to_owned());
        }
        return Ok(None);
    }
    index += 1;
    let description = match tokens.get(index) {
        Some(LuaToken::String(value)) => {
            index += 1;
            value.clone()
        }
        Some(_) if !occupies_preferred_trigger_key(&key) => return Ok(None),
        Some(_) => return Err("the binding description must be a string literal".to_owned()),
        None => return Err("the binding description is missing".to_owned()),
    };
    if !matches!(tokens.get(index), Some(LuaToken::Symbol(','))) {
        if occupies_preferred_trigger_key(&key) {
            return Err("the binding description must be followed by a comma".to_owned());
        }
        return Ok(None);
    }
    index += 1;
    let (command, dispatcher, dispatcher_arguments) = match tokens.get(index) {
        Some(LuaToken::String(value)) => {
            index += 1;
            (value.clone(), None, None)
        }
        Some(LuaToken::Symbol(')')) => return Err("the binding command is missing".to_owned()),
        Some(LuaToken::Identifier(name)) => {
            let name = name.clone();
            index += 1;
            let arguments = if matches!(tokens.get(index), Some(LuaToken::Symbol('('))) {
                let (next, arguments) = parse_dispatcher_arguments(tokens, index)?;
                index = next;
                arguments
            } else {
                index = binding_expression_end(tokens, index)?;
                None
            };
            ("<Lua dispatcher>".to_owned(), Some(name), arguments)
        }
        Some(_) => {
            index = binding_expression_end(tokens, index)?;
            ("<Lua dispatcher>".to_owned(), None, None)
        }
        None => return Err("the binding command is missing".to_owned()),
    };
    if !matches!(tokens.get(index), Some(LuaToken::Symbol(')'))) {
        return Err("the binding command must be followed by a closing parenthesis".to_owned());
    }
    Ok(Some((
        key,
        description,
        command,
        dispatcher,
        dispatcher_arguments,
    )))
}

fn parse_dispatcher_arguments(
    tokens: &[LuaToken],
    open_index: usize,
) -> Result<(usize, Option<Vec<String>>), String> {
    let mut index = open_index + 1;
    let mut arguments = Vec::new();
    loop {
        match tokens.get(index) {
            Some(LuaToken::String(argument)) => {
                arguments.push(argument.clone());
                index += 1;
            }
            Some(LuaToken::Symbol(')')) => return Ok((index + 1, Some(arguments))),
            Some(_) => return Ok((matching_parenthesis(tokens, open_index)?, None)),
            None => return Err("the dispatcher call is missing a closing parenthesis".to_owned()),
        }
        match tokens.get(index) {
            Some(LuaToken::Symbol(',')) => index += 1,
            Some(LuaToken::Symbol(')')) => return Ok((index + 1, Some(arguments))),
            Some(_) => {
                return Err("the dispatcher arguments must be separated by commas".to_owned())
            }
            None => return Err("the dispatcher call is missing a closing parenthesis".to_owned()),
        }
    }
}

fn matching_parenthesis(tokens: &[LuaToken], open_index: usize) -> Result<usize, String> {
    let mut depth = 0;
    for (index, token) in tokens.iter().enumerate().skip(open_index) {
        match token {
            LuaToken::Symbol('(') => depth += 1,
            LuaToken::Symbol(')') => {
                depth -= 1;
                if depth == 0 {
                    return Ok(index + 1);
                }
            }
            _ => {}
        }
    }
    Err("the dispatcher call is missing a closing parenthesis".to_owned())
}

fn binding_expression_end(tokens: &[LuaToken], start_index: usize) -> Result<usize, String> {
    let mut depth = 0;
    for (index, token) in tokens.iter().enumerate().skip(start_index) {
        match token {
            LuaToken::Symbol('(') => depth += 1,
            LuaToken::Symbol(')') if depth == 0 => return Ok(index),
            LuaToken::Symbol(')') => depth -= 1,
            _ => {}
        }
    }
    Err("the binding command is missing a closing parenthesis".to_owned())
}

fn lex_lua(source: &str) -> Result<Vec<LuaToken>, String> {
    let mut chars = source.chars().peekable();
    let mut tokens = Vec::new();
    while let Some(character) = chars.next() {
        if character == '-' && chars.peek() == Some(&'-') {
            chars.next();
            if let Some(closing) = take_long_bracket_open(&mut chars) {
                skip_long_bracket(&mut chars, &closing)?;
                continue;
            }
            for character in chars.by_ref() {
                if character == '\n' {
                    break;
                }
            }
            continue;
        }
        if character == '"' || character == '\'' {
            let value = read_lua_string(&mut chars, character)?;
            tokens.push(LuaToken::String(value));
            continue;
        }
        if character == '[' {
            if let Some(closing) = take_long_bracket_open(&mut chars) {
                skip_long_bracket(&mut chars, &closing)?;
            }
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
        if character.is_ascii_digit() {
            let mut number = String::from(character);
            while let Some(next) = chars.peek().copied() {
                if next.is_ascii_digit() {
                    number.push(next);
                    chars.next();
                } else {
                    break;
                }
            }
            tokens.push(LuaToken::Number(number));
            continue;
        }
        if matches!(character, '.' | '(' | ')' | ',' | '{' | '}' | '=' | ':') {
            tokens.push(LuaToken::Symbol(character));
        }
    }
    Ok(tokens)
}

fn take_long_bracket_open(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Option<String> {
    let mut lookahead = chars.clone();
    let mut equals = 0;
    while lookahead.peek() == Some(&'=') {
        equals += 1;
        lookahead.next();
    }
    if lookahead.next() != Some('[') {
        return None;
    }
    *chars = lookahead;
    Some(format!("]{}]", "=".repeat(equals)))
}

fn skip_long_bracket(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    closing: &str,
) -> Result<(), String> {
    let closing: Vec<char> = closing.chars().collect();
    let mut tail = Vec::new();
    for character in chars.by_ref() {
        tail.push(character);
        if tail.ends_with(&closing) {
            return Ok(());
        }
        if tail.len() > closing.len() {
            tail.drain(..tail.len() - closing.len());
        }
    }
    Err("unterminated Lua long string or comment".to_owned())
}

fn read_lua_string(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    quote: char,
) -> Result<String, String> {
    let mut value = String::new();
    while let Some(character) = chars.next() {
        match character {
            '\\' => read_lua_escape(chars, &mut value)?,
            character if character == quote => return Ok(value),
            '\n' | '\r' => return Err("unterminated Lua string literal".to_owned()),
            character => value.push(character),
        }
    }
    Err("unterminated Lua string literal".to_owned())
}

fn read_lua_escape(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    value: &mut String,
) -> Result<(), String> {
    let Some(escaped) = chars.next() else {
        return Err("Lua string ends with an incomplete escape".to_owned());
    };
    match escaped {
        'a' => value.push('\u{7}'),
        'b' => value.push('\u{8}'),
        'f' => value.push('\u{c}'),
        'n' => value.push('\n'),
        'r' => value.push('\r'),
        't' => value.push('\t'),
        'v' => value.push('\u{b}'),
        '\\' | '"' | '\'' => value.push(escaped),
        '\n' => value.push('\n'),
        '\r' => {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            value.push('\n');
        }
        'z' => {
            while chars
                .peek()
                .is_some_and(|character| character.is_ascii_whitespace())
            {
                chars.next();
            }
        }
        'x' => {
            let high = chars.next().and_then(|character| character.to_digit(16));
            let low = chars.next().and_then(|character| character.to_digit(16));
            let Some(byte) = high.zip(low).map(|(high, low)| (high * 16 + low) as u8) else {
                return Err("Lua hexadecimal escape must contain two digits".to_owned());
            };
            value.push(byte as char);
        }
        'u' => {
            if chars.next() != Some('{') {
                return Err("Lua Unicode escape must use the form \\u{...}".to_owned());
            }
            let mut digits = String::new();
            let mut closed = false;
            for character in chars.by_ref() {
                if character == '}' {
                    closed = true;
                    break;
                }
                if !character.is_ascii_hexdigit() {
                    return Err("Lua Unicode escape contains a non-hex digit".to_owned());
                }
                digits.push(character);
            }
            let Some(codepoint) = closed
                .then(|| u32::from_str_radix(&digits, 16).ok())
                .flatten()
                .and_then(char::from_u32)
            else {
                return Err("Lua Unicode escape is invalid".to_owned());
            };
            value.push(codepoint);
        }
        character if character.is_ascii_digit() => {
            let mut digits = String::from(character);
            while digits.len() < 3 {
                let Some(next) = chars.peek().copied().filter(|next| next.is_ascii_digit()) else {
                    break;
                };
                digits.push(next);
                chars.next();
            }
            let byte = digits
                .parse::<u8>()
                .map_err(|_| "Lua decimal escape is outside the byte range".to_owned())?;
            value.push(byte as char);
        }
        other => {
            return Err(format!("unsupported Lua escape sequence \\\\{other}"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    struct FakeHyprland {
        reload_results: Vec<bool>,
        reload_calls: usize,
        verification_succeeds: bool,
    }

    impl FakeHyprland {
        fn new(reload_results: &[bool], verification_succeeds: bool) -> Self {
            Self {
                reload_results: reload_results.to_vec(),
                reload_calls: 0,
                verification_succeeds,
            }
        }
    }

    impl HyprlandController for FakeHyprland {
        fn reload(&mut self) -> Result<(), String> {
            let succeeds = self
                .reload_results
                .get(self.reload_calls)
                .copied()
                .unwrap_or(false);
            self.reload_calls += 1;
            succeeds
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
            last.split(',')
                .map(str::trim)
                .any(|part| part == BOTH_CAPSLOCK_CANCEL),
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

        let bindings = parse_lua_bindings(source).unwrap();

        assert_eq!(bindings.len(), 2);
        assert!(!bindings[0].is_standalone());
        assert_eq!(bindings[0].description, "Alt Tab");
        assert!(bindings[1].is_standalone());
        assert_eq!(bindings[1].key, "code:108");
        assert_eq!(bindings[1].command, "kitty");
    }

    #[test]
    fn keysym_aliases_occupy_the_same_physical_trigger_keys() {
        let source = r#"
o.bind("Caps_Lock", "Launch terminal", "kitty")
o.bind("ALT, Alt_R", "Modified right alt", "workspace next")
"#;

        assert_eq!(
            plan_trigger_binding(source, true),
            TriggerBindingPlan::Install { key: RIGHT_ALT }
        );

        let both = r#"
o.bind("caps_lock", "Launch terminal", "kitty")
o.bind("Alt_R", "Launcher", "launcher")
"#;
        let TriggerBindingPlan::Conflicts { conflicts } = plan_trigger_binding(both, true) else {
            panic!("Caps_Lock and Alt_R must occupy the preferred physical keys");
        };
        assert_eq!(conflicts.len(), 2);
        assert_eq!(conflicts[0].candidate, CAPS_LOCK);
        assert_eq!(conflicts[1].candidate, RIGHT_ALT);
    }

    #[test]
    fn left_alt_is_not_a_setup_candidate() {
        let source = r#"
o.bind("code:64", "Launch terminal", "kitty")
o.bind("ALT, code:108", "Modified right alt", "workspace next")
"#;

        assert_eq!(
            plan_trigger_binding(source, true),
            TriggerBindingPlan::Install { key: CAPS_LOCK }
        );
        assert!(!LuaBinding {
            key: "ALT, code:64".to_owned(),
            description: "Alt Tab".to_owned(),
            command: "workspace next".to_owned(),
            dispatcher: None,
            dispatcher_arguments: None,
            managed: false,
        }
        .is_standalone());
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
    fn managed_binding_does_not_hide_a_new_user_conflict() {
        let source = format!(
            "{BEGIN_MARKER}\no.bind(\"code:66\", \"Voisu dictation\", \"voisu toggle\")\n{END_MARKER}\no.bind(\"code:108\", \"Launcher\", \"launcher\")\n"
        );

        assert_eq!(
            plan_trigger_binding(&source, true),
            TriggerBindingPlan::AlreadyInstalled { key: CAPS_LOCK }
        );
    }

    #[test]
    fn same_key_user_binding_is_not_accepted_as_already_installed() {
        let source = format!(
            "{BEGIN_MARKER}\no.bind(\"code:66\", \"Voisu dictation\", \"voisu toggle\")\n{END_MARKER}\no.bind(\"code:66\", \"Launcher\", \"launcher\")\n"
        );

        assert_eq!(
            plan_trigger_binding(&source, true),
            TriggerBindingPlan::Install { key: RIGHT_ALT }
        );

        let both_keys_user_owned =
            format!("{source}o.bind(\"code:108\", \"Lock\", \"loginctl lock-session\")\n");
        let TriggerBindingPlan::Conflicts { conflicts } =
            plan_trigger_binding(&both_keys_user_owned, true)
        else {
            panic!("a same-key user binding must not hide conflicts behind AlreadyInstalled");
        };
        assert_eq!(conflicts.len(), 2);
        assert_eq!(conflicts[0].candidate, CAPS_LOCK);
        assert_eq!(conflicts[1].candidate, RIGHT_ALT);
    }

    #[test]
    fn unparseable_candidate_binding_fails_closed() {
        let source = r#"
local caps_lock = "code:66"
o.bind(caps_lock, "Terminal", "kitty")
"#;

        let TriggerBindingPlan::Unparseable { detail } = plan_trigger_binding(source, true) else {
            panic!("a dynamic candidate key must not be treated as free");
        };
        assert!(detail.contains("binding key"));
    }

    #[test]
    fn concatenated_combination_binds_are_skipped_without_hiding_caps_lock() {
        let source = r#"
o.bind("code:66", "Voisu dictation", "voisu toggle")
o.bind(
  "SUPER + " .. key,
  "Switch both monitors to desktop " .. desktop,
  function() paired_desktop_switch(desktop) end
)
o.bind("SUPER + TAB", "Next two-monitor desktop", function() paired_desktop_cycle(1) end)
o.bind("SUPER + CTRL + TAB", "Former two-monitor desktop", paired_desktop_former)
"#;

        assert_eq!(
            plan_trigger_binding(source, true),
            TriggerBindingPlan::AlreadyInstalled { key: CAPS_LOCK }
        );
    }

    #[test]
    fn concatenated_caps_lock_key_still_fails_closed() {
        let source = r#"o.bind("code:66" .. suffix, "Terminal", "kitty")"#;
        let TriggerBindingPlan::Unparseable { detail } = plan_trigger_binding(source, true) else {
            panic!("concatenating a Caps Lock code must not look free");
        };
        assert!(detail.contains("comma"), "{detail}");
    }

    #[test]
    fn imported_lua_bindings_are_checked_before_installing() {
        let (_directory, path) = config_dir();
        let imported = path.with_file_name("bindings.lua");
        fs::write(&path, "dofile(\"bindings.lua\")\n").unwrap();
        fs::write(
            &imported,
            "o.bind(\"code:66\", \"Launch terminal\", \"kitty\")\n",
        )
        .unwrap();
        let original = fs::read_to_string(&path).unwrap();
        let mut hyprland = FakeHyprland::new(&[true], true);
        let report = install_trigger_binding(&path, &LocalBindingFileSystem, &mut hyprland, true)
            .expect("an imported Caps Lock binding must force the Right Alt fallback");

        assert_eq!(report.key, RIGHT_ALT);
        let updated = fs::read_to_string(&path).unwrap();
        assert!(updated.starts_with(&(original + "\n-- BEGIN VOISU MANAGED TRIGGER\n")));
        assert!(updated.contains("code:108"));
        assert_right_alt_managed_block(&updated);
        assert_eq!(hyprland.reload_calls, 1);
    }

    #[test]
    fn parenthesis_free_lua_imports_are_scanned() {
        let imports = parse_lua_imports(r#"require "bindings" dofile 'extra.lua'"#).unwrap();
        assert_eq!(
            imports,
            vec![
                LuaImport {
                    loader: "require",
                    path: "bindings".to_owned(),
                },
                LuaImport {
                    loader: "dofile",
                    path: "extra.lua".to_owned(),
                },
            ]
        );

        let (_directory, path) = config_dir();
        let imported = path.with_file_name("bindings.lua");
        fs::write(&path, "require \"bindings\"\n").unwrap();
        fs::write(
            &imported,
            "o.bind(\"code:66\", \"Launch terminal\", \"kitty\")\n",
        )
        .unwrap();
        let source = fs::read_to_string(&path).unwrap();

        assert_eq!(
            plan_trigger_binding_with_imports(&path, &source, &LocalBindingFileSystem, true).plan,
            TriggerBindingPlan::Install { key: RIGHT_ALT }
        );
    }

    #[test]
    fn non_string_lua_import_args_fail_closed() {
        let error = parse_lua_imports(r#"require(module_name)"#).unwrap_err();
        assert!(error.contains("string literal"));
    }

    const OMARCHY_BOOTSTRAP_DOFILE: &str = r#"dofile((os.getenv("OMARCHY_PATH") or "/usr/share/omarchy") .. "/default/hypr/bootstrap.lua")"#;

    #[test]
    fn omarchy_bootstrap_dofile_is_skipped_and_later_requires_are_found() {
        let source = format!("{OMARCHY_BOOTSTRAP_DOFILE}\nrequire(\"hypr.bindings\")\n");
        assert_eq!(
            parse_lua_imports(&source).unwrap(),
            vec![LuaImport {
                loader: "require",
                path: "hypr.bindings".to_owned(),
            }]
        );
    }

    #[test]
    fn non_literal_dofile_and_loadfile_skip_while_require_stays_fail_closed() {
        assert_eq!(
            parse_lua_imports(
                r#"dofile(os.getenv("X")) loadfile(foo) source(bar) require("hypr.bindings")"#
            )
            .unwrap(),
            vec![LuaImport {
                loader: "require",
                path: "hypr.bindings".to_owned(),
            }]
        );
        let error =
            parse_lua_imports(r#"dofile(os.getenv("X")) require(module_name)"#).unwrap_err();
        assert!(error.contains("string literal"));
        let empty = parse_lua_imports(r#"dofile("")"#).unwrap_err();
        assert!(empty.contains("empty path"));
    }

    #[test]
    fn omarchy_bootstrap_dofile_does_not_hide_sibling_caps_lock_occupancy() {
        let (_directory, path) = config_dir();
        fs::write(
            &path,
            format!("{OMARCHY_BOOTSTRAP_DOFILE}\nrequire(\"hypr.bindings\")\n"),
        )
        .unwrap();
        fs::write(
            path.with_file_name("bindings.lua"),
            "o.bind(\"code:66\", \"Launch terminal\", \"kitty\")\n",
        )
        .unwrap();
        let source = fs::read_to_string(&path).unwrap();

        assert_eq!(
            plan_trigger_binding_with_imports(&path, &source, &LocalBindingFileSystem, true).plan,
            TriggerBindingPlan::Install { key: RIGHT_ALT }
        );
    }

    #[test]
    fn omarchy_bootstrap_dofile_does_not_hide_reachable_kb_options() {
        let (directory, path) = config_dir();
        fs::create_dir(directory.path().join("hypr")).unwrap();
        fs::write(
            &path,
            format!("{OMARCHY_BOOTSTRAP_DOFILE}\nrequire(\"hypr.input\")\n"),
        )
        .unwrap();
        fs::write(
            directory.path().join("hypr").join("input.lua"),
            "hl.config({\n  input = {\n    kb_options = \"compose:caps,grp:alt_shift_toggle\",\n  },\n})\n",
        )
        .unwrap();
        let source = fs::read_to_string(&path).unwrap();
        let options =
            collect_reachable_kb_options(&path, &source, &LocalBindingFileSystem).unwrap();

        assert_eq!(
            options.last().map(String::as_str),
            Some("compose:caps,grp:alt_shift_toggle")
        );
        let occupancy = inspect_trigger_occupancy(&path, &source, &LocalBindingFileSystem).unwrap();
        assert!(occupancy.compose_caps);
        assert!(occupancy.unmanaged_caps_lock.is_none());
    }

    #[test]
    fn sibling_input_lua_kb_options_are_visible_when_not_imported() {
        let (_directory, path) = config_dir();
        fs::write(&path, "-- stock hyprland.lua\n").unwrap();
        fs::write(
            path.with_file_name("input.lua"),
            "kb_options = \"compose:caps\"\n",
        )
        .unwrap();
        let source = fs::read_to_string(&path).unwrap();
        let occupancy = inspect_trigger_occupancy(&path, &source, &LocalBindingFileSystem).unwrap();
        assert!(occupancy.compose_caps);
    }

    #[test]
    fn unmanaged_voisu_toggle_on_caps_lock_is_already_installed_when_preferred() {
        let source = r#"o.bind("code:66", "Voisu dictation", "voisu toggle")"#;
        assert_eq!(
            plan_trigger_binding(source, true),
            TriggerBindingPlan::AlreadyInstalled { key: CAPS_LOCK }
        );
        assert_eq!(
            plan_trigger_binding(source, false),
            TriggerBindingPlan::Install { key: RIGHT_ALT }
        );
    }

    #[test]
    fn require_skips_a_module_directory_and_loads_lua_candidates() {
        let (directory, path) = config_dir();
        fs::create_dir(directory.path().join("bindings")).unwrap();
        fs::write(
            directory.path().join("bindings.lua"),
            "o.bind(\"code:66\", \"Launch terminal\", \"kitty\")\n",
        )
        .unwrap();
        fs::write(&path, "require(\"bindings\")\n").unwrap();
        let source = fs::read_to_string(&path).unwrap();

        assert_eq!(
            plan_trigger_binding_with_imports(&path, &source, &LocalBindingFileSystem, true).plan,
            TriggerBindingPlan::Install { key: RIGHT_ALT }
        );

        fs::remove_file(directory.path().join("bindings.lua")).unwrap();
        fs::write(
            directory.path().join("bindings").join("init.lua"),
            "o.bind(\"code:66\", \"Launch terminal\", \"kitty\")\n",
        )
        .unwrap();

        assert_eq!(
            plan_trigger_binding_with_imports(&path, &source, &LocalBindingFileSystem, true).plan,
            TriggerBindingPlan::Install { key: RIGHT_ALT }
        );
    }

    #[test]
    fn missing_dofile_import_fails_closed() {
        let (directory, path) = config_dir();
        fs::create_dir(directory.path().join("missing")).unwrap();
        let missing_file = plan_trigger_binding_with_imports(
            &path,
            "dofile(\"absent.lua\")\n",
            &LocalBindingFileSystem,
            true,
        )
        .plan;
        let TriggerBindingPlan::Unparseable { detail } = missing_file else {
            panic!("a missing dofile must not be treated as success");
        };
        assert!(detail.contains("missing file"));

        let directory_target = plan_trigger_binding_with_imports(
            &path,
            "dofile(\"missing\")\n",
            &LocalBindingFileSystem,
            true,
        )
        .plan;
        let TriggerBindingPlan::Unparseable { detail } = directory_target else {
            panic!("dofile of a directory must not be treated as success");
        };
        assert!(detail.contains("missing file"));
    }

    #[test]
    fn long_bracket_utf8_content_does_not_panic() {
        let source = "--[[é]]\no.bind(\"code:64\", \"Launch terminal\", \"kitty\")\n";
        let bindings = parse_lua_bindings(source).expect("multibyte long comments must stay UTF-8");
        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings[0].key, "code:64");
        assert_eq!(
            plan_trigger_binding("local docs = [[é]]\n", true),
            TriggerBindingPlan::Install { key: CAPS_LOCK }
        );
    }

    #[test]
    fn marker_like_lines_inside_lua_long_strings_are_not_managed_blocks() {
        let source = r#"
local documentation = [[
-- BEGIN VOISU MANAGED TRIGGER
-- END VOISU MANAGED TRIGGER
]]
"#;

        assert_eq!(
            plan_trigger_binding(source, true),
            TriggerBindingPlan::Install { key: CAPS_LOCK }
        );
    }

    #[test]
    fn unbalanced_managed_markers_fail_closed() {
        let source =
            format!("{BEGIN_MARKER}\no.bind(\"code:64\", \"Voisu dictation\", \"voisu toggle\")\n");

        let TriggerBindingPlan::Unparseable { detail } = plan_trigger_binding(&source, true) else {
            panic!("an unmatched managed marker must not be accepted");
        };
        assert!(detail.contains("no matching end marker"));
    }

    #[test]
    fn stale_managed_binding_is_not_accepted_as_voisu() {
        let source = format!(
            "{BEGIN_MARKER}\no.bind(\"code:66\", \"Voisu dictation\", \"kitty\")\n{END_MARKER}\n"
        );

        let TriggerBindingPlan::Unparseable { detail } = plan_trigger_binding(&source, true) else {
            panic!("a managed binding with a different command must not be accepted");
        };
        assert!(detail.contains("expected description or command"));
    }

    #[test]
    fn lua_hex_escapes_are_decoded_before_conflict_detection() {
        let source = r#"o.bind("code\x3a66", "Launch terminal", "kitty")
o.bind("code:108", "Launcher", "launcher")"#;

        let TriggerBindingPlan::Conflicts { conflicts } = plan_trigger_binding(source, true) else {
            panic!("a Lua hexadecimal escape must still occupy Caps Lock");
        };
        assert_eq!(conflicts[0].candidate, CAPS_LOCK);
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
        let mut hyprland = FakeHyprland::new(&[true], true);

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
        assert!(fs::read_dir(path.parent().unwrap()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".voisu-hyprland.")
        }));
    }

    #[test]
    fn fallback_preserves_the_exact_caps_lock_binding() {
        let (_directory, path) = config_dir();
        let original = "o.bind(\"code:66\", \"Terminal\", \"kitty\")\n";
        fs::write(&path, original).unwrap();
        let mut hyprland = FakeHyprland::new(&[true], true);

        let report =
            install_trigger_binding(&path, &LocalBindingFileSystem, &mut hyprland, true).unwrap();
        let updated = fs::read_to_string(&path).unwrap();

        assert_eq!(report.key, RIGHT_ALT);
        assert!(updated.contains(original));
        assert_right_alt_managed_block(&updated);
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
        let mut hyprland = FakeHyprland::new(&[], false);

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
        let mut first = FakeHyprland::new(&[true], true);
        install_trigger_binding(&path, &LocalBindingFileSystem, &mut first, true).unwrap();
        let before = fs::read_to_string(&path).unwrap();
        let mut second = FakeHyprland::new(&[true], true);

        let report =
            install_trigger_binding(&path, &LocalBindingFileSystem, &mut second, false).unwrap();

        assert_eq!(report.key, CAPS_LOCK);
        assert!(!report.changed);
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
        assert_eq!(before.matches(BEGIN_MARKER).count(), 1);
        assert_caps_lock_managed_block(&before);
        assert_eq!(second.reload_calls, 1);
    }

    #[test]
    fn already_installed_reload_failure_leaves_the_file_untouched() {
        let (_directory, path) = config_dir();
        fs::write(&path, "-- user bindings\n").unwrap();
        let mut first = FakeHyprland::new(&[true], true);
        install_trigger_binding(&path, &LocalBindingFileSystem, &mut first, true).unwrap();
        let before = fs::read_to_string(&path).unwrap();
        let mut second = FakeHyprland::new(&[false], true);

        let error = install_trigger_binding(&path, &LocalBindingFileSystem, &mut second, true)
            .expect_err("a stale compositor must not skip reload on rerun");

        assert!(error.to_string().contains("reload"));
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
        assert_eq!(second.reload_calls, 1);
    }

    #[test]
    fn reload_failure_restores_the_previous_file() {
        let (_directory, path) = config_dir();
        let original = "-- original\n";
        fs::write(&path, original).unwrap();
        let input_path = path.with_file_name("input.lua");
        let original_input = "require = nil -- user input.lua must stay untouched\n";
        fs::write(&input_path, original_input).unwrap();
        let mut hyprland = FakeHyprland::new(&[false, true], true);

        let error = install_trigger_binding(&path, &LocalBindingFileSystem, &mut hyprland, true)
            .expect_err("reload failure must fail setup");

        assert!(error.to_string().contains("reload"));
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        assert_eq!(
            fs::read_to_string(backup_path(Path::new(&path))).unwrap(),
            original
        );
        assert_eq!(fs::read_to_string(&input_path).unwrap(), original_input);
        assert_eq!(hyprland.reload_calls, 2);
    }

    #[test]
    fn verification_failure_restores_the_previous_file() {
        let (_directory, path) = config_dir();
        let original = "-- original\n";
        fs::write(&path, original).unwrap();
        let mut hyprland = FakeHyprland::new(&[true, true], false);

        let error = install_trigger_binding(&path, &LocalBindingFileSystem, &mut hyprland, true)
            .expect_err("verification failure must fail setup");

        assert!(error.to_string().contains("verification"));
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        assert!(!path.with_file_name("input.lua").exists());
        assert_eq!(hyprland.reload_calls, 2);
    }

    #[test]
    fn unmanaged_voisu_toggle_caps_lock_is_kept_without_rewrite() {
        let (_directory, path) = config_dir();
        let original = "o.bind(\"code:66\", \"Voisu dictation\", \"voisu toggle\")\n";
        fs::write(&path, original).unwrap();
        let mut hyprland = FakeHyprland::new(&[true], true);

        let report =
            install_trigger_binding(&path, &LocalBindingFileSystem, &mut hyprland, true).unwrap();

        assert_eq!(report.key, CAPS_LOCK);
        assert!(!report.changed);
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        assert!(!fs::read_to_string(&path).unwrap().contains("code:108"));
        assert_eq!(hyprland.reload_calls, 1);
    }

    #[test]
    fn unmanaged_caps_lock_in_sibling_bindings_falls_back_to_right_alt() {
        let (_directory, path) = config_dir();
        fs::write(&path, "").unwrap();
        let bindings = path.with_file_name("bindings.lua");
        let original_bindings = "o.bind(\"code:66\", \"Launch terminal\", \"kitty\")\n";
        fs::write(&bindings, original_bindings).unwrap();
        let mut hyprland = FakeHyprland::new(&[true], true);

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
        let mut hyprland = FakeHyprland::new(&[true], true);

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
    fn rejected_caps_lock_installs_right_alt_without_input_rewrite() {
        let (_directory, path) = config_dir();
        fs::write(&path, "-- user bindings\n").unwrap();
        let mut hyprland = FakeHyprland::new(&[true], true);

        let report =
            install_trigger_binding(&path, &LocalBindingFileSystem, &mut hyprland, false).unwrap();

        assert_eq!(report.key, RIGHT_ALT);
        let updated = fs::read_to_string(&path).unwrap();
        assert_right_alt_managed_block(&updated);
        assert!(!updated.contains("hl.config"));
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
        let mut hyprland = FakeHyprland::new(&[true], true);

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
        let mut hyprland = FakeHyprland::new(&[true], true);

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
    fn caps_lock_input_merges_kb_options_from_reachable_imports() {
        let (_directory, path) = config_dir();
        let imported = path.with_file_name("layout.lua");
        fs::write(&path, "dofile(\"layout.lua\")\n").unwrap();
        fs::write(
            &imported,
            "hl.config({\n  input = {\n    kb_options = \"us,compose:ralt,grp:alt_shift_toggle\",\n  },\n})\n",
        )
        .unwrap();
        let mut hyprland = FakeHyprland::new(&[true], true);

        install_trigger_binding(&path, &LocalBindingFileSystem, &mut hyprland, true).unwrap();
        let updated = fs::read_to_string(&path).unwrap();
        let managed = managed_block(&updated);
        let options = last_kb_options(&[managed]).expect("managed kb_options");

        assert_eq!(
            options,
            format!("{CAPS_NONE},{BOTH_CAPSLOCK_CANCEL},us,compose:ralt,grp:alt_shift_toggle")
        );
    }

    #[test]
    fn caps_lock_input_keeps_root_kb_options_after_an_import() {
        let (_directory, path) = config_dir();
        let imported = path.with_file_name("layout.lua");
        fs::write(
            &path,
            concat!(
                "dofile(\"layout.lua\")\n",
                "hl.config({\n",
                "  input = {\n",
                "    kb_options = \"root-option\",\n",
                "  },\n",
                "})\n"
            ),
        )
        .unwrap();
        fs::write(
            &imported,
            "hl.config({\n  input = {\n    kb_options = \"imported-option\",\n  },\n})\n",
        )
        .unwrap();
        let mut hyprland = FakeHyprland::new(&[true], true);

        install_trigger_binding(&path, &LocalBindingFileSystem, &mut hyprland, true).unwrap();
        let updated = fs::read_to_string(&path).unwrap();
        let managed = managed_block(&updated);
        let options = last_kb_options(&[managed]).expect("managed kb_options");

        assert_eq!(
            options,
            format!("{CAPS_NONE},{BOTH_CAPSLOCK_CANCEL},root-option")
        );
    }

    #[test]
    fn caps_lock_input_reexecutes_a_repeated_dofile_after_a_root_assignment() {
        let (_directory, path) = config_dir();
        let imported = path.with_file_name("layout.lua");
        fs::write(
            &path,
            concat!(
                "dofile(\"layout.lua\")\n",
                "hl.config({\n",
                "  input = {\n",
                "    kb_options = \"root-option\",\n",
                "  },\n",
                "})\n",
                "dofile(\"layout.lua\")\n"
            ),
        )
        .unwrap();
        fs::write(
            &imported,
            "hl.config({\n  input = {\n    kb_options = \"imported-option\",\n  },\n})\n",
        )
        .unwrap();
        let mut hyprland = FakeHyprland::new(&[true], true);

        install_trigger_binding(&path, &LocalBindingFileSystem, &mut hyprland, true).unwrap();
        let updated = fs::read_to_string(&path).unwrap();
        let managed = managed_block(&updated);
        let options = last_kb_options(&[managed]).expect("managed kb_options");

        assert_eq!(
            options,
            format!("{CAPS_NONE},{BOTH_CAPSLOCK_CANCEL},imported-option")
        );
    }

    #[test]
    fn existing_caps_lock_input_is_not_rewritten() {
        let (_directory, path) = config_dir();
        fs::write(&path, "-- user bindings\n").unwrap();
        let input_path = path.with_file_name("input.lua");
        let original_input = "hl.config({\n  input = {\n    kb_options = \"caps:none,shift:both_capslock_cancel\",\n  },\n})\n";
        fs::write(&input_path, original_input).unwrap();
        let mut hyprland = FakeHyprland::new(&[true], true);

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
        let mut hyprland = FakeHyprland::new(&[true], true);

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
        let mut hyprland = FakeHyprland::new(&[true], true);

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
  local window = hl.get_active_window()
  if not window then
    return false
  end
  for _, tag in ipairs(window.tags or {}) do
    if tag:gsub("%*$", "") == "terminal" then
      return true
    end
  end
  return false
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

    fn universal_paste_live(key: &str, modmask: u64) -> Value {
        serde_json::json!([{
            "key": key,
            "modmask": modmask,
            "description": "Universal paste",
            "dispatcher": "__lua",
            "arg": "91"
        }])
    }

    struct MemoryFiles(std::collections::HashMap<PathBuf, String>);

    impl MemoryFiles {
        fn from_files(files: &[(&str, &str)]) -> Self {
            Self(
                files
                    .iter()
                    .map(|(path, source)| (normalize_path(Path::new(path)), (*source).to_owned()))
                    .collect(),
            )
        }
    }

    impl BindingFileSystem for MemoryFiles {
        fn read_to_string(&self, path: &Path) -> Result<Option<String>, String> {
            Ok(self.0.get(&normalize_path(path)).cloned())
        }

        fn write_atomic(&self, _path: &Path, _contents: &str) -> Result<(), String> {
            Err("memory files are read-only".to_owned())
        }

        fn remove_file(&self, _path: &Path) -> Result<(), String> {
            Err("memory files are read-only".to_owned())
        }
    }

    struct FakeLiveHyprland {
        bindings: Value,
    }

    impl HyprlandController for FakeLiveHyprland {
        fn reload(&mut self) -> Result<(), String> {
            Ok(())
        }

        fn binding_is_installed(&mut self, _key: &str, _command: &str) -> Result<bool, String> {
            Ok(false)
        }

        fn live_bindings(&mut self) -> Result<Value, String> {
            Ok(self.bindings.clone())
        }
    }

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
        assert_eq!(action.live_binding_identity, "91");
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
    fn changed_live_lua_identity_invalidates_the_verified_action() {
        let first = serde_json::json!([{
            "key": "V",
            "modmask": 64,
            "description": "Universal paste",
            "dispatcher": "__lua",
            "arg": "91"
        }]);
        let second = serde_json::json!([{
            "key": "V",
            "modmask": 64,
            "description": "Universal paste",
            "dispatcher": "__lua",
            "arg": "92"
        }]);

        let first = discover_paste_action(&[OMARCHY_PASTE_SOURCE], &first).unwrap();
        let second = discover_paste_action(&[OMARCHY_PASTE_SOURCE], &second).unwrap();
        assert_ne!(first, second);
        assert_eq!(second.live_binding_identity, "92");
    }

    #[test]
    fn lua_paste_binding_requires_a_numeric_live_identity() {
        let live = serde_json::json!([{
            "key": "V",
            "modmask": 64,
            "description": "Universal paste",
            "dispatcher": "__lua",
            "arg": ""
        }]);

        assert!(discover_paste_action(&[OMARCHY_PASTE_SOURCE], &live).is_none());
    }

    #[test]
    fn omarchy_helper_markers_in_strings_or_comments_are_not_verified() {
        let source = r#"
local fake = "function universal_clipboard_shortcut(default_mods, default_key, terminal_mods, terminal_key) if active_window_is_terminal() then send_shortcut_once(terminal_mods, terminal_key)() send_shortcut_once(default_mods, default_key)() end"
-- function universal_clipboard_shortcut(default_mods, default_key, terminal_mods, terminal_key)
o.bind("SUPER + V", "Universal paste", universal_clipboard_shortcut("CTRL", "V", "SHIFT", "Insert"))
"#;
        let live = serde_json::json!([{
            "key": "V",
            "modmask": 64,
            "description": "Universal paste",
            "dispatcher": "__lua",
            "arg": "91"
        }]);

        assert!(discover_paste_action(&[source], &live).is_none());
    }

    #[test]
    fn omarchy_helper_markers_in_long_comments_and_strings_are_not_verified() {
        let source = r#"
--[=[
function universal_clipboard_shortcut(default_mods, default_key, terminal_mods, terminal_key)
  if active_window_is_terminal() then
    send_shortcut_once(terminal_mods, terminal_key)()
    send_shortcut_once(default_mods, default_key)()
  end
end
]=]
local fake = [=[
function universal_clipboard_shortcut(default_mods, default_key, terminal_mods, terminal_key)
  if active_window_is_terminal() then
    send_shortcut_once(terminal_mods, terminal_key)()
    send_shortcut_once(default_mods, default_key)()
  end
end
]=]
o.bind("SUPER + V", "Universal paste", universal_clipboard_shortcut("CTRL", "V", "SHIFT", "Insert"))
"#;
        let live = serde_json::json!([{
            "key": "V",
            "modmask": 64,
            "description": "Universal paste",
            "dispatcher": "__lua",
            "arg": "91"
        }]);

        assert!(discover_paste_action(&[source], &live).is_none());
    }

    #[test]
    fn literal_paste_binding_inside_a_long_comment_is_not_verified() {
        let source = r#"
--[=[
o.bind("SUPER + V", "Paste transcript", "safe-paste")
]=]
"#;
        let live = serde_json::json!([{
            "key": "V",
            "modmask": 64,
            "description": "Paste transcript",
            "dispatcher": "exec",
            "arg": "safe-paste"
        }]);

        assert!(discover_paste_action(&[source], &live).is_none());
    }

    #[test]
    fn dynamic_paste_binding_with_an_unrelated_helper_shape_fails_closed() {
        let source = r#"
local function universal_clipboard_shortcut(default_mods, default_key, terminal_mods, terminal_key)
  if active_window_is_terminal() then
    send_shortcut_once(terminal_mods, terminal_key)()
    send_shortcut_once(default_mods, default_key)()
  end
end
local function unrelated_paste()
  os.execute("paste")
end
o.bind("SUPER + V", "Universal paste", unrelated_paste)
"#;
        let live = serde_json::json!([{
            "key": "V",
            "modmask": 64,
            "description": "Universal paste",
            "dispatcher": "__lua",
            "arg": "91"
        }]);

        assert!(discover_paste_action(&[source], &live).is_none());
    }

    #[test]
    fn omarchy_markers_must_be_inside_the_named_helper_body() {
        let source = r#"
local function unrelated_helper()
  if active_window_is_terminal() then
    send_shortcut_once(terminal_mods, terminal_key)()
    send_shortcut_once(default_mods, default_key)()
  end
end
local function universal_clipboard_shortcut(default_mods, default_key, terminal_mods, terminal_key)
  return function()
  end
end
o.bind("SUPER + V", "Universal paste", universal_clipboard_shortcut("CTRL", "V", "SHIFT", "Insert"))
"#;
        let live = serde_json::json!([{
            "key": "V",
            "modmask": 64,
            "description": "Universal paste",
            "dispatcher": "__lua",
            "arg": "91"
        }]);

        assert!(discover_paste_action(&[source], &live).is_none());
    }

    #[test]
    fn omarchy_helper_rejects_extra_or_reordered_body_logic() {
        let source = r#"
local function universal_clipboard_shortcut(default_mods, default_key, terminal_mods, terminal_key)
  return function()
    if active_window_is_terminal() then
      send_shortcut_once(default_mods, default_key)()
      send_shortcut_once(terminal_mods, terminal_key)()
    else
      send_shortcut_once(default_mods, default_key)()
    end
  end
end
o.bind("SUPER + V", "Universal paste", universal_clipboard_shortcut("CTRL", "V", "SHIFT", "Insert"))
"#;
        let live = serde_json::json!([{
            "key": "V",
            "modmask": 64,
            "description": "Universal paste",
            "dispatcher": "__lua",
            "arg": "91"
        }]);

        assert!(discover_paste_action(&[source], &live).is_none());
    }

    #[test]
    fn omarchy_helper_requires_the_known_shortcut_arguments() {
        let source = OMARCHY_PASTE_SOURCE.replace(
            "universal_clipboard_shortcut(\"CTRL\", \"V\", \"SHIFT\", \"Insert\")",
            "universal_clipboard_shortcut(\"CTRL\", \"V\", \"ALT\", \"Insert\")",
        );
        let live = serde_json::json!([{
            "key": "V",
            "modmask": 64,
            "description": "Universal paste",
            "dispatcher": "__lua",
            "arg": "91"
        }]);

        assert!(discover_paste_action(&[&source], &live).is_none());
    }

    #[test]
    fn omarchy_desktop_label_accepts_multi_label_values() {
        assert!(desktop_has_label("Hyprland:Omarchy", "omarchy"));
        assert!(desktop_has_label("Hyprland;Omarchy", "omarchy"));
        assert!(!desktop_has_label("Hyprland", "omarchy"));
    }

    #[test]
    fn a_literal_lua_paste_binding_is_verified_from_opaque_live_lua() {
        let source = r#"o.bind("CTRL + SHIFT + P", "Paste transcript", "hyprctl dispatch sendshortcut CTRL V")"#;
        let live = serde_json::json!([{
            "key": "",
            "modmask": 0,
            "description": "Paste transcript",
            "dispatcher": "__lua",
            "arg": "92"
        }]);

        let action = discover_paste_action(&[source], &live).expect("literal binding is verified");
        assert_eq!(action.shortcut.binding, "CTRL + SHIFT + P");
        assert_eq!(action.live_binding_identity, "92");
        assert_eq!(action.behavior, PasteBehavior::Simple);
    }

    #[test]
    fn a_literal_paste_binding_still_verifies_when_live_dispatcher_is_exec() {
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

    #[test]
    fn omarchy_universal_paste_accepts_opaque_live_lua_keys() {
        let action = discover_paste_action(&[OMARCHY_PASTE_SOURCE], &universal_paste_live("", 0))
            .expect("known Omarchy helper should verify against an opaque live Lua key");
        assert_eq!(action.shortcut.binding, "SUPER + V");
        assert_eq!(action.live_binding_identity, "91");
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
    fn opaque_live_lua_key_with_a_different_description_fails_closed() {
        let live = serde_json::json!([{
            "key": "",
            "modmask": 0,
            "description": "Other paste",
            "dispatcher": "__lua",
            "arg": "91"
        }]);

        assert!(discover_paste_action(&[OMARCHY_PASTE_SOURCE], &live).is_none());
    }

    #[test]
    fn opaque_live_lua_key_with_an_unrelated_function_fails_closed() {
        let source = r#"
local function unrelated_paste()
  os.execute("paste")
end
o.bind("SUPER + V", "Universal paste", unrelated_paste)
"#;

        assert!(discover_paste_action(&[source], &universal_paste_live("", 0)).is_none());
    }

    #[test]
    fn unimported_bindings_lua_is_not_an_active_paste_source() {
        let root = PathBuf::from("/hypr/hyprland.lua");
        let files = MemoryFiles::from_files(&[
            (root.to_str().unwrap(), "-- no imports\n"),
            ("/hypr/bindings.lua", OMARCHY_PASTE_SOURCE),
        ]);
        let mut hyprland = FakeLiveHyprland {
            bindings: universal_paste_live("", 0),
        };

        let action = discover_paste_action_from_sources(&root, &files, &mut hyprland)
            .expect("discovery should inspect the active root");
        assert!(action.is_none());
    }

    #[test]
    fn imported_bindings_lua_is_an_active_paste_source() {
        let root = PathBuf::from("/hypr/hyprland.lua");
        let files = MemoryFiles::from_files(&[
            (root.to_str().unwrap(), "dofile(\"bindings.lua\")\n"),
            ("/hypr/bindings.lua", OMARCHY_PASTE_SOURCE),
        ]);
        let mut hyprland = FakeLiveHyprland {
            bindings: universal_paste_live("", 0),
        };

        let action = discover_paste_action_from_sources(&root, &files, &mut hyprland)
            .expect("discovery should inspect imported sources")
            .expect("an imported Omarchy helper must still be verified");
        assert_eq!(action.shortcut.binding, "SUPER + V");
    }

    #[test]
    fn omarchy_helper_rejects_a_replaced_send_shortcut_once_body() {
        let source = OMARCHY_PASTE_SOURCE.replace(
            r#"local function send_shortcut_once(mods, key)
  return function()
    hl.dispatch(hl.dsp.send_key_state({ mods = mods, key = key, state = "down" }))
    hl.timer(function()
      hl.dispatch(hl.dsp.send_key_state({ mods = mods, key = key, state = "up" }))
    end, { timeout = 50, type = "oneshot" })
  end
end"#,
            r#"local function send_shortcut_once(mods, key)
  os.execute("malicious")
end"#,
        );

        assert!(discover_paste_action(&[&source], &universal_paste_live("V", 64)).is_none());
    }

    #[test]
    fn omarchy_helper_rejects_an_unrelated_active_window_is_terminal_body() {
        let source = OMARCHY_PASTE_SOURCE.replace(
            r#"local function active_window_is_terminal()
  local window = hl.get_active_window()
  if not window then
    return false
  end
  for _, tag in ipairs(window.tags or {}) do
    if tag:gsub("%*$", "") == "terminal" then
      return true
    end
  end
  return false
end"#,
            r#"local function active_window_is_terminal()
  return true
end"#,
        );

        assert!(discover_paste_action(&[&source], &universal_paste_live("V", 64)).is_none());
    }

    #[test]
    fn earlier_unverified_universal_paste_does_not_hide_a_later_helper() {
        let unrelated = r#"
local function unrelated_paste()
  os.execute("paste")
end
o.bind("SUPER + V", "Universal paste", unrelated_paste)
"#;

        let action = discover_paste_action(
            &[unrelated, OMARCHY_PASTE_SOURCE],
            &universal_paste_live("", 0),
        )
        .expect("a later Omarchy helper must still verify");
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
    fn earlier_unverified_function_does_not_hide_a_later_literal_paste() {
        let function_source = r#"
local function my_paste()
  os.execute("dangerous-command")
end
o.bind("CTRL + SHIFT + P", "Paste transcript", my_paste)
"#;
        let literal = r#"o.bind("CTRL + SHIFT + P", "Paste transcript", "safe-paste")"#;
        let live = serde_json::json!([{
            "key": "",
            "modmask": 0,
            "description": "Paste transcript",
            "dispatcher": "__lua",
            "arg": "92"
        }]);

        let action = discover_paste_action(&[function_source, literal], &live)
            .expect("a later string command must still verify");
        assert_eq!(action.shortcut.binding, "CTRL + SHIFT + P");
        assert_eq!(action.behavior, PasteBehavior::Simple);
    }

    #[test]
    fn a_literal_caps_paste_binding_is_not_verified() {
        let source = r#"o.bind("CAPS + P", "Paste transcript", "safe-paste")"#;
        let live = serde_json::json!([{
            "key": "P",
            "modmask": 2,
            "description": "Paste transcript",
            "dispatcher": "exec",
            "arg": "safe-paste"
        }]);

        assert!(discover_paste_action(&[source], &live).is_none());
    }
}
