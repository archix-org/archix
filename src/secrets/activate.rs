//! The boot's decrypt step, `archix secrets activate` from archix-secrets.service: the booted deployment's secrets into a ramfs at /run/archix/secrets, each all or nothing, `path`s under /run, and crypt_hash passwords into /etc/shadow under the credentials' rules.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs;
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::memory::{MAX_TOTAL, SecretBuf};
use super::{Kind, Manifest, RUN_DIR, crypto};
use crate::paths::Paths;
use crate::system::apply::Accounts;

/// What the boot's step did, for `archix status`: in /run, so it lasts until shutdown.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Report {
    pub deployment: Option<u32>,
    /// Secrets written, by name.
    pub written: Vec<String>,
    /// Secrets left out, with why.
    pub missing: BTreeMap<String, String>,
    pub notes: Vec<String>,
}

pub fn report_path(root: &Path) -> PathBuf {
    root.join("run/archix/secrets-report.json")
}

pub fn read_report(root: &Path) -> Option<Report> {
    serde_json::from_slice(&fs::read(report_path(root)).ok()?).ok()
}

fn c_path(path: &Path) -> Result<CString> {
    CString::new(path.as_os_str().as_bytes()).with_context(|| format!("{} has a NUL byte", path.display()))
}

/// The filesystem type mounted at `path` itself, if anything is.
fn mounted_type(path: &Path) -> Option<String> {
    let info = fs::read_to_string("/proc/self/mountinfo").ok()?;
    let wanted = path.to_string_lossy();
    info.lines().rev().find_map(|line| {
        let fields: Vec<&str> = line.split(' ').collect();
        let point = fields.get(4)?.replace("\\040", " ");
        let dash = fields.iter().position(|f| *f == "-")?;
        (point == wanted).then(|| fields.get(dash + 1).map(|s| s.to_string()))?
    })
}

/// Mounts a ramfs, which never swaps, at `dir`, 0711 so names can't be listed, unless one is there.
pub fn mount_ramfs(dir: &Path) -> Result<()> {
    mount_ramfs_mode(dir, c"mode=0711")
}

fn mount_ramfs_mode(dir: &Path, data: &std::ffi::CStr) -> Result<()> {
    if mounted_type(dir).as_deref() == Some("ramfs") {
        return Ok(());
    }
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let (source, target, fstype) = (c"ramfs", c_path(dir)?, c"ramfs");
    let flags = libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC;
    if unsafe {
        libc::mount(
            source.as_ptr(),
            target.as_ptr(),
            fstype.as_ptr(),
            flags,
            data.as_ptr().cast(),
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error()).with_context(|| format!("mounting a ramfs at {}", dir.display()));
    }
    Ok(())
}

/// Writes `value` as `dir`/`name`, owned by uid:gid with `mode` before a byte is in it, and in place whole or not at all.
pub fn place(dir: &Path, name: &str, value: &[u8], uid: u32, gid: u32, mode: u32) -> Result<()> {
    let target = dir.join(name);
    let tmp = dir.join(format!(".{name}.archix-new"));
    let _ = fs::remove_file(&tmp);
    let dir_c = c_path(dir)?;
    let tmp_c = c_path(&tmp)?;
    let mut fd = unsafe {
        libc::open(
            dir_c.as_ptr(),
            libc::O_TMPFILE | libc::O_WRONLY | libc::O_CLOEXEC,
            0o600,
        )
    };
    let anonymous = fd >= 0;
    if !anonymous {
        fd = unsafe {
            libc::open(
                tmp_c.as_ptr(),
                libc::O_CREAT | libc::O_EXCL | libc::O_WRONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o000,
            )
        };
    }
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| format!("creating {}", target.display()));
    }
    let mut file = unsafe { fs::File::from_raw_fd(fd) };
    let done = (|| -> Result<()> {
        if unsafe { libc::fchown(file.as_raw_fd(), uid, gid) } != 0
            || unsafe { libc::fchmod(file.as_raw_fd(), mode) } != 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        file.write_all(value)?;
        if anonymous {
            let proc = CString::new(format!("/proc/self/fd/{}", file.as_raw_fd()))?;
            if unsafe {
                libc::linkat(
                    libc::AT_FDCWD,
                    proc.as_ptr(),
                    libc::AT_FDCWD,
                    tmp_c.as_ptr(),
                    libc::AT_SYMLINK_FOLLOW,
                )
            } != 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        fs::rename(&tmp, &target)?;
        Ok(())
    })();
    if done.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    done.with_context(|| format!("writing {}", target.display()))
}

