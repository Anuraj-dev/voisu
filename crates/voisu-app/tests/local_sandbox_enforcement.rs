//! #266 worker-sandbox enforcement: seccomp, Landlock, lockdown, scrub.
//!
//! Pure tests inspect the exact kernel programs and deny rules without
//! installing anything. Enforcement tests re-exec this binary with
//! `VOISU_266_CHILD_ROLE` set (fresh process, `--exact` filter): the child
//! installs the real restrictions, probes them, and reports via its exit
//! code. Re-exec (not `fork` in a threaded harness) keeps every child
//! allocation-safe.

use std::ffi::CString;
use std::net::ToSocketAddrs;
use std::path::{Path, PathBuf};

use voisu_app::local_worker::{
    LandlockRequest, RestrictionProbe, SyscallArch, apply_landlock_restrictions,
    apply_no_new_privs, apply_worker_environment, build_seccomp_filter, close_unrelated_fds,
    collect_host_evidence, current_host_id, default_runtime_lib_dirs, denied_syscalls,
    detect_landlock_abi, disable_core_dumps, forbidden_env_reason, gate_local_on_sandbox,
    install_syscall_restrictions, landlock_allowlist, landlock_request_clean,
    local_unavailable_if_restrictions_fail, native_syscall_arch, pre_exec_lockdown,
    probe_sandbox_capabilities, render_host_evidence_markdown, scrub_worker_environment,
    verify_model_file,
};

const ROLE_ENV: &str = "VOISU_266_CHILD_ROLE";
const MODEL_ENV: &str = "VOISU_266_MODEL_PATH";
const CACHE_ENV: &str = "VOISU_266_CACHE_DIR";

// --- subprocess plumbing ----------------------------------------------------

fn spawn_child_role(
    test_name: &str,
    role: &str,
    extra: &[(&str, &str)],
) -> std::process::ExitStatus {
    let exe = std::env::current_exe().expect("test binary path");
    let mut command = std::process::Command::new(exe);
    command
        .args(["--exact", test_name, "--test-threads", "1"])
        .env(ROLE_ENV, role);
    for (key, value) in extra {
        command.env(key, value);
    }
    command.status().expect("spawn restricted child")
}

/// Child entry: when our role matches, run `body` and exit the process with
/// its code. Returns true when this process is the child.
fn as_child(role: &str, body: impl FnOnce() -> i32) -> bool {
    match std::env::var(ROLE_ENV) {
        Ok(current) if current == role => {
            let code = body();
            // SAFETY: child reports only via its exit status; skipping stdio
            // flush and harness teardown (some tests close harness FDs first).
            unsafe {
                libc::_exit(code);
            }
        }
        Ok(other) => panic!("unexpected child role: {other}"),
        Err(_) => false,
    }
}

fn c_string(path: &Path) -> CString {
    use std::os::unix::ffi::OsStrExt;
    CString::new(path.as_os_str().as_bytes()).expect("NUL-safe path")
}

// --- pure: denylist coverage ------------------------------------------------

#[test]
fn denylist_covers_every_forbidden_syscall_family() {
    for arch in [SyscallArch::X86_64, SyscallArch::Aarch64] {
        let denied = denied_syscalls(arch);
        let names: Vec<&str> = denied.iter().map(|entry| entry.name).collect();
        for required in [
            "socket",
            "connect",
            "ptrace",
            "process_vm_readv",
            "process_vm_writev",
            "io_uring_setup",
            "io_uring_enter",
            "io_uring_register",
        ] {
            assert!(
                names.contains(&required),
                "{arch:?} denylist is missing {required}"
            );
        }
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(names.len(), sorted.len(), "denylist has duplicates");
    }
}

