use std::collections::BTreeSet;
use std::ffi::CString;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::{ManagedFile, SystemState, User, blobs};
use crate::fetch::sha256;
use crate::generations::now;
use crate::paths::Paths;

/// The system part of a transaction: what the current generation applied and what the target applies.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SystemChange {
    pub from: SystemState,
    pub to: SystemState,
}

impl SystemChange {
    pub fn reversed(&self) -> SystemChange {
        SystemChange {
            from: self.to.clone(),
            to: self.from.clone(),
        }
    }

    /// Paths whose managed entry differs between the two states, including ones only in one of them.
    fn changed_paths(&self) -> BTreeSet<&str> {
        let mut changed = BTreeSet::new();
        for (path, file) in &self.to.files {
            if self.from.files.get(path) != Some(file) {
                changed.insert(path.as_str());
            }
        }
        for path in self.from.files.keys() {
            if !self.to.files.contains_key(path) {
                changed.insert(path.as_str());
            }
        }
        changed
    }
}

#[derive(Debug, PartialEq)]
enum OnDisk {
    Missing,
    Link(String),
    File {
        sha256: String,
        mode: u32,
        uid: u32,
        gid: u32,
    },
    Other,
}

fn on_disk(path: &Path) -> Result<OnDisk> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(OnDisk::Missing),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    if meta.file_type().is_symlink() {
        return Ok(OnDisk::Link(fs::read_link(path)?.to_string_lossy().into_owned()));
    }
    if !meta.is_file() {
        return Ok(OnDisk::Other);
    }
    Ok(OnDisk::File {
        sha256: sha256(path)?,
        mode: meta.mode() & 0o7777,
        uid: meta.uid(),
        gid: meta.gid(),
    })
}

fn uid(name: &str) -> Option<u32> {
    let name = CString::new(name).ok()?;
    let entry = unsafe { libc::getpwnam(name.as_ptr()) };
    (!entry.is_null()).then(|| unsafe { (*entry).pw_uid })
}

fn gid(name: &str) -> Option<u32> {
    let name = CString::new(name).ok()?;
    let entry = unsafe { libc::getgrnam(name.as_ptr()) };
    (!entry.is_null()).then(|| unsafe { (*entry).gr_gid })
}

fn matches(disk: &OnDisk, want: &ManagedFile) -> bool {
    match (disk, &want.link) {
        (OnDisk::Link(target), Some(link)) => target == link,
        (
            OnDisk::File {
                sha256,
                mode,
                uid: u,
                gid: g,
            },
            None,
        ) => {
            *sha256 == want.sha256 && *mode == want.mode && Some(*u) == uid(&want.owner) && Some(*g) == gid(&want.group)
        }
        _ => false,
    }
}

fn under(root: &Path, path: &str) -> PathBuf {
    root.join(path.trim_start_matches('/'))
}

/// One line per change applying `change` would make, flagging managed files someone edited by hand.
pub fn describe(change: &SystemChange) -> Result<Vec<String>> {
    let mut lines = Vec::new();
    for (path, want) in &change.to.files {
        let disk = on_disk(Path::new(path))?;
        if matches(&disk, want) {
            continue;
        }
        let edited = change
            .from
            .files
            .get(path)
            .is_some_and(|prev| disk != OnDisk::Missing && !matches(&disk, prev));
        let verb = match (&disk, change.from.files.contains_key(path)) {
            (OnDisk::Missing, _) => "create",
            (_, false) => "replace",
            _ => "update",
        };
        let note = if edited {
            " (edited outside archix; your version gets saved)"
        } else {
            ""
        };
        lines.push(format!("{verb:<10} {path}{note}"));
    }
    for path in change
        .from
        .files
        .keys()
        .filter(|path| !change.to.files.contains_key(*path))
    {
        lines.push(format!("unmanage   {path}"));
    }
    for (name, user) in &change.to.users {
        if uid(name).is_none() {
            lines.push(format!("add user   {name}"));
        } else if change.from.users.get(name) != Some(user) {
            lines.push(format!("modify     user {name}"));
        }
    }
    for name in change
        .from
        .users
        .keys()
        .filter(|name| !change.to.users.contains_key(*name))
    {
        lines.push(format!("remove     user {name} (home kept)"));
    }
    for (name, group) in &change.to.groups {
        if gid(name).is_none() || change.from.groups.get(name) != Some(group) {
            lines.push(format!("group      {name}"));
        }
    }
    for name in change
        .from
        .groups
        .keys()
        .filter(|name| !change.to.groups.contains_key(*name))
    {
        lines.push(format!("remove     group {name}"));
    }
    for (unit, enabled) in &change.to.services {
        if is_enabled(unit, false) != *enabled {
            lines.push(format!("{:<10} {unit}", if *enabled { "enable" } else { "disable" }));
        }
    }
    for (unit, enabled) in &change.to.user_services {
        if is_enabled(unit, true) != *enabled {
            lines.push(format!(
                "{:<10} {unit} (user)",
                if *enabled { "enable" } else { "disable" }
            ));
        }
    }
    for unit in change
        .from
        .services
        .keys()
        .filter(|unit| !change.to.services.contains_key(*unit))
    {
        if change.from.services[unit] {
            lines.push(format!("disable    {unit}"));
        }
    }
    Ok(lines)
}

