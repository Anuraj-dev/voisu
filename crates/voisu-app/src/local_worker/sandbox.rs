//! R3 offline threat-model spike: launcher policy, env scrub, unit inventory.
//!
//! Shared desktop IPC to PipeWire, the compositor, and Delivery remains
//! permitted. Local processing itself must perform no DNS or IP traffic,
//! including loopback, and must read no Cloud credentials.

use std::collections::BTreeMap;
use std::ffi::{CString, OsString};
use std::fmt;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::RawFd;
use std::path::{Path, PathBuf};

use super::bounds::WorkerCgroupSpec;

/// Capability sentinel: tests fail if Local work constructs a Cloud client.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CloudCapabilitySentinel {
    cloud_client_constructed: bool,
    credential_read: bool,
    ip_attempted: bool,
}

impl CloudCapabilitySentinel {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn construct_cloud_client(&mut self) {
        self.cloud_client_constructed = true;
    }

    pub fn read_cloud_credential(&mut self) {
        self.credential_read = true;
    }

    pub fn attempt_ip(&mut self) {
        self.ip_attempted = true;
    }

    #[must_use]
    pub fn local_path_clean(&self) -> bool {
        !self.cloud_client_constructed && !self.credential_read && !self.ip_attempted
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RestrictionProbe {
    Available,
    Unsupported,
}

/// Fail closed: Local is unavailable when Landlock/seccomp cannot be installed.
#[must_use]
pub fn local_unavailable_if_restrictions_fail(probe: RestrictionProbe) -> bool {
    matches!(probe, RestrictionProbe::Unsupported)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LauncherPolicy {
    pub no_new_privs: bool,
    pub disable_core_dumps: bool,
    pub deny_ip_sockets: bool,
    pub deny_new_outbound_unix: bool,
    pub native_syscall_arch_only: bool,
    pub deny_io_uring: bool,
    pub deny_ptrace: bool,
    pub landlock_required: bool,
}

impl LauncherPolicy {
    #[must_use]
    pub fn intended_production() -> Self {
        Self {
            no_new_privs: true,
            disable_core_dumps: true,
            deny_ip_sockets: true,
            deny_new_outbound_unix: true,
            native_syscall_arch_only: true,
            deny_io_uring: true,
            deny_ptrace: true,
            landlock_required: true,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackagedUnitRestrictions {
    pub memory_deny_write_execute: bool,
    pub restrict_namespaces: bool,
    pub no_new_privileges: bool,
    pub system_call_architectures_native: bool,
}

/// Inventory of `packaging/voisu.service` — do not silently weaken the unit.
#[must_use]
pub fn packaged_unit_restrictions(unit_text: &str) -> PackagedUnitRestrictions {
    PackagedUnitRestrictions {
        memory_deny_write_execute: unit_text.contains("MemoryDenyWriteExecute=yes"),
        restrict_namespaces: unit_text.contains("RestrictNamespaces=yes"),
        no_new_privileges: unit_text.contains("NoNewPrivileges=yes"),
        system_call_architectures_native: unit_text.contains("SystemCallArchitectures=native"),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScrubbedEnvironment {
    pub retained: BTreeMap<String, OsString>,
}

const FORBIDDEN_ENV: &[&str] = &[
    "http_proxy",
    "https_proxy",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "all_proxy",
    "NO_PROXY",
    "no_proxy",
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
    "LD_DEBUG",
    "LD_AUDIT",
    "GCONV_PATH",
    "HOSTALIASES",
    "RES_OPTIONS",
    "PYTHONPATH",
    "PYTHONHOME",
    "PERL5LIB",
    "PERLLIB",
    "RUBYLIB",
    "NODE_PATH",
    "DBUS_SESSION_BUS_ADDRESS",
    "SSH_AUTH_SOCK",
    "WAYLAND_DISPLAY",
    "DISPLAY",
    "XDG_RUNTIME_DIR",
    "VOISU_GROQ_API_KEY",
    "VOISU_DEEPGRAM_API_KEY",
    "GROQ_API_KEY",
    "DEEPGRAM_API_KEY",
    "OPENAI_API_KEY",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_ACCESS_KEY_ID",
];

/// Why one inherited variable must not reach the worker. Families (loader
/// overrides, proxies, session-bus addresses) are matched structurally so a
/// renamed variable cannot smuggle the same capability past the blocklist.
#[must_use]
pub fn forbidden_env_reason(name: &str) -> Option<&'static str> {
    if FORBIDDEN_ENV.contains(&name) {
        return Some("explicit blocklist");
    }
    if name.starts_with("LD_") {
        return Some("loader override family");
    }
    if name.ends_with("_PROXY") || name.ends_with("_proxy") {
        return Some("proxy family");
    }
    if name.starts_with("DBUS_") {
        return Some("session-bus family");
    }
    if name.starts_with("SSH_") {
        return Some("agent family");
    }
    if name.ends_with("_API_KEY")
        || name.contains("SECRET")
        || name.contains("TOKEN")
        || name.ends_with("_PASSWORD")
        || name.ends_with("_PRIVATE_KEY")
    {
        return Some("credential family");
    }
    if name == "PATH" {
        return Some("PATH dependence");
    }
    None
}

fn forbidden_reason_os(key: &OsString) -> Option<&'static str> {
    if let Some(name) = key.to_str() {
        return forbidden_env_reason(name);
    }
    // Non-UTF8 names cannot match the &str rules above; fail closed on
    // loader/proxy-shaped raw bytes rather than retaining them.
    let bytes = key.as_bytes();
    if bytes.starts_with(b"LD_") {
        return Some("loader override family");
    }
    if bytes.ends_with(b"_PROXY") || bytes.ends_with(b"_proxy") {
        return Some("proxy family");
    }
    None
}

/// Worker environment: no credentials, proxy, preload, or session-bus address.
/// Locale plus `HOME` are kept: the worker's Landlock request needs the home
/// reference for its rejection checks and its post-install effectiveness
/// probe (the domain itself denies every home access), so both scrub paths —
/// this spawn-time scrub and [`apply_worker_environment`] — agree on keeping
/// it instead of one silently skipping the home checks.
#[must_use]
pub fn scrub_worker_environment<I>(inherited: I) -> ScrubbedEnvironment
where
    I: IntoIterator<Item = (OsString, OsString)>,
{
    let mut retained = BTreeMap::new();
    for (key, value) in inherited {
        if forbidden_reason_os(&key).is_some() {
            continue;
        }
        let Some(name) = key.to_str() else {
            // Non-UTF8, non-loader-shaped names are dropped: the worker execs
            // by absolute path and must not depend on inherited locale bytes.
            continue;
        };
        // Absolute worker exec + ld.so (DT_RPATH/ldconfig) resolve libraries.
        // PATH is not retained so a descendant cannot exec an unexpected binary.
        if name == "LANG" || name == "LC_ALL" || name == "HOME" {
            retained.insert(name.to_owned(), value);
        }
    }
    ScrubbedEnvironment { retained }
}

/// Landlock allowlist for the CPU-only L2 spike: runtime libs, chosen model,
/// private cache. GPU device nodes are omitted until a supported GPU host
/// profile is measured; adding `/dev/dri` or NVIDIA nodes without that
/// measurement would over-allow.
#[must_use]
pub fn landlock_allowlist(model: PathBuf, cache: PathBuf) -> Vec<PathBuf> {
    vec![
        PathBuf::from("/usr/lib"),
        PathBuf::from("/lib"),
        PathBuf::from("/usr/lib64"),
        PathBuf::from("/lib64"),
        PathBuf::from("/dev/null"),
        model,
        cache,
    ]
}

// --- #266 enforcement: fail-closed launcher shapes --------------------------
//
// The policy structs above describe intent. Everything below either performs a
// kernel restriction in the calling worker process (and verifies it) or
// returns [`SandboxError`] so the launcher fails Local closed. Call order in
// the worker launcher, before model or native-plugin load:
//   1. [`verify_model_file`] — the model bytes are the verified ones.
//   2. [`apply_worker_environment`] + [`close_unrelated_fds`] — scrub.
//   3. [`pre_exec_lockdown`] — no_new_privs + no core dumps.
//   4. [`apply_landlock_restrictions`] — filesystem allowlist.
//   5. [`install_syscall_restrictions`] — arch-validated seccomp denylist.
//   6. [`join_worker_cgroup`] — resource envelope (before model load).
// Step 6 needs the user manager; steps 3–5 need kernel support. Any failure
// aborts the worker and [`gate_local_on_sandbox`] reports Local unavailable.
// (The call site itself belongs to the supervised lifecycle in #265's files;
// this module must not grow process-spawning or D-Bus code that collides with
// that ticket.)

/// Fail-closed launcher failure: every variant means Local is unavailable,
/// never degraded.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SandboxError {
    /// A restriction could not be installed or verified on this host.
    Unavailable(String),
    /// The kernel or user manager lacks a required capability.
    Unsupported(String),
    /// A restriction was requested but post-install verification failed.
    VerificationFailed(String),
}

impl fmt::Display for SandboxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(detail) => write!(f, "worker sandbox unavailable: {detail}"),
            Self::Unsupported(detail) => write!(f, "worker sandbox unsupported: {detail}"),
            Self::VerificationFailed(detail) => {
                write!(f, "worker sandbox verification failed: {detail}")
            }
        }
    }
}

impl std::error::Error for SandboxError {}

fn last_os_error(context: &str) -> SandboxError {
    SandboxError::Unavailable(format!("{context}: {}", io::Error::last_os_error()))
}

// --- no_new_privs + core dumps (before model/native-plugin load) -------------

/// Verified `PR_SET_NO_NEW_PRIVS` state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NoNewPrivsEvidence {
    pub no_new_privs: bool,
}

/// Read back the calling process's no_new_privs bit. Pure getter, safe to call
/// anywhere including the daemon (it reports state, it changes nothing).
#[must_use]
pub fn is_no_new_privs() -> bool {
    // SAFETY: prctl getter with zero arguments; no side effects.
    let bit = unsafe { libc::prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) };
    bit == 1
}

