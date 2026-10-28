//! Reads an existing Arch install into the pieces `archix init` turns into a config.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CStr;
use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::Command;

use std::sync::LazyLock;

use anyhow::{Context, Result, bail};
use md5::{Digest, Md5};
use serde::Deserialize;

use crate::pacman::parse_desc;
use crate::repos::{Custom, is_official};

/// Which config files init scans for, never touches, treats as secrets and recommends; kept in capture.json.
#[derive(Debug, Deserialize)]
pub struct Lists {
    /// Endings of backups and leftovers (.pacnew, passwd-), which are never offered.
    pub backups: Vec<String>,
    /// Where symlinks belong to services, default_target and masked_units rather than to files.
    pub links_elsewhere: Vec<String>,
    /// Files archix or other tools own; an entry ending in `/` covers everything under it.
    pub never: Vec<String>,
    /// Any path containing one of these holds credentials and is never copied.
    pub secret: Vec<String>,
    /// Text that only shows up in credentials; a file containing any of it is treated as a secret too.
    pub secret_content: Vec<String>,
    /// Credentials outside /etc that stay on this machine, named so they aren't forgotten; an entry ending in `/` is a directory.
    pub secret_elsewhere: Vec<String>,
    /// Worth managing by default; an entry ending in `/` covers everything under it.
    pub recommended: Vec<String>,
    /// Paths under /usr no package owns that pacman hooks make again on any machine (depmod's, caches); `*` stands for any run of characters, and a directory has its trailing `/`.
    pub generated: Vec<String>,
    pub dotfiles: DotfileLists,
}

/// Names relative to a home; one `*` stands for any run of characters.
#[derive(Debug, Deserialize)]
pub struct DotfileLists {
    /// Ticked by default.
    pub recommended: Vec<String>,
    /// Hold credentials: never offered, only counted.
    pub secret: Vec<String>,
    /// Caches, app state and the like: never offered.
    pub skip: Vec<String>,
    /// Single files their app replaces on save, which turns a live link back into a plain file.
    pub replaced: Vec<String>,
}

fn matches_name(list: &[String], name: &str) -> bool {
    list.iter().any(|entry| match entry.split_once('*') {
        Some((prefix, suffix)) => {
            name.len() >= prefix.len() + suffix.len() && name.starts_with(prefix) && name.ends_with(suffix)
        }
        None => entry == name,
    })
}

/// A home entry init can offer to bring into the config.
#[derive(Debug, Clone, PartialEq)]
pub struct Dotfile {
    pub name: String,
    pub recommended: bool,
    /// Its app replaces the file on save, so a live link wouldn't last.
    pub replaced: bool,
}

pub static LISTS: LazyLock<Lists> =
    LazyLock::new(|| serde_json::from_str(include_str!("capture.json")).expect("capture.json is valid"));

fn listed(list: &[String], path: &str) -> bool {
    list.iter().any(|entry| {
        if entry.ends_with('/') {
            path.starts_with(entry.as_str())
        } else {
            entry == path
        }
    })
}

#[derive(Debug, Clone, PartialEq)]
pub struct FoundFile {
    pub path: String,
    pub mode: u32,
    pub owner: String,
    pub group: String,
    /// On the recommended list, so managed unless deselected.
    pub recommended: bool,
    /// Edited from what its package shipped, rather than written from scratch.
    pub edited: bool,
    /// Where to copy it from when that isn't `path`, as for a unit file put under /usr/lib by hand.
    pub source: Option<String>,
    /// A symlink to this target rather than a file.
    pub link: Option<String>,
}