fn is_enabled(unit: &str, user: bool) -> bool {
    let mut cmd = Command::new("systemctl");
    if user {
        cmd.arg("--global");
    }
    cmd.args(["is-enabled", unit])
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim() == "enabled")
}

fn run(cmd: &mut Command, what: &str) -> Result<()> {
    let output = cmd.output().with_context(|| format!("running {what}"))?;
    if !output.status.success() {
        bail!("{what}: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(())
}

/// Creates, updates and removes the users and groups archix manages; never touches passwords.
pub fn users(change: &SystemChange) -> Result<()> {
    for (name, group) in &change.to.groups {
        match gid(name) {
            None => {
                let mut cmd = Command::new("groupadd");
                if let Some(id) = group.gid {
                    cmd.args(["-g", &id.to_string()]);
                }
                if group.system {
                    cmd.arg("-r");
                }
                run(cmd.arg(name), &format!("groupadd {name}"))?;
            }
            Some(current) if group.gid.is_some_and(|id| id != current) => {
                run(
                    Command::new("groupmod").args(["-g", &group.gid.unwrap().to_string(), name]),
                    "groupmod",
                )?;
            }
            Some(_) => {}
        }
    }
    for (name, user) in &change.to.users {
        if uid(name).is_none() {
            run(&mut useradd(name, user), &format!("useradd {name}"))?;
            eprintln!("created user {name}; set a password with `passwd {name}`");
        } else if change.from.users.get(name) != Some(user) {
            run(&mut usermod(name, user), &format!("usermod {name}"))?;
        }
    }
    for name in change
        .from
        .users
        .keys()
        .filter(|name| !change.to.users.contains_key(*name))
    {
        if uid(name).is_some() {
            run(Command::new("userdel").arg(name), &format!("userdel {name}"))?;
        }
    }
    for name in change
        .from
        .groups
        .keys()
        .filter(|name| !change.to.groups.contains_key(*name))
    {
        if gid(name).is_some() {
            run(Command::new("groupdel").arg(name), &format!("groupdel {name}"))?;
        }
    }
    Ok(())
}

fn useradd(name: &str, user: &User) -> Command {
    let mut cmd = Command::new("useradd");
    if user.system {
        cmd.arg("-r");
    } else {
        cmd.arg("-m");
    }
    if let Some(id) = user.uid {
        cmd.args(["-u", &id.to_string()]);
    }
    match &user.group {
        Some(group) => cmd.args(["-g", group]),
        None => cmd.arg("-U"),
    };
    if !user.groups.is_empty() {
        cmd.args(["-G", &user.groups.join(",")]);
    }
    if let Some(home) = &user.home {
        cmd.args(["-d", home]);
    }
    cmd.args(["-s", &user.shell, "-c", &user.description, name]);
    cmd
}

fn usermod(name: &str, user: &User) -> Command {
    let mut cmd = Command::new("usermod");
    if let Some(id) = user.uid {
        cmd.args(["-u", &id.to_string()]);
    }
    if let Some(group) = &user.group {
        cmd.args(["-g", group]);
    }
    if let Some(home) = &user.home {
        cmd.args(["-d", home, "-m"]);
    }
    cmd.args([
        "-G",
        &user.groups.join(","),
        "-s",
        &user.shell,
        "-c",
        &user.description,
        name,
    ]);
    cmd
}

/// Writes, replaces and unmanages files, backing up originals and hand edits instead of losing them.
pub fn files(paths: &Paths, change: &SystemChange) -> Result<()> {
    let backups = paths.state.join("backups");
    let edited = paths.state.join("edited").join(now().to_string());
    for (path, want) in &change.to.files {
        let target = Path::new(path);
        let disk = on_disk(target)?;
        if matches(&disk, want) {
            continue;
        }
        if disk == OnDisk::Other {
            bail!("{path} exists and is not a file or symlink; archix won't replace it");
        }
        if disk != OnDisk::Missing {
            match change.from.files.get(path) {
                None => {
                    let backup = under(&backups, path);
                    if fs::symlink_metadata(&backup).is_err() {
                        fs::create_dir_all(backup.parent().expect("absolute path"))?;
                        fs::rename(target, &backup)
                            .or_else(|_| fs::copy(target, &backup).map(|_| ()))
                            .with_context(|| format!("backing up {path}"))?;
                    }
                }
                Some(previous) if !matches(&disk, previous) => {
                    let copy = under(&edited, path);
                    fs::create_dir_all(copy.parent().expect("absolute path"))?;
                    fs::copy(target, &copy).with_context(|| format!("saving the edited {path}"))?;
                    eprintln!(
                        "warning: {path} was edited outside archix; that version is saved at {}",
                        copy.display()
                    );
                }
                Some(_) => {}
            }
        }
        write(paths, target, want).with_context(|| format!("writing {path}"))?;
    }
    for (path, previous) in change
        .from
        .files
        .iter()
        .filter(|(path, _)| !change.to.files.contains_key(*path))
    {
        let target = Path::new(path);
        let backup = under(&backups, path);
        let disk = on_disk(target)?;
        if fs::symlink_metadata(&backup).is_ok() {
            if disk != OnDisk::Missing && !matches(&disk, previous) {
                eprintln!(
                    "warning: {path} was edited outside archix; restoring the original anyway, see {}",
                    backup.display()
                );
            }
            fs::rename(&backup, target).with_context(|| format!("restoring {path}"))?;
        } else if matches(&disk, previous) {
            fs::remove_file(target).with_context(|| format!("removing {path}"))?;
        }
    }
    Ok(())
}

/// Creates missing parent directories; ones archix creates for a user's file belong to that user.
fn create_parents(dir: &Path, want: &ManagedFile) -> Result<()> {
    let mut missing = Vec::new();
    let mut current = Some(dir);
    while let Some(path) = current {
        if path.exists() {
            break;
        }
        missing.push(path.to_path_buf());
        current = path.parent();
    }
    fs::create_dir_all(dir)?;
    if want.owner != "root" {
        let (Some(u), Some(g)) = (uid(&want.owner), gid(&want.group)) else {
            bail!("owner {}:{} doesn't exist", want.owner, want.group);
        };
        for path in missing {
            std::os::unix::fs::chown(&path, Some(u), Some(g))?;
        }
    }
    Ok(())
}

fn write(paths: &Paths, target: &Path, want: &ManagedFile) -> Result<()> {
    let dir = target.parent().context("file has no parent directory")?;
    create_parents(dir, want)?;
    let name = target.file_name().context("file has no name")?.to_string_lossy();
    let tmp = dir.join(format!(".{name}.archix.{}", std::process::id()));
    let _ = fs::remove_file(&tmp);
    match &want.link {
        Some(link) => {
            symlink(link, &tmp)?;
            if want.owner != "root"
                && let (Some(u), Some(g)) = (uid(&want.owner), gid(&want.group))
            {
                std::os::unix::fs::lchown(&tmp, Some(u), Some(g))?;
            }
        }
        None => {
            fs::write(&tmp, blobs::read(paths, &want.sha256)?)?;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(want.mode))?;
            let (Some(u), Some(g)) = (uid(&want.owner), gid(&want.group)) else {
                let _ = fs::remove_file(&tmp);
                bail!("owner {}:{} doesn't exist", want.owner, want.group);
            };
            std::os::unix::fs::chown(&tmp, Some(u), Some(g))?;
        }
    }
    match fs::rename(&tmp, target) {
        Err(e) if e.raw_os_error() == Some(libc::EBUSY) && want.link.is_none() => {
            // A bind-mounted file (like /etc/resolv.conf inside arch-chroot) can't be replaced, only rewritten.
            fs::copy(&tmp, target)?;
            fs::remove_file(&tmp)?;
            Ok(())
        }
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e.into())
        }
        Ok(()) => Ok(()),
    }
}

