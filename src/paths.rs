use std::path::PathBuf;

/// Where archix and pacman keep their state on a system.
#[derive(Debug, Clone)]
pub struct Paths {
    /// The real root, which holds archix/ with the deployments.
    pub root: PathBuf,
    pub pacman_db: PathBuf,
    pub pacman_cache: PathBuf,
    pub state: PathBuf,
    pub cache: PathBuf,
    pub pacman_conf: PathBuf,
    pub pacman_hooks: PathBuf,
    pub profile_d: PathBuf,
    pub autostart: PathBuf,
}

impl Paths {
    /// The running system's, or with ARCHIX_TEST_ROOT a test root's: its pacman database and /etc, with archix's state and the caches where they are.
    pub fn system() -> Paths {
        let root = crate::testhooks::root();
        let inside = |path: &str| root.join(path);
        Paths {
            pacman_db: inside("var/lib/pacman"),
            pacman_cache: PathBuf::from("/var/cache/pacman/pkg"),
            state: PathBuf::from("/var/lib/archix"),
            cache: PathBuf::from("/var/cache/archix"),
            pacman_conf: inside("etc/pacman.conf"),
            pacman_hooks: inside("etc/pacman.d/hooks"),
            profile_d: inside("etc/profile.d"),
            autostart: inside("etc/xdg/autostart"),
            root,
        }
    }

    /// The same layout inside another root, such as a mounted system.
    pub fn under(root: &std::path::Path) -> Paths {
        let inside = |path: PathBuf| root.join(path.strip_prefix("/").unwrap_or(&path));
        let system = Paths::live();
        Paths {
            root: root.to_path_buf(),
            pacman_db: inside(system.pacman_db),
            pacman_cache: inside(system.pacman_cache),
            state: inside(system.state),
            cache: inside(system.cache),
            pacman_conf: inside(system.pacman_conf),
            pacman_hooks: inside(system.pacman_hooks),
            profile_d: inside(system.profile_d),
            autostart: inside(system.autostart),
        }
    }

    /// `under(root)` for a root mounted on its own, as the rescue entry and a live USB mount it: a root laid out for rollbacks has archix's state and pacman's database under /var/rollback, which only its boot binds over /var/lib.
    pub fn on_root(root: &std::path::Path) -> Paths {
        let mut paths = Paths::under(root);
        let rolled = root.join(crate::snapshot::layout::ON_ROOT);
        if rolled.join("archix").is_dir() {
            paths.state = rolled.join("archix");
        }
        if rolled.join("pacman").is_dir() {
            paths.pacman_db = rolled.join("pacman");
        }
        paths
    }

    fn live() -> Paths {
        Paths {
            pacman_db: PathBuf::from("/var/lib/pacman"),
            pacman_cache: PathBuf::from("/var/cache/pacman/pkg"),
            state: PathBuf::from("/var/lib/archix"),
            cache: PathBuf::from("/var/cache/archix"),
            pacman_conf: PathBuf::from("/etc/pacman.conf"),
            pacman_hooks: PathBuf::from("/etc/pacman.d/hooks"),
            profile_d: PathBuf::from("/etc/profile.d"),
            autostart: PathBuf::from("/etc/xdg/autostart"),
            root: PathBuf::from("/"),
        }
    }

    pub fn local_db(&self) -> PathBuf {
        self.pacman_db.join("local")
    }

    pub fn pacman_lock(&self) -> PathBuf {
        self.pacman_db.join("db.lck")
    }

    /// What a boot fallback wants you to know, shown at login until the next switch or rollback.
    pub fn notes(&self) -> PathBuf {
        self.state.join("notes")
    }

    pub fn outside_changes(&self) -> PathBuf {
        self.state.join("outside-changes.json")
    }

    /// archix's own package cache: on @archix when the root has it, so it stays out of paccache's reach and survives a keep; else under the cache directory.
    pub fn packages(&self) -> PathBuf {
        let on_archix = self.root.join(crate::snapshot::MOUNT);
        if crate::snapshot::archix_on(&self.root) {
            return on_archix.join("packages");
        }
        self.cache.join("pkg")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_root_mounted_without_its_binds_has_its_state_and_database_under_var_rollback() {
        let root = tempfile::tempdir().unwrap();
        let plain = Paths::on_root(root.path());
        assert_eq!(
            plain.state,
            root.path().join("var/lib/archix"),
            "a root not laid out for rollbacks"
        );
        assert_eq!(plain.pacman_db, root.path().join("var/lib/pacman"));
        std::fs::create_dir_all(root.path().join("var/rollback/archix")).unwrap();
        std::fs::create_dir_all(root.path().join("var/rollback/pacman")).unwrap();
        let laid_out = Paths::on_root(root.path());
        assert_eq!(laid_out.state, root.path().join("var/rollback/archix"));
        assert_eq!(laid_out.pacman_db, root.path().join("var/rollback/pacman"));
        assert_eq!(laid_out.pacman_conf, root.path().join("etc/pacman.conf"));
    }
}
