//! Carries /etc changes made between a switch's commit and the reboot into the deployment that boots next, three ways against the /etc both started from.

pub mod accounts;
pub mod ids;
pub mod tree;
pub mod watch;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths::Paths;
use crate::system::fstab::{self, TABLES};
use accounts::{FILES, Texts};
use tree::{Fp, Kind};

/// Paths under /etc never merged: systemd's update stamp, generated caches, the kernel's mount table and pacman's keyring, which the new deployment's own packages keep.
const NEVER: &[&str] = &[".updated", "mtab", "ld.so.cache", "pacman.d/gnupg"];

/// How long the pass at shutdown may take before it stops and records what it didn't get to.
const FINAL_PASS_SECONDS: u64 = 20;

/// One merge into a pending deployment, as kept between passes and across reboots.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct State {
    pub target: u32,
    /// The deployment whose /etc was live when the base was taken.
    pub from: u32,
    /// The live /etc as it was when the target was staged, updated to each version carried since.
    pub base: BTreeMap<String, Fp>,
    pub accounts: Texts,
    pub carried: BTreeSet<String>,
    pub conflicts: BTreeSet<String>,
    /// Paths a pass couldn't carry, and why.
    pub failed: BTreeMap<String, String>,
    pub clash: Option<String>,
    pub last_pass: u64,
    pub final_pass: bool,
    /// How many paths the pass at shutdown didn't get to before its deadline.
    #[serde(default)]
    pub partial: usize,
    /// What the boot of the target carried from the old /etc after an unclean shutdown.
    #[serde(default)]
    pub recovered: BTreeSet<String>,
    /// The base's text of fstab and crypttab, which are merged line by line.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tables: BTreeMap<String, String>,
    /// Which of fstab and crypttab, archix's to write, were edited on the running system and carried.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub edited_tables: BTreeSet<String>,
    /// Paths not carried because they hold a secret's value, with the secrets' names.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub refused: BTreeMap<String, String>,
}

pub fn dir(paths: &Paths) -> PathBuf {
    paths.state.join("etc-merge")
}

fn state_path(paths: &Paths, target: u32) -> PathBuf {
    dir(paths).join(format!("{target}.json"))
}

fn last_path(paths: &Paths) -> PathBuf {
    dir(paths).join("last.json")
}

fn now() -> u64 {
    crate::generations::now()
}

/// A time as `YYYY-MM-DD HH:MM:SS UTC`.
pub fn clock(secs: u64) -> String {
    let day = secs % 86_400;
    format!(
        "{} {:02}:{:02}:{:02} UTC",
        crate::date::Date::from_unix(secs),
        day / 3600,
        day % 3600 / 60,
        day % 60
    )
}

/// Saves through a 0600 temporary file, fsynced and renamed, as the state holds the base's shadow file.
fn save_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let tmp = tree::with_suffix(path, ".tmp");
    let _ = fs::remove_file(&tmp);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("creating {}", tmp.display()))?;
    file.write_all(&serde_json::to_vec(value)?)
        .and_then(|()| file.sync_all())
        .with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
    crate::durable::sync_dir(path)
}

pub fn load(paths: &Paths, target: u32) -> Option<State> {
    let text = fs::read(state_path(paths, target)).ok()?;
    serde_json::from_slice(&text).ok()
}

pub fn save(paths: &Paths, state: &State) -> Result<()> {
    save_json(&state_path(paths, state.target), state)
}

/// The deployment the running system boots into next when it isn't the one running, with the one running.
pub fn pending(root: &Path) -> Option<(u32, u32)> {
    let booted = crate::system::boot::booted_deployment(root)?;
    let current = crate::deploy::Store::open(root).ok()?.current().ok()??;
    (current != booted).then_some((current, booted))
}

pub fn deployment_etc(root: &Path, number: u32) -> PathBuf {
    root.join("archix/deploy").join(number.to_string()).join("etc")
}

/// A base taken from the /etc at `etc` as it is now, leaving out `secrets`, the /etc paths of secrets' links, whose fingerprint would be a value's hash if someone put one there.
pub fn snapshot(etc: &Path, target: u32, from: u32, secrets: &BTreeSet<String>) -> State {
    let mut all = Vec::new();
    tree::walk(etc, "", &|rel| never(rel) || secrets.contains(rel), &mut all);
    let base = all
        .into_iter()
        .filter(|rel| !FILES.contains(&rel.as_str()))
        .filter_map(|rel| Some((rel.clone(), tree::fingerprint(&etc.join(&rel)).ok()??)))
        .collect();
    let tables = TABLES
        .iter()
        .filter_map(|rel| Some((rel.to_string(), fs::read_to_string(etc.join(rel)).ok()?)))
        .collect();
    State {
        target,
        from,
        base,
        accounts: ids::read_texts(etc),
        tables,
        ..State::default()
    }
}

fn never(rel: &str) -> bool {
    NEVER
        .iter()
        .any(|never| rel == *never || rel.starts_with(&format!("{never}/")))
        || rel.ends_with(tree::TEMP_SUFFIX)
}

/// Run by a switch before staging deployment `target`: its base is the base the pending deployment is merged against, which /etc it's staged from holds, or else the live /etc now.
pub fn prepare(paths: &Paths, target: u32) -> Result<()> {
    let Some(booted) = crate::system::boot::booted_deployment(&paths.root) else {
        return Ok(());
    };
    let mut state = match pending(&paths.root).and_then(|(current, _)| load(paths, current)) {
        Some(state) if state.from == booted => state,
        _ => snapshot(&paths.root.join("etc"), target, booted, &secret_links(paths, &[booted])),
    };
    ids::record(&paths.state, &state.accounts)?;
    state = State {
        target,
        from: booted,
        base: state.base,
        accounts: state.accounts,
        tables: state.tables,
        ..State::default()
    };
    save(paths, &state)
}

/// Drops the base `prepare` took for a deployment that wasn't committed.
pub fn discard(paths: &Paths, target: u32) {
    let _ = fs::remove_file(state_path(paths, target));
}

pub fn unit_name(root: &Path) -> String {
    if root == Path::new("/") {
        return "archix-etc-merge.service".to_string();
    }
    let digest = crate::fetch::sha256_bytes(root.as_os_str().as_encoded_bytes());
    format!("archix-etc-merge-{}.service", &digest[..12])
}

/// The systemd properties that stop the watcher after every ordinary service and before filesystems unmount, so its last pass sees their last writes.
pub const UNIT_ORDER: &[&str] = &[
    "DefaultDependencies=no",
    "After=sysinit.target",
    "Before=basic.target shutdown.target",
    "Conflicts=shutdown.target",
];

/// Longer than the pass at shutdown may take, so systemd never kills it in the middle of one.
pub const STOP_TIMEOUT: &str = "TimeoutStopSec=40";