/// Reloads what changed files feed (systemd, sysctl, udev, locales); returns what failed without failing.
pub fn reload(change: &SystemChange) -> Vec<String> {
    let changed = change.changed_paths();
    let touched = |prefix: &str| changed.iter().any(|path| path.starts_with(prefix));
    let mut failures = Vec::new();
    let mut attempt = |cmd: &mut Command, what: &str| {
        if let Err(e) = run(cmd, what) {
            failures.push(format!("{e:#}"));
        }
    };
    if touched("/etc/systemd/") {
        attempt(
            Command::new("systemctl").arg("daemon-reload"),
            "systemctl daemon-reload",
        );
    }
    if touched("/etc/sysctl.d/") {
        attempt(Command::new("sysctl").arg("--system"), "sysctl --system");
    }
    if touched("/etc/udev/rules.d/") {
        attempt(
            Command::new("udevadm").args(["control", "--reload"]),
            "udevadm control --reload",
        );
    }
    if touched("/etc/locale.gen") {
        attempt(&mut Command::new("locale-gen"), "locale-gen");
    }
    failures
}

fn systemctl_attempts() -> (Vec<String>, impl FnMut(&[&str], &mut Vec<String>)) {
    let attempt = |args: &[&str], failures: &mut Vec<String>| {
        if let Err(e) = run(
            Command::new("systemctl").args(args),
            &format!("systemctl {}", args.join(" ")),
        ) {
            failures.push(format!("{e:#}"));
        }
    };
    (Vec::new(), attempt)
}

