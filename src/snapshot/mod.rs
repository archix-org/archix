pub mod layout;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::generations::now;
use crate::lock::Locked;

/// archix's subvolume of its own, mounted here on the root, so a rollback of `@` keeps the snapshots and their records.
pub const MOUNT: &str = ".archix";
const RECORD: &str = "record.json";
const SNAPSHOT: &str = "root";

/// The snapshot record format this archix writes.
pub const FORMAT: u32 = 1;

/// What took a snapshot, always before its change: a switch, a plain pacman transaction archix's hook caught, you, or a keep that replaced this root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Switch,
    Pacman,
    Manual,
    Replaced,
    /// A kind a newer archix wrote; prune leaves it alone.
    #[serde(other)]
    Other,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Switch => "switch",
            Kind::Pacman => "pacman",
            Kind::Manual => "manual",
            Kind::Replaced => "replaced",
            Kind::Other => "other",
        }
    }
}

/// One snapshot of the root and what it contains, which is the system before the change that took it; every field after `number` has a default, so a record of any older format reads, and fields a newer archix added are ignored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    #[serde(default)]
    pub format: u32,
    pub number: u32,
    #[serde(default = "other")]
    pub kind: Kind,
    #[serde(default)]
    pub created: u64,
    /// The config tree applied in it, by git tree hash; none if archix never applied one there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree: Option<String>,
    /// The commit in root's config repository holding that tree, its lock and its plan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// The generation that commit is at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<u32>,
    /// The lock applied in it.
    #[serde(default)]
    pub packages: BTreeMap<String, Locked>,
    /// What came after it, such as "before switch to tree 3f2a" or "before pacman transaction".
    #[serde(default)]
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

fn other() -> Kind {
    Kind::Other
}

/// What a new snapshot records besides its number, kind and time: what it contains and what it was taken before.
#[derive(Debug, Clone, Default)]
pub struct Meta {
    pub tree: Option<String>,
    pub commit: Option<String>,
    pub generation: Option<u32>,
    pub packages: BTreeMap<String, Locked>,
    pub label: String,
    pub note: Option<String>,
}

/// How many snapshots prune keeps, from the config's `rollback` table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    pub keep_switch: u32,
    pub keep_pacman: u32,
    /// Below this percentage of free space, prune drops the oldest snapshots it may until there's more.
    pub min_free: u32,
    pub entries: u32,
}

impl Default for Policy {
    fn default() -> Policy {
        Policy {
            keep_switch: 10,
            keep_pacman: 10,
            min_free: 10,
            entries: 5,
        }
    }
}

impl Policy {
    /// The config's `rollback` table as a policy, none unless rollback.enable is set.
    pub fn from_config(config: &crate::value::Value) -> Result<Option<Policy>> {
        use crate::value::Value;
        let Value::Map(map) = config else { return Ok(None) };
        let Some(Value::Map(rollback)) = map.get("rollback") else {
            return Ok(None);
        };
        if rollback.get("enable") != Some(&Value::Bool(true)) {
            return Ok(None);
        }
        let defaults = Policy::default();
        let number = |key: &str, default: u32, max: u32| match rollback.get(key) {
            Some(Value::Int(n)) if (0..=i64::from(max)).contains(n) => Ok(*n as u32),
            Some(Value::Int(n)) => bail!("rollback.{key} is {n}, but it must be between 0 and {max}"),
            _ => Ok(default),
        };
        Ok(Some(Policy {
            keep_switch: number("keep_switch", defaults.keep_switch, 1000)?,
            keep_pacman: number("keep_pacman", defaults.keep_pacman, 1000)?,
            min_free: number("min_free", defaults.min_free, 90)?,
            entries: number("entries", defaults.entries, 100)?,
        }))
    }
}

/// What prune dropped, and what it couldn't free.
#[derive(Debug, Default, PartialEq)]
pub struct Pruned {
    pub deleted: Vec<u32>,
    pub warnings: Vec<String>,
}