/// Starts the watcher after a commit made a deployment pending, through its unit or, on a system whose /etc predates it, as a transient one; a test root gets one only when ARCHIX_TEST_ETC_MERGE asks. True when a watcher now carries /etc changes.
pub fn start(paths: &Paths) -> Result<bool> {
    if pending(&paths.root).is_none() {
        return Ok(false);
    }
    let test_root = paths.root != Path::new("/");
    if test_root && !(cfg!(debug_assertions) && std::env::var_os("ARCHIX_TEST_ETC_MERGE").is_some()) {
        return Ok(false);
    }
    // An image build or install from a live USB has no running system whose /etc edits the new deployment could miss.
    if crate::system::secureboot::in_chroot() {
        return Ok(false);
    }
    if !Path::new("/run/systemd/system").exists() {
        println!(
            ":: systemd isn't running here (a container or an install), so there's no running system whose /etc changes need carrying; the /etc merge isn't started"
        );
        return Ok(false);
    }
    let unit = unit_name(&paths.root);
    let installed = !test_root && Path::new("/etc/systemd/system").join(&unit).is_file();
    let mut command = std::process::Command::new(if installed { "systemctl" } else { "systemd-run" });
    if installed {
        command.args(["start", "--no-block", &unit]);
    } else {
        if running(paths) {
            return Ok(true);
        }
        command.args(["--no-block", "--quiet", "--collect", &format!("--unit={unit}")]);
        command.arg("--description=archix: carry /etc changes into the deployment that boots next");
        for property in UNIT_ORDER.iter().chain([&STOP_TIMEOUT]) {
            command.arg(format!("--property={property}"));
        }
        if cfg!(debug_assertions)
            && test_root
            && let Some(program) = std::env::var_os("ARCHIX_TEST_MARK")
        {
            let mut setenv = std::ffi::OsString::from("--setenv=ARCHIX_TEST_MARK=");
            setenv.push(program);
            command.arg(setenv);
        }
        command.arg("--");
        command.arg(crate::integration::own_path()?);
        command.args(["etc-merge", "--root"]).arg(&paths.root);
    }
    let output = command.output().context("starting the /etc merge")?;
    if !output.status.success() {
        anyhow::bail!(
            "starting {unit} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(true)
}

/// Stops the watcher before a build stages from the pending deployment it writes into, so the build copies that /etc whole, never between two of the merge's renames such as a new passwd before its shadow, and the watcher can't recover the new deployment's record while the switch saves it; the watcher's last pass runs as it stops, and `start` after the commit, or after a failed build, brings it back against whichever deployment is pending then. True when one was stopped.
pub fn pause(paths: &Paths) -> Result<bool> {
    if pending(&paths.root).is_none() || !running(paths) {
        return Ok(false);
    }
    let unit = unit_name(&paths.root);
    let output = std::process::Command::new("systemctl")
        .args(["stop", &unit])
        .output()
        .context("stopping the /etc merge")?;
    if !output.status.success() || running(paths) {
        anyhow::bail!(
            "stopping {unit} failed, and a build from the deployment it writes into could copy its /etc mid-merge: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(true)
}

/// Whether the watcher for `paths`' root is active or on its way up.
pub fn running(paths: &Paths) -> bool {
    std::process::Command::new("systemctl")
        .args(["is-active", &unit_name(&paths.root)])
        .output()
        .is_ok_and(|out| matches!(String::from_utf8_lossy(&out.stdout).trim(), "active" | "activating"))
}

/// The /etc paths, relative to it, of the links to secrets in the generations `numbers`: never carried, so a value someone wrote over a link stays where it is.
pub(crate) fn secret_links(paths: &Paths, numbers: &[u32]) -> BTreeSet<String> {
    let store = crate::generations::Store::new(paths);
    numbers
        .iter()
        .filter_map(|number| store.get(*number).ok())
        .flat_map(|generation| {
            generation
                .system
                .secrets
                .map(|m| m.links())
                .unwrap_or_default()
                .into_keys()
        })
        .filter_map(|path| path.strip_prefix("/etc/").map(str::to_string))
        .collect()
}

/// Managed /etc files whose content the target's generation changed from the running one's, which archix writes into the target itself, and links to secrets in either.
fn rewritten(paths: &Paths, from: u32, target: u32) -> BTreeSet<String> {
    let store = crate::generations::Store::new(paths);
    let (Ok(old), Ok(new)) = (store.get(from), store.get(target)) else {
        return BTreeSet::new();
    };
    let (old, new) = (old.system.files, new.system.files);
    let mut skipped: BTreeSet<String> = old
        .keys()
        .chain(new.keys())
        .filter(|path| old.get(*path) != new.get(*path))
        .filter_map(|path| path.strip_prefix("/etc/").map(str::to_string))
        .collect();
    skipped.extend(secret_links(paths, &[from, target]));
    skipped
}

/// One pass's outcome.
#[derive(Debug, Default)]
pub struct Pass {
    /// Paths it didn't get to before its deadline.
    pub unfinished: Vec<String>,
}

/// A three-way merge of the live /etc (`live`) into the pending deployment's (`theirs`) against `state`'s base.
pub struct Merge<'a> {
    pub live: PathBuf,
    pub theirs: PathBuf,
    pub skip: BTreeSet<String>,
    pub state: &'a mut State,
    /// archix's state directory, for the login notes and the ID registry; None leaves both alone.
    pub state_dir: Option<PathBuf>,
    /// The root whose ramfs holds the values a carried file must not; None compares none.
    pub secrets_root: Option<PathBuf>,
    /// The values each pass reads from it, None when there are none to compare yet.
    pub values: Option<crate::secrets::live::Values>,
}

impl Merge<'_> {
    /// Managed files the target's generation rewrote stay its own, but for fstab and crypttab, which are merged line by line.
    fn skipped(&self, rel: &str) -> bool {
        never(rel) || (self.skip.contains(rel) && !TABLES.contains(&rel))
    }

    /// Merges everything under the paths in `dirty`, or everything when `dirty` is None; stops at `deadline`, listing what's left.
    pub fn pass(&mut self, dirty: Option<&BTreeSet<String>>, deadline: Option<Instant>) -> Pass {
        let mut paths = BTreeSet::new();
        let mut accounts = dirty.is_none();
        let skip = |rel: &str| self.skipped(rel);
        match dirty {
            None => {
                let mut all = Vec::new();
                tree::walk(&self.live, "", &skip, &mut all);
                paths.extend(all);
                paths.extend(self.state.base.keys().cloned());
            }
            Some(dirty) => {
                for rel in dirty {
                    paths.insert(rel.clone());
                    if fs::symlink_metadata(self.live.join(rel)).is_ok_and(|m| m.is_dir()) {
                        let mut all = Vec::new();
                        tree::walk(&self.live, rel, &skip, &mut all);
                        paths.extend(all);
                    }
                    let prefix = format!("{rel}/");
                    paths.extend(self.state.base.keys().filter(|k| k.starts_with(&prefix)).cloned());
                }
            }
        }
        paths.retain(|rel| !rel.is_empty() && !self.skipped(rel));
        self.read_values();
        if dirty.is_none() {
            self.take_back_carried_values();
        }
        let mut outcome = Pass::default();
        let mut removed_dirs = Vec::new();
        for rel in &paths {
            if deadline.is_some_and(|d| Instant::now() > d) {
                outcome.unfinished.push(rel.clone());
                continue;
            }
            if FILES.contains(&rel.as_str()) {
                accounts = true;
                continue;
            }
            match self.one(rel, &mut removed_dirs) {
                Ok(()) => {
                    self.state.failed.remove(rel);
                }
                Err(e) => {
                    eprintln!("archix: /etc merge: /etc/{rel}: {e:#}");
                    self.state.failed.insert(rel.clone(), format!("{e:#}"));
                }
            }
        }
        removed_dirs.sort_by(|a, b| b.cmp(a));
        for rel in removed_dirs {
            self.remove_dir(&rel);
        }
        if accounts {
            if deadline.is_some_and(|d| Instant::now() > d) {
                outcome.unfinished.extend(FILES.iter().map(|f| f.to_string()));
            } else {
                self.accounts();
            }
        }
        self.state.last_pass = now();
        outcome
    }

    fn read_values(&mut self) {
        let Some(root) = &self.secrets_root else { return };
        self.values = match crate::secrets::live::read(root) {
            Ok(values) => values,
            Err(e) => {
                eprintln!(
                    "archix: /etc merge: reading the secrets' values to keep them out of deployment {} failed: {e:#}",
                    self.state.target
                );
                None
            }
        };
    }

    /// Takes each version the merge put into the target that holds a value back out, as one copied before the values could be read.
    fn take_back_carried_values(&mut self) {
        let Some(values) = &self.values else { return };
        let held: Vec<(String, String)> = self
            .state
            .carried
            .iter()
            .chain(&self.state.conflicts)
            .filter_map(|rel| {
                let bytes = tree::read_file(&self.theirs.join(rel)).ok()?;
                Some((rel.clone(), values.in_bytes(&bytes)?))
            })
            .collect();
        for (rel, names) in held {
            if let Err(e) = self.take_back(&rel, &names) {
                eprintln!("archix: /etc merge: /etc/{rel}: {e:#}");
                self.state.failed.insert(rel, format!("{e:#}"));
            }
        }
    }

    /// The secrets whose values the live `rel` holds when it changed or was refused before; without values to compare, one refused before stays refused.
    fn secret_in(&mut self, rel: &str, live: &Option<Fp>, base: &Option<Fp>) -> Option<String> {
        let refused = self.state.refused.get(rel).cloned();
        if live == base && refused.is_none() {
            return None;
        }
        if !live.as_ref().is_some_and(|fp| matches!(fp.kind, Kind::File(_))) {
            self.state.refused.remove(rel);
            return None;
        }
        let Some(values) = &self.values else {
            return refused;
        };
        let names = tree::read_file(&self.live.join(rel))
            .ok()
            .and_then(|bytes| values.in_bytes(&bytes));
        if names.is_none() {
            self.state.refused.remove(rel);
        }
        names
    }

    /// Leaves `rel`, which holds the values of secrets `names`, out of the target, and its version there too if that holds one; says so once, path and names only.
    fn refuse(&mut self, rel: &str, names: &str) -> Result<()> {
        let target = self.state.target;
        let theirs_holds = self.values.as_ref().is_some_and(|values| {
            tree::read_file(&self.theirs.join(rel)).is_ok_and(|bytes| values.in_bytes(&bytes).is_some())
        });
        if theirs_holds {
            self.take_back(rel, names)?;
        }
        if self.state.refused.get(rel).map(String::as_str) == Some(names) {
            return Ok(());
        }
        self.state.refused.insert(rel.to_string(), names.to_string());
        println!(
            "archix: /etc merge: /etc/{rel} holds secret {names}'s value in plain text, so it isn't carried into deployment {target}"
        );
        self.note(&format!(
            "archix: /etc/{rel} holds secret {names}'s value in plain text, so the /etc merge doesn't carry it into deployment {target}; remove it, and change a NetworkManager connection whose secret archix places with `nmcli connection modify --temporary`, or in the config"
        ));
        Ok(())
    }

    /// Takes the target's `rel`, which holds a value, out: the target's own version back from .pacnew when the merge moved it there, else nothing; forgets its base, so a clean live version is carried again.
    fn take_back(&mut self, rel: &str, names: &str) -> Result<()> {
        let to = self.theirs.join(rel);
        let pacnew = tree::with_suffix(&to, ".pacnew");
        if self.state.conflicts.remove(rel) && fs::symlink_metadata(&pacnew).is_ok() {
            fs::rename(&pacnew, &to).with_context(|| format!("renaming {} back", pacnew.display()))?;
            crate::durable::sync_dir(&to)?;
        } else {
            tree::remove_entry(&to)?;
        }
        self.state.carried.remove(rel);
        self.state.base.remove(rel);
        println!(
            "archix: /etc merge: took /etc/{rel}, which holds secret {names}'s value, back out of deployment {}",
            self.state.target
        );
        Ok(())
    }

    fn note(&self, line: &str) {
        let Some(dir) = &self.state_dir else { return };
        let notes = &dir.join("notes");
        let mut text = fs::read_to_string(notes).unwrap_or_default();
        if !text.contains(line) {
            text.push_str(line);
            text.push('\n');
            let _ = crate::durable::write(notes, text.as_bytes());
        }
    }

    fn remove_dir(&mut self, rel: &str) {
        if fs::symlink_metadata(self.live.join(rel)).is_ok() {
            return;
        }
        match fs::remove_dir(self.theirs.join(rel)) {
            Ok(()) => {
                let _ = crate::durable::sync_dir(&self.theirs.join(rel));
                self.state.base.remove(rel);
                self.state.carried.insert(rel.to_string());
                self.state.failed.remove(rel);
                println!(
                    "archix: /etc merge: removed /etc/{rel} from deployment {}",
                    self.state.target
                );
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.state.base.remove(rel);
            }
            Err(_) => {
                self.state.failed.insert(
                    rel.to_string(),
                    format!(
                        "removed on the running system, but deployment {} has files of its own in it, so it stays",
                        self.state.target
                    ),
                );
            }
        }
    }

    fn one(&mut self, rel: &str, removed_dirs: &mut Vec<String>) -> Result<()> {
        crate::testhooks::fail_at(&format!("etc-merge:{rel}"))?;
        let base = self.state.base.get(rel).cloned();
        let live = match tree::blocked_ancestor(&self.live, rel) {
            Some(_) => None,
            None => tree::fingerprint(&self.live.join(rel))?,
        };
        if let Some(names) = self.secret_in(rel, &live, &base) {
            return self.refuse(rel, &names);
        }
        if live == base {
            return Ok(());
        }
        let target = self.state.target;
        if let Some(at) = tree::blocked_ancestor(&self.theirs, rel) {
            anyhow::bail!(
                "deployment {target} has /etc/{at} as something other than a directory, so /etc/{rel} can't go into it; the running system's version stays in deployment {}'s /etc",
                self.state.from
            );
        }
        let to = self.theirs.join(rel);
        let theirs = tree::fingerprint(&to)?;
        let table = TABLES.contains(&rel).then(|| fs::read_to_string(&to).ok()).flatten();
        if live == theirs {
            self.set_base(rel, live);
            self.carried_table(rel, table.as_deref());
            return Ok(());
        }
        let file = |fp: &Option<Fp>| fp.as_ref().is_some_and(|fp| matches!(fp.kind, Kind::File(_)));
        if let Some(theirs_text) = &table
            && file(&live)
            && file(&theirs)
            && theirs != base
        {
            return self.merge_table(rel, live, theirs_text);
        }
        if live.as_ref().is_some_and(|l| l.kind == Kind::Other) {
            anyhow::bail!("it's a socket, FIFO or device, which the merge doesn't carry");
        }
        let both_dirs = live.as_ref().is_some_and(Fp::is_dir) && theirs.as_ref().is_some_and(Fp::is_dir);
        let mut conflict = theirs.is_some() && theirs != base && !both_dirs && rel != "machine-id";
        if conflict {
            let pacnew = tree::to_pacnew(&to)?;
            println!(
                "archix: /etc merge: /etc/{rel} changed both on the running system and in deployment {target}; the running system's version goes over, deployment {target}'s is {} ({})",
                Path::new("/etc")
                    .join(pacnew.strip_prefix(&self.theirs).unwrap_or(&pacnew))
                    .display(),
                tree::describe(theirs.as_ref())
            );
        } else if let (Some(theirs), Some(live)) = (&theirs, &live)
            && theirs.is_dir() != live.is_dir()
        {
            if theirs.is_dir() && fs::remove_dir(&to).is_err() {
                tree::to_pacnew(&to)?;
                conflict = true;
            } else if !theirs.is_dir() {
                tree::remove_entry(&to)?;
            }
        }
        let from = self.live.join(rel);
        let written = match &live {
            None => {
                if !conflict {
                    match &theirs {
                        Some(fp) if fp.is_dir() => {
                            removed_dirs.push(rel.to_string());
                            return Ok(());
                        }
                        Some(_) => tree::remove_entry(&to)?,
                        None => {}
                    }
                }
                None
            }
            Some(fp) => {
                self.parents(rel)?;
                Some(match fp.kind {
                    Kind::Dir => {
                        tree::copy_dir(&from, &to)?;
                        fp.clone()
                    }
                    Kind::Symlink(_) => tree::copy_symlink(&from, &to)?,
                    _ => match &self.values {
                        None => tree::copy_file(&from, &to)?,
                        Some(values) => match tree::copy_file_unless(&from, &to, |bytes| values.in_bytes(bytes))? {
                            Ok(fp) => fp,
                            Err(names) => {
                                if conflict {
                                    fs::rename(tree::with_suffix(&to, ".pacnew"), &to)?;
                                }
                                return self.refuse(rel, &names);
                            }
                        },
                    },
                })
            }
        };
        self.set_base(rel, written);
        self.carried_table(rel, table.as_deref());
        let verb = if live.is_some() { "carried" } else { "removed" };
        if conflict {
            self.state.conflicts.insert(rel.to_string());
        } else {
            self.state.carried.insert(rel.to_string());
            println!("archix: /etc merge: {verb} /etc/{rel} into deployment {target}");
        }
        Ok(())
    }

    /// Merges the running system's edits to fstab or crypttab into the target's line by line, as archix wrote the target's whole.
    fn merge_table(&mut self, rel: &str, live: Option<Fp>, theirs: &str) -> Result<()> {
        let target = self.state.target;
        let (from, to) = (self.live.join(rel), self.theirs.join(rel));
        let live_text = fs::read_to_string(&from).with_context(|| format!("reading {}", from.display()))?;
        let base = self.state.tables.get(rel).map_or(theirs, String::as_str);
        let (text, both) = fstab::merge_lines(rel, base, &live_text, theirs);
        if both.is_empty() {
            self.state.carried.insert(rel.to_string());
            println!(
                "archix: /etc merge: carried the running system's edits to /etc/{rel} into deployment {target}'s, line by line"
            );
        } else {
            let pacnew = tree::with_suffix(&to, ".pacnew");
            tree::write_like(&pacnew, theirs.as_bytes(), &to)?;
            self.state.conflicts.insert(rel.to_string());
            println!(
                "archix: /etc merge: /etc/{rel}'s lines for {} changed both on the running system and in deployment {target}; the running system's go in, deployment {target}'s whole file is /etc/{rel}.pacnew",
                both.join(", ")
            );
        }
        tree::write_like(&to, text.as_bytes(), &from)?;
        self.set_base(rel, live);
        self.carried_table(rel, Some(theirs));
        Ok(())
    }

    /// After a running system's fstab or crypttab reached the target: keeps its text as the base and warns when archix wrote the target's, as the next switch writes it whole again.
    fn carried_table(&mut self, rel: &str, theirs: Option<&str>) {
        if !TABLES.contains(&rel) {
            return;
        }
        let live = fs::read_to_string(self.live.join(rel)).ok();
        let edited = live.as_deref() != self.state.tables.get(rel).map(String::as_str);
        match live {
            Some(text) => self.state.tables.insert(rel.to_string(), text),
            None => self.state.tables.remove(rel),
        };
        if !edited || !theirs.is_some_and(fstab::written_by_archix) {
            return;
        }
        self.state.edited_tables.insert(rel.to_string());
        let declare = match rel {
            "crypttab" => "declare the volume as encrypted in its mount's file_systems entry",
            _ => "declare the mount in file_systems",
        };
        let line = format!(
            "archix: /etc/{rel} was edited on the running system; the edit is carried into deployment {}, but archix writes /etc/{rel} whole at every switch, so the next switch drops this edit (and refuses to while a line it drops is mounted, open, or for /home, /var or /srv): {declare}",
            self.state.target
        );
        println!("{line}");
        self.note(&line);
    }

    fn set_base(&mut self, rel: &str, fp: Option<Fp>) {
        match fp {
            Some(fp) => self.state.base.insert(rel.to_string(), fp),
            None => self.state.base.remove(rel),
        };
    }

    /// Recreates directories the target removed that a live path needs, as they are live.
    fn parents(&mut self, rel: &str) -> Result<()> {
        let parts: Vec<&str> = rel.split('/').collect();
        let mut at = String::new();
        for part in &parts[..parts.len() - 1] {
            if !at.is_empty() {
                at.push('/');
            }
            at.push_str(part);
            if fs::symlink_metadata(self.theirs.join(&at)).is_err() {
                tree::copy_dir(&self.live.join(&at), &self.theirs.join(&at))?;
                let fp = tree::fingerprint(&self.live.join(&at))?;
                self.set_base(&at, fp);
            }
        }
        Ok(())
    }

    fn accounts(&mut self) {
        let live = ids::read_texts(&self.live);
        if live == self.state.accounts {
            return;
        }
        let theirs = ids::read_texts(&self.theirs);
        let target = self.state.target;
        let merged = match accounts::merge_all(&self.state.accounts, &live, &theirs, target) {
            Ok(merged) => merged,
            Err(clash) => {
                if self.state.clash.as_deref() != Some(clash.as_str()) {
                    eprintln!("archix: /etc merge: stopped carrying the account files: {clash}");
                }
                self.note(&format!(
                    "archix: the /etc merge stopped carrying users and groups into deployment {target}: {clash}. Give the running system's account another number (usermod -u, groupmod -g) and the merge goes on; until then deployment {target} boots without the accounts and group members added since the switch, though deleted accounts, removed members and password changes still reach it"
                ));
                self.state.clash = Some(clash);
                let held = accounts::merge_held(&self.state.accounts, &live, &theirs);
                self.write_accounts(&held, &theirs);
                return;
            }
        };
        if !self.write_accounts(&merged, &theirs) {
            return;
        }
        if let Some(dir) = &self.state_dir {
            let _ = ids::record(dir, &live);
            let _ = ids::record(dir, &merged);
        }
        self.state.accounts = live;
        self.state.clash = None;
    }

    /// Writes each account file `merged` changed from `theirs`; false once one fails.
    fn write_accounts(&mut self, merged: &accounts::Texts, theirs: &accounts::Texts) -> bool {
        let target = self.state.target;
        for (i, file) in FILES.iter().enumerate() {
            let (Some(text), Some(before)) = (&merged[i], &theirs[i]) else {
                continue;
            };
            if text == before {
                continue;
            }
            let path = self.theirs.join(file);
            if let Err(e) = tree::write_like(&path, text.as_bytes(), &path) {
                eprintln!("archix: /etc merge: /etc/{file}: {e:#}");
                self.state.failed.insert(file.to_string(), format!("{e:#}"));
                return false;
            }
            crate::testhooks::mark(&format!("etc-merge-wrote-{file}"));
            self.state.carried.insert(file.to_string());
            self.state.failed.remove(*file);
            println!(
                "archix: /etc merge: carried the running system's changes to /etc/{file} into deployment {target}"
            );
        }
        true
    }
}

/// The merge from the running /etc into deployment `state.target`'s, with what this pair of generations leaves out.
pub fn merge_for<'a>(paths: &Paths, state: &'a mut State, live: PathBuf, theirs: PathBuf) -> Merge<'a> {
    let skip = rewritten(paths, state.from, state.target);
    Merge {
        live,
        theirs,
        skip,
        state,
        state_dir: Some(paths.state.clone()),
        secrets_root: Some(paths.root.clone()),
        values: None,
    }
}