/// Puts a secret at `at`, a path under /run: in a ramfs mounted on its directory, so an app that saves by writing a temporary file and renaming it (NetworkManager) edits it there, atomically and never swapped; a directory that already holds other files instead gets the ramfs file `from` bound over `at`.
fn place_at_path(from: &Path, at: &Path, value: &[u8], (uid, gid, mode): (u32, u32, u32)) -> Result<()> {
    let dir = at.parent().context("a path has a directory")?;
    let name = at
        .file_name()
        .context("a path has a name")?
        .to_string_lossy()
        .into_owned();
    let others = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .any(|entry| entry.file_name().to_string_lossy() != name);
    if mounted_type(dir).as_deref() != Some("ramfs") && others {
        return bind_file(from, at);
    }
    mount_ramfs_mode(dir, c"mode=0755")?;
    place(dir, &name, value, uid, gid, mode)
}

/// Binds the ramfs file `from` over a file at `at`, made empty if missing, so an app that refuses links reads it there while the value stays on the ramfs, never on /run's tmpfs, which swaps.
fn bind_file(from: &Path, at: &Path) -> Result<()> {
    let dir = at.parent().context("a path has a directory")?;
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let target = c_path(at)?;
    while mounted_type(at).is_some() {
        if unsafe { libc::umount2(target.as_ptr(), libc::MNT_DETACH) } != 0 {
            return Err(std::io::Error::last_os_error()).with_context(|| format!("unmounting {}", at.display()));
        }
    }
    if fs::symlink_metadata(at).is_ok_and(|meta| !meta.is_file()) {
        bail!("{} exists and isn't a file", at.display());
    }
    if fs::symlink_metadata(at).is_err() {
        let mut options = fs::OpenOptions::new();
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        options
            .write(true)
            .create_new(true)
            .open(at)
            .with_context(|| format!("creating {}", at.display()))?;
    }
    let source = c_path(from)?;
    if unsafe {
        libc::mount(
            source.as_ptr(),
            target.as_ptr(),
            std::ptr::null(),
            libc::MS_BIND,
            std::ptr::null(),
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("binding {} over {}", from.display(), at.display()));
    }
    Ok(())
}

/// Root's uid and gid for `owner` and `group` in `accounts`.
fn ids(accounts: &Accounts, owner: &str, group: &str) -> Option<(u32, u32)> {
    let uid = if owner == "root" { Some(0) } else { accounts.uid(owner) }?;
    let gid = if group == "root" { Some(0) } else { accounts.gid(group) }?;
    Some((uid, gid))
}