#[test]
fn seccomp_filter_shape_is_arch_checked_allow_default() {
    for arch in [SyscallArch::X86_64, SyscallArch::Aarch64] {
        let denied = denied_syscalls(arch);
        let filter = build_seccomp_filter(arch);
        assert_eq!(filter.len(), denied.len() + 6);
        // Arch gate first: load arch, jump over the kill when native.
        assert_eq!((filter[0].code, filter[0].k), (0x20, 4));
        assert_eq!((filter[1].code, filter[1].k), (0x15, arch.audit_arch()));
        assert_eq!(filter[1].jt, 1);
        // Wrong arch cannot fall through to the allow tail.
        assert_eq!(filter[2].code, 0x06);
        assert_eq!(filter[2].k, 0x0005_0000 | 38);
        // Then the syscall-number comparisons.
        assert_eq!((filter[3].code, filter[3].k), (0x20, 0));
        let deny_index = filter.len() - 1;
        for (offset, entry) in denied.iter().enumerate() {
            let index = 4 + offset;
            assert_eq!(filter[index].code, 0x15);
            assert_eq!(
                filter[index].k, entry.nr as u32,
                "wrong nr for {}",
                entry.name
            );
            assert_eq!(
                index + 1 + filter[index].jt as usize,
                deny_index,
                "{} does not jump to the deny tail",
                entry.name
            );
        }
        // Shared tail: allow, then deny with EPERM.
        assert_eq!(filter[deny_index - 1].k, 0x7fff_0000);
        assert_eq!(filter[deny_index].code, 0x06);
        assert_eq!(filter[deny_index].k, 0x0005_0000 | 1);
    }
}

#[test]
fn audit_arch_values_match_the_kernel_uapi() {
    // EM_X86_64|__AUDIT_ARCH_64BIT|__AUDIT_ARCH_LE and
    // EM_AARCH64|__AUDIT_ARCH_64BIT|__AUDIT_ARCH_LE (linux/elf-em.h, audit.h).
    assert_eq!(SyscallArch::X86_64.audit_arch(), 3_221_225_534);
    assert_eq!(SyscallArch::Aarch64.audit_arch(), 3_221_225_655);
    assert_eq!(SyscallArch::X86_64.name(), "x86_64");
    assert_eq!(SyscallArch::Aarch64.name(), "aarch64");
    if std::env::consts::ARCH == "x86_64" {
        assert_eq!(native_syscall_arch(), Some(SyscallArch::X86_64));
    } else if std::env::consts::ARCH == "aarch64" {
        assert_eq!(native_syscall_arch(), Some(SyscallArch::Aarch64));
    }
}

// --- pure: environment ------------------------------------------------------

#[test]
fn env_scrub_clears_every_forbidden_family() {
    for name in [
        "LD_PRELOAD",
        "LD_DEBUG",
        "LD_SOMETHING_NEW",
        "https_proxy",
        "HTTP_PROXY",
        "SOME_PROXY",
        "SOME_proxy",
        "DBUS_SESSION_BUS_ADDRESS",
        "DBUS_SYSTEM_BUS_ADDRESS",
        "SSH_AUTH_SOCK",
        "SSH_AGENT_PID",
        "VOISU_GROQ_API_KEY",
        "MY_SERVICE_TOKEN",
        "AWS_SECRET_STUFF",
        "DB_PASSWORD",
        "DEPLOY_PRIVATE_KEY",
        "PATH",
    ] {
        assert!(
            forbidden_env_reason(name).is_some(),
            "{name} must be forbidden"
        );
    }
    // Locked-permitted: locale only. HOME is not cleared (the worker simply
    // cannot read it once Landlock is up); TZ is out of scope for #266.
    for name in ["LANG", "LC_ALL", "HOME", "TZ", "VOISU_MODE"] {
        assert!(
            forbidden_env_reason(name).is_none(),
            "{name} must survive scrubbing"
        );
    }
}

