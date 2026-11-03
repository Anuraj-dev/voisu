//! `voisu update` on Linux: upgrades Voisu through the package manager that
//! owns the running `voisu` binary, then restarts the active user services.
//!
//! The package manager owns the installed files. This module never writes to
//! `/usr/bin` and never replaces a binary itself; it only decides which
//! package-manager command fits the install ([`plan_update`], pure) and runs
//! it ([`run`], a thin executor). The network is touched only by
//! `voisu update --check`, through the same restricted `curl` subprocess the
//! provider requests use.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use voisu_cloud::process::{guard_external_child, run_restricted_with_deadline};

pub const UPDATE_USAGE: &str = "usage: voisu update [--check|--dry-run] [--switch-to-bin]";

/// Where `voisu update` sends a build no package owns.
pub const INSTALL_DOCS_URL: &str = "https://github.com/Anuraj-Dev/voisu#installation";

const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/Anuraj-Dev/voisu/releases/latest";

/// Test and diagnostic seam: the binary whose owning package decides the
/// plan, in place of the running executable. Only package names flow from
/// it, so it cannot steer which command runs beyond the fixed plans below.
const BINARY_OVERRIDE_ENV: &str = "VOISU_UPDATE_BINARY";

/// The user services restarted after an upgrade, when they are active.
const USER_SERVICES: [&str; 2] = ["voisu.service", "voisu-overlay.service"];

const QUERY_DEADLINE: Duration = Duration::from_secs(5);
const RELEASE_DEADLINE: Duration = Duration::from_secs(15);
const RESTART_DEADLINE: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UpdateArgs {
    pub check: bool,
    pub dry_run: bool,
    pub switch_to_bin: bool,
}

/// Parses the arguments after `update`. `--check` only reads, so it stands
/// alone; `--dry-run` and `--switch-to-bin` combine in any order.
pub fn parse_update_args(args: &[String]) -> Result<UpdateArgs, String> {
    let mut parsed = UpdateArgs {
        check: false,
        dry_run: false,
        switch_to_bin: false,
    };
    for arg in args {
        let flag = match arg.as_str() {
            "--check" => &mut parsed.check,
            "--dry-run" => &mut parsed.dry_run,
            "--switch-to-bin" => &mut parsed.switch_to_bin,
            _ => return Err(UPDATE_USAGE.to_owned()),
        };
        if *flag {
            return Err(UPDATE_USAGE.to_owned());
        }
        *flag = true;
    }
    if parsed.check && (parsed.dry_run || parsed.switch_to_bin) {
        return Err(UPDATE_USAGE.to_owned());
    }
    Ok(parsed)
}

/// The package database that answered which package owns the binary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OwnerManager {
    Pacman,
    Rpm,
    Dpkg,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackageOwner {
    pub manager: OwnerManager,
    pub package: String,
}

/// AUR helpers, in the order `voisu update` prefers them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AurHelper {
    Yay,
    Paru,
}

impl AurHelper {
    pub const PREFERENCE: [Self; 2] = [Self::Yay, Self::Paru];