/// Set no_new_privs and verify the bit stuck. Irreversible for the calling
/// process: the launcher calls this only in the worker child, never in the
/// daemon.
pub fn apply_no_new_privs() -> Result<NoNewPrivsEvidence, SandboxError> {
    // SAFETY: prctl setter with constant arguments.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(last_os_error("PR_SET_NO_NEW_PRIVS failed"));
    }
    if is_no_new_privs() {
        Ok(NoNewPrivsEvidence { no_new_privs: true })
    } else {
        Err(SandboxError::VerificationFailed(
            "PR_SET_NO_NEW_PRIVS returned success but the bit reads back clear".into(),
        ))
    }
}

/// Verified core-dump policy: zero `RLIMIT_CORE` plus non-dumpable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CoreDumpEvidence {
    pub rlimit_core_zero: bool,
    pub dumpable_disabled: bool,
}

/// Getter: soft `RLIMIT_CORE` is already zero. Safe to call anywhere.
#[must_use]
pub fn core_rlimit_zero() -> bool {
    // SAFETY: getrlimit writes to a live stack struct.
    let mut limit: libc::rlimit = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrlimit(libc::RLIMIT_CORE, &mut limit) } != 0 {
        return false;
    }
    limit.rlim_cur == 0
}

/// Getter: the process is already non-dumpable (`PR_GET_DUMPABLE == 0`).
#[must_use]
pub fn is_dumpable_disabled() -> bool {
    // SAFETY: prctl getter with zero arguments; no side effects.
    let dumpable = unsafe { libc::prctl(libc::PR_GET_DUMPABLE, 0, 0, 0, 0) };
    dumpable == 0
}

/// Disable core dumps (model weights must never land in a crash dump) and
/// verify both knobs. Call in the worker child before model load: lowering the
/// hard limit is irreversible for the calling process.
pub fn disable_core_dumps() -> Result<CoreDumpEvidence, SandboxError> {
    let zero = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: setrlimit with a live stack struct; zeroes core dumps only.
    if unsafe { libc::setrlimit(libc::RLIMIT_CORE, &zero) } != 0 {
        return Err(last_os_error("setrlimit(RLIMIT_CORE, 0) failed"));
    }
    // SAFETY: prctl setter with constant arguments.
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
        return Err(last_os_error("PR_SET_DUMPABLE(0) failed"));
    }
    let evidence = CoreDumpEvidence {
        rlimit_core_zero: core_rlimit_zero(),
        dumpable_disabled: is_dumpable_disabled(),
    };
    if evidence.rlimit_core_zero && evidence.dumpable_disabled {
        Ok(evidence)
    } else {
        Err(SandboxError::VerificationFailed(
            "core-dump knobs did not read back disabled after setting them".into(),
        ))
    }
}

/// Combined pre-exec lockdown. no_new_privs first: it is a precondition for
/// installing unprivileged seccomp filters in [`install_syscall_restrictions`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PreExecEvidence {
    pub no_new_privs: NoNewPrivsEvidence,
    pub core_dumps: CoreDumpEvidence,
}

/// Run [`apply_no_new_privs`] then [`disable_core_dumps`]. Worker child only,
/// before model or native-plugin load.
pub fn pre_exec_lockdown() -> Result<PreExecEvidence, SandboxError> {
    Ok(PreExecEvidence {
        no_new_privs: apply_no_new_privs()?,
        core_dumps: disable_core_dumps()?,
    })
}

// --- architecture-validated syscall restrictions ----------------------------
//
// Classic-BPF denylist installed with `PR_SET_SECCOMP, SECCOMP_MODE_FILTER`.
// UAPI values below come from linux/audit.h, linux/seccomp.h,
// linux/filter.h, and linux/bpf_common.h; syscall numbers from the kernel
// unistd tables for each native arch. Only x86_64 and aarch64 are validated:
// any other machine fails closed via [`native_syscall_arch`].

// `EM_X86_64 | __AUDIT_ARCH_64BIT | __AUDIT_ARCH_LE` (linux/elf-em.h, audit.h).
const AUDIT_ARCH_X86_64: u32 = 62 | 0x8000_0000 | 0x4000_0000;
// `EM_AARCH64 | __AUDIT_ARCH_64BIT | __AUDIT_ARCH_LE`.
const AUDIT_ARCH_AARCH64: u32 = 183 | 0x8000_0000 | 0x4000_0000;

// BPF_LD|BPF_W|BPF_ABS, BPF_JMP|BPF_JEQ|BPF_K, BPF_JMP|BPF_JGT|BPF_K,
// BPF_RET|BPF_K.
const BPF_STMT_LD_W_ABS: u16 = 0x20;
const BPF_JUMP_JEQ_K: u16 = 0x15;
const BPF_JUMP_JGT_K: u16 = 0x35;
const BPF_STMT_RET_K: u16 = 0x06;

// struct seccomp_data offsets: int nr @0, __u32 arch @4.
const SECCOMP_DATA_NR_OFFSET: u32 = 0;
const SECCOMP_DATA_ARCH_OFFSET: u32 = 4;

// Linux errno values (arch-independent for these two).
const ERRNO_EPERM: u32 = 1;
const ERRNO_ENOSYS: u32 = 38;

// Highest syscall number a native ABI can use. x32 syscalls carry the native
// `AUDIT_ARCH_X86_64` in seccomp_data.arch but set `__X32_SYSCALL_BIT`
// (0x40000000) in nr, so on x86_64 any nr above this ceiling is an x32
// number, never a native one — and the nr comparisons below must never see it.
const X32_NR_CEILING: u32 = 0x3FFF_FFFF;

/// Native syscall architecture the denylist below is validated for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyscallArch {
    X86_64,
    Aarch64,
}

impl SyscallArch {
    #[must_use]
    pub fn audit_arch(self) -> u32 {
        match self {
            Self::X86_64 => AUDIT_ARCH_X86_64,
            Self::Aarch64 => AUDIT_ARCH_AARCH64,
        }
    }

    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::X86_64 => "x86_64",
            Self::Aarch64 => "aarch64",
        }
    }
}

/// One denied syscall: stable name plus native number for the arch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeniedSyscall {
    pub name: &'static str,
    pub nr: i64,
}

/// Detect the native syscall arch from `uname(2)`. `None` on any other machine
/// or when uname fails: the caller fails Local closed, never installs a
/// filter built for the wrong table.
#[must_use]
pub fn native_syscall_arch() -> Option<SyscallArch> {
    // SAFETY: uname writes to a live stack struct.
    let mut name: libc::utsname = unsafe { std::mem::zeroed() };
    if unsafe { libc::uname(&mut name) } != 0 {
        return None;
    }
    // SAFETY: the kernel NUL-terminates utsname fields; bound the read.
    let machine = unsafe {
        let bytes = &*(&name.machine as *const _ as *const [u8; 65]);
        let len = bytes.iter().position(|byte| *byte == 0).unwrap_or(65);
        std::str::from_utf8(&bytes[..len]).ok()?
    };
    match machine {
        "x86_64" => Some(SyscallArch::X86_64),
        "aarch64" => Some(SyscallArch::Aarch64),
        _ => None,
    }
}

