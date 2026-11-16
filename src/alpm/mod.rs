pub mod conf;
pub mod handle;
#[cfg(test)]
pub mod testdb;

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};

pub use conf::{PacmanConf, PinMode};
pub use handle::{
    Added, Answers, Prepared, fetch, fetch_files, open, plan_dbpath, prepare_remove, prepare_sync,
    prepare_sync_skipping, root_sync_dir, sync_versions, user_sync_dir, with_plan_dbpath_lock,
};

/// The file lists in a `<repo>.files` db (plain, gzip or zstd tar), by package name.
pub fn read_files_db(path: &Path) -> Result<BTreeMap<String, Vec<String>>> {
    let raw = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let bytes = if raw.starts_with(&[0x1f, 0x8b]) {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(&raw[..]).read_to_end(&mut out)?;
        out
    } else if raw.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        zstd::decode_all(&raw[..])?
    } else {
        raw
    };
    let mut names: BTreeMap<String, String> = BTreeMap::new();
    let mut lists: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut archive = tar::Archive::new(&bytes[..]);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_string_lossy().into_owned();
        let Some((dir, file)) = path.split_once('/') else {
            continue;
        };
        let mut text = String::new();
        if entry.read_to_string(&mut text).is_err() {
            continue;
        }
        let section = |key: &str| -> Vec<String> {
            let mut lines = text.lines();
            let header = format!("%{key}%");
            lines.by_ref().find(|l| *l == header);
            lines.take_while(|l| !l.is_empty()).map(str::to_string).collect()
        };
        match file {
            "desc" => {
                if let Some(name) = section("NAME").into_iter().next() {
                    names.insert(dir.to_string(), name);
                }
            }
            "files" => {
                lists.insert(dir.to_string(), section("FILES"));
            }
            _ => {}
        }
    }
    Ok(lists
        .into_iter()
        .filter_map(|(dir, files)| names.get(&dir).map(|name| (name.clone(), files)))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::testdb::{TestRoot, pkg};
    use super::*;

    #[test]
    fn prepare_resolves_dependencies_from_synthetic_dbs_without_a_lock() {
        let t = TestRoot::new();
        t.install(&[pkg("glibc", "2.40-1").dep(), pkg("old", "1-1")]);
        t.repo(
            "core",
            &[
                pkg("glibc", "2.41-1"),
                pkg("zlib", "1.3-1"),
                pkg("curl", "8.0-1").deps(&["zlib", "glibc>=2.40"]),
            ],
        );
        let conf = t.conf(&["core"]);
        let mut handle = open(&conf, &t.plan_dbpath()).unwrap();
        let prepared = prepare_sync(&mut handle, &["curl".into()], true, Answers::Record).unwrap();
        let names: Vec<_> = prepared.add.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["curl", "glibc", "zlib"]);
        let glibc = prepared.add.iter().find(|a| a.name == "glibc").unwrap();
        assert_eq!(glibc.old_version.as_deref(), Some("2.40-1"));
        assert_eq!(glibc.repo, "core");
        assert!(!t.real_dbpath().join("db.lck").exists());
        assert!(!t.plan_dbpath().join("db.lck").exists());
    }

    #[test]
    fn removal_is_recursive_like_pacman_rs() {
        let t = TestRoot::new();
        t.install(&[
            pkg("app", "1-1").deps(&["lib"]),
            pkg("lib", "1-1").dep(),
            pkg("shared", "1-1").dep(),
            pkg("other", "1-1").deps(&["shared"]),
            pkg("app2", "1-1").deps(&["shared"]),
        ]);
        let mut handle = open(&t.conf(&[]), &t.plan_dbpath()).unwrap();
        let prepared = prepare_remove(&mut handle, &["app".into(), "app2".into()]).unwrap();
        let names: Vec<_> = prepared.remove.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["app", "app2", "lib"]);
        assert!(prepare_remove(&mut handle, &["lib".into()]).is_err());
    }

    #[test]
    fn conflict_replacement_is_recorded_or_refused_as_planned() {
        let t = TestRoot::new();
        t.install(&[pkg("zlib", "1.3-1").dep(), pkg("app", "1-1").deps(&["zlib"])]);
        t.repo(
            "cachyos",
            &[pkg("zlib-ng-compat", "2.2-1")
                .provides(&["zlib=1.3"])
                .conflicts(&["zlib"])
                .replaces(&["zlib"])],
        );
        let conf = t.conf(&["cachyos"]);
        let mut handle = open(&conf, &t.plan_dbpath()).unwrap();
        let recorded = prepare_sync(&mut handle, &["zlib-ng-compat".into()], false, Answers::Record).unwrap();
        assert_eq!(
            recorded.replaced.get("zlib").map(String::as_str),
            Some("zlib-ng-compat")
        );
        assert_eq!(recorded.remove, [("zlib".to_string(), "1.3-1".to_string())]);
        let refused = prepare_sync(
            &mut handle,
            &["zlib-ng-compat".into()],
            false,
            Answers::Only(Default::default()),
        );
        assert!(refused.is_err(), "{refused:?}");
    }

    #[test]
    fn reads_file_lists_from_a_files_db() {
        let t = TestRoot::new();
        t.repo("core", &[pkg("linux", "6.1-1").files(&["usr/lib/modules/6.1/vmlinuz"])]);
        let lists = read_files_db(&t.dir.path().join("plan/sync/core.files")).unwrap();
        assert!(lists["linux"].contains(&"usr/lib/modules/6.1/vmlinuz".to_string()));
        assert!(lists["linux"].contains(&"usr/lib/modules/".to_string()));
    }
}