/// Decrypts the secrets of the deployment booted at `root` into its ramfs and shadow, naming each one it couldn't; Err only when nothing could be done at all.
pub fn activate(root: &Path) -> Result<Report> {
    let paths = Paths::under(root);
    let mut report = Report {
        deployment: crate::system::boot::booted_deployment(root),
        ..Report::default()
    };
    let Some(manifest) = Manifest::read(root) else {
        return Ok(report);
    };
    let run = root.join(RUN_DIR.trim_start_matches('/'));
    mount_ramfs(&run)?;
    fs::create_dir_all(run.join("files"))?;
    fs::set_permissions(run.join("files"), std::os::unix::fs::PermissionsExt::from_mode(0o711))?;
    let keep: Vec<&String> = manifest.secrets.keys().collect();
    for entry in fs::read_dir(&run).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name != "files" && !keep.iter().any(|k| **k == name) {
            let _ = fs::remove_file(entry.path());
        }
    }
    let identities = match crypto::machine_identities(&paths.state) {
        Ok(identities) => identities,
        Err(e) => {
            for name in manifest.secrets.keys() {
                report
                    .missing
                    .insert(name.clone(), "this machine's identity is missing or unreadable".into());
            }
            report.notes.push(format!(
                "no secret was decrypted at this boot: {e:#}. What uses them fails until it's back: restore /var/lib/archix/secrets/identity from your own backup, or `sudo archix secrets init --replace`, put the new key in secrets.machine_key, `archix secrets rekey` with an admin key, switch and reboot"
            ));
            return Ok(report);
        }
    };
    let accounts = Accounts::read(&root.join("etc"));
    let deploy = root.join(super::DEPLOY_DIR.trim_start_matches('/'));
    let mut values: BTreeMap<String, SecretBuf> = BTreeMap::new();
    let mut total = 0;
    for (name, entry) in &manifest.secrets {
        if entry.absent {
            report
                .missing
                .insert(name.clone(), "left out by `archix switch --without-secrets`".into());
            continue;
        }
        let ciphertext = match fs::read(deploy.join(format!("{}.age", entry.sha256))) {
            Ok(bytes) if crate::fetch::sha256_bytes(&bytes) == entry.sha256 => bytes,
            Ok(_) => {
                report
                    .missing
                    .insert(name.clone(), "its ciphertext in the deployment is damaged".into());
                continue;
            }
            Err(e) => {
                report.missing.insert(
                    name.clone(),
                    format!("its ciphertext is missing from the deployment ({})", e.kind()),
                );
                continue;
            }
        };
        let mut value = match crypto::decrypt(&ciphertext, &identities, &format!("secret {name}")) {
            Ok((value, _)) => value,
            Err(e) => {
                report.missing.insert(name.clone(), format!("{e:#}"));
                continue;
            }
        };
        total += value.len();
        if total > MAX_TOTAL {
            report.missing.insert(
                name.clone(),
                format!("the secrets hold more than {} MiB together", MAX_TOTAL >> 20),
            );
            continue;
        }
        if entry.kind == Kind::CryptHash {
            value.trim_end_newlines();
            values.insert(name.clone(), value);
            continue;
        }
        let Some((uid, gid)) = ids(&accounts, &entry.owner, &entry.group) else {
            report.missing.insert(
                name.clone(),
                format!("its owner {}:{} doesn't exist", entry.owner, entry.group),
            );
            continue;
        };
        let mut written = place(&run, name, value.as_slice(), uid, gid, entry.mode);
        if written.is_ok()
            && let Some(path) = &entry.path
        {
            written = place_at_path(
                &run.join(name),
                &root.join(path.trim_start_matches('/')),
                value.as_slice(),
                (uid, gid, entry.mode),
            );
        }
        match written {
            Ok(()) => report.written.push(name.clone()),
            Err(e) => {
                report.missing.insert(name.clone(), format!("{e:#}"));
            }
        }
        values.insert(name.clone(), value);
    }
    for (id, template) in &manifest.templates {
        let missing: Vec<&String> = template.secrets.iter().filter(|s| !values.contains_key(*s)).collect();
        if !missing.is_empty() {
            report.notes.push(format!(
                "{} wasn't written, as it needs {}",
                template.path,
                missing
                    .iter()
                    .map(|s| format!("secret {s}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
            continue;
        }
        let rendered = fs::read(deploy.join("templates").join(id))
            .context("reading its template")
            .and_then(|text| super::render_template(&text, &values.iter().map(|(k, v)| (k.clone(), v)).collect()));
        let written = rendered.and_then(|rendered| {
            let (uid, gid) = ids(&accounts, &template.owner, &template.group)
                .with_context(|| format!("its owner {}:{} doesn't exist", template.owner, template.group))?;
            place(&run.join("files"), id, rendered.as_slice(), uid, gid, template.mode)
        });
        if let Err(e) = written {
            report.notes.push(format!("{} wasn't written: {e:#}", template.path));
        }
    }
    for (user, name) in &manifest.hashes {
        let Some(value) = values.get(name) else { continue };
        if crate::system::credentials::overridden(&paths.state, user) {
            report.notes.push(format!(
                "{user}'s password was set in the rescue entry, so this boot keeps it over the crypt_hash secret {name} until a switch changes that secret"
            ));
            continue;
        }
        let hash = std::str::from_utf8(value.as_slice()).unwrap_or("\u{0}");
        let set = crate::system::credentials::check_hash(&format!("secret {name}"), hash)
            .and_then(|()| crate::system::credentials::enforce(&root.join("etc"), &[], user, hash));
        match set {
            Ok(None) => report.written.push(name.clone()),
            Ok(Some(kept)) => report.notes.push(kept),
            Err(e) => {
                report
                    .missing
                    .insert(name.clone(), format!("setting {user}'s password: {e:#}"));
            }
        }
    }
    Ok(report)
}

/// How the boot step's login notes begin, so the next boot replaces them.
pub const NOTE: &str = "archix: secrets: ";

/// Runs the boot step for the root at `root` and records what it did: the report in /run, a login note for anything missing; fails, so the unit shows failed, when a secret is missing.
pub fn run(root: &Path) -> Result<()> {
    super::memory::harden(true).context("keeping this process's memory out of swap and core dumps")?;
    crate::testhooks::crash_at("secrets-activate");
    let report = activate(root)?;
    let paths = Paths::under(root);
    let notes = fs::read_to_string(paths.notes()).unwrap_or_default();
    let mut notes: String = notes
        .lines()
        .filter(|line| !line.starts_with(NOTE))
        .map(|line| format!("{line}\n"))
        .collect();
    let mut said = report.notes.clone();
    for (name, why) in &report.missing {
        said.push(format!(
            "secret {name} wasn't decrypted at this boot, so what uses it fails: {why}"
        ));
    }
    for line in &said {
        println!("{NOTE}{line}");
        notes.push_str(&format!("{NOTE}{line}\n"));
    }
    let _ = crate::durable::write(&paths.notes(), notes.as_bytes());
    if let Ok(text) = serde_json::to_vec(&report) {
        let _ = fs::create_dir_all(root.join("run/archix"));
        let _ = fs::write(report_path(root), text);
    }
    if let Err(e) = crate::system::sleep::sync(root) {
        println!("archix: couldn't update the hibernation guard: {e:#}");
    }
    println!(
        "archix: decrypted {} of {} secrets into {RUN_DIR}",
        report.written.len(),
        report.written.len() + report.missing.len()
    );
    if !report.missing.is_empty() {
        bail!("{} secrets weren't decrypted", report.missing.len());
    }
    Ok(())
}