    pub fn program(self) -> &'static str {
        match self {
            Self::Yay => "yay",
            Self::Paru => "paru",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UpdatePlan {
    /// Run `commands` in order; on success restart the active user services.
    Upgrade {
        notes: Vec<String>,
        commands: Vec<Vec<String>>,
    },
    /// Nothing can run on the user's behalf: print `message`, exit non-zero.
    ActionRequired { message: String },
}

/// Decides how to update Voisu from the package that owns the binary and the
/// AUR helper on `PATH`. `switch_to_bin` is the explicit consent to replace
/// the Arch source package `voisu` with the prebuilt `voisu-bin`; every other
/// install is already prebuilt, so the flag changes nothing there.
pub fn plan_update(
    owner: Option<&PackageOwner>,
    aur_helper: Option<AurHelper>,
    switch_to_bin: bool,
) -> UpdatePlan {
    let Some(owner) = owner else {
        return UpdatePlan::ActionRequired {
            message: format!(
                "This voisu binary is not owned by a package (for example a cargo or \
                 development build), so `voisu update` cannot upgrade it.\n\
                 Install a released package instead: {INSTALL_DOCS_URL}"
            ),
        };
    };
    match (owner.manager, owner.package.as_str()) {
        (OwnerManager::Pacman, "voisu-bin") => match aur_helper {
            Some(helper) => UpdatePlan::Upgrade {
                notes: Vec::new(),
                commands: vec![argv(&[helper.program(), "-S", "--needed", "voisu-bin"])],
            },
            None => UpdatePlan::ActionRequired {
                message: format!(
                    "Voisu is installed from the AUR package voisu-bin, but no AUR helper \
                     (yay or paru) is on PATH. Update it manually:\n{}",
                    manual_aur_steps("voisu-bin")
                ),
            },
        },
        (OwnerManager::Pacman, "voisu") if switch_to_bin => match aur_helper {
            Some(helper) => UpdatePlan::Upgrade {
                notes: vec![format!(
                    "voisu-bin replaces the source package voisu (they conflict); \
                     confirm the removal when {} asks.",
                    helper.program()
                )],
                commands: vec![argv(&[helper.program(), "-S", "voisu-bin"])],
            },
            None => UpdatePlan::ActionRequired {
                message: format!(
                    "No AUR helper (yay or paru) is on PATH. Switch to the prebuilt \
                     package manually; makepkg offers to remove voisu:\n{}",
                    manual_aur_steps("voisu-bin")
                ),
            },
        },
        (OwnerManager::Pacman, "voisu") => {
            let rebuild = match aur_helper {
                Some(helper) => format!("  {} -S voisu", helper.program()),
                None => manual_aur_steps("voisu"),
            };
            UpdatePlan::ActionRequired {
                message: format!(
                    "Voisu is installed from the AUR source package voisu, which compiles \
                     Voisu on this machine for every update.\n\
                     Switch to the prebuilt package voisu-bin (it replaces voisu):\n  \
                     voisu update --switch-to-bin\n\
                     Or keep building from source:\n{rebuild}"
                ),
            }
        }
        (OwnerManager::Rpm, "voisu") => UpdatePlan::Upgrade {
            notes: Vec::new(),
            commands: vec![argv(&["sudo", "dnf", "upgrade", "--refresh", "voisu"])],
        },
        (OwnerManager::Dpkg, "voisu") => UpdatePlan::Upgrade {
            notes: Vec::new(),
            commands: vec![
                argv(&["sudo", "apt-get", "update"]),
                argv(&["sudo", "apt-get", "install", "--only-upgrade", "voisu"]),
            ],
        },
        (_, package) => UpdatePlan::ActionRequired {
            message: format!(
                "This voisu binary belongs to the package {package}, which Voisu does not \
                 publish. Update it with the package manager that installed it."
            ),
        },
    }
}

/// How Voisu is installed, in words, for `voisu update` and `--check`.
pub fn install_source(owner: Option<&PackageOwner>) -> &'static str {
    let Some(owner) = owner else {
        return "a build no package owns";
    };
    match (owner.manager, owner.package.as_str()) {
        (OwnerManager::Pacman, "voisu-bin") => "Arch package voisu-bin, prebuilt",
        (OwnerManager::Pacman, "voisu") => "Arch package voisu, built from source on this machine",
        (OwnerManager::Rpm, "voisu") => "Fedora package voisu from COPR",
        (OwnerManager::Dpkg, "voisu") => "Debian/Ubuntu package voisu from apt",
        _ => "a package Voisu does not publish",
    }
}

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| (*part).to_owned()).collect()
}

fn manual_aur_steps(package: &str) -> String {
    format!("  git clone https://aur.archlinux.org/{package}.git\n  cd {package}\n  makepkg -si")
}