/// The snapshots under `<root>/.archix/snapshots`, each `N/root` with its `N/record.json`.
pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn open(root: &Path) -> Store {
        Store {
            dir: root.join(MOUNT).join("snapshots"),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Snapshot `number`'s read-only subvolume.
    pub fn path(&self, number: u32) -> PathBuf {
        self.dir.join(number.to_string()).join(SNAPSHOT)
    }

    fn numbers(&self) -> Result<Vec<u32>> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", self.dir.display())),
        };
        let mut numbers: Vec<u32> = entries
            .flatten()
            .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
            .collect();
        numbers.sort_unstable();
        Ok(numbers)
    }

    /// Every snapshot with a record, oldest first; a number without one is a snapshot a crash cut short.
    pub fn list(&self) -> Result<Vec<Record>> {
        let mut records = Vec::new();
        for number in self.numbers()? {
            if let Some(record) = self.get(number)? {
                records.push(record);
            }
        }
        Ok(records)
    }

    pub fn get(&self, number: u32) -> Result<Option<Record>> {
        let path = self.dir.join(number.to_string()).join(RECORD);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let record = serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        Ok(Some(record))
    }

    /// Removes what a crash left between taking a snapshot and saving its record, which is never a snapshot anyone can roll back to.
    pub fn clean_leftovers(&self) -> Result<Vec<u32>> {
        let mut removed = Vec::new();
        for number in self.numbers()? {
            if !self.dir.join(number.to_string()).join(RECORD).exists() {
                self.remove_dir(number)?;
                removed.push(number);
            }
        }
        Ok(removed)
    }

    /// Takes a read-only snapshot of the subvolume at `source` as the next number, saving its record only once the snapshot is whole.
    pub fn create(&self, source: &Path, kind: Kind, meta: Meta) -> Result<Record> {
        self.clean_leftovers()?;
        let number = self.numbers()?.last().map_or(1, |last| last + 1);
        let dir = self.dir.join(number.to_string());
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        btrfs(
            Command::new("btrfs")
                .args(["-q", "subvolume", "snapshot", "-r"])
                .arg(source)
                .arg(dir.join(SNAPSHOT)),
            &format!("snapshot {} to {}", source.display(), dir.join(SNAPSHOT).display()),
        )?;
        crate::testhooks::crash_at("snapshot-before-record");
        let record = Record {
            format: FORMAT,
            number,
            kind,
            created: now(),
            tree: meta.tree,
            commit: meta.commit,
            generation: meta.generation,
            packages: meta.packages,
            label: meta.label,
            note: meta.note,
        };
        crate::durable::write(&dir.join(RECORD), serde_json::to_string_pretty(&record)?.as_bytes())?;
        Ok(record)
    }

    /// Drops snapshot `number`: its record first, so a crash leaves a leftover, never a record without its snapshot.
    pub fn delete(&self, number: u32) -> Result<()> {
        let record = self.dir.join(number.to_string()).join(RECORD);
        match fs::remove_file(&record) {
            Ok(()) => crate::durable::sync_dir(record.parent().unwrap_or(&self.dir))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("removing {}", record.display())),
        }
        self.remove_dir(number)
    }

    fn remove_dir(&self, number: u32) -> Result<()> {
        let dir = self.dir.join(number.to_string());
        let snapshot = dir.join(SNAPSHOT);
        if snapshot.exists() {
            btrfs(
                Command::new("btrfs").args(["-q", "subvolume", "delete"]).arg(&snapshot),
                &format!("delete {}", snapshot.display()),
            )?;
        }
        match fs::remove_dir_all(&dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("removing {}", dir.display())),
        }
    }

    /// Applies `policy` with `free_percent` measuring the root, such as crate::system::btrfs::free_percent: the newest keep_switch switch and keep_pacman pacman snapshots stay, manual and `pinned` ones always do; then, while less than min_free percent is free, the oldest it may drop go too, never the newest snapshot.
    pub fn prune(
        &self,
        policy: &Policy,
        pinned: &BTreeSet<u32>,
        free_percent: impl Fn() -> Option<f64>,
    ) -> Result<Pruned> {
        let records = self.list()?;
        let mut pruned = Pruned::default();
        for number in over_count(&records, policy, pinned) {
            self.delete(number)?;
            pruned.deleted.push(number);
        }
        let newest = records.last().map(|record| record.number);
        let mut candidates: Vec<u32> = records
            .iter()
            .filter(|record| matches!(record.kind, Kind::Switch | Kind::Pacman | Kind::Replaced))
            .filter(|record| !pinned.contains(&record.number))
            .map(|record| record.number)
            .filter(|number| Some(*number) != newest && !pruned.deleted.contains(number))
            .collect();
        candidates.reverse();
        while let Some(free) = free_percent().filter(|free| *free < f64::from(policy.min_free)) {
            let Some(number) = candidates.pop() else {
                pruned.warnings.push(format!(
                    "only {free:.0}% of the root filesystem is free, below rollback.min_free = {}%, and every snapshot left is manual, has a boot entry or is the newest; delete some with `archix snapshot delete N` or free space",
                    policy.min_free
                ));
                break;
            };
            self.delete(number)?;
            pruned.deleted.push(number);
            // btrfs frees a deleted subvolume's space in the background and counts it only once committed.
            btrfs(
                Command::new("btrfs").args(["-q", "subvolume", "sync"]).arg(&self.dir),
                "wait for deleted snapshots to be cleaned up",
            )?;
            btrfs(
                Command::new("btrfs").args(["filesystem", "sync"]).arg(&self.dir),
                "commit the freed space",
            )?;
        }
        Ok(pruned)
    }

    /// Makes snapshot `number` the root from the next boot under the name `@`, as fstab and the command line mount it: a writable clone with pacman's lock taken out, `prepare` run on it (accounts and /var/lib come later), then swapped with `@` in one RENAME_EXCHANGE; the root it replaces becomes a snapshot of kind replaced, recorded with `replaced`. `top` is the btrfs top level, mounted. Returns the replaced root's number.
    pub fn keep(
        &self,
        number: u32,
        top: &Path,
        replaced: Meta,
        prepare: impl FnOnce(&Path) -> Result<()>,
    ) -> Result<u32> {
        self.recover_keep(top)?;
        if self.get(number)?.is_none() {
            bail!("there's no snapshot {number}; `archix snapshot list` shows the ones there are");
        }
        let (root, clone) = (top.join(ROOT), top.join(KEEP_CLONE));
        if !root.is_dir() {
            bail!(
                "{} isn't there; rollbacks need the root on a subvolume named {ROOT}",
                root.display()
            );
        }
        btrfs(
            Command::new("btrfs")
                .args(["-q", "subvolume", "snapshot"])
                .arg(self.path(number))
                .arg(&clone),
            &format!("make a writable copy of snapshot {number} at {}", clone.display()),
        )?;
        // A pacman snapshot is taken while pacman holds its lock.
        match fs::remove_file(clone.join(layout::ON_ROOT).join("pacman/db.lck")) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(e).context("removing pacman's lock from the clone");
            }
            _ => {}
        }
        prepare(&clone)?;
        crate::durable::write(&clone.join(KEEP_MARK), b"")?;
        crate::durable::sync_filesystems_checked(&[&clone])?;
        let next = self.numbers()?.last().map_or(1, |last| last + 1);
        let journal = Keeping {
            from: number,
            replaced: next,
            meta: replaced.into(),
        };
        crate::durable::write(
            &self.dir.join(KEEP_JOURNAL),
            serde_json::to_string_pretty(&journal)?.as_bytes(),
        )?;
        crate::testhooks::crash_at("keep-before-exchange");
        exchange(&clone, &root)?;
        crate::durable::sync_dir(top)?;
        crate::testhooks::crash_at("keep-exchanged");
        self.finish_keep(top, journal)
    }

    /// Finishes or undoes a keep a crash cut short: before the exchange the clone is dropped, after it the replaced root is filed as its snapshot.
    pub fn recover_keep(&self, top: &Path) -> Result<()> {
        let journal_path = self.dir.join(KEEP_JOURNAL);
        let journal: Option<Keeping> = match fs::read_to_string(&journal_path) {
            Ok(text) => {
                Some(serde_json::from_str(&text).with_context(|| format!("parsing {}", journal_path.display()))?)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e).with_context(|| format!("reading {}", journal_path.display())),
        };
        let (root, clone) = (top.join(ROOT), top.join(KEEP_CLONE));
        match journal {
            Some(journal) if root.join(KEEP_MARK).exists() => {
                self.finish_keep(top, journal)?;
            }
            _ => {
                if clone.exists() {
                    btrfs(
                        Command::new("btrfs").args(["-q", "subvolume", "delete"]).arg(&clone),
                        &format!("delete the unfinished clone {}", clone.display()),
                    )?;
                }
                match fs::remove_file(&journal_path) {
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                        return Err(e).context("removing the keep journal");
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    fn finish_keep(&self, top: &Path, journal: Keeping) -> Result<u32> {
        let (root, clone) = (top.join(ROOT), top.join(KEEP_CLONE));
        let dir = self.dir.join(journal.replaced.to_string());
        if clone.exists() {
            // A rename can't cross mounts, so the move goes through the top level's own view of @archix.
            let in_top = top
                .join(layout::ARCHIX)
                .join("snapshots")
                .join(journal.replaced.to_string());
            fs::create_dir_all(&in_top).with_context(|| format!("creating {}", in_top.display()))?;
            // The replaced root stays writable: it's the running system until the reboot.
            fs::rename(&clone, in_top.join(SNAPSHOT))
                .with_context(|| format!("moving the replaced root to {}", in_top.join(SNAPSHOT).display()))?;
            crate::durable::sync_dir(&in_top)?;
            crate::durable::sync_dir(top)?;
        }
        crate::testhooks::crash_at("keep-replaced-moved");
        if !dir.join(RECORD).exists() {
            let record = Record {
                format: FORMAT,
                number: journal.replaced,
                kind: Kind::Replaced,
                created: now(),
                tree: journal.meta.tree,
                commit: journal.meta.commit,
                generation: journal.meta.generation,
                packages: journal.meta.packages,
                label: journal.meta.label,
                note: journal.meta.note,
            };
            crate::durable::write(&dir.join(RECORD), serde_json::to_string_pretty(&record)?.as_bytes())?;
        }
        match fs::remove_file(root.join(KEEP_MARK)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(e).context("removing the keep mark from the new root");
            }
            _ => {}
        }
        crate::durable::sync_dir(&root)?;
        record_root(self.dir.parent().unwrap_or(&self.dir), top)?;
        fs::remove_file(self.dir.join(KEEP_JOURNAL)).context("removing the keep journal")?;
        crate::durable::sync_dir(&self.dir)?;
        Ok(journal.replaced)
    }
}

/// The root's subvolume name, which keep preserves so fstab and the command line need no change.
pub const ROOT: &str = "@";
const KEEP_CLONE: &str = "@archix-keep";
/// In the clone from before the exchange, so after a crash the new root shows whether the exchange happened.
const KEEP_MARK: &str = ".archix-keep";
const KEEP_JOURNAL: &str = "keep.json";

/// A keep in progress: the snapshot becoming the root and the number the replaced root takes.
#[derive(Serialize, Deserialize)]
struct Keeping {
    from: u32,
    replaced: u32,
    meta: Saved,
}

#[derive(Serialize, Deserialize)]
struct Saved {
    tree: Option<String>,
    #[serde(default)]
    commit: Option<String>,
    #[serde(default)]
    generation: Option<u32>,
    packages: BTreeMap<String, Locked>,
    label: String,
    note: Option<String>,
}

impl From<Meta> for Saved {
    fn from(meta: Meta) -> Saved {
        Saved {
            tree: meta.tree,
            commit: meta.commit,
            generation: meta.generation,
            packages: meta.packages,
            label: meta.label,
            note: meta.note,
        }
    }
}

fn exchange(a: &Path, b: &Path) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c = |path: &Path| std::ffi::CString::new(path.as_os_str().as_bytes()).context("a path with a NUL byte");
    let (ca, cb) = (c(a)?, c(b)?);
    if unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            ca.as_ptr(),
            libc::AT_FDCWD,
            cb.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("swapping {} with {}", a.display(), b.display()));
    }
    Ok(())
}