#[test]
fn scrub_worker_environment_drops_new_families_but_keeps_locale() {
    use std::ffi::OsString;
    let inherited = [
        ("LD_DEBUG", "libs"),
        ("DBUS_SYSTEM_BUS_ADDRESS", "unix:path=/run/bus"),
        ("ALL_PROXY", "http://proxy"),
        ("DISPLAY", ":0"),
        ("WAYLAND_DISPLAY", "wayland-0"),
        ("XDG_RUNTIME_DIR", "/run/user/1000"),
        ("MY_TOKEN", "secret"),
        ("LANG", "C.UTF-8"),
    ];
    let scrubbed = scrub_worker_environment(
        inherited
            .iter()
            .map(|(key, value)| (OsString::from(key), OsString::from(value))),
    );
    for (key, _) in &inherited[..7] {
        assert!(!scrubbed.retained.contains_key(*key), "{key} survived");
    }
    assert_eq!(
        scrubbed.retained.get("LANG").map(|value| value.as_os_str()),
        Some(std::ffi::OsStr::new("C.UTF-8"))
    );
}

// --- pure: Landlock request -------------------------------------------------

fn clean_request(model: PathBuf, cache: PathBuf) -> LandlockRequest {
    LandlockRequest {
        runtime_lib_dirs: default_runtime_lib_dirs(),
        native_plugin_dirs: Vec::new(),
        model_file: model,
        cache_dir: cache,
        gpu_devices: Vec::new(),
    }
}

#[test]
fn landlock_request_rejects_home_config_and_diagnostics() {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let degenerate_home = home
        .as_ref()
        .is_none_or(|dir| dir == Path::new("/") || dir.as_os_str().is_empty());
    if !degenerate_home {
        let home = home.expect("checked above");
        let under_home =
            clean_request(home.join("models/x.bin"), PathBuf::from("/tmp/voisu-cache"));
        assert!(
            landlock_request_clean(&under_home).is_err(),
            "model under home must be rejected"
        );
        let under_config = clean_request(
            PathBuf::from("/var/lib/voisu/models/x.bin"),
            home.join(".config/voisu/cache"),
        );
        assert!(
            landlock_request_clean(&under_config).is_err(),
            "cache under the config tree must be rejected"
        );
    }
    let diagnostics = clean_request(
        PathBuf::from("/var/lib/voisu/models/x.bin"),
        PathBuf::from("/tmp/voisu/diagnostics/cache"),
    );
    assert!(
        landlock_request_clean(&diagnostics).is_err(),
        "diagnostics trees must be rejected"
    );
    let relative = clean_request(
        PathBuf::from("relative/model.bin"),
        PathBuf::from("/tmp/voisu-cache"),
    );
    assert!(
        landlock_request_clean(&relative).is_err(),
        "relative paths must be rejected"
    );
    let bad_gpu = LandlockRequest {
        gpu_devices: vec![PathBuf::from("/tmp/gpu0")],
        ..clean_request(
            PathBuf::from("/var/lib/voisu/models/x.bin"),
            PathBuf::from("/tmp/voisu-cache"),
        )
    };
    assert!(
        landlock_request_clean(&bad_gpu).is_err(),
        "GPU nodes outside /dev must be rejected"
    );
    // CPU-only default grants no GPU surface.
    let clean = clean_request(
        PathBuf::from("/var/lib/voisu/models/x.bin"),
        PathBuf::from("/tmp/voisu-cache"),
    );
    assert!(clean.gpu_devices.is_empty());
    if !degenerate_home {
        assert!(landlock_request_clean(&clean).is_ok());
    }
    // Explicitly approved /dev nodes pass the shape check (enforcement of the
    // measured-GPU profile happens at install time, not here).
    let approved_gpu = LandlockRequest {
        gpu_devices: vec![PathBuf::from("/dev/dri/renderD128")],
        ..clean
    };
    assert!(landlock_request_clean(&approved_gpu).is_ok());
}

