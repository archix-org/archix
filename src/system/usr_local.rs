//! /usr/local on a system that boots through deployments: one tree on the root filesystem, /var/usrlocal, shared by every deployment and never rolled back, which the initramfs binds read-write over the booted deployment's empty /usr/local.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::paths::Paths;

/// The shared tree, relative to the root.
pub const DIR: &str = "var/usrlocal";

/// Where deploy-mount records whether this boot bound /usr/local: `mounted`, or `skipped <why>`.
pub const RECORD: &str = "/run/archix/usr-local";

/// Directories under /usr/local that systemd, udev, kmod and their kin read at boot, so a bad file there breaks every deployment.
pub const BOOT_DIRS: &[&str] = &[
    "lib/systemd",
    "lib/udev",
    "lib/modprobe.d",
    "lib/modules-load.d",
    "lib/sysctl.d",
    "lib/tmpfiles.d",
    "lib/sysusers.d",
    "lib/environment.d",
    "lib/binfmt.d",
];

/// The package whose /usr/local skeleton (directories and the share/man link) every deployment may carry.
const SKELETON_PACKAGE: &str = "filesystem";

/// Whether the managed path `path` is under /usr/local.
pub fn holds(path: &str) -> bool {
    path.starts_with("/usr/local/")
}

/// The shared tree of the root at `root`.
pub fn dir(root: &Path) -> PathBuf {
    root.join(DIR)
}

/// Where the managed path `path` is on disk under `root`: one under /usr/local is in /var/usrlocal once `root` has deployments, as the running /usr/local may still be a deployment's until the next boot.
pub fn on_disk(root: &Path, path: &str) -> PathBuf {
    let rel = path.trim_start_matches('/');
    match rel.strip_prefix("usr/local/") {
        Some(rest) if crate::system::deploy_boot::uses_deployments(root) => dir(root).join(rest),
        _ => root.join(rel),
    }
}

/// Why a boot of deployment `generation`, started from `selected`, leaves /usr/local out: the fallback UKI, or a deployment older than current, as systemd-boot's fallback after a newer one's failed tries boots; None binds it.
pub fn skip_reason(selected: Option<&str>, generation: u32, current: Option<u32>) -> Option<String> {
    if let Some(entry) = selected.filter(|entry| crate::system::boot::parse_fallback(entry).is_some()) {
        return Some(format!("systemd-boot started the fallback UKI {entry}"));
    }
    match current {
        Some(current) if selected.is_some() && generation < current => Some(format!(
            "systemd-boot started deployment {generation}, older than the current {current}: its fallback after {current}'s failed boot tries, or an older entry picked in the boot menu"
        )),
        _ => None,
    }
}

/// The login note a boot that left /usr/local out says why in.
fn skipped_note(why: &str) -> String {
    format!(
        "archix: this boot left /usr/local unmounted, as {why}; it shows the deployment's own empty, read-only /usr/local. The shared one, which every deployment uses and no rollback undoes, is /var/usrlocal: if a unit, udev rule or module setting there broke the boot, fix or remove it there, then reboot into the current deployment"
    )
}

fn note_record(paths: &Paths) -> PathBuf {
    paths.state.join("boot").join("usr-local-note")
}

/// At boot, from archix-boot.service: says in the login notes when this boot left /usr/local out, and takes that note away once a boot bound it again.
pub fn sync_note(paths: &Paths) -> Result<()> {
    let record = fs::read_to_string(paths.root.join(RECORD.trim_start_matches('/'))).unwrap_or_default();
    let old = fs::read_to_string(note_record(paths))
        .ok()
        .filter(|note| !note.is_empty());
    let new = record.trim().strip_prefix("skipped ").map(skipped_note);
    let before = fs::read_to_string(paths.notes()).unwrap_or_default();
    let mut notes = before.clone();
    if let Some(old) = &old {
        notes = notes.replace(&format!("{old}\n"), "");
    }
    if new.is_none() && old.is_none() {
        return Ok(());
    }
    match &new {
        Some(note) => {
            println!("{note}");
            notes.push_str(note);
            notes.push('\n');
            crate::durable::write(&note_record(paths), note.as_bytes())?;
        }
        None => {
            let _ = fs::remove_file(note_record(paths));
        }
    }
    if notes == before {
        return Ok(());
    }
    if notes.trim().is_empty() {
        let _ = fs::remove_file(paths.notes());
        return Ok(());
    }
    crate::durable::write(&paths.notes(), notes.as_bytes())
}

/// Non-directory entries under `dir`, as paths relative to it, sorted.
fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        let Ok(entries) = fs::read_dir(dir.join(&rel)) else {
            continue;
        };
        for entry in entries.flatten() {
            let rel = rel.join(entry.file_name());
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => stack.push(rel),
                _ => found.push(rel),
            }
        }
    }
    found.sort();
    found
}

