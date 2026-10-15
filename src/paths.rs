use std::path::PathBuf;

/// Where archix and pacman keep their state on a system.
#[derive(Debug, Clone)]
pub struct Paths {
    pub pacman_db: PathBuf,
    pub pacman_cache: PathBuf,
    pub state: PathBuf,
    pub cache: PathBuf,
}

impl Paths {
    pub fn system() -> Paths {
        Paths {
            pacman_db: PathBuf::from("/var/lib/pacman"),
            pacman_cache: PathBuf::from("/var/cache/pacman/pkg"),
            state: PathBuf::from("/var/lib/archix"),
            cache: PathBuf::from("/var/cache/archix"),
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

    pub fn packages(&self) -> PathBuf {
        self.cache.join("pkg")
    }
}
