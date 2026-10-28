//! /usr/local on a system that boots through deployments: one tree on the root filesystem, /var/usrlocal, shared by every deployment and never rolled back, which the initramfs binds read-write over the booted deployment's empty /usr/local.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

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

/// Where quarantined boot-time files go, relative to the root, each run in a directory named by its time.
pub const QUARANTINE: &str = "var/usrlocal.quarantine";

/// How to bring up systemd-boot's menu, whose timeout may be 0, to start the fallback UKI or an older deployment.
pub const MENU_HINT: &str = "to start the fallback UKI or an older deployment, bring up systemd-boot's menu by holding Space (or pressing it repeatedly) as the machine starts, or from a running system with `systemctl reboot --boot-loader-menu=60`; `sudo bootctl set-timeout 5` shows it at every boot";

/// The state of the hold that keeps /usr/local out of every boot once a boot left it out while it held boot-time files.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Hold {
    /// Out until its boot-time files are gone or `archix usr-local release`.
    Held,
    /// Out until `archix usr-local release`, as the last boot that bound it again didn't finish.
    Failed,
    /// `archix usr-local release` asked the next boot to bind it once.
    Trial,
    /// A boot bound it under the hold, which archix-boot-complete.service lifts once that boot reaches multi-user.target.
    Trying,
}

impl Hold {
    fn word(self) -> &'static str {
        match self {
            Hold::Held => "held",
            Hold::Failed => "failed",
            Hold::Trial => "trial",
            Hold::Trying => "trying",
        }
    }
}

/// The hold's file, under the state directory `state`, on the root so the initramfs reads it.
pub fn hold_path(state: &Path) -> PathBuf {
    state.join("boot").join("usr-local-hold")
}

/// The hold and the reason it was first set, if there is one.
pub fn read_hold(state: &Path) -> Option<(Hold, String)> {
    let text = fs::read_to_string(hold_path(state)).ok()?;
    let (word, why) = text.trim().split_once(' ').unwrap_or((text.trim(), ""));
    let hold = match word {
        "held" => Hold::Held,
        "failed" => Hold::Failed,
        "trial" => Hold::Trial,
        "trying" => Hold::Trying,
        _ => Hold::Held,
    };
    Some((hold, why.to_string()))
}

/// Writes the hold, durably, keeping the reason it was first set.
pub fn write_hold(state: &Path, hold: Hold, why: &str) -> Result<()> {
    let path = hold_path(state);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    crate::durable::write(&path, format!("{} {why}\n", hold.word()).as_bytes())
}

/// In the initramfs, for a boot that would bind /usr/local: why the hold keeps it out, or None to bind it, after recording that this boot tries it again when the hold lets it.
pub fn held(root: &Path, write: &dyn Fn(Hold, &str) -> Result<()>) -> Option<String> {
    let (hold, why) = read_hold(&crate::paths::Paths::under(root).state)?;
    let release = match hold {
        Hold::Trying => {
            return Some(
                "the last boot that bound it again didn't reach multi-user.target, so it stays out until `sudo archix usr-local release`"
                    .to_string(),
            );
        }
        Hold::Failed => {
            return Some(format!(
                "it is held out since {why}, and the last boot that bound it again didn't reach multi-user.target; `sudo archix usr-local release` lets the next boot try it"
            ));
        }
        Hold::Held if !boot_files(root).is_empty() => {
            return Some(format!("it is held out since {why}"));
        }
        Hold::Held => "its boot-time files are gone",
        Hold::Trial => "`archix usr-local release` asked for it",
    };
    match write(Hold::Trying, &why) {
        Ok(()) => {
            println!(
                "archix: binding /usr/local again, held out since {why}, as {release}; if this boot doesn't reach multi-user.target, the next one leaves it out"
            );
            None
        }
        Err(e) => Some(format!(
            "it is held out since {why}, and recording this boot's try failed ({e:#})"
        )),
    }
}

