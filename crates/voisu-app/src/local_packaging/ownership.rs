//! Upgrade/uninstall must not take user-owned Local artifacts.

use std::path::Path;

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

/// Packaged files are under /usr. Home trees are user-owned.
#[must_use]
pub fn package_may_remove(path: &Path) -> bool {
    let text = path.to_string_lossy();
    if text.contains("/home/") || text.contains(".config/voisu") || text.contains(".local/") {
        return false;
    }
    text.starts_with("/usr/")
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
        assert!(USER_OWNED_HINT.contains("left untouched"));
        assert!(user_owned_trees().len() >= 3);
    }
}
