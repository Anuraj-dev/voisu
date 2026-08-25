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

pub fn plan_trigger_binding(source: &str) -> TriggerBindingPlan {
    let bindings = match parse_lua_bindings(source) {
        Ok(bindings) => bindings,
        Err(detail) => return TriggerBindingPlan::Unparseable { detail },
    };

    plan_trigger_binding_from_bindings(&bindings)
}

fn plan_trigger_binding_with_imports(
    path: &Path,
    source: &str,
    files: &dyn BindingFileSystem,
) -> TriggerBindingPlan {
    let bindings = match load_lua_bindings(path, source, files) {
        Ok(bindings) => bindings,
        Err(detail) => return TriggerBindingPlan::Unparseable { detail },
    };

    plan_trigger_binding_from_bindings(&bindings)
}

fn plan_trigger_binding_from_bindings(bindings: &[LuaBinding]) -> TriggerBindingPlan {
    if let Some(binding) = bindings.iter().find(|binding| {
        binding.managed
            && binding.is_standalone()
            && (binding.description != VOISU_TRIGGER_DESCRIPTION
                || binding.command != VOISU_TOGGLE_COMMAND)
    }) {
        let candidate = [LEFT_ALT, RIGHT_ALT]
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

#[derive(Clone, Debug, Eq, PartialEq)]
struct LuaImport {
    loader: &'static str,
    path: String,
}

fn load_lua_bindings(
    root_path: &Path,
    root_source: &str,
    files: &dyn BindingFileSystem,
) -> Result<Vec<LuaBinding>, String> {
    let mut visited = HashSet::new();
    let mut bindings = Vec::new();
    load_lua_bindings_recursive(root_path, root_source, files, &mut visited, &mut bindings)?;
    Ok(bindings)
}

fn load_lua_bindings_recursive(
    path: &Path,
    source: &str,
    files: &dyn BindingFileSystem,
    visited: &mut HashSet<PathBuf>,
    bindings: &mut Vec<LuaBinding>,
) -> Result<(), String> {
    let path = normalize_path(path);
    if !visited.insert(path.clone()) {
        return Ok(());
    }

    bindings.extend(parse_lua_bindings(source)?);
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
        load_lua_bindings_recursive(&imported_path, &imported_source, files, visited, bindings)?;
    }
    Ok(())
}

fn parse_lua_imports(source: &str) -> Result<Vec<LuaImport>, String> {
    let tokens = lex_lua(source)?;
    let mut imports = Vec::new();
    let mut index = 0;
    while index + 2 < tokens.len() {
        let LuaToken::Identifier(loader) = &tokens[index] else {
            index += 1;
            continue;
        };
        if !matches!(
            loader.as_str(),
            "dofile" | "loadfile" | "require" | "source"
        ) || !matches!(tokens.get(index + 1), Some(LuaToken::Symbol('(')))
        {
            index += 1;
            continue;
        }
        let Some(LuaToken::String(path)) = tokens.get(index + 2) else {
            return Err(format!(
                "the Lua import {loader} must use a string literal path"
            ));
        };
        if path.is_empty() {
            return Err(format!("the Lua import {loader} has an empty path"));
        }
        imports.push(LuaImport {
            loader: match loader.as_str() {
                "dofile" => "dofile",
                "loadfile" => "loadfile",
                "require" => "require",
                "source" => "source",
                _ => unreachable!(),
            },
            path: path.clone(),
        });
        index += 3;
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

    if import.loader == "require" && import_path.extension().is_none() {
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
    let plan = plan_trigger_binding_with_imports(path, source, files);

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

    let updated = match append_managed_binding(source, key) {
        Ok(updated) => updated,
        Err(detail) => return Err(TriggerBindingError::Unparseable { detail }),
    };
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

fn append_managed_binding(source: &str, key: TriggerKey) -> Result<String, String> {
    let mut content = remove_managed_blocks(source)?;
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
    Ok(content)
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
    Symbol(char),
}

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
        if matches!(character, '.' | '(' | ')' | ',' | '{' | '}') {
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
    let mut tail = String::new();
    for character in chars.by_ref() {
        tail.push(character);
        if tail.ends_with(closing) {
            return Ok(());
        }
        if tail.len() > closing.len() {
            let trim_at = tail.len() - closing.len();
            tail.drain(..trim_at);
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
    fn imported_lua_bindings_are_checked_before_installing() {
        let (_directory, path) = config_dir();
        let imported = path.with_file_name("bindings.lua");
        fs::write(&path, "dofile(\"bindings.lua\")\n").unwrap();
        fs::write(
            &imported,
            "o.bind(\"code:64\", \"Launch terminal\", \"kitty\")\n",
        )
        .unwrap();
        let original = fs::read_to_string(&path).unwrap();
        let mut hyprland = FakeHyprland::new(&[true], true);
        let report = install_trigger_binding(&path, &LocalBindingFileSystem, &mut hyprland)
            .expect("an imported Left Alt binding must force the Right Alt fallback");

        assert_eq!(report.key, RIGHT_ALT);
        let updated = fs::read_to_string(&path).unwrap();
        assert!(updated.starts_with(&(original + "\n-- BEGIN VOISU MANAGED TRIGGER\n")));
        assert!(updated.contains("code:108"));
        assert_eq!(hyprland.reload_calls, 1);
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
            plan_trigger_binding(source),
            TriggerBindingPlan::Install { key: LEFT_ALT }
        );
    }

    #[test]
    fn unbalanced_managed_markers_fail_closed() {
        let source =
            format!("{BEGIN_MARKER}\no.bind(\"code:64\", \"Voisu dictation\", \"voisu toggle\")\n");

        let TriggerBindingPlan::Unparseable { detail } = plan_trigger_binding(&source) else {
            panic!("an unmatched managed marker must not be accepted");
        };
        assert!(detail.contains("no matching end marker"));
    }

    #[test]
    fn stale_managed_binding_is_not_accepted_as_voisu() {
        let source = format!(
            "{BEGIN_MARKER}\no.bind(\"code:64\", \"Voisu dictation\", \"kitty\")\n{END_MARKER}\n"
        );

        let TriggerBindingPlan::Unparseable { detail } = plan_trigger_binding(&source) else {
            panic!("a managed binding with a different command must not be accepted");
        };
        assert!(detail.contains("expected description or command"));
    }

    #[test]
    fn lua_hex_escapes_are_decoded_before_conflict_detection() {
        let source = r#"o.bind("code\x3a64", "Launch terminal", "kitty")
o.bind("code:108", "Launcher", "launcher")"#;

        let TriggerBindingPlan::Conflicts { conflicts } = plan_trigger_binding(source) else {
            panic!("a Lua hexadecimal escape must still occupy Left Alt");
        };
        assert_eq!(conflicts[0].candidate, LEFT_ALT);
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