/// Config files init can offer, and the secret ones it leaves out without asking.
#[derive(Debug, Default)]
pub struct Found {
    pub files: Vec<FoundFile>,
    pub secrets: Vec<String>,
    /// Package config files this user can't read, so whether they were edited isn't known.
    pub unreadable: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Account {
    pub uid: u32,
    /// Below uid 1000 and made by hand, not by a sysusers.d file.
    pub system: bool,
    /// None when it's the user's own group.
    pub group: Option<String>,
    pub groups: Vec<String>,
    pub shell: String,
    /// None when it's /home/<name>.
    pub home: Option<String>,
    pub description: String,
}

#[derive(Debug, Default)]
pub struct Accounts {
    pub users: BTreeMap<String, Account>,
    /// Groups in the regular id range, and system ones no sysusers.d file makes, that aren't some captured user's own group.
    pub groups: BTreeMap<String, u32>,
    /// Which of `groups` are system groups.
    pub system_groups: BTreeSet<String>,
}

/// One package's entry in the local database, as far as init cares.
#[derive(Debug, Default)]
pub struct Installed {
    pub build_date: Option<u64>,
    pub files: Vec<String>,
    /// Backup-listed files and the md5 pacman installed them with.
    pub backups: Vec<(String, String)>,
}

pub fn read_installed(local_db: &Path) -> Result<BTreeMap<String, Installed>> {
    let mut out = BTreeMap::new();
    for entry in fs::read_dir(local_db).with_context(|| format!("reading {}", local_db.display()))? {
        let dir = entry.with_context(|| format!("reading {}", local_db.display()))?.path();
        let Ok(desc) = fs::read_to_string(dir.join("desc")) else {
            continue;
        };
        let desc = parse_desc(&desc);
        let Some(name) = desc.get("NAME").and_then(|v| v.first()) else {
            continue;
        };
        let files = parse_desc(&fs::read_to_string(dir.join("files")).unwrap_or_default());
        let backups = files
            .get("BACKUP")
            .into_iter()
            .flatten()
            .filter_map(|line| line.split_once('\t'))
            .map(|(path, md5)| (format!("/{path}"), md5.to_string()))
            .collect();
        out.insert(
            name.clone(),
            Installed {
                build_date: desc
                    .get("BUILDDATE")
                    .and_then(|v| v.first())
                    .and_then(|d| d.parse().ok()),
                files: files.get("FILES").cloned().unwrap_or_default(),
                backups,
            },
        );
    }
    Ok(out)
}

pub fn md5_bytes(bytes: &[u8]) -> String {
    Md5::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

pub fn md5(path: &Path) -> Result<String> {
    let mut hasher = Md5::new();
    let mut file = File::open(path).with_context(|| format!("reading {}", path.display()))?;
    let mut buf = vec![0; 1 << 16];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

fn excluded(path: &str, skip: &BTreeSet<String>) -> bool {
    listed(&LISTS.never, path) || skip.contains(path)
}

pub fn is_secret(path: &str) -> bool {
    LISTS.secret.iter().any(|hint| path.contains(hint.as_str()))
}

/// Whether a file, or any file in a directory, contains text that only credentials have; big files and trees are sampled, not read whole.
pub fn holds_secret(path: &Path) -> bool {
    const MAX_FILE: u64 = 1024 * 1024;
    const MAX_FILES: usize = 200;
    let mut stack = vec![path.to_path_buf()];
    let mut seen = 0;
    while let Some(path) = stack.pop() {
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            stack.extend(fs::read_dir(&path).into_iter().flatten().flatten().map(|e| e.path()));
            continue;
        }
        if !meta.is_file() || meta.len() > MAX_FILE {
            continue;
        }
        seen += 1;
        if seen > MAX_FILES {
            break;
        }
        match fs::read(&path) {
            Ok(bytes) => {
                let text = String::from_utf8_lossy(&bytes);
                if LISTS.secret_content.iter().any(|marker| text.contains(marker.as_str())) {
                    return true;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return true,
            Err(_) => {}
        }
    }
    false
}

fn is_backup(path: &str) -> bool {
    LISTS.backups.iter().any(|ending| path.ends_with(ending.as_str()))
}

/// `path`, absolute on the machine, inside `root`.
fn on_disk(root: &Path, path: &str) -> std::path::PathBuf {
    root.join(path.trim_start_matches('/'))
}

fn found(root: &Path, path: &str, edited: bool) -> Option<FoundFile> {
    let meta = fs::symlink_metadata(on_disk(root, path)).ok()?;
    let link = match meta.file_type().is_symlink() {
        true => Some(fs::read_link(on_disk(root, path)).ok()?.to_string_lossy().into_owned()),
        false if meta.is_file() => None,
        false => return None,
    };
    Some(FoundFile {
        path: path.to_string(),
        mode: meta.mode() & 0o7777,
        owner: user_name(meta.uid()).unwrap_or_else(|| meta.uid().to_string()),
        group: group_name(meta.gid()).unwrap_or_else(|| meta.gid().to_string()),
        recommended: listed(&LISTS.recommended, path),
        edited,
        source: None,
        link,
    })
}

/// Whether `pattern`, where each `*` stands for any run of characters, matches all of `path`.
pub fn glob(pattern: &str, path: &str) -> bool {
    let mut parts = pattern.split('*');
    let first = parts.next().unwrap_or_default();
    let Some(mut rest) = path.strip_prefix(first) else {
        return false;
    };
    let parts: Vec<&str> = parts.collect();
    let Some((last, middle)) = parts.split_last() else {
        return rest.is_empty();
    };
    for part in middle {
        match rest.find(part) {
            Some(at) => rest = &rest[at + part.len()..],
            None => return false,
        }
    }
    rest.ends_with(last)
}

/// Whether pacman hooks make `path` (a directory with its trailing `/`) again on any machine with the same packages.
pub fn is_generated(path: &str) -> bool {
    LISTS.generated.iter().any(|pattern| glob(pattern, path))
}

/// Every path the installed packages own, absolute.
pub fn owned(installed: &BTreeMap<String, Installed>) -> BTreeSet<String> {
    installed
        .values()
        .flat_map(|pkg| pkg.files.iter().map(|f| format!("/{f}")))
        .collect()
}

/// Files and symlinks under `dir` (absolute, inside `root`) that no package owns, as absolute paths.
fn unowned_under(root: &Path, dir: &str, owned: &BTreeSet<String>) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![on_disk(root, dir)];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else { continue };
            let relative = entry.path();
            let path = format!("/{}", relative.strip_prefix(root).unwrap_or(&relative).display());
            if kind.is_dir() {
                if !listed(&LISTS.never, &format!("{path}/")) {
                    stack.push(entry.path());
                }
            } else if !owned.contains(&path) {
                out.push(path);
            }
        }
    }
    out
}

/// Backup files edited since their package installed them, plus every file and symlink in /etc no package owns, but for leftovers, generated files and archix's own.
pub fn config_files(root: &Path, installed: &BTreeMap<String, Installed>, skip: &BTreeSet<String>) -> Result<Found> {
    config_files_owned(root, installed, &owned(installed), skip)
}

/// `config_files` with `owned`, every path `installed` owns, already gathered.
pub fn config_files_owned(
    root: &Path,
    installed: &BTreeMap<String, Installed>,
    owned: &BTreeSet<String>,
    skip: &BTreeSet<String>,
) -> Result<Found> {
    let mut candidates: BTreeMap<String, bool> = BTreeMap::new();
    let mut out = Found::default();
    for pkg in installed.values() {
        for (path, sum) in &pkg.backups {
            let file = on_disk(root, path);
            if !file.is_file() {
                continue;
            }
            match md5(&file) {
                Ok(now) if now != *sum => {
                    candidates.insert(path.clone(), true);
                }
                Ok(_) => {}
                Err(_) if !excluded(path, skip) => out.unreadable.push(path.clone()),
                Err(_) => {}
            }
        }
    }
    out.unreadable.sort();
    for path in unowned_under(root, "/etc", owned) {
        candidates.entry(path).or_insert(false);
    }
    for (path, edited) in candidates {
        if excluded(&path, skip) || is_backup(&path) {
            continue;
        }
        let file = on_disk(root, &path);
        let link = fs::symlink_metadata(&file).is_ok_and(|meta| meta.file_type().is_symlink());
        if link && listed(&LISTS.links_elsewhere, &path) {
            continue;
        }
        if is_secret(&path) || (!link && holds_secret(&file)) {
            out.secrets.push(path);
        } else if let Some(file) = found(root, &path, edited) {
            out.files.push(file);
        }
    }
    Ok(out)
}

/// Unit files no package owns in /usr/lib/systemd/system, put there by hand (a `make install`), as copies for /etc/systemd/system: each deployment's /usr holds only what packages install.
pub fn unowned_units(root: &Path, installed: &BTreeMap<String, Installed>) -> Found {
    const VENDOR: &str = "/usr/lib/systemd/system/";
    let mut out = Found::default();
    let mut paths = unowned_under(root, VENDOR.trim_end_matches('/'), &owned(installed));
    paths.sort();
    for source in paths {
        let relative = &source[VENDOR.len()..];
        let path = format!("/etc/systemd/system/{relative}");
        if fs::symlink_metadata(on_disk(root, &path)).is_ok() {
            continue;
        }
        if is_secret(&source) || holds_secret(&on_disk(root, &source)) {
            out.secrets.push(source);
            continue;
        }
        if let Some(found) = found(root, &source, false).filter(|found| found.link.is_none()) {
            out.files.push(FoundFile {
                path,
                recommended: true,
                source: Some(source),
                ..found
            });
        }
    }
    out
}

/// The lines of hand-made /etc/tmpfiles.d files among `files`, for the tmpfiles option, with the paths they come from; a file masking a packaged one of its name, a link, or one with a line archix refuses stays a file.
pub fn tmpfiles(root: &Path, files: &[FoundFile]) -> (Vec<String>, Vec<String>) {
    let (mut lines, mut taken) = (Vec::new(), Vec::new());
    for file in files {
        let Some(name) = file.path.strip_prefix("/etc/tmpfiles.d/") else {
            continue;
        };
        if !name.ends_with(".conf") || name.contains('/') || file.link.is_some() || file.source.is_some() {
            continue;
        }
        if on_disk(root, &format!("/usr/lib/tmpfiles.d/{name}"))
            .symlink_metadata()
            .is_ok()
        {
            continue;
        }
        let Ok(text) = fs::read_to_string(on_disk(root, &file.path)) else {
            continue;
        };
        let own: Vec<String> = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(str::to_string)
            .collect();
        let protected = ["/archix", "/var/lib/archix", "/var/usrlocal", "/efi", "/boot"].map(str::to_string);
        if crate::system::credentials::check_tmpfiles(&own, &protected).is_ok() {
            lines.extend(own);
            taken.push(file.path.clone());
        }
    }
    (lines, taken)
}

/// The keys in /etc/ssh/authorized_keys.d/<user> files among `files`, by user, for users.<name>.ssh_authorized_keys; a file with a line archix refuses stays a file.
pub fn authorized_keys(root: &Path, files: &[FoundFile]) -> BTreeMap<String, Vec<String>> {
    let mut keys = BTreeMap::new();
    for file in files {
        let Some(user) = file
            .path
            .strip_prefix(&format!("{}/", crate::system::credentials::KEYS_DIR))
        else {
            continue;
        };
        if user.contains('/') || file.link.is_some() {
            continue;
        }
        let Ok(text) = fs::read_to_string(on_disk(root, &file.path)) else {
            continue;
        };
        let lines: Vec<String> = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(str::to_string)
            .collect();
        if !lines.is_empty()
            && lines
                .iter()
                .all(|line| crate::system::credentials::check_key("", line).is_ok())
        {
            keys.insert(user.to_string(), lines);
        }
    }
    keys
}

/// Secret paths as a reader wants them: everything under one of the secret directories as that directory with a count.
pub fn group_secrets(paths: &[String]) -> Vec<String> {
    let mut dirs: BTreeMap<&str, usize> = BTreeMap::new();
    let mut out = Vec::new();
    for path in paths {
        match LISTS
            .secret
            .iter()
            .find(|entry| entry.ends_with('/') && path.starts_with(entry.as_str()))
        {
            Some(dir) => *dirs.entry(dir).or_default() += 1,
            None => out.push(path.clone()),
        }
    }
    out.extend(dirs.into_iter().map(|(dir, n)| match n {
        1 => format!("{dir} (1 file)"),
        n => format!("{dir} ({n} files)"),
    }));
    out.sort();
    out
}

/// The credentials outside /etc this machine has, which no config carries.
pub fn secrets_elsewhere(root: &Path) -> Vec<String> {
    LISTS
        .secret_elsewhere
        .iter()
        .filter(|path| fs::symlink_metadata(on_disk(root, path)).is_ok())
        .cloned()
        .collect()
}

fn user_name(uid: u32) -> Option<String> {
    let entry = unsafe { libc::getpwuid(uid) };
    (!entry.is_null()).then(|| {
        unsafe { CStr::from_ptr((*entry).pw_name) }
            .to_string_lossy()
            .into_owned()
    })
}

fn group_name(gid: u32) -> Option<String> {
    let entry = unsafe { libc::getgrgid(gid) };
    (!entry.is_null()).then(|| {
        unsafe { CStr::from_ptr((*entry).gr_name) }
            .to_string_lossy()
            .into_owned()
    })
}

/// Official repos in order, and third-party ones with their servers (Include'd mirrorlists read through `read`).
pub fn parse_pacman_conf(text: &str, read: impl Fn(&str) -> Option<String>) -> (Vec<String>, Vec<(String, Custom)>) {
    let mut official = Vec::new();
    let mut custom: Vec<(String, Custom)> = Vec::new();
    let mut current: Option<String> = None;
    for line in text.lines().map(str::trim) {
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            current = (name != "options").then(|| name.to_string());
            match &current {
                Some(name) if is_official(name) => official.push(name.clone()),
                Some(name) => custom.push((
                    name.clone(),
                    Custom {
                        servers: Vec::new(),
                        keys: Vec::new(),
                        keyserver: "hkps://keyserver.ubuntu.com".into(),
                        sig_level: String::new(),
                        before_official: official.is_empty(),
                    },
                )),
                None => {}
            }
            continue;
        }
        let Some(name) = &current else { continue };
        let Some((_, repo)) = custom.iter_mut().find(|(n, _)| n == name) else {
            continue;
        };
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "Server" => repo.servers.push(value.to_string()),
            "SigLevel" => repo.sig_level = value.to_string(),
            "Include" => {
                for line in read(value).unwrap_or_default().lines().map(str::trim) {
                    if let Some((key, server)) = line.split_once('=')
                        && key.trim() == "Server"
                    {
                        repo.servers.push(server.trim().to_string());
                    }
                }
            }
            _ => {}
        }
    }
    for (_, repo) in &mut custom {
        if repo.sig_level.is_empty() {
            repo.sig_level = "Required DatabaseOptional".into();
        }
    }
    (official, custom)
}