/// Denylist for one arch: IP socket creation, new connections (Unix and IP —
/// the worker only uses pre-opened control FDs), alternate submission via
/// io_uring, ptrace, and cross-process memory access. Deliberately a denylist:
/// innocent libc audio/file syscalls keep working, while every network or
/// introspection path the issue names is closed. No loopback exception: loopback
/// is IP traffic and stays denied.
#[must_use]
pub fn denied_syscalls(arch: SyscallArch) -> Vec<DeniedSyscall> {
    match arch {
        SyscallArch::X86_64 => vec![
            DeniedSyscall {
                name: "socket",
                nr: 41,
            },
            DeniedSyscall {
                name: "connect",
                nr: 42,
            },
            DeniedSyscall {
                name: "ptrace",
                nr: 101,
            },
            DeniedSyscall {
                name: "process_vm_readv",
                nr: 310,
            },
            DeniedSyscall {
                name: "process_vm_writev",
                nr: 311,
            },
            DeniedSyscall {
                name: "io_uring_setup",
                nr: 425,
            },
            DeniedSyscall {
                name: "io_uring_enter",
                nr: 426,
            },
            DeniedSyscall {
                name: "io_uring_register",
                nr: 427,
            },
        ],
        SyscallArch::Aarch64 => vec![
            DeniedSyscall {
                name: "socket",
                nr: 198,
            },
            DeniedSyscall {
                name: "connect",
                nr: 203,
            },
            DeniedSyscall {
                name: "ptrace",
                nr: 117,
            },
            DeniedSyscall {
                name: "process_vm_readv",
                nr: 270,
            },
            DeniedSyscall {
                name: "process_vm_writev",
                nr: 271,
            },
            DeniedSyscall {
                name: "io_uring_setup",
                nr: 425,
            },
            DeniedSyscall {
                name: "io_uring_enter",
                nr: 426,
            },
            DeniedSyscall {
                name: "io_uring_register",
                nr: 427,
            },
        ],
    }
}

fn bpf_stmt(code: u16, k: u32) -> libc::sock_filter {
    libc::sock_filter {
        code,
        jt: 0,
        jf: 0,
        k,
    }
}

fn bpf_jump(code: u16, k: u32, jt: u8, jf: u8) -> libc::sock_filter {
    libc::sock_filter { code, jt, jf, k }
}

/// Build the BPF program: non-native arch → `ENOSYS` (kills compat bypasses,
/// matching `native_syscall_arch_only`), x32-numbered syscalls → `ENOSYS`
/// (x32 shares `AUDIT_ARCH_X86_64`, so only the `__X32_SYSCALL_BIT` range in
/// nr separates it from native — without this guard every x32 variant would
/// miss the native nr comparisons and fall through to allow), denied nr →
/// `EPERM`, everything else → allow. Pure constructor so tests inspect the
/// exact program without installing anything.
#[must_use]
pub fn build_seccomp_filter(arch: SyscallArch) -> Vec<libc::sock_filter> {
    let denied = denied_syscalls(arch);
    // Arch gate (load + jeq + ENOSYS return), nr load, denied comparisons,
    // allow/EPERM tail; x86_64 adds the x32-range guard and its own ENOSYS
    // return (BPF jumps are forward-only, so the guard cannot reuse the arch
    // gate's return).
    let x32_guard = arch == SyscallArch::X86_64;
    let mut filter = Vec::with_capacity(denied.len() + if x32_guard { 8 } else { 7 });
    filter.push(bpf_stmt(BPF_STMT_LD_W_ABS, SECCOMP_DATA_ARCH_OFFSET));
    filter.push(bpf_jump(BPF_JUMP_JEQ_K, arch.audit_arch(), 1, 0));
    filter.push(bpf_stmt(
        BPF_STMT_RET_K,
        libc::SECCOMP_RET_ERRNO | ERRNO_ENOSYS,
    ));
    filter.push(bpf_stmt(BPF_STMT_LD_W_ABS, SECCOMP_DATA_NR_OFFSET));
    let guard_index = if x32_guard {
        filter.push(bpf_jump(BPF_JUMP_JGT_K, X32_NR_CEILING, 0, 0));
        Some(filter.len() - 1)
    } else {
        None
    };
    for entry in &denied {
        filter.push(bpf_jump(BPF_JUMP_JEQ_K, entry.nr as u32, 0, 0));
    }
    let allow_index = filter.len();
    filter.push(bpf_stmt(BPF_STMT_RET_K, libc::SECCOMP_RET_ALLOW));
    let deny_index = filter.len();
    filter.push(bpf_stmt(
        BPF_STMT_RET_K,
        libc::SECCOMP_RET_ERRNO | ERRNO_EPERM,
    ));
    let x32_deny_index = if x32_guard {
        // Forward-only BPF: the guard cannot reuse the arch gate's ENOSYS
        // return, so the x86_64 program carries its own. Other arches have no
        // x32 numbering, so no dead return is emitted.
        let index = filter.len();
        filter.push(bpf_stmt(
            BPF_STMT_RET_K,
            libc::SECCOMP_RET_ERRNO | ERRNO_ENOSYS,
        ));
        Some(index)
    } else {
        None
    };
    // Patch each nr comparison to jump forward to the shared deny tail.
    for (offset, _) in denied.iter().enumerate() {
        let index = allow_index - denied.len() + offset;
        filter[index].jt = (deny_index - index - 1) as u8;
    }
    if let Some(guard_index) = guard_index {
        let x32_deny_index = x32_deny_index.expect("guard implies its ENOSYS return");
        filter[guard_index].jt = (x32_deny_index - guard_index - 1) as u8;
    }
    filter
}

/// Verified seccomp installation evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SeccompEvidence {
    pub arch: SyscallArch,
    pub denied: Vec<String>,
    /// Positive kernel probe: `socket(2)` — the first denylist entry on every
    /// arch — failed with `EPERM` after install, so the filter bites at
    /// runtime rather than only recording intent.
    pub inet_socket_denied: bool,
}