/// The deadline of the pass at shutdown; ARCHIX_TEST_ETC_MERGE_DEADLINE_MS shortens it in a debug build.
pub fn final_deadline() -> Instant {
    let ms = std::env::var("ARCHIX_TEST_ETC_MERGE_DEADLINE_MS")
        .ok()
        .filter(|_| cfg!(debug_assertions))
        .and_then(|ms| ms.parse().ok())
        .unwrap_or(FINAL_PASS_SECONDS * 1000);
    Instant::now() + std::time::Duration::from_millis(ms)
}

/// What became of the merge into the deployment that's running now, kept for `archix status` after its first boot.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Last {
    pub booted: u32,
    pub from: u32,
    pub carried: usize,
    pub conflicts: Vec<String>,
    pub failed: BTreeMap<String, String>,
    pub unclean: bool,
    #[serde(default)]
    pub partial: usize,
    pub last_pass: u64,
    pub recovered: Vec<String>,
    pub clash: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub refused: BTreeMap<String, String>,
}

/// Run early in every boot, before sysusers and activation: when the merge into the deployment that booted didn't finish its pass at shutdown, carries what the old /etc held on disk, and says so in the login notes and `archix status`.
pub fn boot(root: &Path) -> Result<()> {
    let paths = Paths::under(root);
    let Some(booted) = crate::system::boot::booted_deployment(root) else {
        return Ok(());
    };
    let Some(mut state) = load(&paths, booted) else {
        return Ok(());
    };
    let live_etc = root.join("etc");
    tree::sweep_temps(&live_etc);
    let unclean = !state.final_pass;
    let old_etc = deployment_etc(root, state.from);
    let failed_at_shutdown = state.failed.clone();
    if (unclean || state.partial > 0 || !state.failed.is_empty()) && old_etc.is_dir() {
        let before = state.carried.clone();
        let mut merge = merge_for(&paths, &mut state, old_etc, live_etc);
        merge.pass(None, None);
        let recovered: BTreeSet<String> = merge.state.carried.difference(&before).cloned().collect();
        let fixed: Vec<String> = failed_at_shutdown
            .keys()
            .filter(|rel| !merge.state.failed.contains_key(*rel))
            .cloned()
            .collect();
        merge.state.recovered = recovered.into_iter().chain(fixed).collect();
    }
    let list = |names: &mut dyn Iterator<Item = &String>| -> String {
        let all: Vec<String> = names.map(|rel| format!("/etc/{rel}")).collect();
        if all.is_empty() {
            "nothing".to_string()
        } else {
            all.join(", ")
        }
    };
    let mut notes = Vec::new();
    if unclean {
        notes.push(format!(
            "archix: deployment {} didn't shut down cleanly before deployment {booted} first booted (a power cut, a hang or a hard reset), so the /etc merge's last pass was at {}. At this boot archix carried what deployment {}'s /etc held on disk since: {}; a change made just before the power went that the disk hadn't stored yet is lost",
            state.from,
            clock(state.last_pass),
            state.from,
            list(&mut state.recovered.iter())
        ));
    }
    if state.partial > 0 && !unclean {
        notes.push(format!(
            "archix: the /etc merge's pass at shutdown ran out of time with {} paths to go, so these changes made on deployment {} reached deployment {booted} only at this boot, from deployment {}'s /etc: {}",
            state.partial,
            state.from,
            state.from,
            list(&mut state.recovered.iter())
        ));
    }
    let reasons = |failed: &BTreeMap<String, String>| -> String {
        let all: Vec<String> = failed.iter().map(|(rel, why)| format!("/etc/{rel} ({why})")).collect();
        all.join("; ")
    };
    if !failed_at_shutdown.is_empty() && !unclean {
        notes.push(format!(
            "archix: the /etc merge couldn't carry these changes made on deployment {} into deployment {booted} before the reboot: {}; this boot carried them from deployment {}'s /etc unless listed as still missing",
            state.from,
            reasons(&failed_at_shutdown),
            state.from
        ));
    }
    if !state.failed.is_empty() {
        notes.push(format!(
            "archix: these /etc changes made on deployment {} are still missing from deployment {booted}: {}; deployment {}'s versions are in /archix/deploy/{}/etc while it's kept",
            state.from,
            reasons(&state.failed),
            state.from,
            state.from
        ));
    }
    if let Some(clash) = &state.clash {
        notes.push(format!(
            "archix: users and groups added on deployment {} after the switch weren't carried into deployment {booted}: {clash}",
            state.from
        ));
    }
    if !notes.is_empty() {
        let mut text = fs::read_to_string(paths.notes()).unwrap_or_default();
        for note in &notes {
            println!("{note}");
            text.push_str(note);
            text.push('\n');
        }
        crate::durable::write(&paths.notes(), text.as_bytes())?;
    }
    let last = Last {
        booted,
        from: state.from,
        carried: state.carried.len(),
        conflicts: state.conflicts.iter().cloned().collect(),
        failed: state.failed.clone(),
        unclean,
        partial: state.partial,
        last_pass: state.last_pass,
        recovered: state.recovered.iter().cloned().collect(),
        clash: state.clash.clone(),
        refused: state.refused.clone(),
    };
    save_json(&last_path(&paths), &last)?;
    discard(&paths, booted);
    Ok(())
}