/// Reads `pacman -Qqo`, `rpm -qf --qf '%{NAME}'`, or `dpkg -S` output into the
/// owning package's name. `None` when the output names no package.
pub fn parse_owner_output(manager: OwnerManager, stdout: &str) -> Option<String> {
    let first = stdout.lines().next()?.trim();
    let name = match manager {
        OwnerManager::Pacman | OwnerManager::Rpm => first,
        // `voisu: /usr/bin/voisu`, or `voisu:amd64: /usr/bin/voisu` for a
        // multi-arch package.
        OwnerManager::Dpkg => {
            let (package, _) = first.split_once(": ")?;
            package.split(':').next()?
        }
    };
    (!name.is_empty() && !name.contains(char::is_whitespace)).then(|| name.to_owned())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReleaseComparison {
    UpToDate,
    UpdateAvailable,
    InstalledNewer,
    Unknown,
}

/// Compares an installed `X.Y.Z` version with a release tag (`vX.Y.Z` or
/// `X.Y.Z`). Anything that is not three numbers is `Unknown`.
pub fn compare_release(installed: &str, latest_tag: &str) -> ReleaseComparison {
    match (parse_version(installed), parse_version(latest_tag)) {
        (Some(installed), Some(latest)) => match installed.cmp(&latest) {
            std::cmp::Ordering::Less => ReleaseComparison::UpdateAvailable,
            std::cmp::Ordering::Equal => ReleaseComparison::UpToDate,
            std::cmp::Ordering::Greater => ReleaseComparison::InstalledNewer,
        },
        _ => ReleaseComparison::Unknown,
    }
}

fn parse_version(value: &str) -> Option<(u64, u64, u64)> {
    let value = value.trim();
    let value = value.strip_prefix('v').unwrap_or(value);
    let mut parts = value.split('.').map(|part| part.parse::<u64>().ok());
    let version = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some(version)
}

/// Runs `voisu update` and returns the process exit code: 0 done, 1 the user
/// must act (instructions printed), 2 usage, 4 a command failed.
pub fn run(args: &[String]) -> u8 {
    let args = match parse_update_args(args) {
        Ok(args) => args,
        Err(message) => {
            eprintln!("{message}");
            return 2;
        }
    };
    let binary = match installed_binary() {
        Ok(binary) => binary,
        Err(message) => {
            eprintln!("{message}");
            return 4;
        }
    };
    let owner = package_owner(&binary);
    // Read from the owned binary rather than this process, so the version and
    // the owning package always describe the same file.
    let installed =
        installed_version(&binary).unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_owned());
    if args.check {
        return check(&installed, owner.as_ref());
    }
    let plan = plan_update(owner.as_ref(), detect_aur_helper(), args.switch_to_bin);
    let (notes, commands) = match plan {
        UpdatePlan::ActionRequired { message } => {
            println!("{message}");
            return 1;
        }
        UpdatePlan::Upgrade { notes, commands } => (notes, commands),
    };
    println!(
        "Installed: {installed} ({})",
        install_source(owner.as_ref())
    );
    for note in &notes {
        println!("{note}");
    }
    if args.dry_run {
        for command in &commands {
            println!("would run: {}", command.join(" "));
        }
        match active_services() {
            Some(active) => println!(
                "would run: systemctl --user try-restart {}",
                active.join(" ")
            ),
            None => println!("no active Voisu user services to restart"),
        }
        return 0;
    }
    for command in &commands {
        println!("running: {}", command.join(" "));
        if !run_interactive(command) {
            eprintln!("update failed: `{}` did not succeed", command.join(" "));
            return 4;
        }
    }
    if let Some(active) = active_services() {
        if let Err(message) = restart_services(&active) {
            eprintln!("Voisu was updated, but restarting its user services failed: {message}");
            return 4;
        }
        println!("restarted: {}", active.join(" "));
    }
    match installed_version(&binary) {
        Some(version) => println!("Now installed: voisu {version}"),
        None => println!("Update finished; run `voisu --version` to confirm the version."),
    }
    0
}

/// What `--check` suggests when a newer release exists. The source package
/// is never rebuilt by a plain `voisu update`, so it points at the switch.
fn update_hint(owner: Option<&PackageOwner>) -> &'static str {
    match owner {
        Some(owner) if owner.manager == OwnerManager::Pacman && owner.package == "voisu" => {
            "Update available: run `voisu update --switch-to-bin` to install the prebuilt release."
        }
        _ => "Update available: run `voisu update` to install it.",
    }
}