/// Install the denylist filter in the calling worker process. Requires
/// no_new_privs (applied here when missing — harmless when already set) and a
/// validated native arch. Seccomp is inherited across fork/clone, so descendants
/// stay restricted; there is no uninstall, which is exactly the guarantee.
pub fn install_syscall_restrictions() -> Result<SeccompEvidence, SandboxError> {
    let arch = native_syscall_arch()
        .ok_or_else(|| SandboxError::Unsupported("no validated native syscall arch".into()))?;
    apply_no_new_privs()?;
    let mut filter = build_seccomp_filter(arch);
    let program = libc::sock_fprog {
        len: filter.len() as libc::c_ushort,
        filter: filter.as_mut_ptr(),
    };
    // SAFETY: `program` borrows the live filter Vec for the duration of the
    // call; the kernel copies the program before returning.
    let installed = unsafe {
        libc::prctl(
            libc::PR_SET_SECCOMP,
            libc::SECCOMP_MODE_FILTER as libc::c_ulong,
            &program as *const libc::sock_fprog,
            0,
            0,
        )
    };
    // Keep the Vec alive across the prctl call.
    std::hint::black_box(&filter);
    if installed != 0 {
        return Err(last_os_error("PR_SET_SECCOMP(SECCOMP_MODE_FILTER) failed"));
    }
    // Runtime self-probe: a live filter makes even one inet socket fail with
    // EPERM (socket is denied on every arch). An ineffective filter would
    // otherwise record intended denials as if they were enforcement.
    // SAFETY: probe socket with inert arguments; a denied call returns -1.
    let probe = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if probe >= 0 {
        // SAFETY: fd came from the successful socket above.
        unsafe {
            libc::close(probe);
        }
        return Err(SandboxError::VerificationFailed(
            "seccomp denylist is ineffective: socket(AF_INET) still succeeds after install".into(),
        ));
    }
    if std::io::Error::last_os_error().raw_os_error() != Some(libc::EPERM) {
        return Err(SandboxError::VerificationFailed(format!(
            "seccomp denylist probe did not deny socket(AF_INET) with EPERM: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(SeccompEvidence {
        arch,
        denied: denied_syscalls(arch)
            .iter()
            .map(|entry| entry.name.to_owned())
            .collect(),
        inet_socket_denied: true,
    })
}

// --- Landlock filesystem allowlist -------------------------------------------
//
// Raw `landlock_*` syscalls (no extra dependency): probe the ABI, build a
// path-beneath ruleset over exactly the allowlist, restrict self. Descendants
// inherit the domain. Values from linux/landlock.h (UAPI, stable).

const LANDLOCK_CREATE_RULESET_VERSION: libc::c_long = 1;
const LANDLOCK_RULE_PATH_BENEATH: libc::c_long = 1;
// ABI v1 rights EXECUTE..REFER (bits 0..13); TRUNCATE (v2+) and IOCTL_DEV
// negotiated by ABI below.
const LANDLOCK_FS_BASE: u64 = 0x3FFF;
const LANDLOCK_FS_TRUNCATE: u64 = 1 << 14;
const LANDLOCK_FS_IOCTL_DEV: u64 = 1 << 15;
const LANDLOCK_FS_READ_FILE: u64 = 1 << 2;
const LANDLOCK_FS_READ_DIR: u64 = 1 << 3;
const LANDLOCK_FS_EXECUTE: u64 = 1 << 0;
const LANDLOCK_FS_WRITE_FILE: u64 = 1 << 1;

#[repr(C)]
struct LandlockRulesetAttr {
    handled_access_fs: u64,
}

// Matches `struct landlock_path_beneath_attr` in linux/landlock.h exactly:
// `allowed_access` first, then `parent_fd`, packed to 12 bytes with no
// padding. A repr(C) `{ parent_fd: i32, allowed_access: u64 }` is WRONG here
// (16 bytes, swapped offsets): the kernel would read the FD as rights and
// the rights as the FD, failing closed with EBADF/EBADFD.
#[repr(C, packed)]
struct LandlockPathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

/// Highest supported Landlock ABI version, or a negative errno-cast on
/// failure. Never fails closed by itself: callers compare against 1.
#[must_use]
pub fn detect_landlock_abi() -> i32 {
    // SAFETY: version probe passes NULL attrs with the VERSION flag; the
    // kernel returns the ABI version without creating anything.
    let probed = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            0 as libc::c_long,
            0 as libc::c_long,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    probed as i32
}

/// Exact filesystem surface the worker may touch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LandlockRequest {
    /// System runtime library trees (ld.so already resolved them, but
    /// late `dlopen` of codec deps must keep working read-only).
    pub runtime_lib_dirs: Vec<PathBuf>,
    /// Native plugin trees loaded after the sandbox is up.
    pub native_plugin_dirs: Vec<PathBuf>,
    /// The verified model file.
    pub model_file: PathBuf,
    /// The bounded private cache directory (created when missing).
    pub cache_dir: PathBuf,
    /// Explicitly approved GPU device nodes only. Empty on CPU-only hosts:
    /// no GPU access is granted without a measured GPU profile.
    pub gpu_devices: Vec<PathBuf>,
    /// Home tree reference used by the rejection checks and the post-install
    /// effectiveness probe: everything under it must stay denied, and the
    /// probe must be able to open it. Explicit, not read from the environment
    /// at install time — a scrubbed worker env would otherwise skip the home
    /// checks entirely, and a missing reference would make the probe vacuous.
    /// Required: an empty or relative reference fails the request closed.
    pub home_dir: PathBuf,
    /// Config tree reference (usually the home `.config`) that must stay
    /// denied. Required for the same reason.
    pub config_dir: PathBuf,
}

/// Default system runtime library trees. Missing trees are skipped at install
/// time (distros vary); an empty result fails closed.
#[must_use]
pub fn default_runtime_lib_dirs() -> Vec<PathBuf> {
    vec![
        PathBuf::from("/usr/lib"),
        PathBuf::from("/usr/lib64"),
        PathBuf::from("/lib"),
        PathBuf::from("/lib64"),
    ]
}

/// Reject requests that would punch holes in the sandbox: anything under the
/// home or config reference trees, anything in a diagnostics tree, relative
/// paths, the filesystem root, or GPU nodes outside `/dev`. The home/config
/// references come from the request, never the environment, so a scrubbed
/// worker cannot silently skip the checks; a request without them fails
/// closed. Pure so tests prove the deny rules without touching the kernel.
pub fn landlock_request_clean(request: &LandlockRequest) -> Result<(), SandboxError> {
    let home = &request.home_dir;
    let config = &request.config_dir;
    for (kind, path) in [("home reference", home), ("config reference", config)] {
        if path.as_os_str().is_empty() {
            return Err(SandboxError::VerificationFailed(format!(
                "{kind} is required: without it the rejection checks would be skipped, not passed"
            )));
        }
        if !path.is_absolute() {
            return Err(SandboxError::VerificationFailed(format!(
                "{kind} must be absolute, got {}",
                path.display()
            )));
        }
    }
    let mut candidates: Vec<(&str, &Path)> = Vec::new();
    for dir in &request.runtime_lib_dirs {
        candidates.push(("runtime lib dir", dir));
    }
    for dir in &request.native_plugin_dirs {
        candidates.push(("native plugin dir", dir));
    }
    candidates.push(("model file", &request.model_file));
    candidates.push(("cache dir", &request.cache_dir));
    for device in &request.gpu_devices {
        candidates.push(("GPU device", device));
    }
    for (kind, path) in candidates {
        if !path.is_absolute() {
            return Err(SandboxError::VerificationFailed(format!(
                "{kind} must be absolute, got {}",
                path.display()
            )));
        }
        if path
            .components()
            .any(|part| part.as_os_str() == "diagnostics")
        {
            return Err(SandboxError::VerificationFailed(format!(
                "{kind} must not live in a diagnostics tree: {}",
                path.display()
            )));
        }
        if path == home || path.starts_with(home) {
            return Err(SandboxError::VerificationFailed(format!(
                "{kind} must not live under home: {}",
                path.display()
            )));
        }
        if path == config || path.starts_with(config) {
            return Err(SandboxError::VerificationFailed(format!(
                "{kind} must not live under the config tree: {}",
                path.display()
            )));
        }
    }
    if request.cache_dir.parent().is_none() {
        return Err(SandboxError::VerificationFailed(
            "cache dir must not be the filesystem root".into(),
        ));
    }
    for device in &request.gpu_devices {
        if !device.starts_with("/dev") {
            return Err(SandboxError::VerificationFailed(format!(
                "GPU device must be an explicitly approved node under /dev, got {}",
                device.display()
            )));
        }
    }
    Ok(())
}

/// Verified model bytes: the file exists, is non-empty, and its streaming
/// SHA-256 matches the receipt the launcher verified at fetch time.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedModel {
    pub path: PathBuf,
    pub size_bytes: u64,
    pub sha256_hex: String,
}

/// Hash and check the model file without loading it into memory (models are
/// gigabytes; streaming keeps the launcher's footprint flat).
pub fn verify_model_file(
    path: &Path,
    expected_sha256_hex: &str,
) -> Result<VerifiedModel, SandboxError> {
    use sha2::Digest;
    let file = fs::File::open(path)
        .map_err(|error| SandboxError::Unavailable(format!("model file unreadable: {error}")))?;
    let size_bytes = file
        .metadata()
        .map_err(|error| SandboxError::Unavailable(format!("model metadata unreadable: {error}")))?
        .len();
    if size_bytes == 0 {
        return Err(SandboxError::VerificationFailed(
            "model file is empty".into(),
        ));
    }
    let mut reader = io::BufReader::with_capacity(64 * 1024, file);
    let mut hasher = sha2::Sha256::new();
    io::copy(&mut reader, &mut hasher)
        .map_err(|error| SandboxError::Unavailable(format!("model hashing failed: {error}")))?;
    let digest = hasher.finalize();
    let sha256_hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    if !sha256_hex.eq_ignore_ascii_case(expected_sha256_hex) {
        return Err(SandboxError::VerificationFailed(
            "model SHA-256 does not match the verified receipt".into(),
        ));
    }
    Ok(VerifiedModel {
        path: path.to_owned(),
        size_bytes,
        sha256_hex,
    })
}

/// Verified Landlock installation evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LandlockEvidence {
    pub abi: i32,
    pub fs_mask: u64,
    pub allowed: Vec<PathBuf>,
    /// Positive kernel probe: opening the home reference after
    /// `landlock_restrict_self` failed (the domain bites at runtime).
    pub home_denied: bool,
}

fn landlock_candidate_masks(abi: i32) -> Vec<u64> {
    if abi <= 1 {
        vec![LANDLOCK_FS_BASE]
    } else {
        // Newer rights first; EINVAL falls back to the older mask so a wrong
        // ABI-to-right guess degrades to a still-deny-by-default ruleset
        // instead of failing open.
        vec![
            LANDLOCK_FS_BASE | LANDLOCK_FS_TRUNCATE | LANDLOCK_FS_IOCTL_DEV,
            LANDLOCK_FS_BASE | LANDLOCK_FS_TRUNCATE,
            LANDLOCK_FS_BASE,
        ]
    }
}

fn landlock_create_ruleset(mask: u64) -> Result<RawFd, SandboxError> {
    let attr = LandlockRulesetAttr {
        handled_access_fs: mask,
    };
    // SAFETY: attr is a live stack struct of the exact UAPI size (8 bytes);
    // the kernel copies it before returning.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            &attr as *const LandlockRulesetAttr,
            std::mem::size_of::<LandlockRulesetAttr>() as libc::c_long,
            0 as libc::c_long,
        )
    };
    if fd < 0 {
        return Err(last_os_error("landlock_create_ruleset failed"));
    }
    Ok(fd as RawFd)
}

fn landlock_open_parent(path: &Path) -> Result<RawFd, SandboxError> {
    let c_path = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        SandboxError::VerificationFailed(format!("path is not NUL-safe: {}", path.display()))
    })?;
    // SAFETY: c_path is a live NUL-terminated string; O_PATH takes no mode.
    let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(last_os_error(&format!(
            "cannot open sandbox path {}",
            path.display()
        )));
    }
    Ok(fd)
}

fn landlock_allow_path(
    ruleset: RawFd,
    path: &Path,
    allowed_access: u64,
) -> Result<(), SandboxError> {
    let parent = landlock_open_parent(path)?;
    let rule = LandlockPathBeneathAttr {
        allowed_access,
        parent_fd: parent,
    };
    debug_assert_eq!(std::mem::size_of::<LandlockPathBeneathAttr>(), 12);
    // SAFETY: rule is a live 12-byte packed stack struct matching the UAPI
    // layout exactly; the kernel copies it before returning. Fields are never
    // re-read through the packed reference (unaligned), only the whole-struct
    // pointer is shared.
    let added = unsafe {
        libc::syscall(
            libc::SYS_landlock_add_rule,
            ruleset as libc::c_long,
            LANDLOCK_RULE_PATH_BENEATH,
            &rule as *const LandlockPathBeneathAttr,
            0 as libc::c_long,
        )
    };
    // SAFETY: fd came from a successful open above.
    unsafe {
        libc::close(parent);
    }
    if added != 0 {
        return Err(last_os_error(&format!(
            "landlock_add_rule failed for {}",
            path.display()
        )));
    }
    Ok(())
}