/// Lines for `archix status` about the /etc merge into the pending deployment, or about the last one after its boot.
pub fn status_lines(paths: &Paths) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some((target, _)) = pending(&paths.root) {
        let Some(state) = load(paths, target) else {
            lines.push(format!(
                "nothing has carried /etc changes into the pending deployment {target} yet, so changes made now may not reach it; is {} running?",
                unit_name(&paths.root)
            ));
            return lines;
        };
        lines.push(format!(
            "{} /etc changes carried into the pending deployment {target}, {} conflicts (.pacnew)",
            state.carried.len(),
            state.conflicts.len()
        ));
        for rel in &state.conflicts {
            lines.push(format!(
                "  /etc/{rel}: changed on the running system and in deployment {target}; the running system's version goes over, deployment {target}'s is /etc/{rel}.pacnew there"
            ));
        }
        for (rel, why) in &state.failed {
            lines.push(format!("  /etc/{rel} isn't carried: {why}"));
        }
        for (rel, names) in &state.refused {
            lines.push(format!(
                "  /etc/{rel} isn't carried: it holds secret {names}'s value in plain text; remove it, or put the secret back where archix places it"
            ));
        }
        for rel in &state.edited_tables {
            lines.push(format!(
                "  /etc/{rel}: edited on the running system and carried, but the next switch writes it whole again from the config, dropping what file_systems doesn't declare"
            ));
        }
        if let Some(clash) = &state.clash {
            lines.push(format!("  users and groups aren't carried: {clash}"));
        }
        if state.last_pass > 0 {
            lines.push(format!("  last pass: {}", clock(state.last_pass)));
        }
        return lines;
    }
    let booted = crate::system::boot::booted_deployment(&paths.root);
    let Some(last) = fs::read(last_path(paths))
        .ok()
        .and_then(|text| serde_json::from_slice::<Last>(&text).ok())
        .filter(|last| Some(last.booted) == booted)
    else {
        return lines;
    };
    lines.push(format!(
        "{} /etc changes were carried from deployment {} into this one, {} conflicts (.pacnew)",
        last.carried,
        last.from,
        last.conflicts.len()
    ));
    for rel in &last.conflicts {
        lines.push(format!(
            "  /etc/{rel}: deployment {}'s version is /etc/{rel}.pacnew",
            last.booted
        ));
    }
    if last.unclean {
        lines.push(format!(
            "  deployment {} didn't shut down cleanly; /etc changes made on it after {} were taken from its /etc on disk at this boot ({} files), and any the disk hadn't stored are lost",
            last.from,
            clock(last.last_pass),
            last.recovered.len()
        ));
    }
    if last.partial > 0 && !last.unclean {
        lines.push(format!(
            "  the pass at shutdown ran out of time; this boot carried what it hadn't got to ({} files)",
            last.recovered.len()
        ));
    }
    for (rel, why) in &last.failed {
        lines.push(format!("  /etc/{rel} didn't reach this deployment: {why}"));
    }
    for (rel, names) in &last.refused {
        lines.push(format!(
            "  /etc/{rel} didn't reach this deployment, as it held secret {names}'s value in plain text; deployment {}'s /etc still has it while it's kept",
            last.from
        ));
    }
    if let Some(clash) = &last.clash {
        lines.push(format!("  users and groups weren't carried: {clash}"));
    }
    lines
}

