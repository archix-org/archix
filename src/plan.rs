use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt;

use crate::lock::{LOCAL_REPO, Locked};
use crate::pacman::{LocalPkg, vercmp};

/// One change needed to make the installed packages match a lock.
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    Install {
        name: String,
        version: String,
    },
    Upgrade {
        name: String,
        from: String,
        to: String,
    },
    Downgrade {
        name: String,
        from: String,
        to: String,
    },
    Remove {
        name: String,
        version: String,
    },
    Reason {
        name: String,
        explicit: bool,
    },
    /// Same version, but built from a recipe in the config that changed since.
    Rebuild {
        name: String,
        version: String,
    },
}

impl fmt::Display for Change {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Change::Install { name, version } => write!(f, "install    {name} {version}"),
            Change::Upgrade { name, from, to } => write!(f, "upgrade    {name} {from} -> {to}"),
            Change::Downgrade { name, from, to } => write!(f, "downgrade  {name} {from} -> {to}"),
            Change::Remove { name, version } => write!(f, "remove     {name} {version} (not in the lock)"),
            Change::Reason { name, explicit: true } => write!(f, "mark       {name} as explicitly installed"),
            Change::Reason { name, explicit: false } => write!(f, "mark       {name} as a dependency"),
            Change::Rebuild { name, version } => {
                write!(f, "reinstall  {name} {version} (a build from a different recipe)")
            }
        }
    }
}

/// The changes, sorted by package name, that turn `installed` into exactly the `locked` set.
pub fn plan(locked: &BTreeMap<String, Locked>, installed: &BTreeMap<String, LocalPkg>) -> Vec<Change> {
    let mut changes = Vec::new();
    for (name, want) in locked.iter().filter(|(_, want)| !want.build_only) {
        let Some(have) = installed.get(name) else {
            changes.push(Change::Install {
                name: name.clone(),
                version: want.version.clone(),
            });
            continue;
        };
        let (from, to) = (have.version.clone(), want.version.clone());
        match vercmp(&want.version, &have.version) {
            Ordering::Greater => changes.push(Change::Upgrade {
                name: name.clone(),
                from,
                to,
            }),
            Ordering::Less => changes.push(Change::Downgrade {
                name: name.clone(),
                from,
                to,
            }),
            Ordering::Equal if have.version != want.version => changes.push(Change::Upgrade {
                name: name.clone(),
                from,
                to,
            }),
            Ordering::Equal => {}
        }
        if have.explicit != want.explicit {
            changes.push(Change::Reason {
                name: name.clone(),
                explicit: want.explicit,
            });
        }
    }
    for (name, have) in installed {
        if locked.get(name).is_none_or(|want| want.build_only) {
            changes.push(Change::Remove {
                name: name.clone(),
                version: have.version.clone(),
            });
        }
    }
    changes.sort_by(|a, b| change_name(a).cmp(change_name(b)));
    changes
}

fn change_name(change: &Change) -> &str {
    match change {
        Change::Install { name, .. }
        | Change::Upgrade { name, .. }
        | Change::Downgrade { name, .. }
        | Change::Remove { name, .. }
        | Change::Reason { name, .. }
        | Change::Rebuild { name, .. } => name,
    }
}

/// Packages from the config's recipes installed at the locked version but last built by archix from another recipe, or never by archix.
pub fn rebuilds(
    locked: &BTreeMap<String, Locked>,
    installed: &BTreeMap<String, LocalPkg>,
    built_from: Option<&BTreeMap<String, Locked>>,
) -> Vec<Change> {
    locked
        .iter()
        .filter(|(_, want)| want.repo == LOCAL_REPO && !want.build_only)
        .filter(|(name, want)| installed.get(*name).is_some_and(|have| have.version == want.version))
        .filter(|(name, want)| {
            built_from
                .and_then(|was| was.get(*name))
                .is_none_or(|was| was.commit != want.commit)
        })
        .map(|(name, want)| Change::Rebuild {
            name: name.clone(),
            version: want.version.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn locked(entries: &[(&str, &str, bool)]) -> BTreeMap<String, Locked> {
        entries
            .iter()
            .map(|(name, version, explicit)| {
                let pkg = Locked {
                    version: version.to_string(),
                    repo: "core".into(),
                    explicit: *explicit,
                    ..Default::default()
                };
                (name.to_string(), pkg)
            })
            .collect()
    }

    fn installed(entries: &[(&str, &str, bool)]) -> BTreeMap<String, LocalPkg> {
        entries
            .iter()
            .map(|(name, version, explicit)| {
                let pkg = LocalPkg {
                    name: name.to_string(),
                    version: version.to_string(),
                    arch: "x86_64".into(),
                    explicit: *explicit,
                    installed_at: None,
                };
                (name.to_string(), pkg)
            })
            .collect()
    }

    #[test]
    fn covers_every_kind_of_change() {
        let lock = locked(&[
            ("git", "2.51-1", true),
            ("glibc", "2.44-1", false),
            ("kitty", "0.43-1", true),
            ("zlib", "1:1.3-2", false),
        ]);
        let have = installed(&[
            ("git", "2.50-1", true),
            ("glibc", "2.45-1", false),
            ("htop", "3.4-1", true),
            ("zlib", "1:1.3-2", true),
        ]);
        let changes: Vec<String> = plan(&lock, &have).iter().map(ToString::to_string).collect();
        assert_eq!(
            changes,
            [
                "upgrade    git 2.50-1 -> 2.51-1",
                "downgrade  glibc 2.45-1 -> 2.44-1",
                "remove     htop 3.4-1 (not in the lock)",
                "install    kitty 0.43-1",
                "mark       zlib as a dependency",
            ]
        );
    }

    #[test]
    fn nothing_to_do_when_matching() {
        let lock = locked(&[("git", "2.51-1", true)]);
        assert!(plan(&lock, &installed(&[("git", "2.51-1", true)])).is_empty());
    }
}