/// The snapshots over their kind's count, oldest first; manual and pinned ones never are.
fn over_count(records: &[Record], policy: &Policy, pinned: &BTreeSet<u32>) -> Vec<u32> {
    let mut over = Vec::new();
    let kinds: [(&[Kind], u32); 2] = [
        (&[Kind::Switch, Kind::Replaced], policy.keep_switch),
        (&[Kind::Pacman], policy.keep_pacman),
    ];
    for (kind, keep) in kinds {
        let of_kind: Vec<u32> = records
            .iter()
            .filter(|record| kind.contains(&record.kind) && !pinned.contains(&record.number))
            .map(|record| record.number)
            .collect();
        over.extend(of_kind.iter().take(of_kind.len().saturating_sub(keep as usize)));
    }
    over.sort_unstable();
    over
}

/// Records the UUID of the subvolume `@` under the mounted top level `top` in `<archix>/root.json`, after setup and each keep, so a root some other tool put there can be told apart later.
pub fn record_root(archix: &Path, top: &Path) -> Result<String> {
    let output = Command::new("btrfs")
        .args(["subvolume", "show"])
        .arg(top.join(ROOT))
        .output()
        .context("running btrfs")?;
    let text = String::from_utf8_lossy(&output.stdout);
    let uuid = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("UUID:"))
        .map(|uuid| uuid.trim().to_string())
        .filter(|uuid| output.status.success() && !uuid.is_empty() && uuid != "-");
    let Some(uuid) = uuid else {
        bail!(
            "btrfs couldn't say {}'s UUID: {}",
            top.join(ROOT).display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    };
    crate::durable::write(
        &archix.join("root.json"),
        serde_json::to_string_pretty(&serde_json::json!({ "format": FORMAT, "uuid": uuid }))?.as_bytes(),
    )?;
    Ok(uuid)
}

