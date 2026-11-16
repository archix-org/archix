//! SCOPE 2.3's resolver parity against the real repos at the image snapshot: the plan's add/remove sets equal what pacman prints with the same generated config; run only in the Arch test VM via `tests/arch/run.sh`.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use archix::alpm::{self, PacmanConf};
use archix::date::Date;
use archix::plan::model::{PkgAction, PkgReason};
use archix::plan::packages::{self, PackageGroup, PackageInputs};
use common::*;

type Set = BTreeSet<(String, String)>;

/// A plan DBPath fetched for `conf`, with the generated pacman.conf written beside it; removed when dropped.
struct Repos {
    dir: PathBuf,
    conf: PacmanConf,
    conf_file: PathBuf,
}

impl Repos {
    fn at(date: Date) -> Repos {
        let archive = std::env::var("ARCHIX_TEST_ARCHIVE").unwrap_or_else(|_| archix::archive::ARCHIVE_URL.into());
        let conf = PacmanConf::read(None).unwrap().pinned(&archive, date);
        let dir = alpm::root_sync_dir(&format!("parity-test-{date}"));
        let _ = fs::remove_dir_all(&dir);
        alpm::plan_dbpath(&dir, &conf.db_path()).unwrap();
        let mut handle = alpm::open(&conf, &dir).unwrap();
        alpm::fetch(&mut handle).unwrap();
        let conf_file = dir.join("pacman.conf");
        fs::write(&conf_file, conf.render()).unwrap();
        Repos { dir, conf, conf_file }
    }

    fn handle(&self) -> ::alpm::Alpm {
        alpm::open(&self.conf, &self.dir).unwrap()
    }

    fn pacman(&self, args: &[&str]) -> Set {
        let out = Command::new("pacman")
            .arg("--config")
            .arg(&self.conf_file)
            .arg("--dbpath")
            .arg(&self.dir)
            .args(args)
            .args(["--noconfirm", "--print-format", "%n %v"])
            .output()
            .unwrap();
        let said = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(
            out.status.success(),
            "pacman {args:?}: {said}{}",
            String::from_utf8_lossy(&out.stderr)
        );
        said.lines().filter_map(package_line).collect()
    }
}

impl Drop for Repos {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// A `%n %v` line; pacman's provider questions and notices around them aren't.
fn package_line(line: &str) -> Option<(String, String)> {
    let mut parts = line.split(' ');
    let (name, version, rest) = (parts.next()?, parts.next()?, parts.next());
    let named = !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || "@._+-".contains(c));
    (rest.is_none() && named && version.contains('-') && !line.starts_with("::"))
        .then(|| (name.to_string(), version.to_string()))
}

fn kernel_release() -> String {
    fs::read_to_string("/proc/sys/kernel/osrelease")
        .unwrap()
        .trim()
        .to_string()
}

fn explicit(handle: &::alpm::Alpm) -> BTreeSet<String> {
    packages::local_packages(handle)
        .into_iter()
        .filter(|(_, p)| p.explicit)
        .map(|(n, _)| n)
        .collect()
}

fn plan(repos: &Repos, inputs: PackageInputs) -> PackageGroup {
    let mut handle = repos.handle();
    packages::build(
        &mut handle,
        &PackageInputs {
            running_kernel_release: Some(kernel_release()),
            ..inputs
        },
    )
    .unwrap()
}

fn added(group: &PackageGroup) -> Set {
    group
        .items
        .iter()
        .filter(|i| matches!(i.action, PkgAction::Install | PkgAction::Upgrade | PkgAction::Downgrade))
        .map(|i| (i.name.clone(), i.to.clone().unwrap()))
        .collect()
}

fn removed(group: &PackageGroup) -> Set {
    group
        .items
        .iter()
        .filter(|i| i.action == PkgAction::Remove)
        .map(|i| (i.name.clone(), i.from.clone().unwrap()))
        .collect()
}

/// The add set of a plan declaring what's explicit plus `targets`, against `pacman -Sup targets`.
fn install_parity(repos: &Repos, targets: &[String]) {
    let handle = repos.handle();
    let mut declared = explicit(&handle);
    declared.extend(targets.iter().cloned());
    let group = plan(
        repos,
        PackageInputs {
            declared,
            remove_undeclared: false,
            ..PackageInputs::default()
        },
    );
    assert!(group.refusals.is_empty(), "{:?}", group.refusals);
    let mut args = vec!["-Sup"];
    args.extend(targets.iter().map(String::as_str));
    let pacman = repos.pacman(&args);
    assert_eq!(added(&group), pacman, "plan vs pacman -Sup {targets:?}");
    for target in targets {
        assert!(pacman.iter().any(|(n, _)| n == target), "{target} isn't in {pacman:?}");
    }
    assert!(removed(&group).is_empty(), "{:?}", removed(&group));
}

fn snapshot() -> Date {
    IMAGE_SNAPSHOT.parse().unwrap()
}