/// Install the Landlock domain in the calling worker process: ABI probe,
/// request cleanliness, bounded cache creation, negotiated ruleset, per-path
/// rules, restrict-self, then a probe that a denied tree really is denied.
/// Any step failing returns [`SandboxError`] — the launcher must fail Local
/// closed, never run the model outside the domain.
pub fn apply_landlock_restrictions(
    request: &LandlockRequest,
) -> Result<LandlockEvidence, SandboxError> {
    let abi = detect_landlock_abi();
    if abi < 1 {
        return Err(SandboxError::Unsupported(format!(
            "Landlock ABI probe failed (rc {abi}); kernel or config lacks Landlock"
        )));
    }
    landlock_request_clean(request)?;
    // The post-install probe must open a reference that exists, or a missing
    // path would make the effectiveness check vacuous (ENOENT reads as
    // "denied" without the domain doing anything). Fail closed instead.
    if fs::metadata(&request.home_dir).is_err() {
        return Err(SandboxError::Unavailable(format!(
            "home reference path {} does not exist; Landlock effectiveness cannot be verified",
            request.home_dir.display()
        )));
    }
    fs::create_dir_all(&request.cache_dir)
        .map_err(|error| SandboxError::Unavailable(format!("cache dir unreachable: {error}")))?;
    let mut ruleset: Option<RawFd> = None;
    let mut chosen_mask = LANDLOCK_FS_BASE;
    let mut last_error: Option<SandboxError> = None;
    for mask in landlock_candidate_masks(abi) {
        match landlock_create_ruleset(mask) {
            Ok(fd) => {
                ruleset = Some(fd);
                chosen_mask = mask;
                last_error = None;
                break;
            }
            Err(error) => last_error = Some(error),
        }
    }
    let ruleset = ruleset.ok_or_else(|| {
        last_error.unwrap_or_else(|| SandboxError::Unsupported("no Landlock mask worked".into()))
    })?;
    let cleanup = |ruleset: RawFd| {
        // SAFETY: ruleset came from a successful create above.
        unsafe {
            libc::close(ruleset);
        }
    };
    let install = || -> Result<(), SandboxError> {
        let lib_access = LANDLOCK_FS_READ_FILE | LANDLOCK_FS_READ_DIR | LANDLOCK_FS_EXECUTE;
        let mut saw_lib = false;
        for dir in request
            .runtime_lib_dirs
            .iter()
            .chain(request.native_plugin_dirs.iter())
        {
            if !dir.is_dir() {
                continue;
            }
            landlock_allow_path(ruleset, dir, lib_access)?;
            saw_lib = true;
        }
        if !saw_lib {
            return Err(SandboxError::Unavailable(
                "no runtime lib dir exists; refusing an unusable domain".into(),
            ));
        }
        landlock_allow_path(ruleset, &request.model_file, LANDLOCK_FS_READ_FILE)?;
        landlock_allow_path(ruleset, &request.cache_dir, chosen_mask)?;
        let dev_null = Path::new("/dev/null");
        landlock_allow_path(
            ruleset,
            dev_null,
            LANDLOCK_FS_READ_FILE | LANDLOCK_FS_WRITE_FILE,
        )?;
        let mut gpu_access = LANDLOCK_FS_READ_FILE | LANDLOCK_FS_WRITE_FILE | LANDLOCK_FS_READ_DIR;
        if chosen_mask & LANDLOCK_FS_IOCTL_DEV != 0 {
            gpu_access |= LANDLOCK_FS_IOCTL_DEV;
        }
        for device in &request.gpu_devices {
            landlock_allow_path(ruleset, device, gpu_access)?;
        }
        // Unprivileged restrict_self requires no_new_privs; the launcher sets
        // it in pre_exec_lockdown first, and re-applying here is a harmless
        // no-op that keeps this installer self-sufficient.
        apply_no_new_privs()?;
        // SAFETY: ruleset came from a successful create above.
        let restricted = unsafe {
            libc::syscall(
                libc::SYS_landlock_restrict_self,
                ruleset as libc::c_long,
                0 as libc::c_long,
            )
        };
        if restricted != 0 {
            return Err(last_os_error("landlock_restrict_self failed"));
        }
        Ok(())
    };
    if let Err(error) = install() {
        cleanup(ruleset);
        return Err(error);
    }
    cleanup(ruleset);
    // Verify the domain bites: the home reference (rejected from every request
    // by construction, and guaranteed to exist above) must now be unreadable
    // by the calling user — a denial here cannot come from plain DAC, so a
    // readable reference means the restriction is ineffective. Fail closed.
    let probe = &request.home_dir;
    let probe_c = CString::new(probe.as_os_str().as_bytes())
        .map_err(|_| SandboxError::VerificationFailed("home path is not NUL-safe".into()))?;
    // SAFETY: probe_c is live; O_DIRECTORY keeps the probe to a metadata open.
    let probed = unsafe {
        libc::open(
            probe_c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if probed >= 0 {
        // SAFETY: fd came from the successful open above.
        unsafe {
            libc::close(probed);
        }
        return Err(SandboxError::VerificationFailed(format!(
            "Landlock domain is ineffective: {} still opens",
            probe.display()
        )));
    }
    let mut allowed: Vec<PathBuf> = Vec::new();
    allowed.extend(request.runtime_lib_dirs.iter().cloned());
    allowed.extend(request.native_plugin_dirs.iter().cloned());
    allowed.push(request.model_file.clone());
    allowed.push(request.cache_dir.clone());
    allowed.extend(request.gpu_devices.iter().cloned());
    Ok(LandlockEvidence {
        abi,
        fs_mask: chosen_mask,
        allowed,
        home_denied: true,
    })
}

// --- environment + FD scrubbing ------------------------------------------------

/// Evidence for an applied process-environment scrub.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppliedEnvEvidence {
    /// Sorted names removed from the process environment.
    pub removed: Vec<String>,
}

/// Remove every forbidden variable from the calling process environment and
/// verify each removal plus the absence of `PATH`. Worker child only: this
/// mutates global process state. Returns the sorted removal list as evidence.
pub fn apply_worker_environment() -> Result<AppliedEnvEvidence, SandboxError> {
    let mut removed: Vec<String> = Vec::new();
    for (key, _) in std::env::vars_os() {
        if forbidden_reason_os(&key).is_none() {
            continue;
        }
        let Some(name) = key.to_str() else {
            continue;
        };
        // SAFETY: the launcher calls this in the worker child before any
        // worker thread spawns; no other thread can observe the environment.
        unsafe {
            std::env::remove_var(name);
        }
        removed.push(name.to_owned());
    }
    // PATH is dropped by rule, but double-check: worker exec is absolute and
    // no descendant may resolve names through an inherited PATH.
    if std::env::var_os("PATH").is_some() {
        // SAFETY: same single-threaded worker-child context as above.
        unsafe {
            std::env::remove_var("PATH");
        }
        removed.push("PATH".into());
    }
    removed.sort();
    removed.dedup();
    for name in &removed {
        if std::env::var_os(name).is_some() {
            return Err(SandboxError::VerificationFailed(format!(
                "environment variable survived scrubbing: {name}"
            )));
        }
    }
    Ok(AppliedEnvEvidence { removed })
}

/// Evidence for closing unrelated file descriptors.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FdScrubEvidence {
    /// FDs closed, excluding stdio and the kept set.
    pub closed: usize,
    /// FDs explicitly kept open besides stdio.
    pub kept: Vec<RawFd>,
}

/// Close every FD except 0/1/2 and `keep` (pre-opened control pipes). Worker
/// child only, before model load: inherited sockets, secret-service handles,
/// or directory FDs must not cross into the model process.
pub fn close_unrelated_fds(keep: &[RawFd]) -> Result<FdScrubEvidence, SandboxError> {
    let mut observed: Vec<RawFd> = Vec::new();
    let entries = fs::read_dir("/proc/self/fd")
        .map_err(|_| SandboxError::Unsupported("/proc/self/fd is unavailable".into()))?;
    for entry in entries {
        let entry = entry
            .map_err(|error| SandboxError::Unavailable(format!("cannot list fds: {error}")))?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if let Ok(number) = name.parse::<RawFd>() {
            observed.push(number);
        }
    }
    // `entries` is dropped here, so its own FD is already closed; a stale
    // duplicate in `observed` then fails EBADF and is ignored below.
    let mut closed = 0_usize;
    for fd in observed {
        if fd <= 2 || keep.contains(&fd) {
            continue;
        }
        // SAFETY: fd numbers were just observed in our own table; stdio and
        // kept FDs are skipped; EBADF races are ignored.
        if unsafe { libc::close(fd) } == 0 {
            closed += 1;
        }
    }
    Ok(FdScrubEvidence {
        closed,
        kept: keep.to_vec(),
    })
}