/// Carries the accounts of the /etc at `live` into the /etc at `target`, as a rollback or a boot of an older deployment does; returns what was left out.
pub fn carry_accounts(paths: &Paths, live: &Path, target: &Path) -> Result<Vec<String>> {
    if fs::canonicalize(live).ok() == fs::canonicalize(target).ok() {
        return Ok(Vec::new());
    }
    let (from, to) = (ids::read_texts(live), ids::read_texts(target));
    ids::record(&paths.state, &from)?;
    ids::record(&paths.state, &to)?;
    let carried = accounts::carry(&from, &to);
    for (i, file) in FILES.iter().enumerate() {
        if let Some(text) = &carried.texts[i]
            && Some(text) != to[i].as_ref()
        {
            let path = target.join(file);
            tree::write_like(&path, text.as_bytes(), &path)?;
        }
    }
    Ok(carried.skipped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, Copy, PartialEq)]
    enum Make {
        Missing,
        File,
        Dir,
        Link,
    }

    const ALL: [Make; 4] = [Make::Missing, Make::File, Make::Dir, Make::Link];

    fn make(etc: &Path, what: Make, role: &str) {
        fs::create_dir_all(etc).unwrap();
        let at = etc.join("x");
        match what {
            Make::Missing => {}
            Make::File => fs::write(&at, role).unwrap(),
            Make::Dir => {
                fs::create_dir(&at).unwrap();
                if role == "theirs" {
                    fs::write(at.join("inside"), "the new deployment's own file").unwrap();
                }
            }
            Make::Link => std::os::unix::fs::symlink(format!("/usr/lib/systemd/system/{role}.service"), &at).unwrap(),
        }
    }

    fn found(path: &Path) -> Option<(Make, String)> {
        let meta = fs::symlink_metadata(path).ok()?;
        Some(if meta.is_dir() {
            (Make::Dir, fs::read_to_string(path.join("inside")).unwrap_or_default())
        } else if meta.file_type().is_symlink() {
            (Make::Link, fs::read_link(path).unwrap().display().to_string())
        } else {
            (Make::File, fs::read_to_string(path).unwrap())
        })
    }

    fn content(what: Make, role: &str) -> Option<(Make, String)> {
        match what {
            Make::Missing => None,
            Make::File => Some((Make::File, role.to_string())),
            Make::Dir if role == "theirs" => Some((Make::Dir, "the new deployment's own file".to_string())),
            Make::Dir => Some((Make::Dir, String::new())),
            Make::Link => Some((Make::Link, format!("/usr/lib/systemd/system/{role}.service"))),
        }
    }

    fn merge<'a>(live: &Path, theirs: &Path, state: &'a mut State, skip: &[&str]) -> Merge<'a> {
        Merge {
            live: live.to_path_buf(),
            theirs: theirs.to_path_buf(),
            skip: skip.iter().map(|s| s.to_string()).collect(),
            state,
            state_dir: None,
            secrets_root: None,
            values: None,
        }
    }

    #[test]
    fn every_type_change_between_base_live_and_theirs_ends_with_live_and_theirs_kept_as_pacnew() {
        for base in ALL {
            for live in ALL {
                for theirs in ALL {
                    let dir = tempfile::tempdir().unwrap();
                    let (b, l, t) = (
                        dir.path().join("base"),
                        dir.path().join("live"),
                        dir.path().join("theirs"),
                    );
                    make(&b, base, "base");
                    make(&l, live, "live");
                    make(&t, theirs, "theirs");
                    let mut state = snapshot(&b, 2, 1, &BTreeSet::new());
                    let mut merge = merge(&l, &t, &mut state, &[]);
                    merge.pass(Some(&BTreeSet::from(["x".to_string()])), None);
                    let case = format!("base {base:?}, live {live:?}, theirs {theirs:?}");
                    let pacnew = found(&t.join("x.pacnew"));
                    if base == live && matches!(base, Make::Missing | Make::Dir) {
                        assert_eq!(found(&t.join("x")), content(theirs, "theirs"), "{case}");
                        assert_eq!(pacnew, None, "{case}");
                        continue;
                    }
                    let theirs_changed = !(base == theirs && matches!(base, Make::Missing | Make::Dir));
                    let both_dirs = live == Make::Dir && theirs == Make::Dir;
                    if live == Make::Missing && theirs == Make::Dir && !theirs_changed {
                        assert_eq!(
                            found(&t.join("x")),
                            content(theirs, "theirs"),
                            "{case}: its own files keep it"
                        );
                        assert!(merge.state.failed.contains_key("x"), "{case}: reported");
                        continue;
                    }
                    let in_the_way = theirs == Make::Dir && live != Make::Dir && live != Make::Missing;
                    let expect_pacnew = theirs != Make::Missing && !both_dirs && (theirs_changed || in_the_way);
                    let want = if both_dirs {
                        content(theirs, "theirs")
                    } else {
                        content(live, "live")
                    };
                    assert_eq!(found(&t.join("x")), want, "{case}");
                    assert_eq!(
                        pacnew,
                        if expect_pacnew { content(theirs, "theirs") } else { None },
                        "{case}"
                    );
                    assert_eq!(merge.state.conflicts.contains("x"), expect_pacnew, "{case}");
                    assert!(merge.state.failed.is_empty(), "{case}: {:?}", merge.state.failed);
                }
            }
        }
    }

    #[test]
    fn a_second_live_edit_after_a_conflict_is_carried_without_another_pacnew() {
        let dir = tempfile::tempdir().unwrap();
        let (b, l, t) = (
            dir.path().join("base"),
            dir.path().join("live"),
            dir.path().join("theirs"),
        );
        for (etc, text) in [(&b, "base"), (&l, "live 1"), (&t, "package")] {
            fs::create_dir_all(etc).unwrap();
            fs::write(etc.join("x"), text).unwrap();
        }
        let mut state = snapshot(&b, 2, 1, &BTreeSet::new());
        let dirty = BTreeSet::from(["x".to_string()]);
        let mut merge = merge(&l, &t, &mut state, &[]);
        merge.pass(Some(&dirty), None);
        fs::write(l.join("x"), "live 2").unwrap();
        merge.pass(Some(&dirty), None);
        assert_eq!(fs::read_to_string(t.join("x")).unwrap(), "live 2");
        assert_eq!(fs::read_to_string(t.join("x.pacnew")).unwrap(), "package");
        assert_eq!(merge.state.conflicts.len(), 1);
    }

    #[test]
    fn a_file_holding_a_value_is_never_carried_a_carried_one_is_taken_back_and_a_fixed_one_goes_in() {
        use crate::secrets::live::Values;
        use crate::secrets::memory::SecretBuf;
        const VALUE: &str = "0123456789abcdef-key";
        let dir = tempfile::tempdir().unwrap();
        let (b, l, t) = (
            dir.path().join("base"),
            dir.path().join("live"),
            dir.path().join("theirs"),
        );
        for etc in [&b, &l, &t] {
            fs::create_dir_all(etc).unwrap();
        }
        for (etc, text) in [(&b, "base"), (&l, "live 1"), (&t, "package")] {
            fs::write(etc.join("x"), text).unwrap();
        }
        for etc in [&b, &l] {
            fs::write(etc.join("y"), format!("y={VALUE}")).unwrap();
        }
        fs::write(t.join("y"), format!("y={VALUE}")).unwrap();
        fs::write(l.join("p"), format!("[wireguard]\nprivate-key={VALUE}\n")).unwrap();
        let mut state = snapshot(&b, 2, 1, &BTreeSet::new());
        state.carried.insert("y".to_string());
        let mut merge = merge(&l, &t, &mut state, &[]);
        merge.values = Some(Values::new(vec![(
            "wg0".to_string(),
            SecretBuf::from_slice(VALUE.as_bytes()).unwrap(),
        )]));
        merge.pass(Some(&BTreeSet::from(["x".to_string()])), None);
        assert_eq!(
            fs::read_to_string(t.join("x")).unwrap(),
            "live 1",
            "a clean file goes in"
        );
        assert_eq!(fs::read_to_string(t.join("x.pacnew")).unwrap(), "package");
        fs::write(l.join("x"), format!("live 2 {VALUE}")).unwrap();
        fs::write(t.join("x"), format!("live 2 {VALUE}")).unwrap();
        merge.pass(None, None);
        assert!(!t.join("p").exists(), "a new file with the value stays out");
        assert!(
            !t.join("y").exists(),
            "a carried version with the value is taken back out"
        );
        assert_eq!(
            fs::read_to_string(t.join("x")).unwrap(),
            "package",
            "the target's own version comes back from .pacnew"
        );
        assert!(!t.join("x.pacnew").exists() && !merge.state.conflicts.contains("x"));
        assert_eq!(
            merge.state.refused.keys().collect::<Vec<_>>(),
            ["p", "x", "y"],
            "{:?}",
            merge.state.refused
        );
        assert!(
            ["p", "x", "y"].iter().all(|rel| !merge.state.base.contains_key(*rel)),
            "no fingerprint of a version with the value: {:?}",
            merge.state.base
        );
        merge.values = None;
        merge.pass(None, None);
        assert!(
            !t.join("p").exists() && merge.state.refused.contains_key("p"),
            "with no values to compare, what was refused stays refused"
        );
        merge.values = Some(Values::new(vec![(
            "wg0".to_string(),
            SecretBuf::from_slice(VALUE.as_bytes()).unwrap(),
        )]));
        fs::write(l.join("p"), "[wireguard]\nprivate-key-flags=1\n").unwrap();
        merge.pass(Some(&BTreeSet::from(["p".to_string()])), None);
        assert_eq!(
            fs::read_to_string(t.join("p")).unwrap(),
            "[wireguard]\nprivate-key-flags=1\n",
            "fixed, it's carried"
        );
        assert!(!merge.state.refused.contains_key("p"));
        let json = serde_json::to_string(&*merge.state).unwrap();
        assert!(!json.contains(VALUE), "{json}");
    }

    #[test]
    fn a_secrets_link_is_never_carried_nor_fingerprinted_even_when_a_value_replaced_it() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let store = crate::generations::Store::new(&paths);
        let record = |number: u32, secrets: Option<crate::secrets::Manifest>| {
            let mut generation: crate::generations::Generation = serde_json::from_value(serde_json::json!({
                "format": crate::generations::FORMAT, "number": number, "created": number, "host": "box", "date": "2026-10-10",
                "packages": {}, "settings": { "remove_undeclared": true, "ignored": [], "keep": 10, "keep_days": 30, "deployments_keep": 2 },
                "config_commit": null, "config_dirty": false, "system": {}, "flatpak": {}, "before_archix": false
            }))
            .unwrap();
            generation.system.secrets = secrets;
            store.save(&generation).unwrap();
        };
        let entry: crate::secrets::Entry = serde_json::from_value(serde_json::json!({
            "file": "wg0.age", "sha256": "1", "kind": "file", "owner": "root", "group": "root", "mode": 256,
            "consumers": ["/etc/wireguard/wg0.conf"]
        }))
        .unwrap();
        let manifest = crate::secrets::Manifest {
            secrets: [("wg0".to_string(), entry)].into(),
            ..Default::default()
        };
        record(1, Some(manifest));
        record(2, None);
        let skipped = rewritten(&paths, 1, 2);
        assert!(
            skipped.contains("wireguard/wg0.conf"),
            "a link the running generation has: {skipped:?}"
        );
        let (b, l, t) = (
            dir.path().join("base"),
            dir.path().join("live"),
            dir.path().join("theirs"),
        );
        for etc in [&b, &l, &t] {
            fs::create_dir_all(etc.join("wireguard")).unwrap();
            std::os::unix::fs::symlink("/run/archix/secrets/wg0", etc.join("wireguard/wg0.conf")).unwrap();
        }
        fs::remove_file(l.join("wireguard/wg0.conf")).unwrap();
        fs::write(l.join("wireguard/wg0.conf"), "PrivateKey = value-someone-saved").unwrap();
        let secrets = secret_links(&paths, &[1]);
        let mut state = snapshot(&l, 2, 1, &secrets);
        assert!(
            !state.base.contains_key("wireguard/wg0.conf"),
            "no fingerprint of a value"
        );
        let skip: Vec<&str> = skipped.iter().map(String::as_str).collect();
        merge(&l, &t, &mut state, &skip).pass(None, None);
        assert!(
            fs::read_link(t.join("wireguard/wg0.conf")).is_ok(),
            "the pending deployment keeps its link, never the value"
        );
    }

    #[test]
    fn excluded_and_managed_paths_stay_the_new_deployments_and_machine_id_follows_live_only_when_changed() {
        let dir = tempfile::tempdir().unwrap();
        let (b, l, t) = (
            dir.path().join("base"),
            dir.path().join("live"),
            dir.path().join("theirs"),
        );
        let names = [
            ".updated",
            "ld.so.cache",
            "pacman.d/gnupg/pubring.gpg",
            "managed",
            "machine-id",
        ];
        for (etc, text) in [(&b, "base"), (&l, "live"), (&t, "theirs")] {
            fs::create_dir_all(etc.join("pacman.d/gnupg")).unwrap();
            for name in names {
                fs::write(etc.join(name), text).unwrap();
            }
        }
        let mut state = snapshot(&b, 2, 1, &BTreeSet::new());
        merge(&l, &t, &mut state, &["managed"]).pass(None, None);
        for name in &names[..4] {
            assert_eq!(fs::read_to_string(t.join(name)).unwrap(), "theirs", "{name}");
        }
        assert_eq!(fs::read_to_string(t.join("machine-id")).unwrap(), "live");
        assert!(!t.join("machine-id.pacnew").exists());
        fs::write(b.join("machine-id"), "same").unwrap();
        fs::write(l.join("machine-id"), "same").unwrap();
        fs::write(t.join("machine-id"), "theirs").unwrap();
        let mut state = snapshot(&b, 2, 1, &BTreeSet::new());
        merge(&l, &t, &mut state, &[]).pass(None, None);
        assert_eq!(
            fs::read_to_string(t.join("machine-id")).unwrap(),
            "theirs",
            "unchanged live, left alone"
        );
    }

    #[test]
    fn a_password_changed_during_a_clash_still_reaches_the_pending_deployment() {
        let dir = tempfile::tempdir().unwrap();
        let (l, t) = (dir.path().join("live"), dir.path().join("theirs"));
        let passwd = "root:x:0:0::/root:/bin/bash\nalice:x:1000:1000::/home/alice:/bin/bash\n";
        let shadow = "root:$r:20000::::::\nalice:$a1:20000:0:99999:7:::\n";
        for etc in [&l, &t] {
            fs::create_dir_all(etc).unwrap();
            fs::write(etc.join("passwd"), passwd).unwrap();
            fs::write(etc.join("shadow"), shadow).unwrap();
        }
        let mut state = snapshot(&l, 2, 1, &BTreeSet::new());
        fs::write(
            t.join("passwd"),
            format!("{passwd}pkguser:x:960:960::/:/usr/bin/nologin\n"),
        )
        .unwrap();
        fs::write(t.join("shadow"), format!("{shadow}pkguser:!*:20050::::::\n")).unwrap();
        let with_carol = format!("{passwd}carol:x:960:960::/home/carol:/bin/bash\n");
        fs::write(l.join("passwd"), &with_carol).unwrap();
        fs::write(
            l.join("shadow"),
            format!("{}carol:$c:20100::::::\n", shadow.replace("$a1:20000", "$a2:20100")),
        )
        .unwrap();
        let mut merge = merge(&l, &t, &mut state, &[]);
        merge.pass(None, None);
        assert!(merge.state.clash.as_deref().is_some_and(|c| c.contains("UID 960")));
        assert!(!fs::read_to_string(t.join("passwd")).unwrap().contains("carol"));
        assert_eq!(
            fs::read_to_string(t.join("shadow")).unwrap(),
            "root:$r:20000::::::\nalice:$a2:20100:0:99999:7:::\npkguser:!*:20050::::::\n"
        );
        fs::write(l.join("passwd"), with_carol.replace("960:960", "1002:1002")).unwrap();
        merge.pass(None, None);
        assert_eq!(merge.state.clash, None);
        let shadow = fs::read_to_string(t.join("shadow")).unwrap();
        assert!(
            shadow.contains("alice:$a2:20100:") && shadow.contains("carol:$c:"),
            "{shadow}"
        );
    }

    #[test]
    fn running_system_edits_to_the_fstab_and_crypttab_archix_wrote_are_merged_line_by_line_and_warned_about() {
        let dir = tempfile::tempdir().unwrap();
        let (l, t) = (dir.path().join("live"), dir.path().join("theirs"));
        let header = fstab::HEADER;
        let fstab = format!("{header}\nUUID=root / ext4 rw 0 1\nUUID=data /mnt/data ext4 nofail 0 2\n");
        let crypttab = "# Written by archix from file_systems' encrypted devices.\nsecret UUID=s none luks,nofail\n";
        for etc in [&l, &t] {
            fs::create_dir_all(etc).unwrap();
            fs::write(etc.join("fstab"), &fstab).unwrap();
            fs::write(etc.join("crypttab"), crypttab).unwrap();
        }
        let mut state = snapshot(&l, 2, 1, &BTreeSet::new());
        let new_fstab = format!("{fstab}UUID=new /mnt/new ext4 nofail 0 0\n");
        fs::write(t.join("fstab"), &new_fstab).unwrap();
        fs::write(t.join("crypttab"), format!("{crypttab}new UUID=n none luks,nofail\n")).unwrap();
        fs::write(l.join("fstab"), format!("{fstab}UUID=home /home ext4 rw 0 2\n")).unwrap();
        fs::write(l.join("crypttab"), format!("{crypttab}home UUID=h none luks\n")).unwrap();
        let mut merge = merge(&l, &t, &mut state, &["crypttab"]);
        merge.pass(None, None);
        assert_eq!(
            fs::read_to_string(t.join("fstab")).unwrap(),
            format!("{new_fstab}UUID=home /home ext4 rw 0 2\n"),
            "archix's new line and the running system's both reach the pending deployment"
        );
        assert_eq!(
            fs::read_to_string(t.join("crypttab")).unwrap(),
            format!("{crypttab}new UUID=n none luks,nofail\nhome UUID=h none luks\n"),
            "a crypttab the target's generation rewrote is merged too, never skipped"
        );
        assert!(!t.join("fstab.pacnew").exists() && merge.state.conflicts.is_empty());
        assert_eq!(
            merge.state.edited_tables,
            BTreeSet::from(["crypttab".to_string(), "fstab".to_string()])
        );
        fs::write(
            l.join("fstab"),
            format!("{header}\nUUID=root / ext4 rw 0 1\nUUID=home /home ext4 rw 0 2\nUUID=usb /mnt/usb ext4 rw 0 0\n"),
        )
        .unwrap();
        merge.pass(Some(&BTreeSet::from(["fstab".to_string()])), None);
        assert_eq!(
            fs::read_to_string(t.join("fstab")).unwrap(),
            format!(
                "{header}\nUUID=root / ext4 rw 0 1\nUUID=new /mnt/new ext4 nofail 0 0\nUUID=home /home ext4 rw 0 2\nUUID=usb /mnt/usb ext4 rw 0 0\n"
            ),
            "a second edit merges against the first: /mnt/data removed on the running system goes, archix's /mnt/new stays"
        );
    }

    #[test]
    fn a_pass_past_its_deadline_records_what_it_didnt_get_to() {
        let dir = tempfile::tempdir().unwrap();
        let (l, t) = (dir.path().join("live"), dir.path().join("theirs"));
        fs::create_dir_all(&l).unwrap();
        fs::create_dir_all(&t).unwrap();
        let mut state = snapshot(&l, 2, 1, &BTreeSet::new());
        fs::write(l.join("late"), "x").unwrap();
        let mut merge = merge(&l, &t, &mut state, &[]);
        let pass = merge.pass(None, Some(Instant::now()));
        assert_eq!(pass.unfinished, ["late", "passwd", "group", "shadow", "gshadow"]);
        assert!(!t.join("late").exists());
    }
}
