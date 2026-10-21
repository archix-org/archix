use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use crate::paths::Paths;

/// Packages whose updates close holes in what runs as root or faces the network, flagged when they wait for a reboot; kernels are matched by name.
pub const SECURITY: &[&str] = &["glibc", "openssl", "openssh", "systemd", "sudo"];

const MARKER: &str = "waiting for a reboot (deployment ";

/// The package changes of the committed deployment that boots next over the one running.
#[derive(Debug, PartialEq)]
pub struct Waiting {
    pub next: u32,
    /// `name old -> new`, `name new` for an install, `name removed`.
    pub changes: Vec<String>,
    pub security: Vec<String>,
}

/// Whether a package's update matters for security before the reboot: a kernel or one on `SECURITY`.
pub fn security_relevant(name: &str) -> bool {
    let kernel = name == "linux"
        || name.starts_with("linux-")
            && !name.starts_with("linux-firmware")
            && !name.starts_with("linux-api-headers")
            && !name.ends_with("-headers")
            && !name.ends_with("-docs");
    kernel || SECURITY.contains(&name)
}

/// What differs between two pacman databases, as `Waiting` lists it.
pub fn changes(running: &Path, next: &Path) -> (Vec<String>, Vec<String>) {
    let versions = |db: &Path| -> BTreeMap<String, String> {
        crate::pacman::read_local_db(db)
            .unwrap_or_default()
            .into_values()
            .map(|pkg| (pkg.name, pkg.version))
            .collect()
    };
    let (old, new) = (versions(running), versions(next));
    let mut changes = Vec::new();
    let mut security = Vec::new();
    for name in old.keys().chain(new.keys().filter(|name| !old.contains_key(*name))) {
        let line = match (old.get(name), new.get(name)) {
            (Some(a), Some(b)) if a == b => continue,
            (Some(a), Some(b)) => format!("{name} {a} -> {b}"),
            (None, Some(b)) => format!("{name} {b}"),
            (Some(_), None) => format!("{name} removed"),
            (None, None) => continue,
        };
        changes.push(line);
        if security_relevant(name) {
            security.push(name.clone());
        }
    }
    (changes, security)
}

/// The updates the next boot brings, when the current deployment isn't the one running and changes packages.
pub fn waiting(root: &Path) -> Option<Waiting> {
    let booted = crate::system::boot::booted_deployment(root)?;
    let target = fs::read_link(root.join("archix/current")).ok()?;
    let next = crate::deploy::store::parse_current(&target)?;
    if next == booted {
        return None;
    }
    let deploy = root.join("archix/deploy");
    let (changes, security) = changes(
        &deploy.join(booted.to_string()).join("pacman"),
        &deploy.join(next.to_string()).join("pacman"),
    );
    (!changes.is_empty()).then_some(Waiting {
        next,
        changes,
        security,
    })
}

/// One line: how many updates wait, which, and which matter for security.
pub fn line(waiting: &Waiting) -> String {
    const SHOWN: usize = 12;
    let count = waiting.changes.len();
    let mut named = waiting
        .changes
        .iter()
        .take(SHOWN)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if count > SHOWN {
        named.push_str(&format!(" and {} more", count - SHOWN));
    }
    let (noun, verb) = if count == 1 {
        ("update", "is")
    } else {
        ("updates", "are")
    };
    let mut text = format!("{count} {noun} {verb} {MARKER}{}): {named}", waiting.next);
    if !waiting.security.is_empty() {
        text.push_str(&format!(
            "; security-relevant, so reboot soon: {}",
            waiting.security.join(", ")
        ));
    }
    text
}

/// Puts the waiting updates in the notes shown at login, replacing an earlier such line, or takes it out once nothing waits.
pub fn refresh_note(root: &Path) {
    let path = Paths::under(root).notes();
    let old = fs::read_to_string(&path).unwrap_or_default();
    let mut notes: String = old
        .lines()
        .filter(|line| !line.contains(MARKER))
        .map(|line| format!("{line}\n"))
        .collect();
    if let Some(waiting) = waiting(root) {
        notes.push_str(&format!("archix: {}\n", line(&waiting)));
    }
    if notes == old {
        return;
    }
    let _ = match notes.is_empty() {
        true => fs::remove_file(&path).map_err(anyhow::Error::from),
        false => crate::durable::write(&path, notes.as_bytes()),
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db(dir: &Path, packages: &[(&str, &str)]) {
        fs::create_dir_all(dir).unwrap();
        for (name, version) in packages {
            let entry = dir.join(format!("{name}-{version}"));
            fs::create_dir_all(&entry).unwrap();
            fs::write(
                entry.join("desc"),
                format!("%NAME%\n{name}\n\n%VERSION%\n{version}\n\n%ARCH%\nx86_64\n\n"),
            )
            .unwrap();
        }
    }

    #[test]
    fn kernels_and_the_listed_packages_are_security_relevant() {
        for name in [
            "linux",
            "linux-lts",
            "linux-zen",
            "linux-cachyos",
            "glibc",
            "openssl",
            "openssh",
            "sudo",
            "systemd",
        ] {
            assert!(security_relevant(name), "{name}");
        }
        for name in [
            "linux-firmware",
            "linux-headers",
            "linux-lts-headers",
            "linux-api-headers",
            "tree",
            "systemd-ukify",
        ] {
            assert!(!security_relevant(name), "{name}");
        }
    }

    #[test]
    fn a_pending_deployment_names_its_updates_in_a_login_note_until_it_boots() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        db(
            &root.join("archix/deploy/1/pacman"),
            &[("tree", "2.1-1"), ("openssl", "3.5.0-1"), ("bc", "1-1")],
        );
        db(
            &root.join("archix/deploy/2/pacman"),
            &[("tree", "2.2-1"), ("openssl", "3.5.1-1"), ("linux", "6.17-1")],
        );
        std::os::unix::fs::symlink("deploy/2", root.join("archix/current")).unwrap();
        fs::create_dir_all(root.join("run/archix")).unwrap();
        fs::write(root.join("run/archix/deployment"), "1\n").unwrap();
        fs::create_dir_all(Paths::under(root).state).unwrap();
        fs::write(Paths::under(root).notes(), "archix: an earlier note\n").unwrap();

        let waiting = waiting(root).unwrap();
        assert_eq!(
            line(&waiting),
            "4 updates are waiting for a reboot (deployment 2): bc removed, openssl 3.5.0-1 -> 3.5.1-1, tree 2.1-1 -> 2.2-1, linux 6.17-1; security-relevant, so reboot soon: openssl, linux"
        );
        refresh_note(root);
        refresh_note(root);
        let notes = fs::read_to_string(Paths::under(root).notes()).unwrap();
        assert_eq!(notes.lines().count(), 2, "{notes}");
        assert!(
            notes.starts_with("archix: an earlier note\narchix: 4 updates are waiting"),
            "{notes}"
        );

        fs::write(root.join("run/archix/deployment"), "2\n").unwrap();
        assert_eq!(super::waiting(root), None);
        refresh_note(root);
        assert_eq!(
            fs::read_to_string(Paths::under(root).notes()).unwrap(),
            "archix: an earlier note\n"
        );
    }
}
