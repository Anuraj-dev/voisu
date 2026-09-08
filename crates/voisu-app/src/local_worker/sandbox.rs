//! R3 offline threat-model spike: launcher policy, env scrub, unit inventory.
//!
//! Shared desktop IPC to PipeWire, the compositor, and Delivery remains
//! permitted. Local processing itself must perform no DNS or IP traffic,
//! including loopback, and must read no Cloud credentials.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;

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
    "PYTHONPATH",
    "DBUS_SESSION_BUS_ADDRESS",
    "SSH_AUTH_SOCK",
    "VOISU_GROQ_API_KEY",
    "VOISU_DEEPGRAM_API_KEY",
    "GROQ_API_KEY",
    "DEEPGRAM_API_KEY",
    "OPENAI_API_KEY",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_ACCESS_KEY_ID",
];

/// Worker environment: no credentials, proxy, preload, or session-bus address.
#[must_use]
pub fn scrub_worker_environment<I>(inherited: I) -> ScrubbedEnvironment
where
    I: IntoIterator<Item = (OsString, OsString)>,
{
    let mut retained = BTreeMap::new();
    for (key, value) in inherited {
        let Some(name) = key.to_str() else {
            continue;
        };
        if FORBIDDEN_ENV.contains(&name) {
            continue;
        }
        if name.ends_with("_API_KEY") || name.contains("SECRET") || name.contains("TOKEN") {
            continue;
        }
        // PATH is required to resolve allowlisted runtime libraries after
        // Landlock; the launcher still execs an absolute program path.
        if name == "PATH" || name == "LANG" || name == "LC_ALL" {
            retained.insert(name.to_owned(), value);
        }
    }
    ScrubbedEnvironment { retained }
}

/// Landlock allowlist: runtime libs, chosen model, approved GPU nodes, private cache.
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
        assert_eq!(
            scrubbed.retained.get("PATH").map(OsString::as_os_str),
            Some(OsStr::new("/usr/bin"))
        );
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
    }
}