// --- user-manager-owned worker cgroup ----------------------------------------
//
// The launcher creates a leaf scope under the worker's own user slice before
// model load, writes the resolved envelope from bounds.rs, attaches itself,
// and verifies membership plus controller inheritance. Systemd D-Bus
// transient units are deliberately NOT used here: this helper must stay
// dependency-free and testable, and a direct cgroupfs attempt reports the
// honest kernel answer (delegation present or not) instead of hiding behind a
// bus call. No user manager or delegation → [`SandboxError`], Local closed.

/// Verified cgroup placement evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CgroupEvidence {
    /// Leaf scope directory, e.g.
    /// `/sys/fs/cgroup/user.slice/.../voisu-worker-1234.scope`.
    pub scope_dir: PathBuf,
    /// `/proc/self/cgroup` confirms membership after attach.
    pub member: bool,
    /// Controllers delegated into the scope: the enforcement surface the
    /// envelope binds (verified present after attach).
    pub controllers: Vec<String>,
}

/// Leaf scope name for one worker PID.
#[must_use]
pub fn worker_scope_name(worker_pid: u32) -> String {
    format!("voisu-worker-{worker_pid}.scope")
}

/// This process's cgroup v2 path (`0::<path>`), or `None` outside v2.
#[must_use]
pub fn current_cgroup_path() -> Option<String> {
    let text = fs::read_to_string("/proc/self/cgroup").ok()?;
    for line in text.lines() {
        if let Some(path) = line.strip_prefix("0::") {
            return Some(path.to_owned());
        }
    }
    None
}

/// Effective cgroup path of another process (for descendant-inheritance
/// checks), or `None` when unreadable.
#[must_use]
pub fn process_cgroup_path(pid: u32) -> Option<String> {
    let text = fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    for line in text.lines() {
        if let Some(path) = line.strip_prefix("0::") {
            return Some(path.to_owned());
        }
    }
    None
}

