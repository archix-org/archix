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

    /// The ids of the groups `name` belongs to, its primary group included.
    pub fn groups_of(&self, name: &str) -> Vec<u32> {
        let mut ids: Vec<u32> = self
            .groups
            .values()
            .filter(|(_, members)| members.iter().any(|member| member == name))
            .map(|(gid, _)| *gid)
            .collect();
        ids.extend(self.users.get(name).map(|account| account.gid));
        ids
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
    for (path, want) in &change.to.files {
        let Some(target) = locate(path) else { continue };
        if on_disk(&target)? != OnDisk::Other {
            continue;
        }
        let directory = fs::symlink_metadata(&target).is_ok_and(|meta| meta.is_dir());
        if !directory || change.from.files.contains_key(path) {
            bail!("{path} exists and is not a file or symlink; archix won't replace it");
        }
        let users = paths.state.join("users").join(&want.owner).join("backups");
        if fs::symlink_metadata(under(&backups, path)).is_ok() || fs::symlink_metadata(under(&users, path)).is_ok() {
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
    keep_owner(to, &meta)
}

fn is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

/// Gives `path` the owner in `meta`; run as a home's user, what archix makes is already theirs.
fn keep_owner(path: &Path, meta: &fs::Metadata) -> Result<()> {
    if is_root() {
        std::os::unix::fs::lchown(path, Some(meta.uid()), Some(meta.gid()))?;
    }
    Ok(())
}

/// The user a home file is written as, so links in that home can't lead archix anywhere its user couldn't write.
struct Writer {
    name: String,
    uid: u32,
    gid: u32,
    groups: Vec<u32>,
    home: PathBuf,
}

/// The owner of `file` at `target` when it's in that owner's own home, or None for a file root writes.
fn writer(paths: &Paths, target: &Path, file: &ManagedFile, accounts: &Accounts) -> Result<Option<Writer>> {
    let Some((home, _)) = accounts.home(&file.owner) else {
        return Ok(None);
    };
    let home = under(&paths.root, &home.to_string_lossy());
    if file.owner == "root" || home == paths.root || !target.starts_with(&home) {
        return Ok(None);
    }
    let (uid, gid) = owner(file, accounts)?;
    let mut groups = accounts.groups_of(&file.owner);
    groups.push(gid);
    groups.sort_unstable();
    groups.dedup();
    Ok(Some(Writer {
        name: file.owner.clone(),
        uid,
        gid,
        groups,
        home,
    }))
}

/// Where originals and hand-edited copies of managed files are kept.
struct Saves {
    backups: PathBuf,
    edited: PathBuf,
}

/// archix's state for root's files; for a home's files, a directory there that belongs to its user, who writes them.
fn saves(paths: &Paths, stamp: &str, writer: Option<&Writer>) -> Result<Saves> {
    let base = match writer {
        None => paths.state.clone(),
        Some(writer) => {
            let users = paths.state.join("users");
            fs::create_dir_all(&users)?;
            let base = users.join(&writer.name);
            match fs::symlink_metadata(&base) {
                Ok(meta) if meta.is_dir() => {}
                Ok(_) => bail!("{} isn't a directory", base.display()),
                Err(_) => fs::create_dir(&base).with_context(|| format!("creating {}", base.display()))?,
            }
            std::os::unix::fs::lchown(&base, Some(writer.uid), Some(writer.gid))?;
            fs::set_permissions(&base, fs::Permissions::from_mode(0o700))?;
            base
        }
    };
    Ok(Saves {
        backups: base.join("backups"),
        edited: base.join("edited").join(stamp),
    })
}

/// Runs `work` in a child with `writer`'s uid, gid and groups and returns its error; archix runs no other threads here, so the child may allocate.
fn run_as(writer: &Writer, work: impl FnOnce() -> Result<()>) -> Result<()> {
    use std::io::{Read, Write};
    use std::os::fd::FromRawFd;
    if unsafe { libc::geteuid() } == writer.uid {
        return work();
    }
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    let mut fds = [0; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(std::io::Error::last_os_error()).context("making a pipe");
    }
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        let e = std::io::Error::last_os_error();
        unsafe {
            libc::close(fds[0]);
            libc::close(fds[1]);
        }
        return Err(e).context("forking");
    }
    if pid == 0 {
        unsafe { libc::close(fds[0]) };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            become_user(writer).and_then(|()| work())
        }));
        let failed = match result {
            Ok(Ok(())) => None,
            Ok(Err(e)) => Some(format!("{e:#}")),
            Err(_) => Some("panicked".to_string()),
        };
        if let Some(message) = &failed {
            let mut pipe = unsafe { fs::File::from_raw_fd(fds[1]) };
            let _ = pipe.write_all(message.as_bytes());
        }
        unsafe { libc::_exit(i32::from(failed.is_some())) };
    }
    unsafe { libc::close(fds[1]) };
    let mut message = String::new();
    let _ = unsafe { fs::File::from_raw_fd(fds[0]) }.read_to_string(&mut message);
    let mut status = 0;
    while unsafe { libc::waitpid(pid, &mut status, 0) } < 0 {
        if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            break;
        }
    }
    if libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0 {
        return Ok(());
    }
    if message.is_empty() {
        bail!("writing as {} failed (wait status {status})", writer.name);
    }
    Err(anyhow::anyhow!(message))
}