/// At boot, from archix-boot.service, before the booted deployment is made current: holds /usr/local out of every later boot when this one left it out (the fallback UKI, an older deployment) while it held boot-time files, so a file there that broke the current deployment's boot can't do it again once its fallback is current; marks a try that didn't finish; then updates the login note.
pub fn at_boot(paths: &Paths) -> Result<()> {
    let record = fs::read_to_string(paths.root.join(RECORD.trim_start_matches('/'))).unwrap_or_default();
    let record = record.trim();
    let hold = read_hold(&paths.state);
    if let Some(why) = record.strip_prefix("skipped ")
        && hold.is_none()
    {
        let files = boot_files(&paths.root);
        if !files.is_empty() {
            let why = format!("a boot left it out ({why}) while it held {}", shown(&files));
            write_hold(&paths.state, Hold::Held, &why)?;
            println!("archix: /usr/local stays out of every boot from now on: {why}");
        }
    }
    if record.starts_with("held ")
        && let Some((Hold::Trying, why)) = &hold
    {
        write_hold(&paths.state, Hold::Failed, why)?;
    }
    sync_note(paths)
}

/// From archix-boot-complete.service at multi-user.target: a boot that bound /usr/local under the hold made it, so the hold goes.
pub fn confirm(paths: &Paths) -> Result<()> {
    let record = fs::read_to_string(paths.root.join(RECORD.trim_start_matches('/'))).unwrap_or_default();
    if record.trim() != "mounted" || !matches!(read_hold(&paths.state), Some((Hold::Trying, _))) {
        return Ok(());
    }
    fs::remove_file(hold_path(&paths.state)).context("removing the /usr/local hold")?;
    println!("archix: this boot bound /usr/local again and reached multi-user.target, so it is no longer held out");
    Ok(())
}

/// `archix usr-local release`: lets the next boot bind /usr/local again, once; if that boot doesn't reach multi-user.target, the one after leaves it out.
pub fn release(paths: &Paths) -> Result<String> {
    let Some((hold, why)) = read_hold(&paths.state) else {
        return Ok("/usr/local isn't held out; every boot of the current deployment binds it".to_string());
    };
    if hold == Hold::Trying {
        return Ok(
            "this boot bound /usr/local under the hold already; it is lifted once this boot reaches multi-user.target"
                .to_string(),
        );
    }
    write_hold(&paths.state, Hold::Trial, &why)?;
    Ok(format!(
        "the next boot binds /usr/local again (held out since {why}); if it doesn't reach multi-user.target, the boot after leaves /usr/local out again"
    ))
}

/// `archix usr-local quarantine`: moves the files under /var/usrlocal that systemd, udev or the module loader read at boot into /var/usrlocal.quarantine/<time>, keeping their paths, and lets the next boot bind /usr/local again.
pub fn quarantine(paths: &Paths) -> Result<String> {
    let root = &paths.root;
    if !crate::system::deploy_boot::uses_deployments(root) {
        bail!(
            "this machine has no deployments, so /usr/local is the root's own and every boot uses it; move the files yourself"
        );
    }
    let files = boot_files(root);
    let mut lines = Vec::new();
    if !files.is_empty() {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_secs());
        let into = root.join(QUARANTINE).join(secs.to_string());
        for file in &files {
            let rel = file.trim_start_matches("/usr/local/");
            let (from, to) = (dir(root).join(rel), into.join(rel));
            fs::create_dir_all(to.parent().expect("a file under a directory"))
                .with_context(|| format!("creating {}", to.display()))?;
            fs::rename(&from, &to).with_context(|| format!("moving {} to {}", from.display(), to.display()))?;
        }
        crate::durable::sync_filesystems_checked(&[dir(root)])?;
        lines.push(format!(
            "moved {} into {}, under the same paths; move one back to /usr/local to use it again",
            shown(&files),
            into.display()
        ));
    } else {
        lines.push("/var/usrlocal holds no files that systemd, udev or the module loader read at boot".to_string());
    }
    if let Some((_, why)) = read_hold(&paths.state) {
        write_hold(&paths.state, Hold::Trial, &why)?;
        lines.push("the next boot binds /usr/local again".to_string());
    }
    Ok(lines.join("\n"))
}

/// `archix status`'s line about the hold, if there is one.
pub fn hold_status(paths: &Paths) -> Option<String> {
    let (hold, why) = read_hold(&paths.state)?;
    Some(match hold {
        Hold::Trial => format!("/usr/local: held out since {why}; the next boot binds it again, once"),
        Hold::Trying => format!(
            "/usr/local: held out since {why}; this boot bound it again, and the hold goes once it reaches multi-user.target"
        ),
        Hold::Held | Hold::Failed => format!(
            "/usr/local: every boot leaves it out, held since {why}{}; `sudo archix usr-local quarantine` moves its boot-time files to /{QUARANTINE}, or once you've fixed them `sudo archix usr-local release` lets the next boot bind it again",
            if hold == Hold::Failed {
                ", and the last boot that bound it again didn't finish"
            } else {
                ""
            }
        ),
    })
}