/// Controllers the kernel currently offers for delegation.
#[must_use]
pub fn cgroup_controllers() -> Vec<String> {
    fs::read_to_string("/sys/fs/cgroup/cgroup.controllers")
        .map(|text| {
            text.split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

/// Controllers visible inside one scope (descendant inheritance surface).
#[must_use]
pub fn scope_controllers(scope_dir: &Path) -> Vec<String> {
    fs::read_to_string(scope_dir.join("cgroup.controllers"))
        .map(|text| {
            text.split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

/// True when this process currently sits under a path containing the fragment
/// (e.g. the worker scope name after attach).
#[must_use]
pub fn verify_cgroup_membership(expected_fragment: &str) -> bool {
    current_cgroup_path().is_some_and(|path| path.contains(expected_fragment))
}

/// True when `pid`'s cgroup path contains the fragment: forked descendants
/// inherit the scope, which is how the model child stays under the envelope.
#[must_use]
pub fn child_cgroup_matches(child_pid: u32, expected_fragment: &str) -> bool {
    process_cgroup_path(child_pid).is_some_and(|path| path.contains(expected_fragment))
}

/// Create the user-manager-owned leaf scope, attach the calling worker, and
/// enforce the envelope from `spec`, then verify membership plus delegated
/// controller visibility. Only joins trees under `/user.slice/`: the daemon
/// runs as a user unit on product hosts, so anything else (root slice, system
/// slice, container root) is refused rather than mis-attributed.
///
/// The worker scope stays a leaf: nothing creates child cgroups below it, its
/// own limit files only need the controllers the user manager delegated into
/// the *parent*, and `cgroup.subtree_control` therefore stays empty. Enabling
/// controllers in the scope's own subtree_control would trip cgroup v2's
/// no-internal-process constraint and make the self-attach fail with EBUSY.
/// The attach happens before the limits so migration never races a `pids.max`
/// the caller's task count could not fit.
pub fn join_worker_cgroup(spec: &WorkerCgroupSpec) -> Result<CgroupEvidence, SandboxError> {
    use super::bounds::cpu_max_cgroup_value;
    let current = current_cgroup_path()
        .ok_or_else(|| SandboxError::Unsupported("no cgroup v2 membership is visible".into()))?;
    if !current.starts_with("/user.slice/") {
        return Err(SandboxError::Unsupported(format!(
            "worker cgroup requires a user-manager-owned tree; current cgroup is {current}"
        )));
    }
    let scope_dir = PathBuf::from("/sys/fs/cgroup")
        .join(current.trim_start_matches('/'))
        .join(&spec.scope_name);
    fs::create_dir_all(&scope_dir).map_err(|error| {
        SandboxError::Unavailable(format!(
            "cannot create worker scope {}: {error}",
            scope_dir.display()
        ))
    })?;
    let parent_procs = PathBuf::from("/sys/fs/cgroup")
        .join(current.trim_start_matches('/'))
        .join("cgroup.procs");
    // A fresh leaf lists exactly the controllers the parent delegated into
    // this tree: without them the limit files do not exist and no envelope
    // can be enforced, so fail before touching cgroup.procs. The scope is
    // still empty here, so it must not leak.
    let visible = scope_controllers(&scope_dir);
    let missing: Vec<&str> = ["cpu", "memory", "pids"]
        .into_iter()
        .filter(|need| !visible.iter().any(|have| have == need))
        .collect();
    if !missing.is_empty() {
        let residue = match cleanup_worker_cgroup(&scope_dir) {
            Ok(()) => String::new(),
            Err(error) => format!(
                "; empty scope {} could not be removed: {error}",
                scope_dir.display()
            ),
        };
        return Err(with_residue(
            SandboxError::Unavailable(format!(
                "cgroup delegation unavailable: worker scope cannot delegate controllers: missing {}",
                missing.join(",")
            )),
            &residue,
        ));
    }
    // Attach self first: the scope is empty (its subtree_control is empty), so
    // the no-internal-process constraint cannot refuse the migration, and no
    // limit can reject a PID that has not been counted yet.
    let pid = std::process::id();
    if let Err(error) = fs::write(scope_dir.join("cgroup.procs"), pid.to_string()) {
        let residue = match cleanup_worker_cgroup(&scope_dir) {
            Ok(()) => String::new(),
            Err(error) => format!(
                "; empty scope {} could not be removed: {error}",
                scope_dir.display()
            ),
        };
        return Err(with_residue(
            SandboxError::Unavailable(format!(
                "cannot attach worker PID {pid} to {}: {error}",
                scope_dir.display()
            )),
            &residue,
        ));
    }
    if !verify_cgroup_membership(&spec.scope_name) {
        let residue = unattach_best_effort(&scope_dir, &parent_procs, pid);
        return Err(with_residue(
            SandboxError::VerificationFailed(format!(
                "cgroup.procs accepted PID {pid} but membership does not read back under {}",
                spec.scope_name
            )),
            &residue,
        ));
    }
    // Enforce the envelope from inside the scope: the parent delegated the
    // controllers, so the limit files exist, and binding them after the attach
    // can never hit the no-internal-process constraint.
    let enforce = |scope_dir: &Path| -> Result<(), SandboxError> {
        let write_limit = |name: &str, value: &str| -> Result<(), SandboxError> {
            fs::write(scope_dir.join(name), value).map_err(|error| {
                SandboxError::Unavailable(format!(
                    "cannot enforce {name}={value} in {}: {error}",
                    scope_dir.display()
                ))
            })
        };
        write_limit("memory.max", &spec.memory_max_bytes.to_string())?;
        write_limit("cpu.max", &cpu_max_cgroup_value(spec.cpu_quota_cores))?;
        write_limit("pids.max", &spec.tasks_max.to_string())?;
        Ok(())
    };
    if let Err(error) = enforce(&scope_dir) {
        let residue = unattach_best_effort(&scope_dir, &parent_procs, pid);
        return Err(with_residue(error, &residue));
    }
    let controllers = scope_controllers(&scope_dir);
    for need in ["cpu", "memory", "pids"] {
        if !controllers.iter().any(|have| have == need) {
            let residue = unattach_best_effort(&scope_dir, &parent_procs, pid);
            return Err(with_residue(
                SandboxError::VerificationFailed(format!(
                    "worker scope is missing inheritable controller {need}"
                )),
                &residue,
            ));
        }
    }
    Ok(CgroupEvidence {
        scope_dir,
        member: true,
        controllers,
    })
}

/// Remove the leaf scope after the worker exits. Succeeds only when empty
/// (no member processes, no child groups), so a live worker is never torn
/// down by accident; the launcher calls this after reaping the child.
pub fn cleanup_worker_cgroup(scope_dir: &Path) -> io::Result<()> {
    fs::remove_dir(scope_dir)
}

/// Append best-effort retreat context to a join error without masking the
/// primary failure: `residue` is empty when nothing leaked.
fn with_residue(error: SandboxError, residue: &str) -> SandboxError {
    if residue.is_empty() {
        return error;
    }
    match error {
        SandboxError::Unavailable(message) => {
            SandboxError::Unavailable(format!("{message}; {residue}"))
        }
        SandboxError::Unsupported(message) => {
            SandboxError::Unsupported(format!("{message}; {residue}"))
        }
        SandboxError::VerificationFailed(message) => {
            SandboxError::VerificationFailed(format!("{message}; {residue}"))
        }
    }
}

/// Best-effort retreat after a failed post-attach join step: move the worker
/// PID back to the parent cgroup so the scope empties, then remove it. Failed
/// joins leave nothing behind; when the retreat itself fails, the leftover is
/// reported as residue context instead of masking the primary error.
fn unattach_best_effort(scope_dir: &Path, parent_procs: &Path, pid: u32) -> String {
    if fs::write(parent_procs, pid.to_string()).is_err() {
        return format!(
            "worker PID {pid} could not be moved back to {} and scope {} remains occupied",
            parent_procs.display(),
            scope_dir.display()
        );
    }
    match cleanup_worker_cgroup(scope_dir) {
        Ok(()) => String::new(),
        Err(error) => format!(
            "emptied scope {} could not be removed: {error}",
            scope_dir.display()
        ),
    }
}

/// Probe cgroup delegability with a real but non-attaching placement that
/// models `join_worker_cgroup` step for step: create a throwaway leaf scope
/// under the current user-owned cgroup, require cpu/memory/pids to be visible
/// inside it (a fresh leaf lists exactly what the parent delegated), then bind
/// the same resolved envelope placement binds, and remove the still-empty
/// scope afterwards.
///
/// Controller *visibility* at the cgroup root is not *delegability*: a scope
/// can list controllers the user manager never delegated into this tree, and
/// the limit files the envelope needs would not exist. The one placement step
/// the probe does not mirror is the self-attach — it needs no delegation (an
/// empty scope, empty subtree_control, no `pids.max` yet cannot refuse a
/// migration), and the probe never moves the caller's own cgroup membership.
fn probe_worker_cgroup_delegation() -> Result<(), SandboxError> {
    use super::bounds::{
        cpu_max_cgroup_value, desired_cgroup_spec, host_cpu_cores, host_physical_ram_bytes,
    };
    let current = current_cgroup_path()
        .ok_or_else(|| SandboxError::Unsupported("no cgroup v2 membership is visible".into()))?;
    if !current.starts_with("/user.slice/") {
        return Err(SandboxError::Unsupported(format!(
            "worker cgroup requires a user-manager-owned tree; current cgroup is {current}"
        )));
    }
    // Distinct name: the probe never collides with (or is mistaken for) a real
    // worker scope, and no process is ever attached to it, so the caller's own
    // cgroup membership is untouched.
    let probe_dir = PathBuf::from("/sys/fs/cgroup")
        .join(current.trim_start_matches('/'))
        .join(format!(
            "voisu-delegation-probe-{}.scope",
            std::process::id()
        ));
    fs::create_dir_all(&probe_dir).map_err(|error| {
        SandboxError::Unavailable(format!(
            "cgroup delegation unavailable: cannot create probe scope {}: {error}",
            probe_dir.display()
        ))
    })?;
    let placement = (|| -> Result<(), SandboxError> {
        let visible = scope_controllers(&probe_dir);
        let missing: Vec<&str> = ["cpu", "memory", "pids"]
            .into_iter()
            .filter(|need| !visible.iter().any(|have| have == need))
            .collect();
        if !missing.is_empty() {
            return Err(SandboxError::Unavailable(format!(
                "cgroup delegation unavailable: worker scope cannot delegate controllers: missing {}",
                missing.join(",")
            )));
        }
        // The delegated controllers make the limit files exist: bind exactly
        // what a real placement binds, so the probe fails precisely when a
        // join would.
        let spec = desired_cgroup_spec(
            host_physical_ram_bytes().unwrap_or(8 * 1024 * 1024 * 1024),
            host_cpu_cores(),
            std::process::id(),
        );
        let write_limit = |name: &str, value: &str| -> Result<(), SandboxError> {
            fs::write(probe_dir.join(name), value).map_err(|error| {
                SandboxError::Unavailable(format!(
                    "cannot enforce {name}={value} in probe scope {}: {error}",
                    probe_dir.display()
                ))
            })
        };
        write_limit("memory.max", &spec.memory_max_bytes.to_string())?;
        write_limit("cpu.max", &cpu_max_cgroup_value(spec.cpu_quota_cores))?;
        write_limit("pids.max", &spec.tasks_max.to_string())?;
        Ok(())
    })();
    // The probe scope is empty (nothing was attached), so it must always
    // remove cleanly; leaving it behind would leak into the user's cgroup
    // tree on every gate run, and cleanup errors surface rather than pretend
    // nothing happened.
    let cleanup_error = cleanup_worker_cgroup(&probe_dir).err();
    placement?;
    if let Some(error) = cleanup_error {
        return Err(SandboxError::Unavailable(format!(
            "cgroup delegation probe scope {} could not be removed: {error}",
            probe_dir.display()
        )));
    }
    Ok(())
}

// --- fail-closed gate + host evidence ------------------------------------------

/// Probed capabilities. Recorded, never asserted: absence becomes
/// [`SandboxError`], never a silent downgrade.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SandboxCapabilities {
    pub syscall_arch: Option<SyscallArch>,
    pub landlock_abi: i32,
    pub cgroup_path: Option<String>,
    pub cgroup_user_owned: bool,
    pub cgroup_controllers: Vec<String>,
}

/// Probe every enforcement capability without installing anything.
#[must_use]
pub fn probe_sandbox_capabilities() -> SandboxCapabilities {
    let cgroup_path = current_cgroup_path();
    SandboxCapabilities {
        syscall_arch: native_syscall_arch(),
        landlock_abi: detect_landlock_abi(),
        cgroup_user_owned: cgroup_path
            .as_deref()
            .is_some_and(|path| path.starts_with("/user.slice/")),
        cgroup_path,
        cgroup_controllers: cgroup_controllers(),
    }
}

/// Verified readiness: every required enforcement is present on this host.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SandboxReady {
    pub arch: SyscallArch,
    pub landlock_abi: i32,
    pub cgroup_path: String,
}

/// Fail Local closed unless the kernel and user manager can enforce the full
/// envelope: validated syscall arch, Landlock ABI ≥ 1, a user-owned tree, and
/// cpu/memory/pids controllers delegated deep enough that a real worker scope
/// exists, shows them, and accepts the resolved envelope's limit files. The
/// delegation probe models `join_worker_cgroup` without attaching anything:
/// fresh-leaf controller visibility plus the same limit writes, in a
/// throwaway scope that is removed afterwards. The launcher calls this before
/// spawning the worker; `Err` means Local stays unavailable.
pub fn gate_local_on_sandbox() -> Result<SandboxReady, SandboxError> {
    let caps = probe_sandbox_capabilities();
    let arch = caps.syscall_arch.ok_or_else(|| {
        SandboxError::Unsupported(
            "no validated native syscall arch for the seccomp denylist".into(),
        )
    })?;
    if caps.landlock_abi < 1 {
        return Err(SandboxError::Unavailable(format!(
            "Landlock unavailable (ABI probe rc {})",
            caps.landlock_abi
        )));
    }
    let cgroup_path = caps
        .cgroup_path
        .clone()
        .ok_or_else(|| SandboxError::Unsupported("no cgroup v2 membership is visible".into()))?;
    if !caps.cgroup_user_owned {
        return Err(SandboxError::Unavailable(format!(
            "no user-manager-owned worker cgroup (current cgroup is {cgroup_path})"
        )));
    }
    for need in ["cpu", "memory", "pids"] {
        if !caps.cgroup_controllers.iter().any(|have| have == need) {
            return Err(SandboxError::Unavailable(format!(
                "cgroup controller {need} is not available for the worker scope"
            )));
        }
    }
    // Controllers listed at the cgroup root are not enough: placement fails
    // unless the user manager delegated them into this scope. Probe a real
    // (empty, non-attaching) leaf so the gate fails closed exactly when
    // `join_worker_cgroup` would.
    probe_worker_cgroup_delegation()?;
    Ok(SandboxReady {
        arch,
        landlock_abi: caps.landlock_abi,
        cgroup_path,
    })
}

/// First supported product target. Feasibility evidence always names the
/// *current* host separately (see [`current_host_id`]); matching this string
/// is a product claim this module never makes on its own.
pub const PRODUCT_TARGET: &str = "fedora-kde-wayland";

/// Current host ID from `/etc/os-release` (`ID=...`), e.g. `fedora`, `arch`,
/// `debian`. `unknown` when the file is missing or unparsable — never guessed.
#[must_use]
pub fn current_host_id() -> String {
    fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|text| {
            text.lines().find_map(|line| {
                let rest = line.strip_prefix("ID=")?;
                Some(rest.trim_matches('"').trim().to_owned())
            })
        })
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

/// Exact host record for one feasibility run: kernel, user manager,
/// syscall arch, effective limits, and restriction evidence. The launcher (or
/// the feasibility harness) collects this per host; reviewers compare runs
/// instead of trusting prose.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostSandboxEvidence {
    pub kernel_release: String,
    pub os_pretty: String,
    pub host_id: String,
    pub systemd_version: Option<String>,
    pub machine: String,
    pub audit_arch: Option<u32>,
    pub landlock_abi: i32,
    pub cgroup_path: Option<String>,
    pub cgroup_controllers: Vec<String>,
    pub physical_ram_bytes: Option<u64>,
    pub host_cores: u32,
    pub memory_max_bytes: Option<u64>,
    pub cpu_quota_cores: Option<u32>,
    pub seccomp_supported: bool,
    pub gate: Result<SandboxReady, SandboxError>,
}

fn uname_field(selector: fn(&libc::utsname) -> &[libc::c_char; 65]) -> String {
    // SAFETY: uname writes to a live stack struct; fields are NUL-terminated.
    let mut name: libc::utsname = unsafe { std::mem::zeroed() };
    if unsafe { libc::uname(&mut name) } != 0 {
        return "unknown".into();
    }
    let raw = selector(&name);
    // SAFETY: bounded read of a kernel-terminated C string field.
    let bytes: &[u8] = unsafe { std::slice::from_raw_parts(raw.as_ptr().cast::<u8>(), raw.len()) };
    let len = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..len]).into_owned()
}

/// Collect the full host record. Never fails: unknowns are recorded as
/// `None`/`"unknown"` so a feasibility log shows exactly what was missing.
#[must_use]
pub fn collect_host_evidence() -> HostSandboxEvidence {
    use super::bounds::worker_memory_ceiling_bytes;
    use super::bounds::{host_cpu_cores, host_physical_ram_bytes, worker_cpu_quota_cores};
    let arch = native_syscall_arch();
    let ram = host_physical_ram_bytes();
    let cores = host_cpu_cores();
    let systemd_version = std::process::Command::new("systemctl")
        .arg("--version")
        .output()
        .ok()
        .and_then(|output| {
            if !output.status.success() {
                return None;
            }
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .next()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_owned)
        });
    let os_pretty = fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|text| {
            text.lines().find_map(|line| {
                let rest = line.strip_prefix("PRETTY_NAME=")?;
                Some(rest.trim_matches('"').to_owned())
            })
        })
        .unwrap_or_else(|| "unknown".into());
    HostSandboxEvidence {
        kernel_release: uname_field(|name| &name.release),
        os_pretty,
        host_id: current_host_id(),
        systemd_version,
        machine: uname_field(|name| &name.machine),
        audit_arch: arch.map(SyscallArch::audit_arch),
        landlock_abi: detect_landlock_abi(),
        cgroup_path: current_cgroup_path(),
        cgroup_controllers: cgroup_controllers(),
        physical_ram_bytes: ram,
        host_cores: cores,
        memory_max_bytes: ram.map(worker_memory_ceiling_bytes),
        cpu_quota_cores: Some(worker_cpu_quota_cores(cores)),
        seccomp_supported: arch.is_some(),
        gate: gate_local_on_sandbox(),
    }
}