/// Files in the shared /usr/local of the root at `root` that systemd, udev or the module loader read at boot.
pub fn boot_files(root: &Path) -> Vec<String> {
    let base = match crate::system::deploy_boot::uses_deployments(root) {
        true => dir(root),
        false => root.join("usr/local"),
    };
    BOOT_DIRS
        .iter()
        .flat_map(|sub| {
            files_under(&base.join(sub))
                .into_iter()
                .map(move |rel| format!("/usr/local/{sub}/{}", rel.display()))
        })
        .collect()
}

/// `archix status` and `archix verify`'s warning about `boot_files`, if there are any.
pub fn boot_files_warning(root: &Path) -> Option<String> {
    let files = boot_files(root);
    if files.is_empty() {
        return None;
    }
    let shown: Vec<&str> = files.iter().take(10).map(String::as_str).collect();
    let more = match files.len() - shown.len() {
        0 => String::new(),
        n => format!(" and {n} more"),
    };
    Some(format!(
        "warning: /usr/local holds files that systemd, udev or the module loader read at boot: {}{more}. They apply to every deployment and no rollback undoes them; a boot of an older deployment than current and the fallback UKI leave /usr/local out, to get past a bad one",
        shown.join(", ")
    ))
}

/// What a package other than filesystem would install under /usr/local, of its `files` (relative to the root, as pacman lists them).
pub fn packaged<'a>(package: &str, files: &'a [String]) -> Vec<&'a str> {
    if package == SKELETON_PACKAGE {
        return Vec::new();
    }
    files
        .iter()
        .map(String::as_str)
        .filter(|file| file.starts_with("usr/local/") && !file.ends_with('/'))
        .collect()
}

/// The /usr/local skeleton the filesystem package in pacman database `local_db` owns, as paths relative to the root without a trailing slash: usr/local itself, its directories and the share/man link, which every deployment keeps so `pacman -Qkk` finds them on a boot that leaves the shared /usr/local out.
pub fn skeleton(local_db: &Path) -> Vec<String> {
    fs::read_dir(local_db)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| {
            crate::pacman::parse_desc(&fs::read_to_string(entry.path().join("desc")).unwrap_or_default())
                .remove("NAME")
                .is_some_and(|names| names.first().map(String::as_str) == Some(SKELETON_PACKAGE))
        })
        .flat_map(|entry| {
            crate::pacman::parse_desc(&fs::read_to_string(entry.path().join("files")).unwrap_or_default())
                .remove("FILES")
                .unwrap_or_default()
        })
        .filter(|path| path.starts_with("usr/local/"))
        .map(|path| path.trim_end_matches('/').to_string())
        .collect()
}

/// Removes everything under `local`, a deployment's own /usr/local, but the `keep` skeleton (paths relative to the root, as `skeleton` gives them).
pub fn strip_to_skeleton(local: &Path, keep: &[String]) -> Result<()> {
    let mut stack = vec![(local.to_path_buf(), "usr/local".to_string())];
    while let Some((dir, rel)) = stack.pop() {
        for entry in fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
            let entry = entry.with_context(|| format!("reading {}", dir.display()))?;
            let rel = format!("{rel}/{}", entry.file_name().to_string_lossy());
            let path = entry.path();
            if !keep.contains(&rel) {
                crate::deploy::stage::remove_any(&path)?;
            } else if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                stack.push((path, rel));
            }
        }
    }
    Ok(())
}

