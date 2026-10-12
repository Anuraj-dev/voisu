//! Offline builder and runtime tree boundary. No ambient Python or pip settings.
use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Component, Path};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::consented::{ConsentedError, ConsentedLayout, check_deadline};
use super::consented_catalog::ConsentedCatalogEntry;
use super::{INSTALL_DEADLINE, InstallClock};

pub struct RuntimeBuild<'a> {
    pub staging: &'a ConsentedLayout,
    pub published: &'a ConsentedLayout,
    pub entry: &'a ConsentedCatalogEntry,
    pub clock: &'a InstallClock,
}

/// Tests inject a builder; production extracts only previously verified inputs.
pub trait ConsentedRuntimeBuilder {
    fn build(&self, build: &RuntimeBuild<'_>) -> Result<(), ConsentedError>;
}

#[derive(Default)]
pub struct OfflineRuntimeBuilder;

/// Command boundary for offline builder tests; no shell interpolation.
pub trait ConsentedCommandRunner {
    fn run(
        &self,
        command: &mut Command,
        staging: &Path,
        clock: &InstallClock,
    ) -> Result<Vec<u8>, ConsentedError>;
}
pub struct SystemConsentedCommandRunner;
impl ConsentedCommandRunner for SystemConsentedCommandRunner {
    fn run(
        &self,
        command: &mut Command,
        staging: &Path,
        clock: &InstallClock,
    ) -> Result<Vec<u8>, ConsentedError> {
        run_child(command, staging, clock)
    }
}
impl ConsentedRuntimeBuilder for OfflineRuntimeBuilder {
    fn build(&self, build: &RuntimeBuild<'_>) -> Result<(), ConsentedError> {
        self.build_with_runner(build, &SystemConsentedCommandRunner)
    }
}
impl OfflineRuntimeBuilder {
    pub fn build_with_runner(
        &self,
        build: &RuntimeBuild<'_>,
        runner: &dyn ConsentedCommandRunner,
    ) -> Result<(), ConsentedError> {
        let root = &build.staging.root;
        let archive = root.join("inputs").join(build.entry.python.name);
        let runtime = root.join("runtime");
        let listing = prepared_run(
            runner,
            Command::new("/usr/bin/tar").arg("-tzf").arg(&archive),
            root,
            build.clock,
        )?;
        let listing = std::str::from_utf8(&listing)
            .map_err(|_| ConsentedError::Tree("archive names are not UTF-8".into()))?;
        for name in listing.lines() {
            let path = Path::new(name);
            if path.is_absolute()
                || path
                    .components()
                    .any(|part| matches!(part, Component::ParentDir | Component::Prefix(_)))
                || !path.starts_with("python")
            {
                return Err(ConsentedError::Tree("unsafe archive path".into()));
            }
        }
        // GNU tar rejects traversal and extraction through escaping directory
        // symlinks. Audit all link targets and special modes before executing Python.
        prepared_run(
            runner,
            Command::new("/usr/bin/tar")
                .args(["-xzvf"])
                .arg(&archive)
                .args(["--no-same-owner", "--no-same-permissions"])
                .arg("-C")
                .arg(&runtime),
            root,
            build.clock,
        )?;
        audit_runtime_tree(&runtime)?;
        prepared_run(
            runner,
            Command::new(build.staging.python.join("bin/python3"))
                .args(["-I", "-m", "venv", "--copies"])
                .arg(&build.staging.venv),
            root,
            build.clock,
        )?;
        let requirements = build
            .entry
            .wheels
            .iter()
            .map(|pin| {
                format!(
                    "{} --hash=sha256:{}\n",
                    pin.requirement.expect("wheel requirement"),
                    pin.sha256
                )
            })
            .collect::<String>();
        let requirements_path = root.join("inputs/requirements.txt");
        fs::write(&requirements_path, requirements)?;
        prepared_run(
            runner,
            Command::new(build.staging.venv.join("bin/python"))
                .args([
                    "-I",
                    "-m",
                    "pip",
                    "install",
                    "--no-index",
                    "--no-deps",
                    "--require-hashes",
                    "--only-binary=:all:",
                    "--no-cache-dir",
                    "--find-links",
                ])
                .arg(root.join("inputs/wheels"))
                .arg("-r")
                .arg(requirements_path),
            root,
            build.clock,
        )?;
        relocate_venv(build)?;
        audit_runtime_tree(&runtime)?;
        Ok(())
    }
}

// venv records absolute paths even with --copies. Rewrite pyvenv.cfg and text
// entrypoints before atomic rename, so the runtime never depends on staging.
pub(super) fn relocate_venv(build: &RuntimeBuild<'_>) -> Result<(), ConsentedError> {
    let old = build
        .staging
        .root
        .to_str()
        .ok_or_else(|| ConsentedError::Build("non-UTF-8 store".into()))?;
    let new = build
        .published
        .root
        .to_str()
        .ok_or_else(|| ConsentedError::Build("non-UTF-8 store".into()))?;
    let mut paths = vec![build.staging.venv.join("pyvenv.cfg")];
    for file in fs::read_dir(build.staging.venv.join("bin"))? {
        paths.push(file?.path());
    }
    for path in paths {
        if !fs::symlink_metadata(&path)?.is_file() {
            continue;
        }
        let bytes = fs::read(&path)?;
        if let Ok(text) = std::str::from_utf8(&bytes)
            && text.contains(old)
        {
            fs::write(path, text.replace(old, new))?;
        }
    }
    Ok(())
}

pub(super) fn prepared_run(
    runner: &dyn ConsentedCommandRunner,
    command: &mut Command,
    staging: &Path,
    clock: &InstallClock,
) -> Result<Vec<u8>, ConsentedError> {
    check_deadline(clock)?;
    let home = staging.join("build-home");
    let tmp = staging.join("build-tmp");
    super::ensure_private_dir(&home).map_err(ConsentedError::Fs)?;
    super::ensure_private_dir(&tmp).map_err(ConsentedError::Fs)?;
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &home)
        .env("TMPDIR", &tmp)
        .env("PYTHONNOUSERSITE", "1")
        // pip treats os.devnull specially and skips ALL configuration files,
        // including global/XDG and venv config. Leave pip out of --isolated
        // mode so the explicit PIP_* controls below remain effective.
        .env("PIP_CONFIG_FILE", "/dev/null")
        .env("PIP_NO_INDEX", "1")
        .env("PIP_DISABLE_PIP_VERSION_CHECK", "1")
        .env("PIP_NO_INPUT", "1")
        .env("LANG", "C")
        .current_dir(staging)
        .stdin(Stdio::null())
        .process_group(0);
    let result = runner.run(command, staging, clock)?;
    check_deadline(clock)?;
    Ok(result)
}

