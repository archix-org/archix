use std::collections::{BTreeMap, BTreeSet};
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

/// The system part of a switch: what the current generation applied and what the target applies.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SystemChange {
    pub from: SystemState,
    pub to: SystemState,
}

impl SystemChange {
    /// Paths whose managed entry differs between the two states, including ones only in one of them.
    pub(crate) fn changed_paths(&self) -> BTreeSet<&str> {
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

/// Where a managed path is on disk for one pass over the files, or None when the pass leaves it to another.
pub type Locate<'a> = &'a dyn Fn(&str) -> Option<PathBuf>;

/// The deployment tree a managed path belongs to, if any; everything else is written on the live system.
pub fn deployment_tree(path: &str) -> Option<&'static str> {
    ["usr", "etc", "opt", "boot"].into_iter().find(|tree| {
        path.strip_prefix('/')
            .and_then(|rest| rest.strip_prefix(tree))
            .is_some_and(|rest| rest.starts_with('/'))
    })
}

/// The users and groups in one root's account files, read directly instead of through NSS, so they can be a deployment's.
#[derive(Debug, Default)]
pub struct Accounts {
    users: BTreeMap<String, Account>,
    groups: BTreeMap<String, (u32, Vec<String>)>,
}

#[derive(Debug)]
struct Account {
    uid: u32,
    gid: u32,
    gecos: String,
    home: String,
    shell: String,
}

impl Accounts {
    /// The accounts in `etc`'s passwd and group.
    pub fn read(etc: &Path) -> Accounts {
        let read = |name: &str| fs::read_to_string(etc.join(name)).unwrap_or_default();
        let mut accounts = Accounts::default();
        for line in read("passwd").lines() {
            let fields: Vec<&str> = line.split(':').collect();
            if let [name, _, uid, gid, gecos, home, shell] = fields[..]
                && let (Ok(uid), Ok(gid)) = (uid.parse(), gid.parse())
            {
                let account = Account {
                    uid,
                    gid,
                    gecos: gecos.to_string(),
                    home: home.to_string(),
                    shell: shell.to_string(),
                };
                accounts.users.insert(name.to_string(), account);
            }
        }
        for line in read("group").lines() {
            let fields: Vec<&str> = line.split(':').collect();
            if let [name, _, gid, members] = fields[..]
                && let Ok(gid) = gid.parse()
            {
                let members = members
                    .split(',')
                    .filter(|m| !m.is_empty())
                    .map(str::to_string)
                    .collect();
                accounts.groups.insert(name.to_string(), (gid, members));
            }
        }
        accounts
    }

    pub fn uid(&self, name: &str) -> Option<u32> {
        self.users.get(name).map(|account| account.uid)
    }

    pub fn gid(&self, name: &str) -> Option<u32> {
        self.groups.get(name).map(|(gid, _)| *gid)
    }

    /// The home directory and primary group id of a user.
    pub fn home(&self, name: &str) -> Option<(PathBuf, u32)> {
        self.users
            .get(name)
            .map(|account| (PathBuf::from(&account.home), account.gid))
    }

    fn uid_taken(&self, id: u32) -> bool {
        self.users.values().any(|account| account.uid == id)
    }

    fn gid_taken(&self, id: u32) -> bool {
        self.groups.values().any(|(gid, _)| *gid == id)
    }