/// The kinds of unit `services` can enable; targets too, such as nfs-client.target.
pub const UNIT_TYPES: &str = "service,socket,timer,path,target,mount,automount,swap";

/// What `systemctl list-unit-files` says init should keep.
#[derive(Debug, Default, PartialEq)]
pub struct UnitStates {
    /// Units whose enabled state differs from their preset.
    pub changed: BTreeMap<String, bool>,
    /// Units masked in /etc (not just for this boot).
    pub masked: Vec<String>,
}

/// Reads `systemctl list-unit-files --no-legend`.
pub fn parse_unit_files(text: &str) -> UnitStates {
    let mut out = UnitStates::default();
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let [unit, state, preset, ..] = fields[..] else {
            continue;
        };
        if unit.contains("@.") {
            continue;
        }
        match (state, preset) {
            ("enabled", "disabled") => {
                out.changed.insert(unit.to_string(), true);
            }
            ("disabled", "enabled") => {
                out.changed.insert(unit.to_string(), false);
            }
            ("masked", _) => out.masked.push(unit.to_string()),
            _ => {}
        }
    }
    out
}

pub fn unit_states() -> Result<UnitStates> {
    let output = Command::new("systemctl")
        .args(["list-unit-files", "--no-legend", "--no-pager"])
        .arg(format!("--type={UNIT_TYPES}"))
        .output()
        .context("running systemctl")?;
    Ok(parse_unit_files(&String::from_utf8_lossy(&output.stdout)))
}