/// Render the host record as markdown for feasibility logs. Names the current
/// host and the product target on separate lines: a non-Fedora host never
/// renders as Fedora evidence.
#[must_use]
pub fn render_host_evidence_markdown(evidence: &HostSandboxEvidence) -> String {
    let mut out = String::from("# Worker sandbox host evidence\n\n");
    out.push_str(&format!("- kernel: {}\n", evidence.kernel_release));
    out.push_str(&format!("- os: {}\n", evidence.os_pretty));
    out.push_str(&format!("- current host id: {}\n", evidence.host_id));
    out.push_str(&format!(
        "- product target (not claimed): {PRODUCT_TARGET}\n"
    ));
    out.push_str(&format!(
        "- systemd: {}\n",
        evidence.systemd_version.as_deref().unwrap_or("unknown")
    ));
    out.push_str(&format!("- machine: {}\n", evidence.machine));
    out.push_str(&format!(
        "- audit arch: {}\n",
        evidence
            .audit_arch
            .map(|arch| arch.to_string())
            .unwrap_or_else(|| "unsupported".into())
    ));
    out.push_str(&format!("- landlock ABI: {}\n", evidence.landlock_abi));
    out.push_str(&format!(
        "- cgroup: {}\n",
        evidence.cgroup_path.as_deref().unwrap_or("none visible")
    ));
    out.push_str(&format!(
        "- cgroup controllers: {}\n",
        if evidence.cgroup_controllers.is_empty() {
            "none".into()
        } else {
            evidence.cgroup_controllers.join(" ")
        }
    ));
    out.push_str(&format!(
        "- physical RAM bytes: {}\n",
        evidence
            .physical_ram_bytes
            .map(|ram| ram.to_string())
            .unwrap_or_else(|| "unknown".into())
    ));
    out.push_str(&format!("- host cores: {}\n", evidence.host_cores));
    out.push_str(&format!(
        "- worker memory_max bytes: {}\n",
        evidence
            .memory_max_bytes
            .map(|max| max.to_string())
            .unwrap_or_else(|| "unknown (fail closed)".into())
    ));
    out.push_str(&format!(
        "- worker cpu quota cores: {}\n",
        evidence
            .cpu_quota_cores
            .map(|cores| cores.to_string())
            .unwrap_or_else(|| "unknown (fail closed)".into())
    ));
    out.push_str(&format!(
        "- seccomp denylist supported: {}\n",
        evidence.seccomp_supported
    ));
    match &evidence.gate {
        Ok(ready) => out.push_str(&format!(
            "- gate: READY (arch {}, landlock ABI {}, cgroup {})\n",
            ready.arch.name(),
            ready.landlock_abi,
            ready.cgroup_path
        )),
        Err(error) => out.push_str(&format!("- gate: LOCAL UNAVAILABLE ({error})\n")),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn sentinel_fails_tests_when_cloud_capability_is_used() {
        let mut sentinel = CloudCapabilitySentinel::new();
        assert!(sentinel.local_path_clean());
        sentinel.read_cloud_credential();
        assert!(!sentinel.local_path_clean());
    }

    #[test]
    fn unsupported_restrictions_make_local_unavailable() {
        assert!(local_unavailable_if_restrictions_fail(
            RestrictionProbe::Unsupported
        ));
        assert!(!local_unavailable_if_restrictions_fail(
            RestrictionProbe::Available
        ));
    }

    #[test]
    fn shipped_unit_keeps_mdwe_and_restrict_namespaces() {
        let unit = include_str!("../../../../packaging/voisu.service");
        let restrictions = packaged_unit_restrictions(unit);
        assert!(restrictions.memory_deny_write_execute);
        assert!(restrictions.restrict_namespaces);
        assert!(restrictions.no_new_privileges);
        assert!(restrictions.system_call_architectures_native);
    }

    #[test]
    fn worker_env_drops_credentials_proxy_preload_and_session_bus() {
        let inherited = [
            (OsString::from("PATH"), OsString::from("/usr/bin")),
            (
                OsString::from("VOISU_GROQ_API_KEY"),
                OsString::from("secret"),
            ),
            (
                OsString::from("https_proxy"),
                OsString::from("http://127.0.0.1"),
            ),
            (OsString::from("LD_PRELOAD"), OsString::from("evil.so")),
            (
                OsString::from("DBUS_SESSION_BUS_ADDRESS"),
                OsString::from("unix:path=/run/user/1000/bus"),
            ),
            (OsString::from("LANG"), OsString::from("C")),
        ];
        let scrubbed = scrub_worker_environment(inherited);
        assert!(!scrubbed.retained.contains_key("PATH"));
        assert!(!scrubbed.retained.contains_key("VOISU_GROQ_API_KEY"));
        assert!(!scrubbed.retained.contains_key("https_proxy"));
        assert!(!scrubbed.retained.contains_key("LD_PRELOAD"));
        assert!(!scrubbed.retained.contains_key("DBUS_SESSION_BUS_ADDRESS"));
        assert_eq!(
            scrubbed.retained.get("LANG").map(OsString::as_os_str),
            Some(OsStr::new("C"))
        );
    }

    #[test]
    fn intended_launcher_does_not_carve_loopback_or_jit_exceptions() {
        let policy = LauncherPolicy::intended_production();
        assert!(policy.deny_ip_sockets);
        assert!(policy.no_new_privs);
        assert!(policy.landlock_required);
        assert!(policy.deny_io_uring);
    }

    #[test]
    fn landlock_allowlist_excludes_home_config_and_diagnostics() {
        let paths = landlock_allowlist(
            PathBuf::from("/var/lib/voisu/models/ggml-small.en.bin"),
            PathBuf::from("/tmp/voisu-worker-cache"),
        );
        let rendered = paths
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(
            rendered
                .iter()
                .any(|path| path.contains("ggml-small.en.bin"))
        );
        assert!(!rendered.iter().any(|path| path.contains("/home")));
        assert!(!rendered.iter().any(|path| path.contains(".config")));
        assert!(!rendered.iter().any(|path| path.contains("diagnostics")));
        assert!(
            !rendered.iter().any(|path| path.contains("/dev/dri")
                || path.contains("nvidia")
                || path.contains("/dev/dxg")),
            "CPU-only spike must not allow GPU nodes before a measured GPU profile"
        );
    }
}