fn check(installed: &str, owner: Option<&PackageOwner>) -> u8 {
    println!("Installed: {installed} ({})", install_source(owner));
    let latest = match latest_release_tag() {
        Ok(tag) => tag,
        Err(detail) => {
            eprintln!("cannot read the latest release from GitHub: {detail}");
            return 4;
        }
    };
    let latest_display = latest.strip_prefix('v').unwrap_or(&latest);
    println!("Latest release: {latest_display}");
    match compare_release(installed, &latest) {
        ReleaseComparison::UpToDate => println!("Voisu is up to date."),
        ReleaseComparison::UpdateAvailable => println!("{}", update_hint(owner)),
        ReleaseComparison::InstalledNewer => {
            println!("The installed version is newer than the latest release.")
        }
        ReleaseComparison::Unknown => println!("Cannot compare these versions."),
    }
    0
}

fn installed_binary() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os(BINARY_OVERRIDE_ENV) {
        return Ok(PathBuf::from(path));
    }
    std::env::current_exe().map_err(|_| "cannot locate the running voisu binary".to_owned())
}

/// Asks each package database in turn which package owns `binary`. A missing
/// tool or a "not owned" answer moves on to the next database.
fn package_owner(binary: &Path) -> Option<PackageOwner> {
    let path = binary.to_str()?;
    let queries: [(OwnerManager, &str, &[&str]); 3] = [
        (OwnerManager::Pacman, "pacman", &["-Qqo"]),
        (OwnerManager::Rpm, "rpm", &["-qf", "--qf", "%{NAME}"]),
        (OwnerManager::Dpkg, "dpkg", &["-S"]),
    ];
    queries.into_iter().find_map(|(manager, program, args)| {
        let mut arguments = args.to_vec();
        arguments.push(path);
        let outcome =
            run_restricted_with_deadline(program, &arguments, None, true, QUERY_DEADLINE, None)
                .ok()?;
        if !outcome.success {
            return None;
        }
        let package = parse_owner_output(manager, &String::from_utf8_lossy(&outcome.stdout))?;
        Some(PackageOwner { manager, package })
    })
}

fn detect_aur_helper() -> Option<AurHelper> {
    let path = std::env::var_os("PATH")?;
    AurHelper::PREFERENCE
        .into_iter()
        .find(|helper| crate::system::resolve_on_path(&path, helper.program()).is_some())
}

/// The Voisu user services that are active now, or `None` when none are.
fn active_services() -> Option<Vec<&'static str>> {
    let active: Vec<_> = USER_SERVICES
        .into_iter()
        .filter(|unit| {
            run_restricted_with_deadline(
                "systemctl",
                &["--user", "is-active", "--quiet", unit],
                None,
                false,
                QUERY_DEADLINE,
                None,
            )
            .is_ok_and(|outcome| outcome.success)
        })
        .collect();
    (!active.is_empty()).then_some(active)
}

fn restart_services(units: &[&str]) -> Result<(), String> {
    let mut arguments = vec!["--user", "try-restart"];
    arguments.extend_from_slice(units);
    let outcome =
        run_restricted_with_deadline("systemctl", &arguments, None, false, RESTART_DEADLINE, None)
            .map_err(|_| "systemctl --user did not complete".to_owned())?;
    if outcome.success {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&outcome.stderr).trim().to_owned())
    }
}

/// Runs a package-manager command attached to the user's terminal, so `sudo`
/// and the AUR helper can prompt. No deadline: the user drives it.
fn run_interactive(command: &[String]) -> bool {
    let Some((program, arguments)) = command.split_first() else {
        return false;
    };
    let mut process = Command::new(program);
    process.args(arguments);
    guard_external_child(&mut process);
    process.status().is_ok_and(|status| status.success())
}