struct ChildGroup {
    child: std::process::Child,
    pgid: i32,
}
impl Drop for ChildGroup {
    fn drop(&mut self) {
        // SAFETY: this is our own child's group, created with process_group(0).
        // Reap on every error path, including failed metadata reads.
        unsafe {
            libc::killpg(self.pgid, libc::SIGKILL);
        }
        let _ = self.child.wait();
    }
}

fn run_child(
    command: &mut Command,
    staging: &Path,
    clock: &InstallClock,
) -> Result<Vec<u8>, ConsentedError> {
    check_deadline(clock)?;
    let mut stdout = tempfile::tempfile_in(staging)?;
    let mut stderr = tempfile::tempfile_in(staging)?;
    command
        .stdout(stdout.try_clone()?)
        .stderr(stderr.try_clone()?);
    let child = command.spawn()?;
    let pgid = i32::try_from(child.id()).expect("Linux pid fits i32");
    let mut group = ChildGroup { child, pgid };
    let started = Instant::now();
    // The shared 30-minute install deadline covers extraction/install/fsync on
    // slow storage (~1.4 GB wheels, ~2.3 GB expanded). Quiet pip unpacking must
    // not be judged by stdout activity. The clock includes all previous phases.
    let result = loop {
        let next = stdout.metadata()?.len() + stderr.metadata()?.len();
        if next > 4 * 1024 * 1024 {
            break Err(ConsentedError::Build("child output cap".into()));
        }
        if clock.elapsed() > INSTALL_DEADLINE || started.elapsed() > INSTALL_DEADLINE {
            break Err(ConsentedError::Deadline);
        }
        match group.child.try_wait() {
            Ok(Some(status)) if status.success() => break Ok(()),
            Ok(Some(status)) => break Err(ConsentedError::Build(format!("child exited {status}"))),
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(error) => break Err(error.into()),
        }
    };
    drop(group);
    if let Err(error) = result {
        if let ConsentedError::Build(reason) = error {
            use std::io::{Seek, SeekFrom};
            stderr.seek(SeekFrom::Start(0))?;
            let mut detail = String::new();
            stderr.take(4096).read_to_string(&mut detail)?;
            return Err(ConsentedError::Build(format!("{reason}: {detail}")));
        }
        return Err(error);
    }
    use std::io::{Seek, SeekFrom};
    stdout.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    stdout.take(4 * 1024 * 1024).read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Full tree audit, without executing files. Symlinks must resolve inside root;
/// absolute links are refused so publication can safely relocate the tree.
pub fn audit_runtime_tree(root: &Path) -> Result<(), ConsentedError> {
    if root
        .components()
        .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
    {
        return Err(ConsentedError::Tree("runtime traversal".into()));
    }
    if !super::safe_fs::dir_present(root).map_err(ConsentedError::Fs)? {
        return Err(ConsentedError::Tree("missing runtime root".into()));
    }
    let canonical = fs::canonicalize(root)?;
    audit_node(root, &canonical)
}

fn audit_node(path: &Path, root: &Path) -> Result<(), ConsentedError> {
    let meta = fs::symlink_metadata(path)?;
    // SAFETY: geteuid has no pointer arguments or preconditions.
    if meta.uid() != unsafe { libc::geteuid() }
        || (!meta.file_type().is_symlink() && meta.mode() & 0o6022 != 0)
    {
        return Err(ConsentedError::Tree(format!(
            "unsafe owner or mode: {}",
            path.display()
        )));
    }
    if meta.file_type().is_symlink() {
        let target = fs::read_link(path)?;
        if target.is_absolute() || !fs::canonicalize(path)?.starts_with(root) {
            return Err(ConsentedError::Tree(format!(
                "escaping symlink: {}",
                path.display()
            )));
        }
    } else if meta.is_dir() {
        for child in fs::read_dir(path)? {
            audit_node(&child?.path(), root)?;
        }
    } else if !meta.is_file() || meta.nlink() != 1 {
        return Err(ConsentedError::Tree(format!(
            "special node: {}",
            path.display()
        )));
    }
    Ok(())
}

pub(super) fn sync_tree(path: &Path) -> Result<(), ConsentedError> {
    let meta = fs::symlink_metadata(path)?;
    if meta.is_dir() {
        for child in fs::read_dir(path)? {
            sync_tree(&child?.path())?;
        }
    }
    if !meta.file_type().is_symlink() {
        File::open(path)?.sync_all()?;
    }
    Ok(())
}

pub(super) fn executable(path: &Path) -> Result<(), ConsentedError> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_file() || meta.permissions().mode() & 0o100 == 0 {
        return Err(ConsentedError::Tree(
            "venv Python must be a regular executable".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::consented_catalog::CONSENTED_ENTRY;
    use super::*;
    use std::cell::Cell;
    use std::os::unix::fs::symlink;

    struct FakeTarPython {
        calls: Cell<usize>,
        bad_listing: Option<&'static str>,
    }
    impl ConsentedCommandRunner for FakeTarPython {
        fn run(
            &self,
            command: &mut Command,
            staging: &Path,
            _: &InstallClock,
        ) -> Result<Vec<u8>, ConsentedError> {
            let call = self.calls.get();
            self.calls.set(call + 1);
            let env = command
                .get_envs()
                .map(|(key, value)| (key.to_str().unwrap(), value.unwrap().to_str().unwrap()))
                .collect::<std::collections::BTreeMap<_, _>>();
            assert_eq!(env.len(), 9);
            assert_eq!(env["PIP_CONFIG_FILE"], "/dev/null");
            assert_eq!(env["PIP_NO_INDEX"], "1");
            assert_eq!(env["PIP_DISABLE_PIP_VERSION_CHECK"], "1");
            assert_eq!(env["PIP_NO_INPUT"], "1");
            assert_eq!(env["PATH"], "/usr/bin:/bin");
            assert_eq!(env["LANG"], "C");
            assert_eq!(env["PYTHONNOUSERSITE"], "1");
            assert!(Path::new(env["HOME"]).starts_with(staging));
            assert!(Path::new(env["TMPDIR"]).starts_with(staging));
            let args = command
                .get_args()
                .map(|arg| arg.to_str().unwrap())
                .collect::<Vec<_>>();
            match call {
                0 => {
                    assert_eq!(command.get_program(), "/usr/bin/tar");
                    assert_eq!(args[0], "-tzf");
                    return Ok(self
                        .bad_listing
                        .unwrap_or("python/\npython/bin/python3\n")
                        .as_bytes()
                        .to_vec());
                }
                1 => {
                    assert!(args.contains(&"--no-same-owner"));
                    assert!(args.contains(&"--no-same-permissions"));
                    fs::create_dir_all(staging.join("runtime/python/bin"))?;
                    fs::write(staging.join("runtime/python/bin/python3"), b"fake Python")?;
                }
                2 => {
                    assert_eq!(&args[..4], &["-I", "-m", "venv", "--copies"]);
                    let venv = Path::new(args[4]);
                    fs::create_dir_all(venv.join("bin"))?;
                    fs::create_dir(venv.join("lib"))?;
                    symlink("lib", venv.join("lib64"))?;
                    fs::write(
                        venv.join("pyvenv.cfg"),
                        format!("home = {}\n", staging.join("runtime/python/bin").display()),
                    )?;
                    fs::write(venv.join("bin/python"), b"fake Python")?;
                    fs::set_permissions(
                        venv.join("bin/python"),
                        fs::Permissions::from_mode(0o700),
                    )?;
                    fs::write(
                        venv.join("bin/pip"),
                        format!("#!{}/runtime/venv/bin/python\n", staging.display()),
                    )?;
                }
                3 => {
                    assert_eq!(&args[..4], &["-I", "-m", "pip", "install"]);
                    for arg in [
                        "--no-index",
                        "--no-deps",
                        "--require-hashes",
                        "--only-binary=:all:",
                        "--no-cache-dir",
                        "--find-links",
                    ] {
                        assert!(args.contains(&arg));
                    }
                    let requirements = fs::read_to_string(staging.join("inputs/requirements.txt"))?;
                    assert_eq!(requirements.lines().count(), 26);
                    assert!(requirements.contains("ctranslate2==4.8.2 --hash=sha256:"));
                    assert!(requirements.contains("faster-whisper==1.2.1 --hash=sha256:"));
                }
                _ => panic!("unexpected child"),
            }
            Ok(vec![])
        }
    }
    #[test]
    fn consented_offline_builder_uses_isolated_hashed_commands_and_relocates_venv() {
        let temp = tempfile::tempdir().unwrap();
        let staging = ConsentedLayout::at(temp.path().join("staging"));
        let published = ConsentedLayout::at(temp.path().join("published"));
        for dir in ["runtime", "inputs/wheels"] {
            super::super::ensure_private_dir(&staging.root.join(dir)).unwrap();
        }
        let runner = FakeTarPython {
            calls: Cell::new(0),
            bad_listing: None,
        };
        OfflineRuntimeBuilder
            .build_with_runner(
                &RuntimeBuild {
                    staging: &staging,
                    published: &published,
                    entry: &CONSENTED_ENTRY,
                    clock: &InstallClock::default(),
                },
                &runner,
            )
            .unwrap();
        assert_eq!(runner.calls.get(), 4);
        let cfg = fs::read_to_string(staging.venv.join("pyvenv.cfg")).unwrap();
        assert!(cfg.contains(published.python.to_str().unwrap()));
        assert!(!cfg.contains(staging.root.to_str().unwrap()));
        let pip = fs::read_to_string(staging.venv.join("bin/pip")).unwrap();
        assert_eq!(pip, format!("#!{}\n", published.venv_python().display()));
        audit_runtime_tree(&staging.root).unwrap();
    }
    #[test]
    fn consented_archive_paths_are_rejected_before_extraction() {
        for listing in [
            "python/../../escape\n",
            "/python/bin/python3\n",
            "elsewhere/file\n",
        ] {
            let temp = tempfile::tempdir().unwrap();
            let staging = ConsentedLayout::at(temp.path().to_path_buf());
            let runner = FakeTarPython {
                calls: Cell::new(0),
                bad_listing: Some(listing),
            };
            let build = RuntimeBuild {
                staging: &staging,
                published: &staging,
                entry: &CONSENTED_ENTRY,
                clock: &InstallClock::default(),
            };
            assert!(matches!(
                OfflineRuntimeBuilder.build_with_runner(&build, &runner),
                Err(ConsentedError::Tree(_))
            ));
            assert_eq!(runner.calls.get(), 1);
        }
    }
    #[test]
    fn consented_real_child_environment_is_exactly_the_allowed_set() {
        let temp = tempfile::tempdir().unwrap();
        let mut command = Command::new("/usr/bin/env");
        // Command-local poison avoids racing other tests over process env.
        // /usr/bin/env also observes any real inherited host environment.
        for key in [
            "PIP_INDEX_URL",
            "PIP_FIND_LINKS",
            "PIP_TARGET",
            "PYTHONPATH",
            "LD_PRELOAD",
            "VOISU_POISON",
        ] {
            command.env(key, "poison");
        }
        let output = prepared_run(
            &SystemConsentedCommandRunner,
            &mut command,
            temp.path(),
            &InstallClock::default(),
        )
        .unwrap();
        let actual = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| {
                let (key, value) = line.split_once('=').unwrap();
                (key.to_owned(), value.to_owned())
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        let expected = [
            ("PATH", "/usr/bin:/bin".to_owned()),
            (
                "HOME",
                temp.path().join("build-home").to_str().unwrap().to_owned(),
            ),
            (
                "TMPDIR",
                temp.path().join("build-tmp").to_str().unwrap().to_owned(),
            ),
            ("PYTHONNOUSERSITE", "1".to_owned()),
            ("LANG", "C".to_owned()),
            ("PIP_CONFIG_FILE", "/dev/null".to_owned()),
            ("PIP_NO_INDEX", "1".to_owned()),
            ("PIP_DISABLE_PIP_VERSION_CHECK", "1".to_owned()),
            ("PIP_NO_INPUT", "1".to_owned()),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect();
        assert_eq!(actual, expected);
    }
    #[test]
    fn consented_pip_devnull_skips_global_user_and_site_configuration() {
        let temp = tempfile::tempdir().unwrap();
        // Use the host's bundled pip without installing it or fetching anything.
        // Redirect its real configuration loader to poisoned fixture files so
        // this test never writes /etc or the user's configuration directories.
        for name in ["global.conf", "user.conf", "site.conf"] {
            fs::write(temp.path().join(name), "[global]\nfind-links = https://invalid.test/\n[install]\ntarget = /invalid-target\n").unwrap();
        }
        let mut command = Command::new("python3");
        command.args([
            "-I",
            "-c",
            r#"
import os, pathlib, sys
try:
    import ensurepip
except ImportError:
    print('SKIP: host python3 has no bundled ensurepip')
    sys.exit(0)
wheel = next((pathlib.Path(ensurepip.__file__).parent / '_bundled').glob('pip-*.whl'), None)
if wheel is None:
    print('SKIP: host python3 ships ensurepip without a bundled pip wheel')
    sys.exit(0)
sys.path.insert(0, str(wheel))
from pip._internal import configuration as c
c.get_configuration_files = lambda: {
    c.kinds.GLOBAL: ['global.conf'],
    c.kinds.USER: ['user.conf'],
    c.kinds.SITE: ['site.conf'],
}
config = c.Configuration(isolated=False)
config.load()
assert config._parsers == {kind: [] for kind in c.OVERRIDE_ORDER}, config._parsers
assert not any(key in dict(config.items()) for key in ['global.find-links', 'install.target'])
assert os.environ['PIP_CONFIG_FILE'] == '/dev/null'
print('all configuration files skipped')
"#,
        ]);
        let output = match prepared_run(
            &SystemConsentedCommandRunner,
            &mut command,
            temp.path(),
            &InstallClock::default(),
        ) {
            Err(ConsentedError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                eprintln!(
                    "SKIP consented_pip_devnull_skips_global_user_and_site_configuration: no host python3"
                );
                return;
            }
            result => String::from_utf8(result.unwrap()).unwrap(),
        };
        if output.starts_with("SKIP:") {
            eprintln!("{output}");
        } else {
            assert_eq!(output.trim(), "all configuration files skipped");
        }
    }
    #[test]
    fn consented_quiet_child_can_pass_old_silence_window() {
        let temp = tempfile::tempdir().unwrap();
        let clock = InstallClock::default();
        // Exercise actual silence beyond the old watchdog, not a clock offset
        // (the old watchdog measured its own Instant and ignored that offset).
        let mut command = Command::new("/bin/sleep");
        command.arg((super::super::NO_PROGRESS.as_secs() + 1).to_string());
        assert!(
            prepared_run(
                &SystemConsentedCommandRunner,
                &mut command,
                temp.path(),
                &clock
            )
            .is_ok()
        );
    }
    #[test]
    fn consented_child_timeout_kills_its_process_group() {
        let temp = tempfile::tempdir().unwrap();
        let clock = InstallClock::default();
        let ticking = clock.clone();
        let marker = temp.path().join("child.pid");
        let observed = marker.clone();
        let updater = std::thread::spawn(move || {
            let start = Instant::now();
            let pid = loop {
                if let Ok(text) = fs::read_to_string(&observed)
                    && let Ok(pid) = text.parse::<u32>()
                {
                    break pid;
                }
                assert!(
                    start.elapsed() < Duration::from_secs(5),
                    "child never published its pid"
                );
                std::thread::sleep(Duration::from_millis(10));
            };
            ticking.advance(Duration::from_secs(1801));
            pid
        });
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "sleep 60 & printf '%s' $! > child.pid; wait"]);
        let result = prepared_run(&SystemConsentedCommandRunner, &mut cmd, temp.path(), &clock);
        let pid = updater.join().unwrap();
        assert!(matches!(result, Err(ConsentedError::Deadline)));
        // A grandchild can briefly remain as a zombie until the host reaps it;
        // it must never still be running after our group kill.
        let start = Instant::now();
        loop {
            match fs::read_to_string(format!("/proc/{pid}/stat")) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                Ok(stat) if stat.split_once(") ").unwrap().1.starts_with('Z') => break,
                _ if start.elapsed() < Duration::from_secs(2) => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                other => panic!("descendant survived: {other:?}"),
            }
        }
    }
}
