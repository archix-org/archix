use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::model::Plan;

/// SCOPE 3.4's fingerprint inputs: each one a hash, so a saved plan names what moved.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Fingerprint {
    pub config_tree: String,
    pub lock: String,
    pub pacman_conf: String,
    pub sync_dbs: BTreeMap<String, String>,
    pub local_db: String,
    pub managed_files: BTreeMap<String, String>,
    pub archix_version: String,
    pub unchecked: Vec<String>,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl Fingerprint {
    pub fn digest(&self) -> String {
        let json = serde_json::to_vec(self).expect("a fingerprint serializes");
        hex(&Sha256::digest(&json))
    }
}

fn walk(dir: &Path, base: &Path, skip: &dyn Fn(&str) -> bool, out: &mut BTreeMap<String, Vec<u8>>) -> Result<()> {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .flatten()
        .collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        let rel = path.strip_prefix(base).unwrap_or(&path).to_string_lossy().into_owned();
        if skip(&rel) {
            continue;
        }
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            out.insert(rel, format!("link:{}", fs::read_link(&path)?.display()).into_bytes());
        } else if kind.is_dir() {
            walk(&path, base, skip, out)?;
        } else {
            out.insert(
                rel,
                fs::read(&path).with_context(|| format!("reading {}", path.display()))?,
            );
        }
    }
    Ok(())
}

/// A hash over every relative path and its content (symlinks by target) under `dir`, minus what `skip` names.
pub fn hash_tree(dir: &Path, skip: &dyn Fn(&str) -> bool) -> Result<String> {
    let mut files = BTreeMap::new();
    walk(dir, dir, skip, &mut files)?;
    let mut hasher = Sha256::new();
    for (path, content) in files {
        hasher.update((path.len() as u64).to_le_bytes());
        hasher.update(path.as_bytes());
        hasher.update((content.len() as u64).to_le_bytes());
        hasher.update(&content);
    }
    Ok(hex(&hasher.finalize()))
}

pub fn hash_config_tree(dir: &Path) -> Result<String> {
    hash_tree(dir, &|rel| rel == ".git" || rel.starts_with(".git/"))
}

pub fn hash_local_db(dbpath: &Path) -> Result<String> {
    hash_tree(&dbpath.join("local"), &|_| false)
}

pub fn hash_bytes(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

pub fn hash_file(path: &Path) -> String {
    fs::read(path).map_or_else(|_| "missing".into(), |b| hash_bytes(&b))
}

/// Each repo's sync db hash in a plan DBPath's sync/.
pub fn hash_sync_dbs(dbpath: &Path, repos: &[String]) -> BTreeMap<String, String> {
    repos
        .iter()
        .map(|repo| (repo.clone(), hash_file(&dbpath.join("sync").join(format!("{repo}.db")))))
        .collect()
}

fn moved_map(what: &str, saved: &BTreeMap<String, String>, now: &BTreeMap<String, String>, out: &mut Vec<String>) {
    for (name, hash) in now {
        match saved.get(name) {
            None => out.push(format!("{what} {name} is new since the plan")),
            Some(old) if old != hash => out.push(format!("{what} {name} changed since the plan")),
            Some(_) => {}
        }
    }
    for name in saved.keys().filter(|n| !now.contains_key(*n)) {
        out.push(format!("{what} {name} is gone since the plan"));
    }
}

/// What moved between a saved plan's fingerprint and one recomputed now; empty when the plan still holds.
pub fn compare(saved: &Fingerprint, now: &Fingerprint) -> Vec<String> {
    let mut out = Vec::new();
    let mut scalar = |what: &str, a: &str, b: &str| {
        if a != b {
            out.push(format!("{what} changed since the plan"));
        }
    };
    scalar("the config tree", &saved.config_tree, &now.config_tree);
    scalar("the lock", &saved.lock, &now.lock);
    scalar("the parsed pacman.conf", &saved.pacman_conf, &now.pacman_conf);
    scalar("the local package db", &saved.local_db, &now.local_db);
    if saved.archix_version != now.archix_version {
        out.push(format!(
            "archix changed from {} to {} since the plan",
            saved.archix_version, now.archix_version
        ));
    }
    moved_map("repo", &saved.sync_dbs, &now.sync_dbs, &mut out);
    moved_map("managed file", &saved.managed_files, &now.managed_files, &mut out);
    for item in &saved.unchecked {
        if now.unchecked.contains(item) {
            out.push(format!("still unchecked: {item}"));
        }
    }
    for item in now.unchecked.iter().filter(|i| !saved.unchecked.contains(i)) {
        out.push(format!("newly unchecked: {item}"));
    }
    out
}

/// What `diff --save` writes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedPlan {
    pub plan: Plan,
    pub fingerprint: Fingerprint,
}