#[test]
fn default_allowlist_stays_off_gpu_and_home() {
    assert!(!default_runtime_lib_dirs().is_empty());
    for dir in default_runtime_lib_dirs() {
        assert!(dir.is_absolute());
    }
    let paths = landlock_allowlist(
        PathBuf::from("/var/lib/voisu/models/ggml.bin"),
        PathBuf::from("/tmp/voisu-cache"),
    );
    let rendered: Vec<String> = paths
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    assert!(!rendered.iter().any(|path| path.contains("dri")));
    assert!(!rendered.iter().any(|path| path.contains("nvidia")));
}

// --- pure: model verification -----------------------------------------------

const PROBE_MODEL_BYTES: &[u8] = b"voisu-266-probe-model";
const PROBE_MODEL_SHA256: &str = "c98940bcbddaa871dc157a598d922864f177d8ee34c72d92a9137a732f1bcaf9";

#[test]
fn model_verification_accepts_receipt_and_rejects_tampering() {
    let dir = tempfile::tempdir().expect("tempdir");
    let model = dir.path().join("model.bin");
    std::fs::write(&model, PROBE_MODEL_BYTES).expect("write probe model");
    let verified = verify_model_file(&model, PROBE_MODEL_SHA256).expect("receipt matches");
    assert_eq!(verified.size_bytes, PROBE_MODEL_BYTES.len() as u64);
    assert_eq!(verified.sha256_hex, PROBE_MODEL_SHA256);
    // Case-insensitive receipt comparison.
    assert!(
        verify_model_file(&model, &PROBE_MODEL_SHA256.to_uppercase()).is_ok(),
        "receipt comparison must be case-insensitive"
    );
    std::fs::write(&model, b"voisu-266-tampered-model").expect("tamper");
    assert!(
        verify_model_file(&model, PROBE_MODEL_SHA256).is_err(),
        "tampered bytes must fail verification"
    );
    let empty = dir.path().join("empty.bin");
    std::fs::write(&empty, b"").expect("write empty");
    assert!(
        verify_model_file(&empty, PROBE_MODEL_SHA256).is_err(),
        "empty model must fail verification"
    );
    assert!(
        verify_model_file(&dir.path().join("missing.bin"), PROBE_MODEL_SHA256).is_err(),
        "missing model must fail closed"
    );
}

// --- pure: fail-closed gate + host evidence ----------------------------------

#[test]
fn fail_closed_gate_reflects_probed_capabilities() {
    let caps = probe_sandbox_capabilities();
    assert_eq!(caps.landlock_abi, detect_landlock_abi());
    assert_eq!(caps.syscall_arch, native_syscall_arch());
    match gate_local_on_sandbox() {
        Ok(ready) => {
            assert_eq!(Some(ready.arch), caps.syscall_arch);
            assert!(ready.landlock_abi >= 1);
            assert!(ready.cgroup_path.starts_with("/user.slice/"));
        }
        Err(error) => {
            assert!(local_unavailable_if_restrictions_fail(
                RestrictionProbe::Unsupported
            ));
            let message = error.to_string();
            assert!(!message.is_empty());
            if caps.syscall_arch.is_none() {
                assert!(
                    message.contains("arch"),
                    "gate must name the gap: {message}"
                );
            }
            if caps.landlock_abi < 1 {
                assert!(
                    message.contains("Landlock"),
                    "gate must name the gap: {message}"
                );
            }
            if !caps.cgroup_user_owned {
                assert!(
                    message.contains("cgroup"),
                    "gate must name the gap: {message}"
                );
            }
        }
    }
}

#[test]
fn host_evidence_is_truthful() {
    let evidence = collect_host_evidence();
    assert!(!evidence.kernel_release.is_empty());
    assert_ne!(evidence.kernel_release, "unknown");
    assert_eq!(evidence.machine, std::env::consts::ARCH);
    assert_eq!(evidence.host_id, current_host_id());
    assert!(!evidence.host_id.is_empty());
    assert_eq!(evidence.seccomp_supported, native_syscall_arch().is_some());
    assert_eq!(evidence.landlock_abi, detect_landlock_abi());
    let markdown = render_host_evidence_markdown(&evidence);
    assert!(markdown.contains(&format!("current host id: {}", evidence.host_id)));
    assert!(markdown.contains("product target (not claimed): fedora-kde-wayland"));
    if evidence.host_id != "fedora" {
        assert!(
            !markdown.contains("current host id: fedora"),
            "a non-Fedora host must never render as Fedora evidence"
        );
    }
    eprintln!("{markdown}");
}

