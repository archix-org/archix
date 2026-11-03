//! Whether moving the declared packages to another lock breaks packages archix doesn't track: their declared dependencies against that lock, and the sonames their binaries load.
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use crate::aur::srcinfo::dep_name;
use crate::pacman::parse_desc;
use crate::pacman::vercmp;

/// One installed package's dependencies and what it provides, from its local database entry.
struct Entry {
    version: String,
    depends: Vec<String>,
    provides: Vec<String>,
}

fn read(local_db: &Path) -> BTreeMap<String, Entry> {
    let mut entries = BTreeMap::new();
    for dir in fs::read_dir(local_db).into_iter().flatten().flatten() {
        let Ok(text) = fs::read_to_string(dir.path().join("desc")) else {
            continue;
        };
        let desc = parse_desc(&text);
        let first = |key: &str| desc.get(key).and_then(|values| values.first()).cloned();
        let Some(name) = first("NAME") else { continue };
        entries.insert(
            name,
            Entry {
                version: first("VERSION").unwrap_or_default(),
                depends: desc.get("DEPENDS").cloned().unwrap_or_default(),
                provides: desc.get("PROVIDES").cloned().unwrap_or_default(),
            },
        );
    }
    entries
}

/// Whether `version` meets the constraint in `dep`, such as `>=1.2` in `foo>=1.2`; a bare name always does.
fn meets(dep: &str, version: &str) -> bool {
    let rest = &dep[dep_name(dep).len()..];
    let (op, wanted) = ["<=", ">=", "<", ">", "="]
        .iter()
        .find_map(|op| rest.strip_prefix(op).map(|wanted| (*op, wanted)))
        .unwrap_or(("", ""));
    if op.is_empty() || wanted.is_empty() {
        return true;
    }
    let version = if wanted.contains('-') {
        version
    } else {
        version.split('-').next().unwrap_or(version)
    };
    let order = vercmp(version, wanted);
    match op {
        "<=" => order.is_le(),
        ">=" => order.is_ge(),
        "<" => order.is_lt(),
        ">" => order.is_gt(),
        _ => order.is_eq(),
    }
}

/// The untracked packages whose declared dependencies the target versions don't meet, each with why; `target` maps every package that stays installed to its version afterwards, untracked ones included.
pub fn dependency_breaks(local_db: &Path, untracked: &[String], target: &BTreeMap<String, String>) -> Vec<String> {
    let installed = read(local_db);
    let mut provided: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for (name, version) in target {
        provided
            .entry(name.clone())
            .or_default()
            .push((name.clone(), version.clone()));
        let Some(entry) = installed.get(name) else { continue };
        if &entry.version != version {
            continue;
        }
        for provide in &entry.provides {
            let virtual_version = provide.split_once('=').map_or(version.as_str(), |(_, v)| v);
            provided
                .entry(dep_name(provide).to_string())
                .or_default()
                .push((name.clone(), virtual_version.to_string()));
        }
    }
    let mut breaks = Vec::new();
    for package in untracked {
        let Some(entry) = installed.get(package) else { continue };
        for dep in &entry.depends {
            let name = dep_name(dep);
            if name.contains(".so") {
                continue;
            }
            match provided.get(name) {
                None => breaks.push(format!(
                    "{package} needs {dep}, which nothing provides after the switch"
                )),
                Some(providers) if !providers.iter().any(|(_, version)| meets(dep, version)) => {
                    let (owner, version) = &providers[0];
                    breaks.push(format!("{package} needs {dep}, but {owner} would be {version}"));
                }
                Some(_) => {}
            }
        }
    }
    breaks
}

/// The packages the drift policy keeps installed but untracked, in archix's state.
pub const UNTRACKED: &str = "untracked.json";

pub fn kept_untracked(state: &Path) -> BTreeSet<String> {
    fs::read_to_string(state.join(UNTRACKED))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Saves the untracked packages kept, writing only when they changed.
pub fn save_untracked(state: &Path, names: &BTreeSet<String>) -> anyhow::Result<()> {
    if *names == kept_untracked(state) {
        return Ok(());
    }
    crate::durable::write(&state.join(UNTRACKED), serde_json::to_string_pretty(names)?.as_bytes())
}

/// `roots` and every installed package they need, by name or by what it provides, from the local database.
pub fn closure(local_db: &Path, roots: &BTreeSet<String>) -> BTreeSet<String> {
    let installed = read(local_db);
    let mut by_provide: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (name, entry) in &installed {
        by_provide.entry(name).or_default().push(name);
        for provide in &entry.provides {
            by_provide.entry(dep_name(provide)).or_default().push(name);
        }
    }
    let mut found: BTreeSet<String> = BTreeSet::new();
    let mut queue: Vec<&str> = roots
        .iter()
        .map(String::as_str)
        .filter(|name| installed.contains_key(*name))
        .collect();
    while let Some(name) = queue.pop() {
        if !found.insert(name.to_string()) {
            continue;
        }
        for dep in &installed[name].depends {
            for provider in by_provide.get(dep_name(dep)).into_iter().flatten() {
                if !found.contains(*provider) {
                    queue.push(provider);
                }
            }
        }
    }
    found
}

/// The installed packages neither the lock nor the switch's removals cover, which stay as they are.
pub fn untracked(installed: &BTreeSet<String>, locked: &BTreeSet<String>, removed: &BTreeSet<String>) -> Vec<String> {
    installed
        .iter()
        .filter(|name| !locked.contains(*name) && !removed.contains(*name))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(db: &Path, name: &str, version: &str, depends: &[&str], provides: &[&str]) {
        let dir = db.join(format!("{name}-{version}"));
        fs::create_dir_all(&dir).unwrap();
        let mut desc = format!("%NAME%\n{name}\n\n%VERSION%\n{version}\n\n");
        if !depends.is_empty() {
            desc += &format!("%DEPENDS%\n{}\n\n", depends.join("\n"));
        }
        if !provides.is_empty() {
            desc += &format!("%PROVIDES%\n{}\n\n", provides.join("\n"));
        }
        fs::write(dir.join("desc"), desc).unwrap();
    }

    #[test]
    fn a_revert_that_takes_an_untracked_packages_dependency_below_its_floor_or_away_is_named() {
        let db = tempfile::TempDir::new().unwrap();
        entry(
            db.path(),
            "app",
            "1-1",
            &["libfoo>=2", "sh", "gone", "libbar.so=1-64"],
            &[],
        );
        entry(db.path(), "fine", "1-1", &["libfoo", "bash"], &[]);
        entry(db.path(), "libfoo", "2.1-1", &[], &[]);
        entry(db.path(), "bash", "5.2-1", &[], &["sh"]);
        let target = BTreeMap::from([
            ("libfoo".to_string(), "1.9-1".to_string()),
            ("bash".to_string(), "5.2-1".to_string()),
            ("app".to_string(), "1-1".to_string()),
            ("fine".to_string(), "1-1".to_string()),
        ]);
        let breaks = dependency_breaks(db.path(), &["app".to_string(), "fine".to_string()], &target);
        assert_eq!(
            breaks,
            [
                "app needs libfoo>=2, but libfoo would be 1.9-1",
                "app needs gone, which nothing provides after the switch",
            ]
        );
    }

    #[test]
    fn version_constraints_compare_like_pacman() {
        assert!(meets("foo>=1.2", "1.10-1"));
        assert!(!meets("foo<1.2", "1.10-1"));
        assert!(meets("foo=1.2-3", "1.2-3"));
        assert!(meets("foo=1.2", "1.2-7"));
        assert!(meets("foo", "0"));
    }
}