    /// Whether the existing account `name` differs from `want` in anything usermod would change.
    fn differs(&self, name: &str, want: &User) -> bool {
        let Some(account) = self.users.get(name) else {
            return true;
        };
        let group = self
            .groups
            .iter()
            .find(|(_, (gid, _))| *gid == account.gid)
            .map(|(name, _)| name.as_str())
            .unwrap_or_default();
        let mut groups: Vec<&str> = self
            .groups
            .iter()
            .filter(|(_, (_, members))| members.iter().any(|member| member == name))
            .map(|(group, _)| group.as_str())
            .collect();
        groups.sort();
        let mut wanted_groups: Vec<&str> = want.groups.iter().map(String::as_str).collect();
        wanted_groups.sort();
        wanted_groups.dedup();
        want.uid.is_some_and(|id| id != account.uid)
            || group != want.group.as_deref().unwrap_or(name)
            || want.home.as_ref().is_some_and(|dir| *dir != account.home)
            || account.shell != want.shell
            || account.gecos != want.description
            || groups != wanted_groups
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

fn matches(path: &Path, disk: &OnDisk, want: &ManagedFile, accounts: &Accounts) -> bool {
    if let Some(merge) = &want.merge {
        return matches!(disk, OnDisk::File { .. })
            && fs::read_to_string(path).is_ok_and(|text| merge.satisfied(&text));
    }
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
            *sha256 == want.sha256
                && (!keeps_modes(path)
                    || (*mode == want.mode
                        && Some(*u) == accounts.uid(&want.owner)
                        && Some(*g) == accounts.gid(&want.group)))
        }
        _ => false,
    }
}

/// FAT and exFAT (the ESP) report the mount's fmask and uid for every file, whatever archix set.
fn keeps_modes(path: &Path) -> bool {
    const MSDOS_SUPER_MAGIC: i64 = 0x4d44;
    const EXFAT_SUPER_MAGIC: i64 = 0x2011_bab0;
    let Ok(c_path) = CString::new(path.as_os_str().as_encoded_bytes()) else {
        return true;
    };
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    let found = unsafe { libc::statfs(c_path.as_ptr(), &mut stat) == 0 };
    !found || ![MSDOS_SUPER_MAGIC, EXFAT_SUPER_MAGIC].contains(&(stat.f_type as i64))
}

fn under(root: &Path, path: &str) -> PathBuf {
    root.join(path.trim_start_matches('/'))
}

/// Whether something is mounted on `path` itself, like the /etc/resolv.conf Docker and arch-chroot bind-mount; it can be written through but not replaced by a symlink.
fn mount_point(path: &Path) -> bool {
    let Ok(info) = fs::read_to_string("/proc/self/mountinfo") else {
        return false;
    };
    let wanted = path.to_string_lossy();
    info.lines()
        .filter_map(|line| line.split_whitespace().nth(4))
        .any(|mounted| mounted.replace("\\040", " ") == wanted)
}

/// Target files that are missing or differ on disk, so applying `change` rewrites them.
pub fn out_of_date(change: &SystemChange, locate: Locate, accounts: &Accounts) -> Result<BTreeSet<String>> {
    let mut paths = BTreeSet::new();
    for (path, want) in &change.to.files {
        let Some(target) = locate(path) else { continue };
        if !matches(&target, &on_disk(&target)?, want, accounts) {
            paths.insert(path.clone());
        }
    }
    Ok(paths)
}

/// Whether `unit` is enabled under `root`, the live system or a deployment directory, which holds etc/ and usr/ like a root.
pub fn is_enabled(root: &Path, unit: &str, user: bool) -> bool {
    let mut cmd = Command::new("systemctl");
    if root != Path::new("/") {
        cmd.arg(format!("--root={}", root.display()));
    }
    if user {
        cmd.arg("--global");
    }
    cmd.args(["is-enabled", unit])
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim() == "enabled")
}