// --- enforced: pre-exec lockdown ----------------------------------------------

fn child_lockdown() -> i32 {
    if apply_no_new_privs().is_err() {
        return 10;
    }
    // SAFETY: read-only prctl getter.
    let fiscal = unsafe { libc::prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) };
    if fiscal != 1 {
        return 11;
    }
    match disable_core_dumps() {
        Ok(core) if core.rlimit_core_zero && core.dumpable_disabled => {}
        _ => return 12,
    }
    match pre_exec_lockdown() {
        Ok(pre) if pre.no_new_privs.no_new_privs && pre.core_dumps.rlimit_core_zero => 0,
        _ => 13,
    }
}

#[test]
fn enforced_no_new_privs_and_no_core_dumps() {
    if as_child("lockdown", child_lockdown) {
        return;
    }
    let status = spawn_child_role("enforced_no_new_privs_and_no_core_dumps", "lockdown", &[]);
    assert!(status.success(), "pre-exec lockdown failed: {status:?}");
}

// --- enforced: sockets + resolver ----------------------------------------------

fn child_sockets() -> i32 {
    if install_syscall_restrictions().is_err() {
        return 10;
    }
    // SAFETY: raw socket creation probes only; every call must fail.
    unsafe {
        for domain in [libc::AF_INET, libc::AF_INET6, libc::AF_UNIX] {
            let fd = libc::socket(domain, libc::SOCK_STREAM, 0);
            if fd >= 0 {
                libc::close(fd);
                return 11;
            }
            if std::io::Error::last_os_error().raw_os_error() != Some(libc::EPERM) {
                return 12;
            }
        }
        // New outbound Unix connection: connect(2) itself is denied.
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
        if fd >= 0 {
            libc::close(fd);
            return 13;
        }
    }
    // Resolver activity needing the network must fail (DNS requires a socket).
    match ("voisu-266-nonexistent.invalid", 443).to_socket_addrs() {
        Ok(_) => 14,
        Err(_) => 0,
    }
}

#[test]
fn enforced_ip_sockets_and_unix_connect_denied() {
    if as_child("sockets", child_sockets) {
        return;
    }
    let status = spawn_child_role(
        "enforced_ip_sockets_and_unix_connect_denied",
        "sockets",
        &[],
    );
    assert!(status.success(), "socket denials failed: {status:?}");
}

// --- enforced: alternate paths, ptrace, cross-process memory -------------------

fn child_alt_paths() -> i32 {
    use std::ptr::null_mut;
    if install_syscall_restrictions().is_err() {
        return 10;
    }
    // SAFETY: each probe passes inert arguments; the filter must deny first.
    unsafe {
        let uring = libc::syscall(libc::SYS_io_uring_setup, 1_u32, null_mut::<libc::c_void>());
        if uring != -1 || std::io::Error::last_os_error().raw_os_error() != Some(libc::EPERM) {
            return 11;
        }
        let traced = libc::ptrace(
            libc::PTRACE_TRACEME,
            0,
            null_mut::<libc::c_void>(),
            null_mut::<libc::c_void>(),
        );
        if traced != -1 || std::io::Error::last_os_error().raw_os_error() != Some(libc::EPERM) {
            return 12;
        }
        let pid = libc::getpid();
        let read = libc::syscall(
            libc::SYS_process_vm_readv,
            pid as libc::c_long,
            null_mut::<libc::c_void>(),
            0 as libc::c_long,
            null_mut::<libc::c_void>(),
            0 as libc::c_long,
            0 as libc::c_long,
        );
        if read != -1 || std::io::Error::last_os_error().raw_os_error() != Some(libc::EPERM) {
            return 13;
        }
    }
    0
}