/// The target /etc/systemd/system/default.target points to, when it's set there rather than left to systemd's default.
pub fn default_target(root: &Path) -> Option<String> {
    let target = fs::read_link(on_disk(root, "/etc/systemd/system/default.target")).ok()?;
    let name = target.file_name()?.to_string_lossy().into_owned();
    name.ends_with(".target").then_some(name)
}

/// Whether a masked unit's mask is a link to /dev/null in /etc/systemd/system, which masked_units writes.
pub fn masked_in_etc(root: &Path, unit: &str) -> bool {
    fs::read_link(on_disk(root, &format!("/etc/systemd/system/{unit}")))
        .is_ok_and(|target| target == Path::new("/dev/null"))
}

/// Where a unit's file lives, per systemd.
pub fn unit_path(unit: &str) -> Option<String> {
    let output = Command::new("systemctl")
        .args(["show", "--property=FragmentPath", "--value", unit])
        .output()
        .ok()?;
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (output.status.success() && !path.is_empty()).then_some(path)
}

/// The kernel command line minus what the boot loader and archix add themselves.
pub fn kernel_params(cmdline: &str) -> Vec<String> {
    cmdline
        .split_whitespace()
        .filter(|p| !p.starts_with("BOOT_IMAGE=") && !p.starts_with("initrd=") && !p.starts_with("archix."))
        .map(str::to_string)
        .collect()
}