/// One line per change applying `change` would make to what `locate` finds and the accounts and services of `root`, flagging managed files someone edited by hand.
pub fn describe(change: &SystemChange, locate: Locate, accounts: &Accounts, root: &Path) -> Result<Vec<String>> {
    let mut lines = Vec::new();
    for (path, want) in &change.to.files {
        let Some(target) = locate(path) else { continue };
        let disk = on_disk(&target)?;
        if matches(&target, &disk, want, accounts) {
            continue;
        }
        let edited = change.from.files.get(path).is_some_and(|prev| {
            prev.merge.is_none() && disk != OnDisk::Missing && !matches(&target, &disk, prev, accounts)
        });
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
        if accounts.uid(name).is_none() {
            lines.push(format!("add user   {name}"));
        } else if accounts.differs(name, user) {
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
        if accounts.gid(name).is_none() || group.gid.is_some_and(|id| Some(id) != accounts.gid(name)) {
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
        if is_enabled(root, unit, false) != *enabled {
            lines.push(format!("{:<10} {unit}", if *enabled { "enable" } else { "disable" }));
        }
    }
    for (unit, enabled) in &change.to.user_services {
        if is_enabled(root, unit, true) != *enabled {
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

/// Creates and updates the users and groups archix manages in a staging root through `run`, which runs a command in it, reading its accounts from `etc`; never touches passwords. Returns the regular users it created, whose homes `create_homes` makes once the deployment is committed.
pub fn users(
    change: &SystemChange,
    etc: &Path,
    run: &dyn Fn(&[String]) -> Result<()>,
    state: &Path,
) -> Result<Vec<String>> {
    let mut ids = IdMap::load(state);
    let accounts = Accounts::read(etc);
    let text = |items: &[&str]| -> Vec<String> { items.iter().map(|s| s.to_string()).collect() };
    for (name, group) in &change.to.groups {
        match accounts.gid(name) {
            None => {
                let mut cmd = text(&["groupadd"]);
                // A group created again gets its old gid back, so files it owned still belong to it.
                if let Some(id) = group.gid.or_else(|| ids.free_gid(name, &accounts)) {
                    cmd.extend(text(&["-g", &id.to_string()]));
                }
                if group.system {
                    cmd.push("-r".to_string());
                }
                cmd.push(name.clone());
                run(&cmd)?;
            }
            Some(current) if group.gid.is_some_and(|id| id != current) => {
                run(&text(&["groupmod", "-g", &group.gid.unwrap().to_string(), name]))?;
            }
            Some(_) => {}
        }
    }
    let accounts = Accounts::read(etc);
    let mut created = Vec::new();
    for (name, user) in &change.to.users {
        if accounts.uid(name).is_none() {
            let mut revived = user.clone();
            revived.uid = user.uid.or_else(|| ids.free_uid(name, &accounts));
            run(&useradd(name, &revived))?;
            if !user.system {
                created.push(name.clone());
            }
        } else if change.from.users.get(name) != Some(user) && accounts.differs(name, user) {
            run(&usermod(name, user))?;
        }
    }
    for name in change
        .from
        .users
        .keys()
        .filter(|name| !change.to.users.contains_key(*name))
    {
        if accounts.uid(name).is_some() {
            run(&text(&["userdel", name]))?;
        }
    }
    let accounts = Accounts::read(etc);
    for name in change
        .from
        .groups
        .keys()
        .filter(|name| !change.to.groups.contains_key(*name))
    {
        if accounts.gid(name).is_some() {
            run(&text(&["groupdel", name]))?;
        }
    }
    let accounts = Accounts::read(etc);
    for name in change.to.groups.keys() {
        ids.record_gid(name, accounts.gid(name));
    }
    for name in change.to.users.keys() {
        ids.record_uid(name, accounts.uid(name));
    }
    ids.save(state)?;
    Ok(created)
}

/// Makes the homes of users a committed deployment created, from its /etc/skel, owned by them.
pub fn create_homes(created: &[String], accounts: &Accounts, skel: &Path, root: &Path) -> Result<()> {
    for name in created {
        let Some((home, gid)) = accounts.home(name) else {
            continue;
        };
        let Some(uid) = accounts.uid(name) else { continue };
        let home = under(root, &home.to_string_lossy());
        if home.symlink_metadata().is_ok() {
            continue;
        }
        if let Some(parent) = home.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        if skel.is_dir() {
            copy_preserving(skel, &home)?;
        } else {
            fs::create_dir_all(&home)?;
        }
        fs::set_permissions(&home, fs::Permissions::from_mode(0o700))?;
        chown_tree(&home, uid, gid)?;
        eprintln!("created user {name}; set a password with `passwd {name}` after the reboot");
    }
    Ok(())
}

fn chown_tree(path: &Path, uid: u32, gid: u32) -> Result<()> {
    std::os::unix::fs::lchown(path, Some(uid), Some(gid))?;
    if fs::symlink_metadata(path)?.is_dir() {
        for entry in fs::read_dir(path)? {
            chown_tree(&entry?.path(), uid, gid)?;
        }
    }
    Ok(())
}

/// Every uid and gid archix has given a name, kept after the account is removed, like NixOS's uid-map.
#[derive(Default, Serialize, Deserialize)]
struct IdMap {
    uids: std::collections::BTreeMap<String, u32>,
    gids: std::collections::BTreeMap<String, u32>,
}

impl IdMap {
    fn path(state: &Path) -> PathBuf {
        state.join("id-map.json")
    }

    fn load(state: &Path) -> IdMap {
        fs::read_to_string(Self::path(state))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    fn save(&self, state: &Path) -> Result<()> {
        crate::durable::write(&Self::path(state), serde_json::to_string_pretty(self)?.as_bytes())
    }

    fn free_uid(&self, name: &str, accounts: &Accounts) -> Option<u32> {
        self.uids.get(name).copied().filter(|id| !accounts.uid_taken(*id))
    }

    fn free_gid(&self, name: &str, accounts: &Accounts) -> Option<u32> {
        self.gids.get(name).copied().filter(|id| !accounts.gid_taken(*id))
    }

    fn record_uid(&mut self, name: &str, id: Option<u32>) {
        if let Some(id) = id {
            self.uids.insert(name.to_string(), id);
        }
    }

    fn record_gid(&mut self, name: &str, id: Option<u32>) {
        if let Some(id) = id {
            self.gids.insert(name.to_string(), id);
        }
    }
}

fn useradd(name: &str, user: &User) -> Vec<String> {
    let mut cmd = vec!["useradd".to_string()];
    // The home is made on the live system once the deployment is committed, as the staging root's /home isn't it.
    cmd.push(if user.system { "-r" } else { "-M" }.to_string());
    if let Some(id) = user.uid {
        cmd.extend(["-u".to_string(), id.to_string()]);
    }
    match &user.group {
        Some(group) => cmd.extend(["-g".to_string(), group.clone()]),
        None => cmd.push("-U".to_string()),
    }
    if !user.groups.is_empty() {
        cmd.extend(["-G".to_string(), user.groups.join(",")]);
    }
    if let Some(home) = &user.home {
        cmd.extend(["-d".to_string(), home.clone()]);
    }
    cmd.extend([
        "-s".to_string(),
        user.shell.clone(),
        "-c".to_string(),
        user.description.clone(),
        name.to_string(),
    ]);
    cmd
}

fn usermod(name: &str, user: &User) -> Vec<String> {
    let mut cmd = vec!["usermod".to_string()];
    if let Some(id) = user.uid {
        cmd.extend(["-u".to_string(), id.to_string()]);
    }
    if let Some(group) = &user.group {
        cmd.extend(["-g".to_string(), group.clone()]);
    }
    if let Some(home) = &user.home {
        cmd.extend(["-d".to_string(), home.clone()]);
    }
    cmd.extend([
        "-G".to_string(),
        user.groups.join(","),
        "-s".to_string(),
        user.shell.clone(),
        "-c".to_string(),
        user.description.clone(),
        name.to_string(),
    ]);
    cmd
}

fn systemctl(args: &[&str], run: &dyn Fn(&[String]) -> Result<()>, failures: &mut Vec<String>) {
    let argv: Vec<String> = std::iter::once("systemctl")
        .chain(args.iter().copied())
        .map(str::to_string)
        .collect();
    if let Err(e) = run(&argv) {
        failures.push(format!("{e:#}"));
    }
}

/// Disables the units turned off or leaving the config in a staging root through `run`, while their unit files are still there; failures are returned.
pub fn disable_services(change: &SystemChange, run: &dyn Fn(&[String]) -> Result<()>) -> Vec<String> {
    let mut failures = Vec::new();
    for (unit, enabled) in &change.to.services {
        if !*enabled {
            systemctl(&["disable", unit], run, &mut failures);
        }
    }
    for (unit, enabled) in &change.from.services {
        if !change.to.services.contains_key(unit) && *enabled {
            systemctl(&["disable", unit], run, &mut failures);
        }
    }
    for (unit, enabled) in &change.to.user_services {
        if !*enabled {
            systemctl(&["--global", "disable", unit], run, &mut failures);
        }
    }
    for (unit, enabled) in &change.from.user_services {
        if !change.to.user_services.contains_key(unit) && *enabled {
            systemctl(&["--global", "disable", unit], run, &mut failures);
        }
    }
    failures
}

/// Enables units in a staging root through `run`, once the managed unit files are written; a unit that can't be is a soft failure, returned.
pub fn enable_services(change: &SystemChange, run: &dyn Fn(&[String]) -> Result<()>) -> Vec<String> {
    let mut failures = Vec::new();
    for (unit, enabled) in &change.to.services {
        if *enabled {
            systemctl(&["enable", unit], run, &mut failures);
        }
    }
    for (unit, enabled) in &change.to.user_services {
        if *enabled {
            systemctl(&["--global", "enable", unit], run, &mut failures);
        }
    }
    failures
}

/// Refuses, before anything is staged, managed files that would replace something archix can't safely back up.
pub fn check(paths: &Paths, change: &SystemChange, locate: Locate) -> Result<()> {
    let backups = paths.state.join("backups");
    for (path, want) in &change.to.files {
        let Some(target) = locate(path) else { continue };
        if let Some(merge) = &want.merge
            && let Ok(text) = fs::read_to_string(&target)
        {
            merge
                .apply(&text)
                .with_context(|| format!("{path} can't take its merged keys; nothing was changed"))?;
        }
    }
    for path in change.to.files.keys() {
        let Some(target) = locate(path) else { continue };
        if on_disk(&target)? != OnDisk::Other {
            continue;
        }
        let directory = fs::symlink_metadata(&target).is_ok_and(|meta| meta.is_dir());
        if !directory || change.from.files.contains_key(path) {
            bail!("{path} exists and is not a file or symlink; archix won't replace it");
        }
        if fs::symlink_metadata(under(&backups, path)).is_ok() {
            bail!("{path} is a directory and an earlier backup of it exists; move one of them away");
        }
    }
    Ok(())
}

/// Renames, or copies and deletes when `from` and `to` are on different filesystems (a separate /home, a btrfs subvolume).
fn move_path(from: &Path, to: &Path) -> Result<()> {
    match fs::rename(from, to) {
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
            copy_preserving(from, to)?;
            if fs::symlink_metadata(from)?.is_dir() {
                fs::remove_dir_all(from)?;
            } else {
                fs::remove_file(from)?;
            }
            Ok(())
        }
        Err(e) if e.raw_os_error() == Some(libc::EBUSY) && !fs::symlink_metadata(from)?.is_dir() => {
            // A bind-mounted file (Docker's /etc/hosts, arch-chroot's resolv.conf) can't be renamed, only copied.
            fs::copy(from, to)?;
            match fs::remove_file(from) {
                Err(e) if e.raw_os_error() == Some(libc::EBUSY) => Ok(()),
                result => Ok(result?),
            }
        }
        result => Ok(result?),
    }
}

/// A copy of a file, symlink or directory tree that keeps modes and owners.
fn copy_preserving(from: &Path, to: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(from)?;
    if meta.file_type().is_symlink() {
        symlink(fs::read_link(from)?, to)?;
    } else if meta.is_dir() {
        fs::create_dir(to)?;
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            copy_preserving(&entry.path(), &to.join(entry.file_name()))?;
        }
        fs::set_permissions(to, meta.permissions())?;
    } else {
        fs::copy(from, to)?;
    }
    std::os::unix::fs::lchown(to, Some(meta.uid()), Some(meta.gid()))?;
    Ok(())
}

/// Writes, replaces and unmanages the files `locate` places, backing up originals and hand edits instead of losing them; owners come from `accounts`.
pub fn files(paths: &Paths, change: &SystemChange, locate: Locate, accounts: &Accounts) -> Result<()> {
    let backups = paths.state.join("backups");
    let edited = paths.state.join("edited").join(now().to_string());
    for (path, want) in &change.to.files {
        let Some(target) = locate(path) else { continue };
        let target = target.as_path();
        let disk = on_disk(target)?;
        if want.link.is_some() && mount_point(target) && !matches(target, &disk, want, accounts) {
            eprintln!(
                "warning: {path} is mounted over here (a container or arch-chroot does that), so it's left alone; the next switch outside replaces it"
            );
            continue;
        }
        if let Some(merge) = &want.merge {
            if !matches(target, &disk, want, accounts) {
                write_merged(target, want, merge, accounts).with_context(|| format!("setting keys in {path}"))?;
            }
            continue;
        }
        if matches(target, &disk, want, accounts) {
            if disk != OnDisk::Missing && !change.from.files.contains_key(path) {
                keep_original(target, &under(&backups, path)).with_context(|| format!("backing up {path}"))?;
            }
            continue;
        }
        if disk != OnDisk::Missing {
            match change.from.files.get(path) {
                None => {
                    let backup = under(&backups, path);
                    if fs::symlink_metadata(&backup).is_err() {
                        fs::create_dir_all(backup.parent().expect("absolute path"))?;
                        move_path(target, &backup).with_context(|| format!("backing up {path}"))?;
                    }
                }
                Some(previous) if !matches(target, &disk, previous, accounts) => {
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
        write(paths, target, want, accounts).with_context(|| format!("writing {path}"))?;
    }
    for (path, previous) in change
        .from
        .files
        .iter()
        .filter(|(path, previous)| !change.to.files.contains_key(*path) && previous.merge.is_none())
    {
        let Some(target) = locate(path) else { continue };
        let target = target.as_path();
        let backup = under(&backups, path);
        let disk = on_disk(target)?;
        if fs::symlink_metadata(&backup).is_ok() {
            if disk != OnDisk::Missing && !matches(target, &disk, previous, accounts) {
                eprintln!(
                    "warning: {path} was edited outside archix; restoring the original anyway, see {}",
                    backup.display()
                );
            }
            if fs::symlink_metadata(&backup).is_ok_and(|meta| meta.is_dir())
                && matches!(disk, OnDisk::Link(_) | OnDisk::File { .. })
            {
                fs::remove_file(target).with_context(|| format!("removing {path}"))?;
            }
            move_path(&backup, target).with_context(|| format!("restoring {path}"))?;
        } else if matches(target, &disk, previous, accounts) {
            fs::remove_file(target).with_context(|| format!("removing {path}"))?;
        }
    }
    Ok(())
}

/// Backs up every file the target manages that's already on disk exactly as wanted, for a switch that writes no files.
pub fn keep_originals(paths: &Paths, change: &SystemChange, locate: Locate, accounts: &Accounts) -> Result<()> {
    let backups = paths.state.join("backups");
    for (path, want) in &change.to.files {
        let Some(target) = locate(path) else { continue };
        let disk = on_disk(&target)?;
        if want.merge.is_none()
            && disk != OnDisk::Missing
            && matches(&target, &disk, want, accounts)
            && !change.from.files.contains_key(path)
        {
            keep_original(&target, &under(&backups, path)).with_context(|| format!("backing up {path}"))?;
        }
    }
    Ok(())
}

/// Copies a file archix takes over as it is, so unmanaging it later puts it back instead of deleting it.
fn keep_original(target: &Path, backup: &Path) -> Result<()> {
    if fs::symlink_metadata(backup).is_ok() {
        return Ok(());
    }
    fs::create_dir_all(backup.parent().expect("absolute path"))?;
    let meta = fs::symlink_metadata(target)?;
    if meta.file_type().is_symlink() {
        symlink(fs::read_link(target)?, backup)?;
    } else {
        fs::copy(target, backup)?;
    }
    std::os::unix::fs::lchown(backup, Some(meta.uid()), Some(meta.gid()))?;
    Ok(())
}

fn owner(want: &ManagedFile, accounts: &Accounts) -> Result<(u32, u32)> {
    match (accounts.uid(&want.owner), accounts.gid(&want.group)) {
        (Some(u), Some(g)) => Ok((u, g)),
        _ => bail!("owner {}:{} doesn't exist", want.owner, want.group),
    }
}

/// Sets a merged file's keys in place, keeping the app's other keys, mode and owner; a new file gets the managed mode and owner.
fn write_merged(target: &Path, want: &ManagedFile, merge: &super::merge::Merge, accounts: &Accounts) -> Result<()> {
    let existing = fs::metadata(target).ok();
    let text = match &existing {
        Some(_) => fs::read_to_string(target)?,
        None => String::new(),
    };
    let merged = merge.apply(&text)?;
    let dir = target.parent().context("file has no parent directory")?;
    create_parents(dir, want, accounts)?;
    let name = target.file_name().context("file has no name")?.to_string_lossy();
    let tmp = dir.join(format!(".{name}.archix.{}", std::process::id()));
    fs::write(&tmp, merged)?;
    let (mode, owner) = match &existing {
        Some(meta) => (meta.mode() & 0o7777, (meta.uid(), meta.gid())),
        None => match owner(want, accounts) {
            Ok(owner) => (want.mode, owner),
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                return Err(e);
            }
        },
    };
    fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
    std::os::unix::fs::chown(&tmp, Some(owner.0), Some(owner.1))?;
    fs::rename(&tmp, target).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })?;
    Ok(())
}

/// Creates missing parent directories; ones archix creates for a user's file belong to that user.
fn create_parents(dir: &Path, want: &ManagedFile, accounts: &Accounts) -> Result<()> {
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
        let (u, g) = owner(want, accounts)?;
        for path in missing {
            std::os::unix::fs::chown(&path, Some(u), Some(g))?;
        }
    }
    Ok(())
}

fn write(paths: &Paths, target: &Path, want: &ManagedFile, accounts: &Accounts) -> Result<()> {
    let dir = target.parent().context("file has no parent directory")?;
    create_parents(dir, want, accounts)?;
    let name = target.file_name().context("file has no name")?.to_string_lossy();
    let tmp = dir.join(format!(".{name}.archix.{}", std::process::id()));
    let _ = fs::remove_file(&tmp);
    match &want.link {
        Some(link) => {
            symlink(link, &tmp)?;
            if want.owner != "root"
                && let Ok((u, g)) = owner(want, accounts)
            {
                std::os::unix::fs::lchown(&tmp, Some(u), Some(g))?;
            }
        }
        None => {
            let blob = blobs::path(paths, &want.sha256);
            fs::copy(&blob, &tmp).with_context(|| format!("copying {}", blob.display()))?;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(want.mode))?;
            let (u, g) = match owner(want, accounts) {
                Ok(owner) => owner,
                Err(e) => {
                    let _ = fs::remove_file(&tmp);
                    return Err(e);
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_paths_split_into_deployment_trees_and_the_live_system() {
        assert_eq!(deployment_tree("/etc/x"), Some("etc"));
        assert_eq!(deployment_tree("/usr/local/bin/x"), Some("usr"));
        assert_eq!(deployment_tree("/opt/x/y"), Some("opt"));
        assert_eq!(deployment_tree("/etcetera/x"), None);
        assert_eq!(deployment_tree("/home/u/.x"), None);
        assert_eq!(deployment_tree("/efi/loader/entries/m.conf"), None);
        assert_eq!(deployment_tree("/var/lib/x"), None);
    }

    #[test]
    fn accounts_come_from_the_roots_own_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("passwd"),
            "root:x:0:0::/root:/bin/bash\nalice:x:1000:1000:Alice:/home/alice:/bin/zsh\nbroken line\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("group"),
            "root:x:0:\nalice:x:1000:\nwheel:x:998:alice\nvideo:x:985:bob,alice\n",
        )
        .unwrap();
        let accounts = Accounts::read(dir.path());
        assert_eq!((accounts.uid("alice"), accounts.gid("wheel")), (Some(1000), Some(998)));
        assert_eq!(accounts.uid("bob"), None);
        assert!(accounts.uid_taken(1000) && !accounts.uid_taken(1001));
        let alice = User {
            uid: None,
            group: None,
            groups: vec!["video".into(), "wheel".into()],
            shell: "/bin/zsh".into(),
            home: None,
            description: "Alice".into(),
            system: false,
        };
        assert!(!accounts.differs("alice", &alice));
        let more = User {
            groups: vec!["wheel".into()],
            ..alice.clone()
        };
        assert!(accounts.differs("alice", &more));
        assert!(accounts.differs("carol", &alice));
    }
}
