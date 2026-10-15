use std::path::PathBuf;

/// Where archix and pacman keep their state on a system.
#[derive(Debug, Clone)]
pub struct Paths {
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
    pub fn system() -> Paths {
        Paths {
            pacman_db: PathBuf::from("/var/lib/pacman"),
            pacman_cache: PathBuf::from("/var/cache/pacman/pkg"),
            state: PathBuf::from("/var/lib/archix"),
            cache: PathBuf::from("/var/cache/archix"),
            pacman_conf: PathBuf::from("/etc/pacman.conf"),
            pacman_hooks: PathBuf::from("/etc/pacman.d/hooks"),
            profile_d: PathBuf::from("/etc/profile.d"),
            autostart: PathBuf::from("/etc/xdg/autostart"),
        }
    }

    /// The same layout inside another root, such as a mounted system being repaired from a live USB.
    pub fn under(root: &std::path::Path) -> Paths {
        let inside = |path: PathBuf| root.join(path.strip_prefix("/").unwrap_or(&path));
        let system = Paths::system();
        Paths {
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

    pub fn local_db(&self) -> PathBuf {
        self.pacman_db.join("local")
    }

    pub fn pacman_lock(&self) -> PathBuf {
        self.pacman_db.join("db.lck")
    }

    pub fn journal(&self) -> PathBuf {
        self.state.join("journal.json")
    }

    /// What a boot-time rollback wants you to know, shown at login until the next switch or rollback.
    pub fn notes(&self) -> PathBuf {
        self.state.join("notes")
    }

    /// Files whose on_change scripts a boot-time rollback skipped, run by the next switch or rollback.
    pub fn pending_on_change(&self) -> PathBuf {
        self.state.join("pending-on-change")
    }

    pub fn outside_changes(&self) -> PathBuf {
        self.state.join("outside-changes.json")
    }

    pub fn packages(&self) -> PathBuf {
        self.cache.join("pkg")
    }
}