impl SavedPlan {
    pub fn save(&self, path: &Path) -> Result<()> {
        let mut text = serde_json::to_string_pretty(self)?;
        text.push('\n');
        fs::write(path, text).with_context(|| format!("writing {}", path.display()))
    }

    pub fn load(path: &Path) -> Result<SavedPlan> {
        let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Fingerprint {
        Fingerprint {
            config_tree: "c".into(),
            lock: "l".into(),
            pacman_conf: "p".into(),
            sync_dbs: BTreeMap::from([("core".into(), "1".into()), ("cachyos-extra-znver4".into(), "2".into())]),
            local_db: "d".into(),
            managed_files: BTreeMap::from([("/etc/hosts".into(), "h".into())]),
            archix_version: "0.2.0".into(),
            unchecked: Vec::new(),
        }
    }

    #[test]
    fn an_unchanged_fingerprint_moves_nothing() {
        assert!(compare(&base(), &base()).is_empty());
        assert_eq!(base().digest(), base().digest());
    }

    #[test]
    fn each_input_is_named_when_it_moves() {
        let saved = base();
        let mut now = base();
        now.sync_dbs.insert("cachyos-extra-znver4".into(), "3".into());
        now.config_tree = "c2".into();
        now.managed_files.remove("/etc/hosts");
        now.archix_version = "0.3.0".into();
        let moved = compare(&saved, &now);
        assert_eq!(
            moved,
            [
                "the config tree changed since the plan",
                "archix changed from 0.2.0 to 0.3.0 since the plan",
                "repo cachyos-extra-znver4 changed since the plan",
                "managed file /etc/hosts is gone since the plan",
            ]
        );
        assert_ne!(saved.digest(), now.digest());
    }

    #[test]
    fn an_item_unchecked_at_diff_time_that_still_cannot_be_checked_refuses() {
        let mut saved = base();
        saved.unchecked = vec!["secret: not checked without root (/etc/wifi)".into()];
        let now = saved.clone();
        assert_eq!(
            compare(&saved, &now),
            ["still unchecked: secret: not checked without root (/etc/wifi)"]
        );
        let mut checked = saved.clone();
        checked.unchecked.clear();
        assert!(compare(&saved, &checked).is_empty());
    }

    #[test]
    fn tree_hash_covers_paths_contents_and_links_but_not_git() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("init.lua"), "return {}").unwrap();
        fs::create_dir_all(tmp.path().join(".git")).unwrap();
        let first = hash_config_tree(tmp.path()).unwrap();
        fs::write(tmp.path().join(".git/HEAD"), "x").unwrap();
        assert_eq!(hash_config_tree(tmp.path()).unwrap(), first);
        fs::write(tmp.path().join("init.lua"), "return { }").unwrap();
        let second = hash_config_tree(tmp.path()).unwrap();
        assert_ne!(second, first);
        std::os::unix::fs::symlink("init.lua", tmp.path().join("l")).unwrap();
        assert_ne!(hash_config_tree(tmp.path()).unwrap(), second);
    }

    #[test]
    fn saved_plans_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let plan = Plan::new(super::super::model::Header {
            mode: crate::alpm::PinMode::Live,
            date: None,
            moved_from: None,
            fingerprint: base().digest(),
            cached_dbs: false,
        });
        let saved = SavedPlan {
            plan,
            fingerprint: base(),
        };
        let path = tmp.path().join("plan.json");
        saved.save(&path).unwrap();
        assert_eq!(SavedPlan::load(&path).unwrap(), saved);
    }
}