fn installed_version(binary: &Path) -> Option<String> {
    let outcome = run_restricted_with_deadline(
        binary.to_str()?,
        &["--version"],
        None,
        true,
        QUERY_DEADLINE,
        None,
    )
    .ok()?;
    if !outcome.success {
        return None;
    }
    // `voisu --version` prints `voisu X.Y.Z`.
    let printed = String::from_utf8_lossy(&outcome.stdout);
    let version = printed.trim().strip_prefix("voisu ")?.trim();
    (!version.is_empty()).then(|| version.to_owned())
}

/// The latest GitHub Release tag, fetched through the restricted `curl`
/// subprocess. The tag sits near the top of the response, well inside the
/// runner's retained-stdout cap.
fn latest_release_tag() -> Result<String, String> {
    let user_agent = format!("User-Agent: voisu/{}", env!("CARGO_PKG_VERSION"));
    let outcome = run_restricted_with_deadline(
        "curl",
        &[
            "-q",
            "--silent",
            "--show-error",
            "--fail",
            "--location",
            "--max-time",
            "10",
            "--header",
            "Accept: application/vnd.github+json",
            "--header",
            &user_agent,
            LATEST_RELEASE_URL,
        ],
        None,
        true,
        RELEASE_DEADLINE,
        None,
    )
    .map_err(|_| "curl is unavailable or timed out".to_owned())?;
    if !outcome.success {
        let detail = String::from_utf8_lossy(&outcome.stderr).trim().to_owned();
        return Err(if detail.is_empty() {
            "request failed".to_owned()
        } else {
            detail
        });
    }
    let release: serde_json::Value = serde_json::from_slice(&outcome.stdout)
        .map_err(|_| "GitHub returned an unreadable release".to_owned())?;
    release
        .get("tag_name")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| "GitHub returned a release without a tag".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner(manager: OwnerManager, package: &str) -> PackageOwner {
        PackageOwner {
            manager,
            package: package.to_owned(),
        }
    }

    fn commands(plan: &UpdatePlan) -> Vec<String> {
        match plan {
            UpdatePlan::Upgrade { commands, .. } => {
                commands.iter().map(|command| command.join(" ")).collect()
            }
            UpdatePlan::ActionRequired { message } => panic!("expected commands: {message}"),
        }
    }

    fn message(plan: &UpdatePlan) -> &str {
        match plan {
            UpdatePlan::ActionRequired { message } => message,
            UpdatePlan::Upgrade { commands, .. } => panic!("expected a message: {commands:?}"),
        }
    }

    #[test]
    fn prebuilt_arch_package_upgrades_through_the_aur_helper() {
        let installed = owner(OwnerManager::Pacman, "voisu-bin");
        for helper in AurHelper::PREFERENCE {
            let plan = plan_update(Some(&installed), Some(helper), false);
            assert_eq!(
                commands(&plan),
                [format!("{} -S --needed voisu-bin", helper.program())]
            );
        }
    }

    #[test]
    fn prebuilt_arch_package_without_a_helper_prints_manual_aur_steps() {
        let plan = plan_update(Some(&owner(OwnerManager::Pacman, "voisu-bin")), None, false);
        let text = message(&plan);
        assert!(text.contains("yay or paru"), "{text}");
        assert!(
            text.contains("git clone https://aur.archlinux.org/voisu-bin.git"),
            "{text}"
        );
        assert!(text.contains("makepkg -si"), "{text}");
    }

    #[test]
    fn source_arch_package_offers_the_switch_and_runs_nothing_without_consent() {
        let installed = owner(OwnerManager::Pacman, "voisu");
        let plan = plan_update(Some(&installed), Some(AurHelper::Paru), false);
        let text = message(&plan);
        assert!(text.contains("compiles"), "{text}");
        assert!(text.contains("voisu update --switch-to-bin"), "{text}");
        assert!(text.contains("paru -S voisu"), "{text}");

        let no_helper = plan_update(Some(&installed), None, false);
        assert!(
            message(&no_helper).contains("aur.archlinux.org/voisu.git"),
            "{no_helper:?}"
        );
    }

    #[test]
    fn switch_to_bin_replaces_the_source_package_with_voisu_bin() {
        let installed = owner(OwnerManager::Pacman, "voisu");
        let plan = plan_update(Some(&installed), Some(AurHelper::Yay), true);
        assert_eq!(commands(&plan), ["yay -S voisu-bin"]);
        let UpdatePlan::Upgrade { notes, .. } = &plan else {
            unreachable!()
        };
        assert!(
            notes.iter().any(|note| note.contains("conflict")),
            "{notes:?}"
        );

        let no_helper = plan_update(Some(&installed), None, true);
        assert!(
            message(&no_helper).contains("aur.archlinux.org/voisu-bin.git"),
            "{no_helper:?}"
        );
    }

    #[test]
    fn fedora_and_debian_packages_upgrade_through_their_package_manager() {
        // The flag only matters for the Arch source package.
        for switch_to_bin in [false, true] {
            let fedora = plan_update(
                Some(&owner(OwnerManager::Rpm, "voisu")),
                Some(AurHelper::Yay),
                switch_to_bin,
            );
            assert_eq!(commands(&fedora), ["sudo dnf upgrade --refresh voisu"]);

            let debian = plan_update(
                Some(&owner(OwnerManager::Dpkg, "voisu")),
                None,
                switch_to_bin,
            );
            assert_eq!(
                commands(&debian),
                [
                    "sudo apt-get update",
                    "sudo apt-get install --only-upgrade voisu"
                ]
            );
        }
    }

    #[test]
    fn unowned_and_foreign_binaries_get_instructions_not_commands() {
        let unowned = plan_update(None, Some(AurHelper::Yay), true);
        assert!(message(&unowned).contains(INSTALL_DOCS_URL), "{unowned:?}");

        let foreign = plan_update(
            Some(&owner(OwnerManager::Pacman, "voisu-git")),
            Some(AurHelper::Yay),
            true,
        );
        assert!(message(&foreign).contains("voisu-git"), "{foreign:?}");
    }

    #[test]
    fn owner_output_parses_for_each_package_database() {
        assert_eq!(
            parse_owner_output(OwnerManager::Pacman, "voisu-bin\n").as_deref(),
            Some("voisu-bin")
        );
        assert_eq!(
            parse_owner_output(OwnerManager::Rpm, "voisu").as_deref(),
            Some("voisu")
        );
        assert_eq!(
            parse_owner_output(OwnerManager::Dpkg, "voisu: /usr/bin/voisu\n").as_deref(),
            Some("voisu")
        );
        assert_eq!(
            parse_owner_output(OwnerManager::Dpkg, "voisu:amd64: /usr/bin/voisu\n").as_deref(),
            Some("voisu")
        );
        assert_eq!(parse_owner_output(OwnerManager::Pacman, ""), None);
        assert_eq!(
            parse_owner_output(OwnerManager::Dpkg, "no path found matching"),
            None
        );
    }

    #[test]
    fn release_comparison_reads_tags_with_or_without_the_v_prefix() {
        assert_eq!(
            compare_release("0.67.0", "v0.68.0"),
            ReleaseComparison::UpdateAvailable
        );
        assert_eq!(
            compare_release("0.68.0", "0.68.0"),
            ReleaseComparison::UpToDate
        );
        assert_eq!(
            compare_release("0.10.0", "v0.9.9"),
            ReleaseComparison::InstalledNewer
        );
        assert_eq!(
            compare_release("0.68.0", "nightly"),
            ReleaseComparison::Unknown
        );
    }

    #[test]
    fn update_flags_parse_and_reject_contradictions() {
        let args = |values: &[&str]| -> Vec<String> {
            values.iter().map(|value| (*value).to_owned()).collect()
        };
        assert_eq!(
            parse_update_args(&args(&["--switch-to-bin", "--dry-run"])),
            Ok(UpdateArgs {
                check: false,
                dry_run: true,
                switch_to_bin: true
            })
        );
        assert!(parse_update_args(&args(&["--check"])).unwrap().check);
        for invalid in [
            &["--check", "--dry-run"][..],
            &["--dry-run", "--dry-run"],
            &["--yes"],
        ] {
            assert_eq!(
                parse_update_args(&args(invalid)),
                Err(UPDATE_USAGE.to_owned())
            );
        }
    }
}