/// The users and groups sysusers.d lines (`u`, `g`, `m`) make, which packages bring along.
pub fn parse_sysusers(text: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        match fields[..] {
            [kind, name, ..] if kind.starts_with('u') || kind.starts_with('g') => {
                names.insert(name.to_string());
            }
            [kind, user, group, ..] if kind.starts_with('m') => {
                names.extend([user.to_string(), group.to_string()]);
            }
            _ => {}
        }
    }
    names
}

/// The users and groups every sysusers.d file under `root` makes.
pub fn sysusers(root: &Path) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for dir in ["/usr/lib/sysusers.d", "/etc/sysusers.d", "/run/sysusers.d"] {
        for entry in fs::read_dir(on_disk(root, dir)).into_iter().flatten().flatten() {
            if entry.file_name().to_string_lossy().ends_with(".conf") {
                names.extend(parse_sysusers(&fs::read_to_string(entry.path()).unwrap_or_default()));
            }
        }
    }
    names
}

/// Regular users (uid 1000 to 59999), system ones (uid 1 to 999) that `sysusers` doesn't make, and the groups of either kind that aren't one of their own groups.
pub fn parse_accounts(passwd: &str, group: &str, sysusers: &BTreeSet<String>) -> Accounts {
    let groups: Vec<(String, u32, Vec<String>)> = group
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split(':').collect();
            let [name, _, gid, members] = fields[..] else {
                return None;
            };
            let members = members
                .split(',')
                .filter(|m| !m.is_empty())
                .map(str::to_string)
                .collect();
            Some((name.to_string(), gid.parse().ok()?, members))
        })
        .collect();
    let regular = |id: u32| (1000..60000).contains(&id);
    let hand_made = |id: u32, name: &str| (1..1000).contains(&id) && !sysusers.contains(name);
    let mut accounts = Accounts::default();
    for line in passwd.lines() {
        let fields: Vec<&str> = line.split(':').collect();
        let [name, _, uid, gid, gecos, home, shell] = fields[..] else {
            continue;
        };
        let (Ok(uid), Ok(gid)) = (uid.parse::<u32>(), gid.parse::<u32>()) else {
            continue;
        };
        let system = hand_made(uid, name);
        if !regular(uid) && !system {
            continue;
        }
        let primary = groups.iter().find(|(_, id, _)| *id == gid).map(|(n, _, _)| n.clone());
        if system && primary.is_none() {
            continue;
        }
        let mut supplementary: Vec<String> = groups
            .iter()
            .filter(|(_, _, members)| members.iter().any(|m| m == name))
            .map(|(n, _, _)| n.clone())
            .collect();
        supplementary.sort();
        accounts.users.insert(
            name.to_string(),
            Account {
                uid,
                system,
                group: primary.filter(|g| g != name),
                groups: supplementary,
                shell: shell.to_string(),
                home: (home != format!("/home/{name}")).then(|| home.to_string()),
                description: gecos.to_string(),
            },
        );
    }
    for (name, gid, _) in &groups {
        let own = accounts.users.get(name).is_some_and(|user| user.group.is_none());
        if own {
            continue;
        }
        if regular(*gid) {
            accounts.groups.insert(name.clone(), *gid);
        } else if hand_made(*gid, name) {
            accounts.groups.insert(name.clone(), *gid);
            accounts.system_groups.insert(name.clone());
        }
    }
    accounts
}

/// The packages `pacman -Qq<flags>` lists; an empty list makes pacman exit 1, which isn't an error here.
pub fn pacman_query(flags: &str) -> Result<Vec<String>> {
    let output = Command::new("pacman")
        .arg(format!("-Qq{flags}"))
        .output()
        .context("running pacman")?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() && !stderr.trim().is_empty() {
        bail!("pacman -Qq{flags} failed: {}", stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_string)
        .collect())
}

pub fn is_btrfs(path: &Path) -> bool {
    const BTRFS_SUPER_MAGIC: i64 = 0x9123_683e;
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) else {
        return false;
    };
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    unsafe { libc::statfs(c_path.as_ptr(), &mut stat) == 0 && stat.f_type as i64 == BTRFS_SUPER_MAGIC }
}