fn become_user(writer: &Writer) -> Result<()> {
    let failed =
        |call: &str| Err(std::io::Error::last_os_error()).with_context(|| format!("{call} for {}", writer.name));
    unsafe {
        if libc::setgroups(writer.groups.len(), writer.groups.as_ptr()) != 0 {
            return failed("setgroups");
        }
        if libc::setresgid(writer.gid, writer.gid, writer.gid) != 0 {
            return failed("setresgid");
        }
        if libc::setresuid(writer.uid, writer.uid, writer.uid) != 0 {
            return failed("setresuid");
        }
    }
    Ok(())
}

/// Runs `work` on the managed `file` at `target`, as its user when it's in their home, with where its originals go; a user's content is read from archix's store beforehand.
fn as_owner(
    paths: &Paths,
    stamp: &str,
    target: &Path,
    file: &ManagedFile,
    accounts: &Accounts,
    work: impl FnOnce(&Saves, Option<&[u8]>) -> Result<()>,
) -> Result<()> {
    let writer = writer(paths, target, file, accounts)?;
    let saves = saves(paths, stamp, writer.as_ref())?;
    let Some(writer) = writer else {
        return work(&saves, None);
    };
    if fs::symlink_metadata(&writer.home).is_err() {
        fs::create_dir_all(&writer.home).with_context(|| format!("creating {}", writer.home.display()))?;
        std::os::unix::fs::lchown(&writer.home, Some(writer.uid), Some(writer.gid))?;
        fs::set_permissions(&writer.home, fs::Permissions::from_mode(0o700))?;
    }
    let content = match file.link.is_none() && file.merge.is_none() && !file.sha256.is_empty() {
        true => {
            let blob = blobs::path(paths, &file.sha256);
            Some(fs::read(&blob).with_context(|| format!("reading {}", blob.display()))?)
        }
        false => None,
    };
    run_as(&writer, || work(&saves, content.as_deref()))
}