/// Files a build left in the staged deployment `dir`'s own /usr/local, or in its var-upper's /var/usrlocal, but the filesystem package's skeleton: the booted /usr/local hides the first and nothing would apply the second.
pub fn staged_files(dir: &Path) -> Vec<String> {
    let skeleton = skeleton(&dir.join("pacman"));
    let mut found: Vec<String> = files_under(&dir.join("usr/local"))
        .into_iter()
        .map(|rel| format!("usr/local/{}", rel.display()))
        .filter(|rel| !skeleton.contains(rel))
        .map(|rel| format!("/{rel}"))
        .collect();
    found.extend(
        files_under(&dir.join("var-upper/usrlocal"))
            .into_iter()
            .map(|rel| format!("/var/usrlocal/{}", rel.display())),
    );
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fallback_uki_and_older_deployments_than_current_leave_usr_local_out() {
        assert_eq!(skip_reason(Some("archix-5.conf"), 5, Some(5)), None);
        assert_eq!(
            skip_reason(Some("archix-6+2-1.conf"), 6, Some(5)),
            None,
            "a newer deployment's try"
        );
        assert_eq!(skip_reason(None, 4, Some(5)), None, "no entry was selected");
        let older = skip_reason(Some("archix-4.conf"), 4, Some(5)).unwrap();
        assert!(older.contains("older than the current 5"), "{older}");
        let fallback = skip_reason(Some("archix-fallback-5.efi"), 5, Some(5)).unwrap();
        assert!(fallback.contains("fallback UKI archix-fallback-5.efi"), "{fallback}");
    }

    #[test]
    fn a_skipped_boot_leaves_a_login_note_that_the_next_bound_boot_takes_away() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let record = dir.path().join(RECORD.trim_start_matches('/'));
        fs::create_dir_all(record.parent().unwrap()).unwrap();
        fs::create_dir_all(paths.notes().parent().unwrap()).unwrap();
        fs::write(paths.notes(), "archix: an earlier note\n").unwrap();
        fs::write(
            &record,
            "skipped systemd-boot started the fallback UKI archix-fallback-5.efi\n",
        )
        .unwrap();
        sync_note(&paths).unwrap();
        sync_note(&paths).unwrap();
        let notes = fs::read_to_string(paths.notes()).unwrap();
        assert_eq!(notes.matches("left /usr/local unmounted").count(), 1, "{notes}");
        assert!(
            notes.starts_with("archix: an earlier note\n") && notes.contains("/var/usrlocal"),
            "{notes}"
        );
        fs::write(&record, "mounted\n").unwrap();
        sync_note(&paths).unwrap();
        assert_eq!(fs::read_to_string(paths.notes()).unwrap(), "archix: an earlier note\n");
        assert!(!note_record(&paths).exists());
    }

    #[test]
    fn managed_usr_local_paths_live_in_var_usrlocal_once_the_root_has_deployments() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert_eq!(on_disk(root, "/usr/local/bin/x"), root.join("usr/local/bin/x"));
        fs::create_dir_all(root.join("archix/deploy")).unwrap();
        assert_eq!(on_disk(root, "/usr/local/bin/x"), root.join("var/usrlocal/bin/x"));
        assert_eq!(on_disk(root, "/srv/x"), root.join("srv/x"));
        assert_eq!(on_disk(root, "/usr/localx/y"), root.join("usr/localx/y"));
    }

    #[test]
    fn boot_files_are_listed_from_the_shared_tree_and_others_are_not() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("archix/deploy")).unwrap();
        assert_eq!(boot_files_warning(root), None);
        for file in [
            "lib/systemd/system/x.service",
            "lib/udev/rules.d/1.rules",
            "bin/tool",
            "lib/python3/x.py",
        ] {
            let path = dir(root).join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "").unwrap();
        }
        assert_eq!(
            boot_files(root),
            [
                "/usr/local/lib/systemd/system/x.service",
                "/usr/local/lib/udev/rules.d/1.rules"
            ]
        );
        assert!(boot_files_warning(root).unwrap().contains("no rollback undoes them"));
    }

    #[test]
    fn only_filesystem_may_ship_into_usr_local_and_a_staged_tree_keeps_only_its_skeleton() {
        let files = [
            "usr/local/".to_string(),
            "usr/local/bin/x".to_string(),
            "usr/bin/y".to_string(),
        ];
        assert_eq!(packaged("tool", &files), ["usr/local/bin/x"]);
        assert!(packaged("filesystem", &files).is_empty());
        let dir = tempfile::tempdir().unwrap();
        let staged = dir.path();
        let db = staged.join("pacman/filesystem-2025.10.12-1");
        fs::create_dir_all(&db).unwrap();
        fs::write(db.join("desc"), "%NAME%\nfilesystem\n\n").unwrap();
        fs::write(
            db.join("files"),
            "%FILES%\nusr/local/\nusr/local/share/\nusr/local/share/man\n\n",
        )
        .unwrap();
        fs::create_dir_all(staged.join("usr/local/share")).unwrap();
        fs::create_dir_all(staged.join("usr/local/bin")).unwrap();
        std::os::unix::fs::symlink("../man", staged.join("usr/local/share/man")).unwrap();
        assert!(staged_files(staged).is_empty());
        fs::write(staged.join("usr/local/bin/x"), "").unwrap();
        fs::create_dir_all(staged.join("var-upper/usrlocal/lib")).unwrap();
        fs::write(staged.join("var-upper/usrlocal/lib/y"), "").unwrap();
        assert_eq!(staged_files(staged), ["/usr/local/bin/x", "/var/usrlocal/lib/y"]);
        fs::create_dir_all(staged.join("usr/local/share/mine")).unwrap();
        let skeleton = skeleton(&staged.join("pacman"));
        assert_eq!(skeleton, ["usr/local", "usr/local/share", "usr/local/share/man"]);
        strip_to_skeleton(&staged.join("usr/local"), &skeleton).unwrap();
        assert_eq!(files_under(&staged.join("usr/local")), [PathBuf::from("share/man")]);
        assert!(!staged.join("usr/local/bin").exists() && !staged.join("usr/local/share/mine").exists());
    }
}