/// The ESP when systemd-boot is installed on it.
pub fn systemd_boot_esp() -> Option<String> {
    let installed = Command::new("bootctl")
        .arg("is-installed")
        .output()
        .is_ok_and(|o| o.status.success());
    if !installed {
        return None;
    }
    let output = Command::new("bootctl").arg("--print-esp-path").output().ok()?;
    let esp = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (output.status.success() && !esp.is_empty()).then_some(esp)
}

/// What in a home is worth offering as dotfiles (~/.config entries and top-level dotfiles), and how many hold secrets.
pub fn dotfiles(home: &Path) -> (Vec<Dotfile>, usize) {
    let lists = &LISTS.dotfiles;
    let mut names = Vec::new();
    for entry in fs::read_dir(home.join(".config")).into_iter().flatten().flatten() {
        names.push(format!(".config/{}", entry.file_name().to_string_lossy()));
    }
    for entry in fs::read_dir(home).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            names.push(name);
        }
    }
    names.sort();
    let mut secrets = 0;
    let mut out = Vec::new();
    for name in names {
        if matches_name(&lists.secret, &name) || is_secret(&name) {
            secrets += 1;
            continue;
        }
        if matches_name(&lists.skip, &name) {
            continue;
        }
        if holds_secret(&home.join(&name)) {
            secrets += 1;
            continue;
        }
        let recommended = matches_name(&lists.recommended, &name);
        let replaced = !home.join(&name).is_dir() && matches_name(&lists.replaced, &name);
        out.push(Dotfile {
            name,
            recommended,
            replaced,
        });
    }
    (out, secrets)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn pacman_conf_repos_and_mirrorlists() {
        let conf = "[options]\nSigLevel = Required\n\n[cachyos]\nInclude = /etc/pacman.d/cachyos-mirrorlist\n\n\
                    [core]\nInclude = /etc/pacman.d/mirrorlist\n\n[extra]\nInclude = /etc/pacman.d/mirrorlist\n\n\
                    #[multilib]\n#Include = /etc/pacman.d/mirrorlist\n\n[chaotic-aur]\nSigLevel = Optional\n\
                    Server = https://a.example/$repo/$arch\n";
        let (official, custom) = parse_pacman_conf(conf, |path| {
            (path == "/etc/pacman.d/cachyos-mirrorlist")
                .then(|| "# Server = https://off.example\nServer = https://on.example/$arch/$repo\n".to_string())
        });
        assert_eq!(official, ["core", "extra"]);
        assert_eq!(custom[0].0, "cachyos");
        assert!(custom[0].1.before_official);
        assert_eq!(custom[0].1.servers, ["https://on.example/$arch/$repo"]);
        assert_eq!(custom[0].1.sig_level, "Required DatabaseOptional");
        assert_eq!(custom[1].0, "chaotic-aur");
        assert!(!custom[1].1.before_official);
        assert_eq!(custom[1].1.sig_level, "Optional");
    }

    #[test]
    fn services_that_differ_from_their_preset() {
        let text = "sshd.service enabled disabled\nsystemd-networkd.service disabled enabled\n\
                    getty@.service enabled enabled\nfoo@.service enabled disabled\nstatic.service static -\n\
                    bar.timer enabled enabled\n";
        assert_eq!(
            parse_unit_files(text).changed,
            BTreeMap::from([
                ("sshd.service".to_string(), true),
                ("systemd-networkd.service".to_string(), false)
            ])
        );
    }

    #[test]
    fn targets_count_as_services_and_masks_are_kept() {
        assert!(UNIT_TYPES.split(',').any(|kind| kind == "target"), "{UNIT_TYPES}");
        let states = parse_unit_files(
            "nfs-client.target enabled disabled\nsleep.target masked disabled\nfoo.service masked-runtime disabled\n",
        );
        assert_eq!(
            states.changed,
            BTreeMap::from([("nfs-client.target".to_string(), true)])
        );
        assert_eq!(states.masked, ["sleep.target"]);
    }

    #[test]
    fn kernel_params_drop_loader_additions() {
        assert_eq!(
            kernel_params(
                "BOOT_IMAGE=/vmlinuz-linux initrd=\\initramfs.img root=UUID=x rw archix.generation=3 quiet\n"
            ),
            ["root=UUID=x", "rw", "quiet"]
        );
    }

    #[test]
    fn accounts_skip_system_users_and_private_groups() {
        let passwd = "root:x:0:0::/root:/bin/bash\nme:x:1000:1000:Me:/home/me:/usr/bin/zsh\n\
                      other:x:1001:100::/srv/other:/bin/bash\nnobody:x:65534:65534::/:/usr/bin/nologin\n";
        let group = "root:x:0:\nwheel:x:998:me,other\nusers:x:100:\nme:x:1000:\nshared:x:1500:me\n";
        let accounts = parse_accounts(
            passwd,
            group,
            &BTreeSet::from(["wheel".to_string(), "users".to_string()]),
        );
        assert_eq!(accounts.users.keys().collect::<Vec<_>>(), ["me", "other"]);
        let me = &accounts.users["me"];
        assert_eq!((me.group.as_deref(), me.home.as_deref()), (None, None));
        assert_eq!(me.groups, ["shared", "wheel"]);
        let other = &accounts.users["other"];
        assert_eq!(other.group.as_deref(), Some("users"));
        assert_eq!(other.home.as_deref(), Some("/srv/other"));
        assert_eq!(accounts.groups, BTreeMap::from([("shared".to_string(), 1500)]));
    }

    #[test]
    fn system_users_and_groups_made_by_hand_are_kept() {
        let passwd = "root:x:0:0::/root:/bin/bash\npostgres:x:975:975::/var/lib/postgres:/usr/bin/bash\n\
                      scanner:x:950:950::/opt/scanner:/bin/false\nmetrics:x:220:220::/:/usr/bin/nologin\n\
                      me:x:1000:1000::/home/me:/bin/bash\n";
        let group =
            "root:x:0:\npostgres:x:975:\nscanner:x:950:\nmetrics:x:220:\ni2c:x:961:me\ndocker:x:972:me\nme:x:1000:\n";
        let sysusers = parse_sysusers(
            "# made by packages\nu postgres 975 - /var/lib/postgres\ng docker 972\nu! root 0\nm me wheel\n",
        );
        let accounts = parse_accounts(passwd, group, &sysusers);
        assert_eq!(accounts.users.keys().collect::<Vec<_>>(), ["me", "metrics", "scanner"]);
        let scanner = &accounts.users["scanner"];
        assert!(scanner.system && scanner.uid == 950 && scanner.group.is_none());
        assert_eq!(
            (scanner.home.as_deref(), scanner.shell.as_str()),
            (Some("/opt/scanner"), "/bin/false")
        );
        assert!(!accounts.users["me"].system);
        assert_eq!(accounts.users["me"].groups, ["docker", "i2c"]);
        assert_eq!(accounts.groups, BTreeMap::from([("i2c".to_string(), 961)]));
        assert_eq!(accounts.system_groups, BTreeSet::from(["i2c".to_string()]));
    }

    #[test]
    fn lists_parse_and_sort_files() {
        assert!(
            LISTS
                .never
                .iter()
                .chain(&LISTS.recommended)
                .chain(&LISTS.links_elsewhere)
                .all(|path| path.starts_with("/etc/"))
        );
        assert!(is_secret("/etc/NetworkManager/system-connections/home.nmconnection"));
        assert!(is_secret("/etc/ssh/ssh_host_ed25519_key"));
        assert!(!is_secret("/etc/sudoers"));
        assert!(listed(&LISTS.recommended, "/etc/fstab"));
        assert!(listed(&LISTS.recommended, "/etc/sysctl.d/99-swappiness.conf"));
        assert!(!listed(&LISTS.recommended, "/etc/shells"));
        assert!(listed(&LISTS.never, "/etc/pacman.d/gnupg/pubring.gpg"));
    }

    #[test]
    fn hand_made_tmpfiles_become_lines_unless_they_mask_a_packaged_file_or_touch_archixs_paths() {
        let root = tempfile::tempdir().unwrap();
        let etc = root.path().join("etc/tmpfiles.d");
        fs::create_dir_all(&etc).unwrap();
        fs::create_dir_all(root.path().join("usr/lib/tmpfiles.d")).unwrap();
        fs::write(etc.join("foo.conf"), "# mine\nd /var/lib/foo 0750 root root -\n\n").unwrap();
        fs::write(etc.join("systemd.conf"), "").unwrap();
        fs::write(root.path().join("usr/lib/tmpfiles.d/systemd.conf"), "d /run 0755\n").unwrap();
        fs::write(etc.join("bad.conf"), "R /var/lib\n").unwrap();
        let file = |path: &str| FoundFile {
            path: path.into(),
            mode: 0o644,
            owner: "root".into(),
            group: "root".into(),
            recommended: true,
            edited: false,
            source: None,
            link: None,
        };
        let files = ["foo", "systemd", "bad"].map(|name| file(&format!("/etc/tmpfiles.d/{name}.conf")));
        let (lines, taken) = tmpfiles(root.path(), &files);
        assert_eq!(lines, ["d /var/lib/foo 0750 root root -"]);
        assert_eq!(taken, ["/etc/tmpfiles.d/foo.conf"]);
    }

    #[test]
    fn every_unowned_file_in_etc_is_offered_but_leftovers_and_secrets() {
        let root = tempfile::tempdir().unwrap();
        let write = |path: &str, text: &str| {
            let path = root.path().join(path.trim_start_matches('/'));
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        };
        for path in [
            "/etc/docker/daemon.json",
            "/etc/app/config.yml",
            "/etc/nginx/sites-available/site",
            "/etc/systemd/zram-generator.conf",
            "/etc/ld.so.conf.d/app.conf",
            "/etc/vconsole.conf",
            "/etc/owned.conf",
            "/etc/crontab.pacnew",
            "/etc/passwd-",
            "/etc/sshd_config.bak",
            "/etc/.pwd.lock",
            "/etc/wireguard/wg0.conf",
            "/etc/letsencrypt/live/site/privkey.pem",
            "/etc/letsencrypt/renewal/site.conf",
            "/etc/archix/init.lua",
            "/usr/lib/systemd/system/handmade.timer",
            "/usr/lib/systemd/system/handmade.service",
            "/usr/lib/systemd/system/packaged.service",
        ] {
            write(path, "x = 1\n");
        }
        fs::create_dir_all(root.path().join("etc/nginx/sites-enabled")).unwrap();
        symlink(
            "/etc/nginx/sites-available/site",
            root.path().join("etc/nginx/sites-enabled/site"),
        )
        .unwrap();
        fs::create_dir_all(root.path().join("etc/systemd/system/multi-user.target.wants")).unwrap();
        symlink(
            "/usr/lib/systemd/system/handmade.service",
            root.path()
                .join("etc/systemd/system/multi-user.target.wants/handmade.service"),
        )
        .unwrap();
        let installed = BTreeMap::from([(
            "pkg".to_string(),
            Installed {
                files: vec![
                    "etc/owned.conf".into(),
                    "usr/lib/systemd/system/packaged.service".into(),
                ],
                ..Default::default()
            },
        )]);
        let found = config_files(root.path(), &installed, &BTreeSet::new()).unwrap();
        let offered: Vec<(&str, Option<&str>, bool)> = found
            .files
            .iter()
            .map(|f| (f.path.as_str(), f.link.as_deref(), f.recommended))
            .collect();
        assert_eq!(
            offered,
            [
                ("/etc/app/config.yml", None, false),
                ("/etc/docker/daemon.json", None, true),
                ("/etc/ld.so.conf.d/app.conf", None, true),
                ("/etc/nginx/sites-available/site", None, true),
                (
                    "/etc/nginx/sites-enabled/site",
                    Some("/etc/nginx/sites-available/site"),
                    true
                ),
                ("/etc/systemd/zram-generator.conf", None, true),
                ("/etc/vconsole.conf", None, true),
            ]
        );
        assert_eq!(
            group_secrets(&found.secrets),
            ["/etc/letsencrypt/ (2 files)", "/etc/wireguard/ (1 file)"]
        );
        let units = unowned_units(root.path(), &installed);
        let units: Vec<(&str, Option<&str>)> = units
            .files
            .iter()
            .map(|f| (f.path.as_str(), f.source.as_deref()))
            .collect();
        assert_eq!(
            units,
            [
                (
                    "/etc/systemd/system/handmade.service",
                    Some("/usr/lib/systemd/system/handmade.service")
                ),
                (
                    "/etc/systemd/system/handmade.timer",
                    Some("/usr/lib/systemd/system/handmade.timer")
                ),
            ]
        );
    }

    #[test]
    fn content_marks_secrets_that_names_dont() {
        let home = tempfile::tempdir().unwrap();
        fs::create_dir_all(home.path().join(".config/someapp")).unwrap();
        fs::write(
            home.path().join(".config/someapp/settings.json"),
            "{\"theme\": \"dark\"}",
        )
        .unwrap();
        fs::create_dir_all(home.path().join(".config/sometool")).unwrap();
        fs::write(
            home.path().join(".config/sometool/auth.yml"),
            "token: ghp_abcdefghijklmnop\n",
        )
        .unwrap();
        fs::write(home.path().join(".deploy-key"), "-----BEGIN OPENSSH PRIVATE KEY-----\n").unwrap();
        let (offered, secrets) = dotfiles(home.path());
        let names: Vec<&str> = offered.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&".config/someapp"), "{names:?}");
        assert!(
            !names.contains(&".config/sometool") && !names.contains(&".deploy-key"),
            "{names:?}"
        );
        assert_eq!(secrets, 2);
    }

    #[test]
    fn dotfiles_are_recommended_skipped_or_kept_secret() {
        let home = tempfile::tempdir().unwrap();
        for dir in [
            ".config/nvim",
            ".config/gh",
            ".config/chromium",
            ".config/kwinrc.d",
            ".ssh",
            ".cache",
        ] {
            fs::create_dir_all(home.path().join(dir)).unwrap();
        }
        for file in [
            ".pulse-cookie",
            ".steampid",
            ".bashrc",
            ".bash_history",
            ".claude.json",
            ".claude.json.backup",
            ".config/kdeglobals",
            ".config/plasmashellrc",
        ] {
            fs::write(home.path().join(file), "").unwrap();
        }
        let (offered, secrets) = dotfiles(home.path());
        let replaced: Vec<&str> = offered.iter().filter(|d| d.replaced).map(|d| d.name.as_str()).collect();
        assert_eq!(
            replaced,
            [".config/kdeglobals", ".config/plasmashellrc"],
            "single rc files are replaced on save; directories and .bashrc aren't"
        );
        let names: Vec<(&str, bool)> = offered.iter().map(|d| (d.name.as_str(), d.recommended)).collect();
        assert_eq!(
            names,
            [
                (".bashrc", true),
                (".config/kdeglobals", false),
                (".config/kwinrc.d", false),
                (".config/nvim", true),
                (".config/plasmashellrc", false),
            ]
        );
        assert_eq!(
            secrets, 5,
            ".ssh, .config/gh, .claude.json, its backup and .pulse-cookie"
        );
    }
}
