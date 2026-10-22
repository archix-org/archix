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
    file.write_all(&serde_json::to_vec(value)?)?;
    file.sync_all()?;
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

/// A base taken from the /etc at `etc` as it is now.
pub fn snapshot(etc: &Path, target: u32, from: u32) -> State {
    let mut all = Vec::new();
    tree::walk(etc, "", &|rel| never(rel), &mut all);
    let base = all
        .into_iter()
        .filter(|rel| !FILES.contains(&rel.as_str()))
        .filter_map(|rel| Some((rel.clone(), tree::fingerprint(&etc.join(&rel)).ok()??)))
        .collect();
    State {
        target,
        from,
        base,
        accounts: ids::read_texts(etc),
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
        _ => snapshot(&paths.root.join("etc"), target, booted),
    };
    ids::record(&paths.state, &state.accounts)?;
    state = State {
        target,
        from: booted,
        base: state.base,
        accounts: state.accounts,
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

/// Starts the watcher after a commit made a deployment pending, through its unit or, on a system whose /etc predates it, as a transient one; a test root gets one only when ARCHIX_TEST_ETC_MERGE asks.
pub fn start(paths: &Paths) -> Result<()> {
    if pending(&paths.root).is_none() {
        return Ok(());
    }
    let test_root = paths.root != Path::new("/");
    if test_root && !(cfg!(debug_assertions) && std::env::var_os("ARCHIX_TEST_ETC_MERGE").is_some()) {
        return Ok(());
    }
    if !Path::new("/run/systemd/system").exists() {
        anyhow::bail!(
            "systemd isn't running, so nothing carries /etc changes into the new deployment until the reboot"
        );
    }
    let unit = unit_name(&paths.root);
    let installed = !test_root && Path::new("/etc/systemd/system").join(&unit).is_file();
    let mut command = std::process::Command::new(if installed { "systemctl" } else { "systemd-run" });
    if installed {
        command.args(["start", "--no-block", &unit]);
    } else {
        let active = std::process::Command::new("systemctl")
            .args(["is-active", "--quiet", &unit])
            .status()
            .is_ok_and(|s| s.success());
        if active {
            return Ok(());
        }
        command.args(["--no-block", "--quiet", "--collect", &format!("--unit={unit}")]);
        command.arg("--description=archix: carry /etc changes into the deployment that boots next");
        for property in UNIT_ORDER.iter().chain([&STOP_TIMEOUT]) {
            command.arg(format!("--property={property}"));
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
    Ok(())
}

/// Managed /etc files whose content the target's generation changed from the running one's; archix writes those into the target itself.
fn rewritten(paths: &Paths, from: u32, target: u32) -> BTreeSet<String> {
    let store = crate::generations::Store::new(paths);
    let (Ok(old), Ok(new)) = (store.get(from), store.get(target)) else {
        return BTreeSet::new();
    };
    let (old, new) = (old.system.files, new.system.files);
    old.keys()
        .chain(new.keys())
        .filter(|path| old.get(*path) != new.get(*path))
        .filter_map(|path| path.strip_prefix("/etc/").map(str::to_string))
        .collect()
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
}

impl Merge<'_> {
    fn skipped(&self, rel: &str) -> bool {
        never(rel) || self.skip.contains(rel)
    }

    /// Merges everything under the paths in `dirty`, or everything when `dirty` is None; stops at `deadline`, listing what's left.
    pub fn pass(&mut self, dirty: Option<&BTreeSet<String>>, deadline: Option<Instant>) -> Pass {
        let mut paths = BTreeSet::new();
        let mut accounts = dirty.is_none();
        let skip = |rel: &str| never(rel) || self.skip.contains(rel);
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
        if live == theirs {
            self.set_base(rel, live);
            return Ok(());
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
                    _ => tree::copy_file(&from, &to)?,
                })
            }
        };
        self.set_base(rel, written);
        let verb = if live.is_some() { "carried" } else { "removed" };
        if conflict {
            self.state.conflicts.insert(rel.to_string());
        } else {
            self.state.carried.insert(rel.to_string());
            println!("archix: /etc merge: {verb} /etc/{rel} into deployment {target}");
        }
        Ok(())
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
                    "archix: the /etc merge stopped carrying users and groups into deployment {target}: {clash}. Give the running system's account another number (usermod -u, groupmod -g) and the merge goes on; until then deployment {target} boots without the accounts added since the switch"
                ));
                self.state.clash = Some(clash);
                return;
            }
        };
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
                return;
            }
            self.state.carried.insert(file.to_string());
            self.state.failed.remove(*file);
            println!(
                "archix: /etc merge: carried the running system's changes to /etc/{file} into deployment {target}"
            );
        }
        if let Some(dir) = &self.state_dir {
            let _ = ids::record(dir, &live);
            let _ = ids::record(dir, &merged);
        }
        self.state.accounts = live;
        self.state.clash = None;
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
                    let mut state = snapshot(&b, 2, 1);
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
        let mut state = snapshot(&b, 2, 1);
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
        let mut state = snapshot(&b, 2, 1);
        merge(&l, &t, &mut state, &["managed"]).pass(None, None);
        for name in &names[..4] {
            assert_eq!(fs::read_to_string(t.join(name)).unwrap(), "theirs", "{name}");
        }
        assert_eq!(fs::read_to_string(t.join("machine-id")).unwrap(), "live");
        assert!(!t.join("machine-id.pacnew").exists());
        fs::write(b.join("machine-id"), "same").unwrap();
        fs::write(l.join("machine-id"), "same").unwrap();
        fs::write(t.join("machine-id"), "theirs").unwrap();
        let mut state = snapshot(&b, 2, 1);
        merge(&l, &t, &mut state, &[]).pass(None, None);
        assert_eq!(
            fs::read_to_string(t.join("machine-id")).unwrap(),
            "theirs",
            "unchanged live, left alone"
        );
    }

    #[test]
    fn a_pass_past_its_deadline_records_what_it_didnt_get_to() {
        let dir = tempfile::tempdir().unwrap();
        let (l, t) = (dir.path().join("live"), dir.path().join("theirs"));
        fs::create_dir_all(&l).unwrap();
        fs::create_dir_all(&t).unwrap();
        let mut state = snapshot(&l, 2, 1);
        fs::write(l.join("late"), "x").unwrap();
        let mut merge = merge(&l, &t, &mut state, &[]);
        let pass = merge.pass(None, Some(Instant::now()));
        assert_eq!(pass.unfinished, ["late", "passwd", "group", "shadow", "gshadow"]);
        assert!(!t.join("late").exists());
    }
}