#[test]
#[ignore]
fn a_few_installs_resolve_as_pacman_does() {
    let _system = lock_system();
    let repos = Repos::at(snapshot());
    let handle = repos.handle();
    let local = packages::local_packages(&handle);
    let in_sync = |n: &str| handle.syncdbs().iter().any(|db| db.pkg(n).is_ok());
    let targets: Vec<String> = ["tree", "figlet", "cowsay", "pv", "bc", "jq", "ncdu", "htop"]
        .into_iter()
        .filter(|n| !local.contains_key(*n) && in_sync(n))
        .take(3)
        .map(String::from)
        .collect();
    assert!(targets.len() >= 2, "too few uninstalled test packages: {targets:?}");
    install_parity(&repos, &targets);
}

#[test]
#[ignore]
fn an_install_choosing_among_providers_resolves_as_pacman_does() {
    let _system = lock_system();
    let repos = Repos::at(snapshot());
    let handle = repos.handle();
    let mut real: BTreeSet<String> = BTreeSet::new();
    let mut providers: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for db in handle.syncdbs() {
        for pkg in db.pkgs() {
            real.insert(pkg.name().to_string());
            for provided in pkg.provides() {
                providers
                    .entry(provided.name().to_string())
                    .or_default()
                    .insert(pkg.name().to_string());
            }
        }
    }
    let local = packages::local_packages(&handle);
    let mut local_provides: BTreeSet<String> = local.keys().cloned().collect();
    for pkg in handle.localdb().pkgs() {
        local_provides.extend(pkg.provides().iter().map(|p| p.name().to_string()));
    }
    let mut candidates: Vec<(String, String)> = Vec::new();
    for db in handle.syncdbs() {
        for pkg in db.pkgs() {
            if local.contains_key(pkg.name()) {
                continue;
            }
            let virtual_dep = pkg.depends().iter().map(|d| d.name().to_string()).find(|d| {
                !real.contains(d) && !local_provides.contains(d) && providers.get(d).is_some_and(|p| p.len() >= 2)
            });
            if let Some(dep) = virtual_dep {
                candidates.push((pkg.name().to_string(), dep));
            }
        }
    }
    candidates.sort();
    let (target, dep) = candidates
        .first()
        .cloned()
        .expect("a package depending on a virtual with several providers");
    println!(
        "EVIDENCE providers: {target} needs {dep}, provided by {:?}",
        providers[&dep]
    );
    install_parity(&repos, &[target]);
}

#[test]
#[ignore]
fn a_removal_with_dependencies_resolves_as_pacman_rs_does() {
    let _system = lock_system();
    let repos = Repos::at(snapshot());
    let handle = repos.handle();
    let local = packages::local_packages(&handle);
    let base = PackageInputs {
        running_kernel_release: Some(kernel_release()),
        ..PackageInputs::default()
    };
    let implicit = packages::implicit(&local, &base);
    let (target, pacman) = local
        .iter()
        .filter(|(n, p)| p.explicit && p.required_by.is_empty() && !implicit.contains(*n))
        .find_map(|(name, _)| {
            let set = repos.pacman(&["-Rsp", name]);
            (set.len() >= 2).then(|| (name.clone(), set))
        })
        .expect("an explicit package whose -Rs takes dependencies with it");
    let mut declared = explicit(&handle);
    declared.remove(&target);
    let group = plan(
        &repos,
        PackageInputs {
            declared,
            remove_undeclared: true,
            ..PackageInputs::default()
        },
    );
    println!("EVIDENCE removal: {target} takes {pacman:?}");
    assert_eq!(removed(&group), pacman, "plan vs pacman -Rsp {target}");
    assert!(
        group
            .items
            .iter()
            .any(|i| i.name == target && i.reason == PkgReason::DriftInstalled)
    );
    assert_eq!(
        added(&group),
        repos.pacman(&["-Sup"]),
        "the install side is a full upgrade at the snapshot"
    );
}

#[test]
#[ignore]
fn a_full_upgrade_where_the_lock_moved_resolves_as_pacman_syu_does() {
    let _system = lock_system();
    let at_lock = Repos::at(snapshot());
    let moved_from = alpm::sync_versions(&at_lock.handle());
    let later = Repos::at("2026-10-14".parse().unwrap());
    let handle = later.handle();
    let group = plan(
        &later,
        PackageInputs {
            declared: explicit(&handle),
            remove_undeclared: false,
            moved_from: Some(moved_from),
            ..PackageInputs::default()
        },
    );
    let pacman = later.pacman(&["-Sup"]);
    assert!(!pacman.is_empty(), "nothing to upgrade at 2026-10-14");
    assert_eq!(added(&group), pacman, "plan vs pacman -Sup at 2026-10-14");
    assert!(
        group.items.iter().any(|i| i.reason == PkgReason::LockMoved),
        "the upgrades are labelled lock moved: {:?}",
        group.items
    );
    let conflicts: Set = group
        .install
        .remove
        .iter()
        .map(|(n, v)| (n.clone(), v.clone()))
        .collect();
    assert_eq!(removed(&group), conflicts, "only conflict replacements are removed");
}

#[test]
fn package_lines_are_told_from_pacman_questions() {
    assert_eq!(package_line("tree 2.2.1-1"), Some(("tree".into(), "2.2.1-1".into())));
    assert_eq!(
        package_line("glibc 2:2.42-1"),
        Some(("glibc".into(), "2:2.42-1".into()))
    );
    assert_eq!(package_line(":: There are 2 providers available for sh:"), None);
    assert_eq!(package_line("   1) bash  2) dash"), None);
    assert_eq!(package_line("Enter a number (default=1): "), None);
}