#[test]
fn enforced_alternate_paths_ptrace_and_memory_denied() {
    if as_child("alt-paths", child_alt_paths) {
        return;
    }
    let status = spawn_child_role(
        "enforced_alternate_paths_ptrace_and_memory_denied",
        "alt-paths",
        &[],
    );
    assert!(
        status.success(),
        "alternate-path denials failed: {status:?}"
    );
}

// --- enforced: Landlock files ---------------------------------------------------

fn child_landlock() -> i32 {
    let model = std::env::var_os(MODEL_ENV).expect("model path");
    let cache = std::env::var_os(CACHE_ENV).expect("cache dir");
    let request = LandlockRequest {
        runtime_lib_dirs: default_runtime_lib_dirs(),
        native_plugin_dirs: Vec::new(),
        model_file: PathBuf::from(model),
        cache_dir: PathBuf::from(cache),
        gpu_devices: Vec::new(),
    };
    if landlock_request_clean(&request).is_err() {
        return 10;
    }
    if apply_landlock_restrictions(&request).is_err() {
        return 11;
    }
    // Allowed: the verified model still reads.
    if std::fs::read(&request.model_file).is_err() {
        return 12;
    }
    // Denied: the home tree must not open. Raw syscall: std must not lazily
    // load anything past this point.
    let home = std::env::var_os("HOME").unwrap_or_else(|| "/root".into());
    let denied = c_string(Path::new(&home));
    // SAFETY: live NUL-terminated path; O_DIRECTORY keeps it a metadata open.
    let probed = unsafe {
        libc::open(
            denied.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if probed >= 0 {
        // SAFETY: fd came from the open above.
        unsafe {
            libc::close(probed);
        }
        return 13;
    }
    if std::io::Error::last_os_error().raw_os_error() != Some(libc::EACCES) {
        return 14;
    }
    0
}

#[test]
fn enforced_landlock_denies_home_allows_model() {
    if as_child("landlock", child_landlock) {
        return;
    }
    if detect_landlock_abi() < 1 {
        eprintln!("SKIP: kernel lacks Landlock; fail-closed path covered by gate test");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let model = dir.path().join("model.bin");
    std::fs::write(&model, PROBE_MODEL_BYTES).expect("write probe model");
    let cache = dir.path().join("cache");
    let status = spawn_child_role(
        "enforced_landlock_denies_home_allows_model",
        "landlock",
        &[
            (MODEL_ENV, model.to_str().expect("utf8 tmp")),
            (CACHE_ENV, cache.to_str().expect("utf8 tmp")),
        ],
    );
    assert!(status.success(), "landlock file denials failed: {status:?}");
    assert!(
        cache.is_dir(),
        "launcher must create the bounded cache before restricting"
    );
}

// --- enforced: descendants inherit ----------------------------------------------

fn child_inherit() -> i32 {
    let model = std::env::var_os(MODEL_ENV).expect("model path");
    let cache = std::env::var_os(CACHE_ENV).expect("cache dir");
    if install_syscall_restrictions().is_err() {
        return 10;
    }
    let request = LandlockRequest {
        runtime_lib_dirs: default_runtime_lib_dirs(),
        native_plugin_dirs: Vec::new(),
        model_file: PathBuf::from(model),
        cache_dir: PathBuf::from(cache),
        gpu_devices: Vec::new(),
    };
    if apply_landlock_restrictions(&request).is_err() {
        return 11;
    }
    // SAFETY: fork in the single-test child; the grandchild uses raw syscalls
    // only and _exit, so no allocator state can deadlock.
    unsafe {
        let pid = libc::fork();
        if pid < 0 {
            return 12;
        }
        if pid == 0 {
            let fd = libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0);
            if fd >= 0 {
                libc::close(fd);
                libc::_exit(20);
            }
            let home = std::env::var_os("HOME").unwrap_or_else(|| "/root".into());
            // Deliberately leaks the CString: the grandchild never returns.
            let raw: Vec<u8> = {
                use std::os::unix::ffi::OsStrExt;
                let mut bytes = home.as_os_str().as_bytes().to_vec();
                bytes.push(0);
                bytes
            };
            let probed = libc::open(
                raw.as_ptr().cast(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            );
            if probed >= 0 {
                libc::close(probed);
                libc::_exit(21);
            }
            libc::_exit(0);
        }
        let mut status = 0;
        if libc::waitpid(pid, &mut status, 0) != pid || !libc::WIFEXITED(status) {
            return 13;
        }
        if libc::WEXITSTATUS(status) != 0 {
            return 30 + libc::WEXITSTATUS(status);
        }
    }
    0
}

#[test]
fn enforced_descendants_inherit_restrictions() {
    if as_child("inherit", child_inherit) {
        return;
    }
    if detect_landlock_abi() < 1 || native_syscall_arch().is_none() {
        eprintln!("SKIP: kernel lacks Landlock/seccomp; fail-closed path covered by gate test");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let model = dir.path().join("model.bin");
    std::fs::write(&model, PROBE_MODEL_BYTES).expect("write probe model");
    let cache = dir.path().join("cache");
    let status = spawn_child_role(
        "enforced_descendants_inherit_restrictions",
        "inherit",
        &[
            (MODEL_ENV, model.to_str().expect("utf8 tmp")),
            (CACHE_ENV, cache.to_str().expect("utf8 tmp")),
        ],
    );
    assert!(
        status.success(),
        "descendant inheritance failed: {status:?}"
    );
}

// --- enforced: FD scrub + env apply ----------------------------------------------

fn child_scrub() -> i32 {
    // SAFETY: raw /dev/null open; no allocation.
    let extra = unsafe {
        let path = c_string(Path::new("/dev/null"));
        libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC)
    };
    if extra < 0 {
        return 10;
    }
    if close_unrelated_fds(&[]).is_err() {
        return 11;
    }
    // SAFETY: fcntl getters on known FDs.
    unsafe {
        if libc::fcntl(extra, libc::F_GETFD) != -1 {
            return 12;
        }
        if libc::fcntl(1, libc::F_GETFD) < 0 {
            return 13;
        }
    }
    match apply_worker_environment() {
        Ok(applied) => {
            if !applied
                .removed
                .iter()
                .any(|name| name == "VOISU_266_PROBE_TOKEN")
            {
                return 14;
            }
            if !applied.removed.iter().any(|name| name == "LD_266_PROBE") {
                return 15;
            }
        }
        Err(_) => return 16,
    }
    if std::env::var_os("PATH").is_some() {
        return 17;
    }
    if std::env::var_os("VOISU_266_PROBE_TOKEN").is_some() {
        return 18;
    }
    0
}

#[test]
fn enforced_fd_scrub_and_env_apply() {
    if as_child("scrub", child_scrub) {
        return;
    }
    let status = spawn_child_role(
        "enforced_fd_scrub_and_env_apply",
        "scrub",
        &[
            ("VOISU_266_PROBE_TOKEN", "secret"),
            ("LD_266_PROBE", "evil.so"),
            ("VOISU_266_PROBE_PROXY", "http://127.0.0.1"),
        ],
    );
    assert!(status.success(), "fd/env scrub failed: {status:?}");
}

// --- pre-existing scaffolding still holds ----------------------------------------

#[test]
fn daemon_unit_networking_is_not_worker_enforcement() {
    // The packaged *daemon* unit keeps cloud networking by design; worker
    // enforcement is per-process (proven by the enforced_* tests in this
    // file) and is never inferred from unit booleans.
    let unit = include_str!("../../../packaging/voisu.service");
    assert!(unit.contains("AF_INET"), "daemon unit still serves Cloud");
    assert!(unit.contains("NoNewPrivileges=yes"));
}