/// Stops and disables units turned off or leaving the config, while their unit files still exist.
pub fn stop_services(change: &SystemChange) -> Vec<String> {
    let (mut failures, mut attempt) = systemctl_attempts();
    for (unit, enabled) in &change.to.services {
        if !*enabled && is_enabled(unit, false) {
            attempt(&["disable", "--now", unit], &mut failures);
        }
    }
    for (unit, enabled) in &change.from.services {
        if !change.to.services.contains_key(unit) && *enabled && is_enabled(unit, false) {
            attempt(&["disable", "--now", unit], &mut failures);
        }
    }
    let user_off = change
        .to
        .user_services
        .iter()
        .filter(|(_, enabled)| !**enabled)
        .map(|(unit, _)| unit);
    let user_gone = change
        .from
        .user_services
        .keys()
        .filter(|unit| !change.to.user_services.contains_key(*unit));
    for unit in user_off.chain(user_gone) {
        if is_enabled(unit, true) {
            attempt(&["--global", "disable", unit], &mut failures);
        }
    }
    failures
}

/// Enables and starts units, restarting changed ones; failures are reported, not fatal, so a broken unit can't undo a switch.
pub fn start_services(change: &SystemChange) -> Vec<String> {
    let changed = change.changed_paths();
    let (mut failures, mut attempt) = systemctl_attempts();
    for (unit, enabled) in &change.to.services {
        if *enabled && !is_enabled(unit, false) {
            attempt(&["enable", "--now", unit], &mut failures);
        } else if *enabled && changed.contains(format!("/etc/systemd/system/{unit}").as_str()) {
            attempt(&["try-restart", unit], &mut failures);
        }
    }
    for (unit, enabled) in &change.to.user_services {
        if *enabled && !is_enabled(unit, true) {
            attempt(&["--global", "enable", unit], &mut failures);
        }
    }
    failures
}

/// Every way the managed files and users differ from the target.
pub fn mismatches(change: &SystemChange) -> Result<Vec<String>> {
    let mut problems = Vec::new();
    for (path, want) in &change.to.files {
        if !matches(&on_disk(Path::new(path))?, want) {
            problems.push(format!("{path} doesn't have its managed contents"));
        }
    }
    for name in change.to.users.keys() {
        if uid(name).is_none() {
            problems.push(format!("user {name} doesn't exist"));
        }
    }
    for name in change.to.groups.keys() {
        if gid(name).is_none() {
            problems.push(format!("group {name} doesn't exist"));
        }
    }
    Ok(problems)
}
