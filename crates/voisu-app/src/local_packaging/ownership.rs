//! Upgrade/uninstall must not take user-owned Local artifacts.

use std::path::{Component, Path};

/// User-owned trees as trailing path components, so the check holds for
/// `/home/*`, `/root`, `/var/home/*` (Silverblue), and any other `$HOME`.
const USER_OWNED_TAILS: [&[&str]; 3] = [
    &[".config", "voisu"],
    &[".local", "state", "voisu"],
    &[".local", "share", "voisu"],
];

fn ends_with_components(path: &Path, tail: &[&str]) -> bool {
    let owned: Vec<&str> = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => part.to_str(),
            _ => None,
        })
        .collect();
    owned.len() >= tail.len() && owned[owned.len() - tail.len()..] == *tail
}

fn under_user_owned_tree(path: &Path) -> bool {
    if USER_OWNED_TAILS
        .iter()
        .any(|tail| ends_with_components(path, tail))
    {
        return true;
    }
    path.starts_with("/home")
        || path.starts_with("/var/home")
        || path == Path::new("/root")
        || path.starts_with("/root/")
}

/// Printed by package scriptlets so uninstall does not look like a wipe.
pub const USER_OWNED_HINT: &str = "\
Your Voisu configuration, Local models, recovery/debug audio, and credentials \
under ~/.config/voisu, ~/.local/state/voisu, and ~/.local/share/voisu are left \
untouched. Uninstall does not remove unrelated models, user debug audio, or \
active leased artifacts.";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UserOwnedTree {
    pub label: &'static str,
    pub relative: &'static str,
}

#[must_use]
pub fn user_owned_trees() -> &'static [UserOwnedTree] {
    &[
        UserOwnedTree {
            label: "config",
            relative: ".config/voisu",
        },
        UserOwnedTree {
            label: "state",
            relative: ".local/state/voisu",
        },
        UserOwnedTree {
            label: "share",
            relative: ".local/share/voisu",
        },
    ]
}

/// Packaged payloads live under /usr. Anything user-owned — by tree suffix
/// or by home-root prefix — is never removed. Fail-closed: non-absolute and
/// unknown roots (e.g. /opt) are not package-removable either.
#[must_use]
pub fn package_may_remove(path: &Path) -> bool {
    if under_user_owned_tree(path) {
        return false;
    }
    path.starts_with("/usr/")
}

#[must_use]
pub fn user_tree_survives(path: &Path) -> bool {
    !package_may_remove(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn package_owns_usr_not_home() {
        assert!(package_may_remove(Path::new("/usr/bin/voisu")));
        assert!(package_may_remove(Path::new(
            "/usr/lib/systemd/user/voisu.service"
        )));
        assert!(!package_may_remove(Path::new(
            "/home/raja/.config/voisu/config.toml"
        )));
        assert!(!package_may_remove(Path::new(
            "/home/raja/.local/state/voisu/models/private/active.receipt"
        )));
        assert!(!package_may_remove(Path::new(
            "/home/raja/.local/state/voisu/local-recovery/rec.wav"
        )));
        assert!(!package_may_remove(Path::new(
            "/home/raja/.local/share/voisu/models/unrelated.bin"
        )));
        assert!(user_tree_survives(&PathBuf::from(
            "/home/raja/.local/state/voisu/models"
        )));
        // Non-/home roots and sneaky payloads stay user-owned (fail-closed).
        assert!(!package_may_remove(Path::new(
            "/root/.config/voisu/config.toml"
        )));
        assert!(!package_may_remove(Path::new(
            "/var/home/raja/.local/state/voisu/models/private/active.receipt"
        )));
        assert!(!package_may_remove(Path::new("/usr/.config/voisu")));
        assert!(!package_may_remove(Path::new("/opt/voisu/voisu")));
        assert!(!package_may_remove(Path::new("relative/voisu")));
        assert!(USER_OWNED_HINT.contains("left untouched"));
        assert!(user_owned_trees().len() >= 3);
    }
}