/// Whether rollbacks can work on the root at `root`: btrfs, with `/` a subvolume other than the top level, which no snapshot can replace.
pub fn unsupported(root: &Path) -> Option<String> {
    if !crate::capture::is_btrfs(root) {
        return Some(format!(
            "{} isn't on btrfs, so archix can't snapshot it: rollbacks are off, and switches, config reverts and drift tracking work as usual",
            root.display()
        ));
    }
    match layout::subvolume_of(root) {
        Some(path) if path == "/" || path.is_empty() => Some(format!(
            "{} is btrfs's top level, not a subvolume, so a snapshot of it can't become the root; reinstall with an `@` subvolume, as CachyOS and archinstall lay it out",
            root.display()
        )),
        _ => None,
    }
}

fn btrfs(command: &mut Command, what: &str) -> Result<()> {
    let output = command.output().context("running btrfs")?;
    if !output.status.success() {
        bail!(
            "btrfs couldn't {what}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(number: u32, kind: Kind) -> Record {
        Record {
            format: FORMAT,
            number,
            kind,
            created: u64::from(number),
            tree: None,
            commit: None,
            generation: None,
            packages: BTreeMap::new(),
            label: String::new(),
            note: None,
        }
    }

    #[test]
    fn a_record_of_any_format_reads_older_ones_with_defaults_and_newer_ones_without_their_new_fields() {
        let oldest: Record = serde_json::from_str(r#"{"number": 3}"#).unwrap();
        assert_eq!(
            (oldest.format, oldest.number, oldest.kind, oldest.tree),
            (0, 3, Kind::Other, None)
        );
        let newer: Record = serde_json::from_str(
            r#"{"format": 9, "number": 4, "kind": "rebased", "tree": "ab", "plan": {"units": []}, "label": "before switch to tree cd"}"#,
        )
        .unwrap();
        assert_eq!(
            (newer.kind, newer.tree.as_deref(), newer.label.as_str()),
            (Kind::Other, Some("ab"), "before switch to tree cd")
        );
        let other = record(5, Kind::Other);
        let policy = Policy {
            keep_switch: 0,
            keep_pacman: 0,
            ..Policy::default()
        };
        assert!(
            over_count(&[other], &policy, &BTreeSet::new()).is_empty(),
            "prune leaves kinds it doesn't know alone"
        );
    }

    #[test]
    fn the_rollback_table_is_a_policy_only_when_enabled_and_its_numbers_are_checked() {
        use crate::value::Value;
        let config = |pairs: &[(&str, Value)]| {
            let table = pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
            Value::Map(BTreeMap::from([("rollback".to_string(), Value::Map(table))]))
        };
        assert_eq!(
            Policy::from_config(&config(&[
                ("enable", Value::Bool(false)),
                ("keep_switch", Value::Int(3))
            ]))
            .unwrap(),
            None
        );
        assert_eq!(
            Policy::from_config(&config(&[
                ("enable", Value::Bool(true)),
                ("keep_switch", Value::Int(3)),
                ("min_free", Value::Int(20))
            ]))
            .unwrap(),
            Some(Policy {
                keep_switch: 3,
                min_free: 20,
                ..Policy::default()
            })
        );
        assert!(Policy::from_config(&config(&[("enable", Value::Bool(true)), ("min_free", Value::Int(95))])).is_err());
    }

    #[test]
    fn each_kind_keeps_its_newest_and_manual_and_pinned_snapshots_always_stay() {
        let records: Vec<Record> = [
            (1, Kind::Switch),
            (2, Kind::Pacman),
            (3, Kind::Manual),
            (4, Kind::Switch),
            (5, Kind::Pacman),
            (6, Kind::Switch),
            (7, Kind::Pacman),
        ]
        .into_iter()
        .map(|(n, kind)| record(n, kind))
        .collect();
        let policy = Policy {
            keep_switch: 1,
            keep_pacman: 2,
            ..Policy::default()
        };
        assert_eq!(over_count(&records, &policy, &BTreeSet::new()), vec![1, 2, 4]);
        assert_eq!(over_count(&records, &policy, &BTreeSet::from([4])), vec![1, 2]);
        let none = Policy {
            keep_switch: 0,
            keep_pacman: 0,
            ..Policy::default()
        };
        assert_eq!(over_count(&records, &none, &BTreeSet::new()), vec![1, 2, 4, 5, 6, 7]);
    }
}