/// Up to ten of `files` and how many more.
fn shown(files: &[String]) -> String {
    let listed: Vec<&str> = files.iter().take(10).map(String::as_str).collect();
    match files.len() - listed.len() {
        0 => listed.join(", "),
        n => format!("{} and {n} more", listed.join(", ")),
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

/// The login note a boot that left /usr/local out says why in, with how to clear the hold, if there is one, of the boot-time `files`.
fn skipped_note(why: &str, hold: Option<Hold>, files: &[String]) -> String {
    let mut note = format!(
        "archix: this boot left /usr/local unmounted, as {why}; it shows the deployment's own empty, read-only /usr/local. The shared one, which every deployment uses and no rollback undoes, is /var/usrlocal"
    );
    match (hold, files.is_empty()) {
        (Some(Hold::Held | Hold::Failed | Hold::Trying), false) => note.push_str(&format!(
            ". It holds files that systemd, udev or the module loader read at boot ({}), so every boot leaves it out until you clear them: `sudo archix usr-local quarantine` moves them to /{QUARANTINE}, or fix them and `sudo archix usr-local release`; the next boot then binds it, and if that boot doesn't reach multi-user.target, the one after leaves it out again",
            shown(files)
        )),
        (Some(Hold::Held | Hold::Failed | Hold::Trying), true) => note.push_str(
            ". Every boot leaves it out until `sudo archix usr-local release`; the next boot then binds it, and if that boot doesn't reach multi-user.target, the one after leaves it out again",
        ),
        _ => note.push_str(
            ": if a unit, udev rule or module setting there broke the boot, fix or remove it there, then reboot into the current deployment",
        ),
    }
    note.push_str(&format!("; {MENU_HINT}"));
    note
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
    let record = record.trim();
    let hold = read_hold(&paths.state).map(|(hold, _)| hold);
    let new = record
        .strip_prefix("skipped ")
        .or_else(|| record.strip_prefix("held "))
        .or_else(|| record.strip_prefix("unbound "))
        .map(|why| skipped_note(why, hold, &boot_files(&paths.root)));
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
        "warning: /usr/local holds files that systemd, udev or the module loader read at boot: {}{more}. They apply to every deployment and no rollback undoes them; a boot of an older deployment than current and the fallback UKI leave /usr/local out, to get past a bad one, and then every boot leaves it out until `sudo archix usr-local quarantine` or `release`; {MENU_HINT}",
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

    fn boot_record(paths: &Paths, record: &str) {
        let path = paths.root.join(RECORD.trim_start_matches('/'));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, format!("{record}\n")).unwrap();
    }

    fn deploy_mount(paths: &Paths) -> Option<String> {
        held(&paths.root, &|hold, why| write_hold(&paths.state, hold, why))
    }

    fn hold_of(paths: &Paths) -> Option<Hold> {
        read_hold(&paths.state).map(|(hold, _)| hold)
    }

    #[test]
    fn a_boot_that_left_boot_time_files_out_holds_them_out_until_they_are_gone_or_released() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        fs::create_dir_all(paths.root.join("archix/deploy")).unwrap();
        let unit = dir_of(&paths).join("lib/systemd/system/broken.service");
        fs::create_dir_all(unit.parent().unwrap()).unwrap();
        fs::write(&unit, "").unwrap();
        fs::create_dir_all(dir_of(&paths).join("bin")).unwrap();
        fs::write(dir_of(&paths).join("bin/tool"), "").unwrap();

        boot_record(&paths, "mounted");
        at_boot(&paths).unwrap();
        assert_eq!(hold_of(&paths), None, "a boot that bound it holds nothing");
        assert_eq!(deploy_mount(&paths), None);

        boot_record(
            &paths,
            "skipped systemd-boot started deployment 2, older than the current 3",
        );
        at_boot(&paths).unwrap();
        assert_eq!(hold_of(&paths), Some(Hold::Held));
        let notes = fs::read_to_string(paths.notes()).unwrap();
        assert!(
            notes.contains("/usr/local/lib/systemd/system/broken.service")
                && notes.contains("sudo archix usr-local quarantine")
                && notes.contains("--boot-loader-menu"),
            "{notes}"
        );
        for _ in 0..2 {
            let why = deploy_mount(&paths).expect("every later boot leaves it out");
            assert!(why.contains("older than the current 3"), "{why}");
            assert_eq!(hold_of(&paths), Some(Hold::Held));
            boot_record(&paths, &format!("held {why}"));
            at_boot(&paths).unwrap();
            assert_eq!(hold_of(&paths), Some(Hold::Held));
        }
        assert!(fs::read_to_string(paths.notes()).unwrap().contains("broken.service"));
        assert!(hold_status(&paths).unwrap().contains("every boot leaves it out"));

        assert!(
            release(&paths)
                .unwrap()
                .contains("the next boot binds /usr/local again")
        );
        assert_eq!(deploy_mount(&paths), None, "the released boot binds it");
        assert_eq!(hold_of(&paths), Some(Hold::Trying));
        let why = deploy_mount(&paths).expect("a try that didn't reach multi-user.target");
        assert!(why.contains("didn't reach multi-user.target"), "{why}");
        boot_record(&paths, &format!("held {why}"));
        at_boot(&paths).unwrap();
        assert_eq!(hold_of(&paths), Some(Hold::Failed));
        fs::remove_file(&unit).unwrap();
        assert!(
            deploy_mount(&paths).is_some(),
            "after a failed try only a release lets it in, though its boot-time files are gone"
        );

        fs::write(&unit, "").unwrap();
        let said = quarantine(&paths).unwrap();
        assert!(
            said.contains("broken.service") && said.contains("next boot binds"),
            "{said}"
        );
        assert!(!unit.exists() && dir_of(&paths).join("bin/tool").exists());
        let moved: Vec<PathBuf> = fs::read_dir(paths.root.join(QUARANTINE))
            .unwrap()
            .flatten()
            .map(|run| run.path().join("lib/systemd/system/broken.service"))
            .collect();
        assert!(moved.len() == 1 && moved[0].is_file(), "{moved:?}");
        assert_eq!(deploy_mount(&paths), None);
        boot_record(&paths, "mounted");
        at_boot(&paths).unwrap();
        assert_eq!(hold_of(&paths), Some(Hold::Trying), "lifted only at multi-user.target");
        assert!(
            !fs::read_to_string(paths.notes())
                .unwrap_or_default()
                .contains("/usr/local")
        );
        confirm(&paths).unwrap();
        assert_eq!(hold_of(&paths), None);
        assert_eq!(hold_status(&paths), None);
    }

    #[test]
    fn a_held_usr_local_comes_back_by_itself_once_its_boot_time_files_are_gone() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        fs::create_dir_all(paths.root.join("archix/deploy")).unwrap();
        write_hold(&paths.state, Hold::Held, "a boot left it out").unwrap();
        let rule = dir_of(&paths).join("lib/udev/rules.d/1.rules");
        fs::create_dir_all(rule.parent().unwrap()).unwrap();
        fs::write(&rule, "").unwrap();
        assert!(deploy_mount(&paths).is_some());
        fs::remove_file(&rule).unwrap();
        assert_eq!(deploy_mount(&paths), None);
        assert_eq!(hold_of(&paths), Some(Hold::Trying));
        boot_record(
            &paths,
            "skipped systemd-boot started the fallback UKI archix-fallback-3.efi",
        );
        confirm(&paths).unwrap();
        assert_eq!(
            hold_of(&paths),
            Some(Hold::Trying),
            "only a boot that bound it lifts the hold"
        );
        assert!(
            release(&paths)
                .unwrap()
                .contains("bound /usr/local under the hold already"),
            "a release mid-try changes nothing"
        );
        let unwritable = held(&paths.root, &|_, _| anyhow::bail!("read-only"));
        assert!(unwritable.is_some(), "a try that can't be recorded isn't made");
    }

    #[test]
    fn a_boot_that_left_usr_local_out_without_boot_time_files_holds_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        fs::create_dir_all(paths.root.join("archix/deploy")).unwrap();
        fs::create_dir_all(dir_of(&paths).join("bin")).unwrap();
        fs::write(dir_of(&paths).join("bin/tool"), "").unwrap();
        boot_record(
            &paths,
            "skipped systemd-boot started the fallback UKI archix-fallback-3.efi",
        );
        at_boot(&paths).unwrap();
        assert_eq!(hold_of(&paths), None);
        let notes = fs::read_to_string(paths.notes()).unwrap();
        assert!(
            notes.contains("left /usr/local unmounted") && !notes.contains("quarantine"),
            "{notes}"
        );
        assert!(release(&paths).unwrap().contains("isn't held out"));
    }

    fn dir_of(paths: &Paths) -> PathBuf {
        dir(&paths.root)
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
