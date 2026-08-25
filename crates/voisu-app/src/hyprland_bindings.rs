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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TriggerKey {
    pub label: &'static str,
    pub code: &'static str,
}

pub const LEFT_ALT: TriggerKey = TriggerKey {
    label: "Left Alt",
    code: "code:64",
};

pub const RIGHT_ALT: TriggerKey = TriggerKey {
    label: "Right Alt",
    code: "code:108",
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
        self.is_standalone_for(LEFT_ALT) || self.is_standalone_for(RIGHT_ALT)
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
    Unparseable { detail: String },
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
                    "both preferred Trigger Key candidates are already bound:"
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

pub fn parse_lua_bindings(source: &str) -> Result<Vec<LuaBinding>, String> {
    let mut bindings = Vec::new();
    let mut chunk = String::new();
    let mut managed = false;

    for line in source.split_inclusive('\n') {
        match line.trim() {
            BEGIN_MARKER => {
                bindings.extend(parse_lua_chunk(&chunk, managed)?);
                chunk.clear();
                managed = true;
            }
            END_MARKER => {
                bindings.extend(parse_lua_chunk(&chunk, managed)?);
                chunk.clear();
                managed = false;
            }
            _ => chunk.push_str(line),
        }
    }
    bindings.extend(parse_lua_chunk(&chunk, managed)?);
    Ok(bindings)
}

pub fn plan_trigger_binding(source: &str) -> TriggerBindingPlan {
    let bindings = match parse_lua_bindings(source) {
        Ok(bindings) => bindings,
        Err(detail) => return TriggerBindingPlan::Unparseable { detail },
    };

    let mut conflicts = Vec::new();
    for candidate in [LEFT_ALT, RIGHT_ALT] {
        if let Some(binding) = bindings
            .iter()
            .find(|binding| !binding.managed && binding.is_standalone_for(candidate))
        {
            conflicts.push(BindingConflict {
                candidate,
                description: binding.description.clone(),
                command: binding.command.clone(),
            });
        }
    }
    if conflicts.is_empty() {
        for candidate in [LEFT_ALT, RIGHT_ALT] {
            if bindings
                .iter()
                .any(|binding| binding.managed && binding.is_standalone_for(candidate))
            {
                return TriggerBindingPlan::AlreadyInstalled { key: candidate };
            }
        }
    }

    for candidate in [LEFT_ALT, RIGHT_ALT] {
        if !bindings
            .iter()
            .any(|binding| binding.is_standalone_for(candidate))
        {
            return TriggerBindingPlan::Install { key: candidate };
        }
    }

    if !conflicts.is_empty() {
        return TriggerBindingPlan::Conflicts { conflicts };
    }

    unreachable!("a conflict-free binding plan always finds an available candidate")
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
    let plan = plan_trigger_binding(source);

    let key = match plan {
        TriggerBindingPlan::AlreadyInstalled { key } => {
            let verified = hyprland
                .binding_is_installed(key.code, VOISU_TOGGLE_COMMAND)
                .map_err(|detail| TriggerBindingError::VerificationFailed {
                    detail,
                    backup_path: backup.clone(),
                    restore_error: None,
                })?;
            if !verified {
                return Err(TriggerBindingError::VerificationFailed {
                    detail: format!(
                        "{} ({}) is managed by Voisu but is not reported by Hyprland",
                        key.label, key.code
                    ),
                    backup_path: backup.clone(),
                    restore_error: None,
                });
            }
            return Ok(TriggerBindingInstallReport {
                key,
                changed: false,
                backup_path: backup,
            });
        }
        TriggerBindingPlan::Conflicts { conflicts } => {
            return Err(TriggerBindingError::from_conflicts(conflicts));
        }
        TriggerBindingPlan::Unparseable { detail } => {
            return Err(TriggerBindingError::Unparseable { detail });
        }
        TriggerBindingPlan::Install { key } => key,
    };

    files
        .write_atomic(&backup, source)
        .map_err(|detail| TriggerBindingError::File {
            action: "save a recoverable backup",
            path: backup.clone(),
            detail,
        })?;

    let updated = append_managed_binding(source, key);
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

fn append_managed_binding(source: &str, key: TriggerKey) -> String {
    let mut content = remove_managed_blocks(source);
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    if !content.is_empty() && !content.ends_with("\n\n") {
        content.push('\n');
    }
    content.push_str(BEGIN_MARKER);
    content.push('\n');
    content.push_str(&format!(
        "o.bind(\"{}\", \"{VOISU_TRIGGER_DESCRIPTION}\", \"{VOISU_TOGGLE_COMMAND}\")\n",
        key.code
    ));
    content.push_str(END_MARKER);
    content.push('\n');
    content
}

fn remove_managed_blocks(source: &str) -> String {
    let mut result = String::new();
    let mut managed = false;
    for line in source.split_inclusive('\n') {
        match line.trim() {
            BEGIN_MARKER => managed = true,
            END_MARKER if managed => managed = false,
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

fn parse_lua_chunk(source: &str, managed: bool) -> Result<Vec<LuaBinding>, String> {
    let tokens = lex_lua(source);
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
        let (key, description, command) = parse_bind_arguments(&tokens[index + 4..])?;
        bindings.push(LuaBinding {
            key,
            description,
            command,
            managed,
        });
        index += 4;
    }
    Ok(bindings)
}

fn parse_bind_arguments(tokens: &[LuaToken]) -> Result<(String, String, String), String> {
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
        return Err("the binding key must be followed by a comma".to_owned());
    }
    index += 1;
    let description = read_string(&mut index, "binding description")?;
    if !matches!(tokens.get(index), Some(LuaToken::Symbol(','))) {
        return Err("the binding description must be followed by a comma".to_owned());
    }
    index += 1;
    let command = match tokens.get(index) {
        Some(LuaToken::String(value)) => value.clone(),
        Some(LuaToken::Symbol(')')) => return Err("the binding command is missing".to_owned()),
        Some(_) => "<Lua dispatcher>".to_owned(),
        None => return Err("the binding command is missing".to_owned()),
    };
    Ok((key, description, command))
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
    fn left_alt_conflict_falls_back_to_right_alt() {
        let source = r#"
o.bind("code:64", "Launch terminal", "kitty")
o.bind("ALT, code:108", "Modified right alt", "workspace next")
"#;

        assert_eq!(
            plan_trigger_binding(source),
            TriggerBindingPlan::Install { key: RIGHT_ALT }
        );
    }

    #[test]
    fn exact_bindings_with_lua_dispatchers_are_still_conflicts() {
        let source = r#"
o.bind("code:64", "Terminal", { omarchy = "terminal" })
o.bind("code:108", "Launcher", hl.dsp.exec_cmd("launcher"))
"#;

        let TriggerBindingPlan::Conflicts { conflicts } = plan_trigger_binding(source) else {
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
            "{BEGIN_MARKER}\no.bind(\"code:64\", \"Voisu dictation\", \"voisu toggle\")\n{END_MARKER}\no.bind(\"code:108\", \"Launcher\", \"launcher\")\n"
        );

        let TriggerBindingPlan::Conflicts { conflicts } = plan_trigger_binding(&source) else {
            panic!("a user-owned exact binding must remain a conflict on rerun");
        };
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].candidate, RIGHT_ALT);
    }

    #[test]
    fn unparseable_candidate_binding_fails_closed() {
        let source = r#"
local left_alt = "code:64"
o.bind(left_alt, "Terminal", "kitty")
"#;

        let TriggerBindingPlan::Unparseable { detail } = plan_trigger_binding(source) else {
            panic!("a dynamic candidate key must not be treated as free");
        };
        assert!(detail.contains("binding key"));
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
o.bind("code:64", "Launch terminal", "kitty")
o.bind("code:108", "Lock screen", "loginctl lock-session")
"#;

        let plan = plan_trigger_binding(source);
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
            install_trigger_binding(&path, &LocalBindingFileSystem, &mut hyprland).unwrap();

        assert_eq!(report.key, LEFT_ALT);
        assert!(report.changed);
        assert_eq!(
            fs::read_to_string(&path)
                .unwrap()
                .matches(BEGIN_MARKER)
                .count(),
            1
        );
        assert_eq!(fs::read_to_string(&report.backup_path).unwrap(), original);
        assert!(fs::read_dir(path.parent().unwrap())
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".voisu-hyprland.")));
    }

    #[test]
    fn fallback_preserves_the_exact_left_alt_binding() {
        let (_directory, path) = config_dir();
        let original = "o.bind(\"code:64\", \"Terminal\", \"kitty\")\n";
        fs::write(&path, original).unwrap();
        let mut hyprland = FakeHyprland::new(&[true], true);

        let report =
            install_trigger_binding(&path, &LocalBindingFileSystem, &mut hyprland).unwrap();
        let updated = fs::read_to_string(&path).unwrap();

        assert_eq!(report.key, RIGHT_ALT);
        assert!(updated.contains(original));
        assert!(updated.contains("o.bind(\"code:108\", \"Voisu dictation\", \"voisu toggle\")"));
    }

    #[test]
    fn both_conflicts_leave_the_file_and_compositor_untouched() {
        let (_directory, path) = config_dir();
        let original = concat!(
            "o.bind(\"code:64\", \"Terminal\", \"kitty\")\n",
            "o.bind(\"code:108\", \"Launcher\", \"launcher\")\n"
        );
        fs::write(&path, original).unwrap();
        let mut hyprland = FakeHyprland::new(&[], false);

        let error = install_trigger_binding(&path, &LocalBindingFileSystem, &mut hyprland)
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
        install_trigger_binding(&path, &LocalBindingFileSystem, &mut first).unwrap();
        let before = fs::read_to_string(&path).unwrap();
        let mut second = FakeHyprland::new(&[], true);

        let report = install_trigger_binding(&path, &LocalBindingFileSystem, &mut second).unwrap();

        assert!(!report.changed);
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
        assert_eq!(before.matches(BEGIN_MARKER).count(), 1);
    }

    #[test]
    fn reload_failure_restores_the_previous_file() {
        let (_directory, path) = config_dir();
        let original = "-- original\n";
        fs::write(&path, original).unwrap();
        let mut hyprland = FakeHyprland::new(&[false, true], true);

        let error = install_trigger_binding(&path, &LocalBindingFileSystem, &mut hyprland)
            .expect_err("reload failure must fail setup");

        assert!(error.to_string().contains("reload"));
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        assert_eq!(
            fs::read_to_string(backup_path(Path::new(&path))).unwrap(),
            original
        );
        assert_eq!(hyprland.reload_calls, 2);
    }

    #[test]
    fn verification_failure_restores_the_previous_file() {
        let (_directory, path) = config_dir();
        let original = "-- original\n";
        fs::write(&path, original).unwrap();
        let mut hyprland = FakeHyprland::new(&[true, true], false);

        let error = install_trigger_binding(&path, &LocalBindingFileSystem, &mut hyprland)
            .expect_err("verification failure must fail setup");

        assert!(error.to_string().contains("verification"));
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        assert_eq!(hyprland.reload_calls, 2);
    }
}