/// Writes, replaces and unmanages the files `locate` places, backing up originals and hand edits instead of losing them; owners come from `accounts`, and a home's files are written as its user.
pub fn files(paths: &Paths, change: &SystemChange, locate: Locate, accounts: &Accounts) -> Result<()> {
    let stamp = now().to_string();
    let mut failures = Vec::new();
    for (path, want) in &change.to.files {
        let Some(target) = locate(path) else { continue };
        let previous = change.from.files.get(path);
        let result = as_owner(paths, &stamp, &target, want, accounts, |saves, content| {
            apply_file(paths, saves, path, &target, want, previous, accounts, content)
        });
        if let Err(e) = result {
            failures.push(format!("{e:#}"));
        }
    }
    for (path, previous) in change
        .from
        .files
        .iter()
        .filter(|(path, previous)| !change.to.files.contains_key(*path) && previous.merge.is_none())
    {
        let Some(target) = locate(path) else { continue };
        if previous.owner != "root" && accounts.uid(&previous.owner).is_none() {
            continue;
        }
        let root_backup = under(&paths.state.join("backups"), path);
        let as_user = matches!(writer(paths, &target, previous, accounts), Ok(Some(_)));
        let legacy = (as_user && fs::symlink_metadata(&root_backup).is_ok()).then_some(root_backup.as_path());
        let result = as_owner(paths, &stamp, &target, previous, accounts, |saves, _| {
            unmanage(saves, legacy, path, &target, previous, accounts)
        });
        match result {
            Ok(()) if legacy.is_some() => {
                let removed = match fs::symlink_metadata(&root_backup).is_ok_and(|meta| meta.is_dir()) {
                    true => fs::remove_dir_all(&root_backup),
                    false => fs::remove_file(&root_backup),
                };
                if let Err(e) = removed {
                    failures.push(format!("removing the restored {}: {e}", root_backup.display()));
                }
            }
            Ok(()) => {}
            Err(e) => failures.push(format!("{e:#}")),
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        bail!("{}", failures.join("\n"))
    }
}

/// Brings one managed file to what `want` says, keeping an original or a hand-edited copy in `saves`.
#[allow(clippy::too_many_arguments)]
fn apply_file(
    paths: &Paths,
    saves: &Saves,
    path: &str,
    target: &Path,
    want: &ManagedFile,
    previous: Option<&ManagedFile>,
    accounts: &Accounts,
    content: Option<&[u8]>,
) -> Result<()> {
    let disk = on_disk(target)?;
    if want.link.is_some() && mount_point(target) && !matches(target, &disk, want, accounts) {
        eprintln!(
            "warning: {path} is mounted over here (a container or arch-chroot does that), so it's left alone; the next switch outside replaces it"
        );
        return Ok(());
    }
    if let Some(merge) = &want.merge {
        if !matches(target, &disk, want, accounts) {
            write_merged(target, want, merge, accounts).with_context(|| format!("setting keys in {path}"))?;
        }
        return Ok(());
    }
    if matches(target, &disk, want, accounts) {
        if disk != OnDisk::Missing && previous.is_none() {
            keep_original(target, &under(&saves.backups, path)).with_context(|| format!("backing up {path}"))?;
        }
        return Ok(());
    }
    if disk != OnDisk::Missing {
        match previous {
            None => {
                let backup = under(&saves.backups, path);
                if fs::symlink_metadata(&backup).is_err() {
                    fs::create_dir_all(backup.parent().expect("absolute path"))?;
                    move_path(target, &backup).with_context(|| format!("backing up {path}"))?;
                }
            }
            Some(previous) if !matches(target, &disk, previous, accounts) => {
                let copy = under(&saves.edited, path);
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
    write(paths, target, want, accounts, content).with_context(|| format!("writing {path}"))
}

/// Puts back the original of a file archix stops managing, from `saves` or an older backup in `legacy`, or removes the file archix wrote.
fn unmanage(
    saves: &Saves,
    legacy: Option<&Path>,
    path: &str,
    target: &Path,
    previous: &ManagedFile,
    accounts: &Accounts,
) -> Result<()> {
    let backup = under(&saves.backups, path);
    let disk = on_disk(target)?;
    let source = match fs::symlink_metadata(&backup) {
        Ok(_) => Some(backup.as_path()),
        Err(_) => legacy,
    };
    let Some(source) = source else {
        if matches(target, &disk, previous, accounts) {
            fs::remove_file(target).with_context(|| format!("removing {path}"))?;
        }
        return Ok(());
    };
    if disk != OnDisk::Missing && !matches(target, &disk, previous, accounts) {
        eprintln!(
            "warning: {path} was edited outside archix; restoring the original anyway, see {}",
            source.display()
        );
    }
    if matches!(disk, OnDisk::Link(_) | OnDisk::File { .. }) {
        fs::remove_file(target).with_context(|| format!("removing {path}"))?;
    }
    match source == backup {
        true => move_path(source, target),
        false => copy_preserving(source, target),
    }
    .with_context(|| format!("restoring {path}"))
}

/// Backs up every file the target manages that's already on disk exactly as wanted, for a switch that writes no files.
pub fn keep_originals(paths: &Paths, change: &SystemChange, locate: Locate, accounts: &Accounts) -> Result<()> {
    let stamp = now().to_string();
    for (path, want) in &change.to.files {
        let Some(target) = locate(path) else { continue };
        if want.merge.is_some() || change.from.files.contains_key(path) {
            continue;
        }
        as_owner(paths, &stamp, &target, want, accounts, |saves, _| {
            let disk = on_disk(&target)?;
            if disk != OnDisk::Missing && matches(&target, &disk, want, accounts) {
                keep_original(&target, &under(&saves.backups, path)).with_context(|| format!("backing up {path}"))?;
            }
            Ok(())
        })?;
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
    keep_owner(backup, &meta)
}

fn owner(want: &ManagedFile, accounts: &Accounts) -> Result<(u32, u32)> {
    match (accounts.uid(&want.owner), accounts.gid(&want.group)) {
        (Some(u), Some(g)) => Ok((u, g)),
        _ => bail!("owner {}:{} doesn't exist", want.owner, want.group),
    }
}

/// A fresh name beside `target` for a file being written, which nobody can guess to plant a link at.
fn temp_beside(target: &Path) -> Result<PathBuf> {
    let dir = target.parent().context("file has no parent directory")?;
    let name = target.file_name().context("file has no name")?.to_string_lossy();
    let mut bytes = [0u8; 8];
    if unsafe { libc::getrandom(bytes.as_mut_ptr().cast(), bytes.len(), 0) } != bytes.len() as isize {
        return Err(std::io::Error::last_os_error()).context("getrandom");
    }
    let suffix: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    Ok(dir.join(format!(".{name}.archix.{suffix}")))
}

/// A new file at an unguessable name beside `target`, created exclusively, so never through a link.
fn create_temp(target: &Path) -> Result<(PathBuf, fs::File)> {
    use std::os::unix::fs::OpenOptionsExt;
    loop {
        let tmp = temp_beside(target)?;
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&tmp)
        {
            Ok(file) => return Ok((tmp, file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).with_context(|| format!("creating {}", tmp.display())),
        }
    }
}

/// Fills a new temporary file and gives it its owner, then its mode, as chown drops setuid bits; the file is removed if that fails.
fn finish_temp(
    tmp: &Path,
    file: &mut fs::File,
    fill: impl FnOnce(&mut fs::File) -> Result<()>,
    owner: Option<(u32, u32)>,
    mode: u32,
) -> Result<()> {
    let result = (|| {
        fill(file)?;
        if let Some((u, g)) = owner
            && is_root()
        {
            std::os::unix::fs::fchown(&*file, Some(u), Some(g))?;
        }
        file.set_permissions(fs::Permissions::from_mode(mode))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}

/// What identifies one version of a file: a rename-over gives a new inode, an in-place write a new size or mtime; not ctime, which a rename changes.
#[derive(PartialEq)]
struct Version(u64, u64, u64, i64, i64);

impl Version {
    fn of(meta: &fs::Metadata) -> Version {
        Version(meta.dev(), meta.ino(), meta.size(), meta.mtime(), meta.mtime_nsec())
    }
}

fn read_version(path: &Path) -> Result<Option<(String, Version)>> {
    use std::io::Read;
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let version = Version::of(&file.metadata()?);
    let mut text = String::new();
    file.read_to_string(&mut text)
        .with_context(|| format!("reading {}", path.display()))?;
    Ok(Some((text, version)))
}

fn renameat2(from: &Path, to: &Path, flags: libc::c_uint) -> std::io::Result<()> {
    let (from, to) = (
        CString::new(from.as_os_str().as_encoded_bytes())?,
        CString::new(to.as_os_str().as_encoded_bytes())?,
    );
    match unsafe { libc::renameat2(libc::AT_FDCWD, from.as_ptr(), libc::AT_FDCWD, to.as_ptr(), flags) } {
        0 => Ok(()),
        _ => Err(std::io::Error::last_os_error()),
    }
}

/// Puts `tmp` at `target` only if `target` is still the version read, None for no file; otherwise returns what's there now to merge again, and `tmp` is gone either way.
fn replace_if_unchanged(
    tmp: &Path,
    target: &Path,
    read: Option<&Version>,
    ours: Version,
) -> Result<Option<(String, Option<Version>)>> {
    let again = || -> Result<Option<(String, Option<Version>)>> {
        let _ = fs::remove_file(tmp);
        Ok(Some(match read_version(target)? {
            Some((text, version)) => (text, Some(version)),
            None => (String::new(), None),
        }))
    };
    let Some(read) = read else {
        return match renameat2(tmp, target, libc::RENAME_NOREPLACE) {
            Ok(()) => Ok(None),
            Err(e) if e.raw_os_error() == Some(libc::EEXIST) => again(),
            Err(e) if e.raw_os_error() == Some(libc::EINVAL) && fs::symlink_metadata(target).is_err() => {
                fs::rename(tmp, target)?;
                Ok(None)
            }
            Err(e) if e.raw_os_error() == Some(libc::EINVAL) => again(),
            Err(e) => {
                let _ = fs::remove_file(tmp);
                Err(e.into())
            }
        };
    };
    match renameat2(tmp, target, libc::RENAME_EXCHANGE) {
        Ok(()) => {
            let displaced = read_version(tmp)?;
            let _ = fs::remove_file(tmp);
            match displaced {
                Some((_, version)) if version == *read => Ok(None),
                Some((text, _)) => Ok(Some((text, Some(ours)))),
                None => Ok(Some((String::new(), Some(ours)))),
            }
        }
        Err(e) if e.raw_os_error() == Some(libc::ENOENT) => {
            let _ = fs::remove_file(tmp);
            Ok(Some((String::new(), None)))
        }
        Err(e) if e.raw_os_error() == Some(libc::EINVAL) => match fs::metadata(target).map(|meta| Version::of(&meta)) {
            Ok(now) if now == *read => {
                fs::rename(tmp, target)?;
                Ok(None)
            }
            _ => again(),
        },
        Err(e) => {
            let _ = fs::remove_file(tmp);
            Err(e.into())
        }
    }
}

/// KDE's lock on a config file, `<file>.lock` in QLockFile's format, held while archix sets keys so a KDE app saving meanwhile waits for it.
struct KdeLock(PathBuf);

impl KdeLock {
    fn take(target: &Path) -> Result<KdeLock> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut name = target.as_os_str().to_os_string();
        name.push(".lock");
        let path = PathBuf::from(name);
        let host = fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default();
        let host = host.trim();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o644)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&path)
            {
                Ok(mut file) => {
                    let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap_or_default();
                    let held = KdeLock(path);
                    file.write_all(format!("{}\narchix\n{host}\n{}\n", std::process::id(), boot.trim()).as_bytes())?;
                    return Ok(held);
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let holder = fs::read_to_string(&path).unwrap_or_default();
                    let mut lines = holder.lines();
                    let pid = lines.next().and_then(|pid| pid.trim().parse::<i32>().ok());
                    let on = lines.nth(1).unwrap_or_default().trim();
                    let gone = pid.is_some_and(|pid| {
                        (on.is_empty() || on == host)
                            && unsafe { libc::kill(pid, 0) } != 0
                            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
                    });
                    if gone {
                        let _ = fs::remove_file(&path);
                        continue;
                    }
                    if std::time::Instant::now() > deadline {
                        bail!(
                            "{} is held by {}; nothing in the file was changed, so switch again once that program saved it",
                            path.display(),
                            pid.map_or_else(|| "another program".to_string(), |pid| format!("process {pid}"))
                        );
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(e) => return Err(e).with_context(|| format!("locking {}", path.display())),
            }
        }
    }
}

impl Drop for KdeLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Sets a merged file's keys, keeping the app's other keys, mode and owner; a new file gets the managed mode and owner. KDE files are locked as KDE locks them, and an app's save between the read and the rename is merged again instead of lost.
fn write_merged(target: &Path, want: &ManagedFile, merge: &super::merge::Merge, accounts: &Accounts) -> Result<()> {
    use std::io::Write;
    crate::testhooks::mark("merge-start");
    let dir = target.parent().context("file has no parent directory")?;
    create_parents(dir, want, accounts)?;
    let _lock = match merge.format.as_str() {
        "kconfig" => Some(KdeLock::take(target)?),
        _ => None,
    };
    let (mode, owner) = match fs::metadata(target) {
        Ok(meta) => (meta.mode() & 0o7777, (meta.uid(), meta.gid())),
        Err(_) => (want.mode, owner(want, accounts)?),
    };
    let (mut text, mut read) = match read_version(target)? {
        Some((text, version)) => (text, Some(version)),
        None => (String::new(), None),
    };
    for _ in 0..20 {
        let merged = merge.apply(&text)?;
        crate::testhooks::mark("merge-read");
        let (tmp, mut file) = create_temp(target)?;
        finish_temp(
            &tmp,
            &mut file,
            |file| Ok(file.write_all(merged.as_bytes())?),
            Some(owner),
            mode,
        )?;
        let ours = Version::of(&file.metadata()?);
        drop(file);
        match replace_if_unchanged(&tmp, target, read.as_ref(), ours)? {
            None => return Ok(()),
            Some((now, version)) => (text, read) = (now, version),
        }
    }
    bail!("another program kept rewriting the file while archix set its keys; switch again")
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

/// Writes `want` at `target` through a new file at an unguessable name renamed over it; `content` stands in for the blob store, which a home's user can't read.
fn write(paths: &Paths, target: &Path, want: &ManagedFile, accounts: &Accounts, content: Option<&[u8]>) -> Result<()> {
    let dir = target.parent().context("file has no parent directory")?;
    create_parents(dir, want, accounts)?;
    let tmp = match &want.link {
        Some(link) => {
            let tmp = loop {
                let tmp = temp_beside(target)?;
                match symlink(link, &tmp) {
                    Ok(()) => break tmp,
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(e) => return Err(e.into()),
                }
            };
            if want.owner != "root"
                && is_root()
                && let Ok((u, g)) = owner(want, accounts)
            {
                std::os::unix::fs::lchown(&tmp, Some(u), Some(g))?;
            }
            tmp
        }
        None => {
            let owner = owner(want, accounts)?;
            let (tmp, mut file) = create_temp(target)?;
            let fill = |file: &mut fs::File| -> Result<()> {
                use std::io::Write;
                match content {
                    Some(bytes) => file.write_all(bytes)?,
                    None => {
                        let blob = blobs::path(paths, &want.sha256);
                        let mut from = fs::File::open(&blob).with_context(|| format!("copying {}", blob.display()))?;
                        std::io::copy(&mut from, file).with_context(|| format!("copying {}", blob.display()))?;
                    }
                }
                Ok(())
            };
            finish_temp(&tmp, &mut file, fill, Some(owner), want.mode)?;
            tmp
        }
    };
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
    fn an_app_save_between_the_read_and_the_rename_is_merged_again() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("settings");
        fs::write(&target, "old").unwrap();
        let (_, read) = read_version(&target).unwrap().unwrap();
        fs::write(dir.path().join("app.tmp"), "app").unwrap();
        fs::rename(dir.path().join("app.tmp"), &target).unwrap();
        let (tmp, file) = create_temp(&target).unwrap();
        fs::write(&tmp, "merged from old").unwrap();
        let ours = Version::of(&file.metadata().unwrap());
        let (text, read) = replace_if_unchanged(&tmp, &target, Some(&read), ours).unwrap().unwrap();
        assert_eq!(text, "app");
        assert!(!tmp.exists());
        let (tmp, _) = create_temp(&target).unwrap();
        fs::write(&tmp, "merged from app").unwrap();
        let ours = Version::of(&fs::metadata(&tmp).unwrap());
        assert!(
            replace_if_unchanged(&tmp, &target, read.as_ref(), ours)
                .unwrap()
                .is_none()
        );
        assert_eq!(fs::read_to_string(&target).unwrap(), "merged from app");
        assert_eq!(
            fs::read_dir(dir.path()).unwrap().count(),
            1,
            "no temporary file is left"
        );
    }

    #[test]
    fn a_kde_lock_left_by_a_dead_process_is_taken_over_and_ours_goes_when_done() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("kdeglobals");
        let lock = dir.path().join("kdeglobals.lock");
        let host = fs::read_to_string("/proc/sys/kernel/hostname").unwrap();
        fs::write(&lock, format!("{}\nkwin\n{}\n", i32::MAX, host.trim())).unwrap();
        let held = KdeLock::take(&target).unwrap();
        assert!(
            fs::read_to_string(&lock)
                .unwrap()
                .starts_with(&format!("{}\narchix\n", std::process::id()))
        );
        drop(held);
        assert!(!lock.exists());
    }

    #[test]
    fn a_kde_lock_held_by_a_live_process_is_waited_for() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("kdeglobals");
        let lock = dir.path().join("kdeglobals.lock");
        let mut holder = Command::new("sleep").arg("1").spawn().unwrap();
        let host = fs::read_to_string("/proc/sys/kernel/hostname").unwrap();
        fs::write(&lock, format!("{}\nkwin\n{}\n", holder.id(), host.trim())).unwrap();
        let started = std::time::Instant::now();
        let waiter = std::thread::spawn(move || KdeLock::take(&target).map(drop));
        holder.wait().unwrap();
        fs::remove_file(&lock).unwrap();
        waiter.join().unwrap().unwrap();
        assert!(started.elapsed() >= std::time::Duration::from_millis(900));
    }

    #[test]
    fn temporary_names_are_unguessable_and_never_follow_a_link() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("x");
        let (a, b) = (temp_beside(&target).unwrap(), temp_beside(&target).unwrap());
        assert_ne!(a, b);
        assert!(a.file_name().unwrap().to_string_lossy().starts_with(".x.archix."));
        let (tmp, _) = create_temp(&target).unwrap();
        assert_eq!(fs::symlink_metadata(&tmp).unwrap().mode() & 0o777, 0o600);
    }

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
