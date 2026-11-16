use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use alpm::{Alpm, PackageReason};
use anyhow::Result;

use super::model::{AurItem, PackageItem, PkgAction, PkgReason};
use super::record::LastApplied;
use super::risk::{self, PkgFacts, RiskContext};
use crate::alpm::{Answers, Prepared, prepare_remove, prepare_sync, prepare_sync_skipping};

const BOOTLOADERS: [&str; 4] = ["limine", "grub", "refind", "systemd-boot"];
const SNAPSHOT_TOOLS: [&str; 3] = ["snapper", "snap-pac", "limine-snapper-sync"];

#[derive(Debug, Clone)]
pub struct LocalPkg {
    pub version: String,
    pub explicit: bool,
    pub files: Vec<String>,
    pub required_by: Vec<String>,
    pub in_repo: bool,
}

pub fn local_packages(handle: &Alpm) -> BTreeMap<String, LocalPkg> {
    let syncdbs = handle.syncdbs();
    handle
        .localdb()
        .pkgs()
        .iter()
        .map(|pkg| {
            let files = pkg
                .files()
                .files()
                .iter()
                .map(|f| String::from_utf8_lossy(f.name()).into_owned())
                .collect();
            let local = LocalPkg {
                version: pkg.version().to_string(),
                explicit: pkg.reason() == PackageReason::Explicit,
                files,
                required_by: pkg.required_by().iter().map(|s| s.to_string()).collect(),
                in_repo: syncdbs.iter().any(|db| db.pkg(pkg.name()).is_ok()),
            };
            (pkg.name().to_string(), local)
        })
        .collect()
}

#[derive(Debug, Clone, Default)]
pub struct PackageInputs<'a> {
    pub declared: BTreeSet<String>,
    pub ignored: BTreeSet<String>,
    pub remove_undeclared: bool,
    pub last: Option<&'a LastApplied>,
    /// Versions in the lock's own date's dbs when the plan moved the date forward.
    pub moved_from: Option<BTreeMap<String, String>>,
    /// File lists of repo packages from the .files dbs, for packages not installed yet.
    pub new_files: BTreeMap<String, Vec<String>>,
    pub running_kernel_release: Option<String>,
    pub risk: RiskContext,
    pub aur_commits: BTreeMap<String, String>,
    /// Declared users' login shells, and the /etc/shells the switch leaves.
    pub shells: Vec<(String, String)>,
    pub etc_shells: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct PackageGroup {
    pub items: Vec<PackageItem>,
    pub aur: Vec<AurItem>,
    pub refusals: Vec<String>,
    pub notes: Vec<String>,
    pub install: Prepared,
    pub removal: Prepared,
    /// Each package's files after the plan, for hooks and refusals.
    pub final_files: BTreeMap<String, Vec<String>>,
    pub implicit: BTreeSet<String>,
}

pub fn running_kernel(local: &BTreeMap<String, LocalPkg>, release: Option<&str>) -> Option<String> {
    let vmlinuz = format!("usr/lib/modules/{}/vmlinuz", release?);
    local
        .iter()
        .find(|(_, p)| p.files.contains(&vmlinuz))
        .map(|(n, _)| n.clone())
}

/// SCOPE 2.6's implicit declarations among what's installed.
pub fn implicit(local: &BTreeMap<String, LocalPkg>, inputs: &PackageInputs) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for (name, pkg) in local {
        let tool = matches!(name.as_str(), "archix" | "archix-git" | "pacman")
            || inputs.risk.helper.as_deref() == Some(name.as_str())
            || BOOTLOADERS.contains(&name.as_str())
            || SNAPSHOT_TOOLS.contains(&name.as_str());
        if tool || risk::is_keyring(&pkg.files) || risk::is_mirrorlist(name, &pkg.files) {
            out.insert(name.clone());
        }
    }
    out.extend(running_kernel(local, inputs.running_kernel_release.as_deref()));
    out
}

fn files_of(name: &str, local: &BTreeMap<String, LocalPkg>, new: &BTreeMap<String, Vec<String>>) -> Vec<String> {
    let mut files: BTreeSet<String> = local
        .get(name)
        .map(|p| p.files.iter().cloned().collect())
        .unwrap_or_default();
    files.extend(new.get(name).into_iter().flatten().cloned());
    files.into_iter().collect()
}

fn removal_reason(name: &str, last: Option<&LastApplied>) -> PkgReason {
    match last {
        Some(last) if last.declared.contains(name) || last.installed.contains(name) => PkgReason::NoLongerDeclared,
        _ => PkgReason::DriftInstalled,
    }
}

/// The packages group: declared packages against the real local db, resolved by libalpm prepare (sync, then -Rs).
pub fn build(handle: &mut Alpm, inputs: &PackageInputs) -> Result<PackageGroup> {
    let local = local_packages(handle);
    let implicit = implicit(&local, inputs);
    let mut group = PackageGroup {
        implicit: implicit.clone(),
        ..PackageGroup::default()
    };
    let in_sync = |name: &str| handle.syncdbs().iter().any(|db| db.pkg(name).is_ok());

    let mut targets = Vec::new();
    for name in &inputs.declared {
        match (local.get(name), in_sync(name)) {
            (None, true) => targets.push(name.clone()),
            (None, false) => group.aur.push(AurItem {
                name: name.clone(),
                commit: inputs.aur_commits.get(name).cloned(),
                helper: inputs.risk.helper.clone(),
                reason: "declared, in no repo: built from the AUR".into(),
                risk: Vec::new(),
            }),
            (Some(pkg), _) if !pkg.explicit => group.items.push(PackageItem {
                action: PkgAction::MarkExplicit,
                name: name.clone(),
                from: None,
                to: None,
                repo: None,
                reason: PkgReason::Declared,
                detail: None,
                risk: Vec::new(),
            }),
            (Some(_), _) => {}
        }
    }

    let first = prepare_sync(handle, &targets, true, Answers::Record)?;
    let gone_in_install: BTreeSet<&str> = first.remove.iter().map(|(n, _)| n.as_str()).collect();
    let mut candidates: BTreeSet<String> = BTreeSet::new();
    if inputs.remove_undeclared {
        for (name, pkg) in &local {
            if pkg.explicit
                && !inputs.declared.contains(name)
                && !implicit.contains(name)
                && !inputs.ignored.contains(name)
                && !gone_in_install.contains(name.as_str())
            {
                candidates.insert(name.clone());
            }
        }
        loop {
            let needed: Vec<String> = candidates
                .iter()
                .filter(|name| local[*name].required_by.iter().any(|by| !candidates.contains(by)))
                .cloned()
                .collect();
            if needed.is_empty() {
                break;
            }
            for name in needed {
                candidates.remove(&name);
                group.items.push(PackageItem {
                    action: PkgAction::MarkDependency,
                    name: name.clone(),
                    from: None,
                    to: None,
                    repo: None,
                    reason: removal_reason(&name, inputs.last),
                    detail: Some("not declared, but other packages need it".into()),
                    risk: Vec::new(),
                });
            }
        }
    } else {
        let undeclared = local
            .iter()
            .filter(|(n, p)| {
                p.explicit && !inputs.declared.contains(*n) && !implicit.contains(*n) && !inputs.ignored.contains(*n)
            })
            .count();
        if undeclared > 0 {
            group.notes.push(format!(
                "{undeclared} undeclared explicit packages kept (remove_undeclared = false)"
            ));
        }
    }
    let rm_targets: Vec<String> = candidates.iter().cloned().collect();
    let removal = if rm_targets.is_empty() {
        Prepared::default()
    } else {
        match prepare_remove(handle, &rm_targets) {
            Ok(prepared) => prepared,
            Err(e) => {
                group.refusals.push(format!("removing undeclared packages: {e:#}"));
                Prepared::default()
            }
        }
    };
    for (name, version) in &removal.remove {
        let target = candidates.contains(name);
        group.items.push(PackageItem {
            action: PkgAction::Remove,
            name: name.clone(),
            from: Some(version.clone()),
            to: None,
            repo: None,
            reason: removal_reason(name, inputs.last),
            detail: (!target).then(|| "only needed by removed packages".into()),
            risk: Vec::new(),
        });
    }

    let skip: Vec<String> = removal.remove.iter().map(|(n, _)| n.clone()).collect();
    let install = match skip.is_empty() {
        true => first,
        false => prepare_sync_skipping(handle, &targets, true, Answers::Record, &skip).unwrap_or(first),
    };
    let replacing: BTreeMap<&str, &str> = install
        .replaced
        .iter()
        .map(|(old, new)| (new.as_str(), old.as_str()))
        .collect();
    for added in &install.add {
        let (action, reason, detail) = match &added.old_version {
            None => {
                let action = PkgAction::Install;
                if let Some(old) = replacing.get(added.name.as_str()) {
                    (action, PkgReason::ConflictReplacement, Some(format!("replaces {old}")))
                } else if inputs.last.is_some_and(|l| l.installed.contains(&added.name)) {
                    (action, PkgReason::DriftRemoved, None)
                } else if inputs.declared.contains(&added.name) {
                    (action, PkgReason::Declared, None)
                } else {
                    (action, PkgReason::Declared, Some("dependency".into()))
                }
            }
            Some(old) => {
                let action = match alpm::vercmp(added.version.as_str(), old.as_str()) {
                    Ordering::Less => PkgAction::Downgrade,
                    _ => PkgAction::Upgrade,
                };
                let moved = inputs.moved_from.as_ref().is_some_and(|at_lock| {
                    at_lock
                        .get(&added.name)
                        .is_none_or(|v| alpm::vercmp(v.as_str(), old.as_str()) != Ordering::Greater)
                });
                match moved {
                    true => (action, PkgReason::LockMoved, None),
                    false => (action, PkgReason::Declared, None),
                }
            }
        };
        group.items.push(PackageItem {
            action,
            name: added.name.clone(),
            from: added.old_version.clone(),
            to: Some(added.version.clone()),
            repo: Some(added.repo.clone()).filter(|r| !r.is_empty()),
            reason,
            detail,
            risk: Vec::new(),
        });
    }
    for (name, version) in &install.remove {
        group.items.push(PackageItem {
            action: PkgAction::Remove,
            name: name.clone(),
            from: Some(version.clone()),
            to: None,
            repo: None,
            reason: PkgReason::ConflictReplacement,
            detail: install.replaced.get(name).map(|new| format!("replaced by {new}")),
            risk: Vec::new(),
        });
    }

    let removed: BTreeSet<&str> = install
        .remove
        .iter()
        .chain(&removal.remove)
        .map(|(n, _)| n.as_str())
        .collect();
    for name in local.keys().filter(|n| !removed.contains(n.as_str())) {
        group
            .final_files
            .insert(name.clone(), files_of(name, &local, &inputs.new_files));
    }
    for added in &install.add {
        group
            .final_files
            .insert(added.name.clone(), files_of(&added.name, &local, &inputs.new_files));
    }

    for item in &mut group.items {
        let facts = PkgFacts {
            name: item.name.clone(),
            files: files_of(&item.name, &local, &inputs.new_files),
            swap: item.action == PkgAction::Downgrade || item.reason == PkgReason::ConflictReplacement,
        };
        item.risk = risk::package_risks(&facts, &inputs.risk);
    }
    group
        .items
        .sort_by(|a, b| (a.action, &a.name).cmp(&(b.action, &b.name)));
    group.refusals.extend(refusals(&local, &group, &removed, inputs));
    group.install = install;
    group.removal = removal;
    Ok(group)
}

fn refusals(
    local: &BTreeMap<String, LocalPkg>,
    group: &PackageGroup,
    removed: &BTreeSet<&str>,
    inputs: &PackageInputs,
) -> Vec<String> {
    let mut out = Vec::new();
    let replaced = &group.install.replaced;
    let kernels = risk::kernels(&group.final_files);
    if kernels.is_empty() {
        out.push("no kernel would be installed".into());
    }
    if let Some(running) = running_kernel(local, inputs.running_kernel_release.as_deref())
        && removed.contains(running.as_str())
        && kernels.keys().all(|k| *k == running)
    {
        out.push(format!(
            "the running kernel's package {running} would be removed without another kernel"
        ));
    }
    for name in removed.iter().filter(|n| !replaced.contains_key(**n)) {
        let pkg = &local[*name];
        let essential = if matches!(*name, "pacman" | "archix" | "archix-git") {
            Some("")
        } else if inputs.risk.helper.as_deref() == Some(*name) {
            Some(" (the AUR helper)")
        } else if risk::is_keyring(&pkg.files) {
            Some(" (a keyring)")
        } else if risk::is_mirrorlist(name, &pkg.files) {
            Some(" (a mirrorlist)")
        } else {
            None
        };
        if let Some(what) = essential {
            out.push(format!("{name}{what} would be removed"));
        }
    }
    for (user, shell) in &inputs.shells {
        let file = shell.trim_start_matches('/');
        if let Some((owner, _)) = local.iter().find(|(_, p)| p.files.iter().any(|f| f == file))
            && removed.contains(owner.as_str())
        {
            out.push(format!("{user}'s login shell {shell} would be removed with {owner}"));
        }
        if let Some(shells) = &inputs.etc_shells
            && !shells.lines().any(|line| line.trim() == shell)
        {
            out.push(format!("{user}'s login shell {shell} is not in /etc/shells"));
        }
    }
    out.extend(risk::dkms_refusals(&group.final_files));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alpm::testdb::{TestRoot, pkg};
    use crate::alpm::{open, prepare_remove};

    const KERNEL: &[&str] = &["usr/lib/modules/6.1-arch1/vmlinuz"];

    fn base(t: &TestRoot) {
        t.install(&[
            pkg("linux", "6.1-1").files(KERNEL),
            pkg("pacman", "7.1-1").files(&["usr/bin/pacman"]),
            pkg("bash", "5.2-1").files(&["usr/bin/bash"]),
        ]);
    }

    fn inputs<'a>(declared: &[&str]) -> PackageInputs<'a> {
        PackageInputs {
            declared: declared.iter().map(|s| s.to_string()).collect(),
            remove_undeclared: true,
            running_kernel_release: Some("6.1-arch1".into()),
            ..PackageInputs::default()
        }
    }

    fn find<'a>(g: &'a PackageGroup, name: &str) -> &'a PackageItem {
        g.items
            .iter()
            .find(|i| i.name == name)
            .unwrap_or_else(|| panic!("{name} not in {:?}", g.items))
    }

    #[test]
    fn installs_upgrades_and_marks_with_reasons() {
        let t = TestRoot::new();
        base(&t);
        t.install(&[pkg("vim", "9.0-1").dep(), pkg("glibc", "2.40-1").dep()]);
        t.repo(
            "core",
            &[
                pkg("linux", "6.1-1").files(KERNEL),
                pkg("bash", "5.2-1"),
                pkg("pacman", "7.1-1"),
                pkg("vim", "9.0-1"),
                pkg("glibc", "2.41-1"),
                pkg("zlib", "1.3-1"),
                pkg("curl", "8-1").deps(&["zlib"]),
            ],
        );
        let mut handle = open(&t.conf(&["core"]), &t.plan_dbpath()).unwrap();
        let g = build(&mut handle, &inputs(&["linux", "bash", "vim", "curl"])).unwrap();
        assert_eq!(find(&g, "curl").action, PkgAction::Install);
        assert_eq!(find(&g, "curl").reason, PkgReason::Declared);
        assert_eq!(find(&g, "zlib").detail.as_deref(), Some("dependency"));
        assert_eq!(find(&g, "glibc").action, PkgAction::Upgrade);
        assert_eq!(find(&g, "vim").action, PkgAction::MarkExplicit);
        assert!(
            !g.items.iter().any(|i| i.name == "pacman"),
            "pacman is implicitly declared"
        );
        assert!(g.refusals.is_empty(), "{:?}", g.refusals);
    }

    #[test]
    fn undeclared_explicit_packages_are_drift_or_no_longer_declared() {
        let t = TestRoot::new();
        base(&t);
        t.install(&[
            pkg("htop", "3-1"),
            pkg("old", "1-1").deps(&["libold"]),
            pkg("libold", "1-1").dep(),
            pkg("needed", "1-1"),
            pkg("user-of-needed", "1-1").dep().deps(&["needed"]),
        ]);
        t.repo("core", &[pkg("htop", "3-1")]);
        let mut handle = open(&t.conf(&["core"]), &t.plan_dbpath()).unwrap();
        let declared = ["linux", "bash", "user-of-needed"];
        let g = build(&mut handle, &inputs(&declared)).unwrap();
        assert_eq!(find(&g, "htop").reason, PkgReason::DriftInstalled);
        assert_eq!(
            find(&g, "libold").detail.as_deref(),
            Some("only needed by removed packages")
        );
        assert_eq!(find(&g, "needed").action, PkgAction::MarkDependency);

        let last = LastApplied {
            declared: ["old".to_string()].into(),
            installed: ["old".to_string(), "libold".to_string()].into(),
            ..LastApplied::default()
        };
        let with_record = PackageInputs {
            last: Some(&last),
            ..inputs(&declared)
        };
        let g = build(&mut handle, &with_record).unwrap();
        assert_eq!(find(&g, "old").reason, PkgReason::NoLongerDeclared);
        assert_eq!(find(&g, "htop").reason, PkgReason::DriftInstalled);
    }

    #[test]
    fn a_hand_removed_package_comes_back_as_drift_removed() {
        let t = TestRoot::new();
        base(&t);
        t.repo("core", &[pkg("git", "2-1")]);
        let last = LastApplied {
            installed: ["git".to_string()].into(),
            ..LastApplied::default()
        };
        let mut handle = open(&t.conf(&["core"]), &t.plan_dbpath()).unwrap();
        let g = build(
            &mut handle,
            &PackageInputs {
                last: Some(&last),
                ..inputs(&["git"])
            },
        )
        .unwrap();
        assert_eq!(find(&g, "git").reason, PkgReason::DriftRemoved);
    }

    #[test]
    fn lock_moved_upgrades_are_labelled() {
        let t = TestRoot::new();
        base(&t);
        t.install(&[pkg("glibc", "2.40-1").dep(), pkg("zlib", "1.2-1").dep()]);
        t.repo("core", &[pkg("glibc", "2.41-1"), pkg("zlib", "1.3-1")]);
        let at_lock = BTreeMap::from([
            ("glibc".to_string(), "2.40-1".to_string()),
            ("zlib".to_string(), "1.3-1".to_string()),
        ]);
        let mut handle = open(&t.conf(&["core"]), &t.plan_dbpath()).unwrap();
        let g = build(
            &mut handle,
            &PackageInputs {
                moved_from: Some(at_lock),
                ..inputs(&[])
            },
        )
        .unwrap();
        assert_eq!(find(&g, "glibc").reason, PkgReason::LockMoved);
        assert_eq!(find(&g, "zlib").reason, PkgReason::Declared);
    }

    #[test]
    fn conflict_replacement_is_planned_and_flagged() {
        let t = TestRoot::new();
        base(&t);
        t.install(&[
            pkg("zlib", "1.3-1").dep().files(&["usr/lib/libz.so.1"]),
            pkg("app", "1-1").deps(&["zlib"]),
        ]);
        t.repo(
            "cachyos",
            &[pkg("zlib-ng-compat", "2.2-1")
                .provides(&["zlib=1.3"])
                .conflicts(&["zlib"])
                .replaces(&["zlib"])],
        );
        let mut handle = open(&t.conf(&["cachyos"]), &t.plan_dbpath()).unwrap();
        let g = build(&mut handle, &inputs(&["linux", "bash", "app"])).unwrap();
        let new = find(&g, "zlib-ng-compat");
        assert_eq!(new.reason, PkgReason::ConflictReplacement);
        assert!(!new.risk.is_empty());
        let old = g
            .items
            .iter()
            .find(|i| i.name == "zlib" && i.action == PkgAction::Remove)
            .unwrap();
        assert_eq!(old.reason, PkgReason::ConflictReplacement);
    }

    #[test]
    fn hard_refusals() {
        let t = TestRoot::new();
        t.install(&[
            pkg("pacman", "7.1-1"),
            pkg("zsh", "5.9-1").files(&["usr/bin/zsh"]),
            pkg("nvidia-open-dkms", "560-1").files(&["usr/src/nvidia-560/dkms.conf"]),
        ]);
        let mut handle = open(&t.conf(&[]), &t.plan_dbpath()).unwrap();
        let mut i = inputs(&["nvidia-open-dkms"]);
        i.shells = vec![
            ("alice".into(), "/usr/bin/zsh".into()),
            ("bob".into(), "/usr/bin/fish".into()),
        ];
        i.etc_shells = Some("/bin/sh\n/usr/bin/zsh\n".into());
        let g = build(&mut handle, &i).unwrap();
        let all = g.refusals.join("\n");
        assert!(all.contains("no kernel would be installed"), "{all}");
        assert!(
            all.contains("alice's login shell /usr/bin/zsh would be removed with zsh"),
            "{all}"
        );
        assert!(all.contains("/usr/bin/fish is not in /etc/shells"), "{all}");
    }

    #[test]
    fn dkms_without_headers_refuses() {
        let t = TestRoot::new();
        base(&t);
        t.install(&[pkg("nvidia-open-dkms", "560-1").files(&["usr/src/nvidia-560/dkms.conf"])]);
        let mut handle = open(&t.conf(&[]), &t.plan_dbpath()).unwrap();
        let g = build(&mut handle, &inputs(&["linux", "bash", "nvidia-open-dkms"])).unwrap();
        assert!(g.refusals.iter().any(|r| r.contains("no headers")), "{:?}", g.refusals);
    }

    #[test]
    fn additive_mode_removes_nothing() {
        let t = TestRoot::new();
        base(&t);
        t.install(&[pkg("htop", "3-1")]);
        let mut handle = open(&t.conf(&[]), &t.plan_dbpath()).unwrap();
        let g = build(
            &mut handle,
            &PackageInputs {
                remove_undeclared: false,
                ..inputs(&["linux", "bash"])
            },
        )
        .unwrap();
        assert!(g.items.is_empty(), "{:?}", g.items);
        assert_eq!(g.notes.len(), 1);
    }

    #[test]
    fn declared_packages_in_no_repo_go_to_the_aur_group() {
        let t = TestRoot::new();
        base(&t);
        let mut handle = open(&t.conf(&[]), &t.plan_dbpath()).unwrap();
        let mut i = inputs(&["linux", "bash", "paru-bin"]);
        i.aur_commits.insert("paru-bin".into(), "abc123".into());
        let g = build(&mut handle, &i).unwrap();
        assert_eq!(g.aur.len(), 1);
        assert_eq!(g.aur[0].commit.as_deref(), Some("abc123"));
    }

    #[test]
    fn plan_add_and_remove_sets_equal_what_alpm_resolves() {
        let t = TestRoot::new();
        base(&t);
        t.install(&[
            pkg("htop", "3-1").deps(&["ncurses"]),
            pkg("ncurses", "6-1").dep(),
            pkg("glibc", "2.40-1").dep(),
        ]);
        t.repo(
            "core",
            &[
                pkg("glibc", "2.41-1"),
                pkg("ncurses", "6-2"),
                pkg("gcc-libs", "14-1").deps(&["glibc"]),
                pkg("ripgrep", "14-1").deps(&["gcc-libs"]),
            ],
        );
        let conf = t.conf(&["core"]);
        let mut handle = open(&conf, &t.plan_dbpath()).unwrap();
        let g = build(&mut handle, &inputs(&["linux", "bash", "ripgrep"])).unwrap();
        let mut fresh = open(&conf, &t.plan_dbpath()).unwrap();
        let rm = prepare_remove(&mut fresh, &["htop".into()]).unwrap();
        let skip: Vec<String> = rm.remove.iter().map(|(n, _)| n.clone()).collect();
        let sync = prepare_sync_skipping(&mut fresh, &["ripgrep".into()], true, Answers::Record, &skip).unwrap();
        let planned_add: BTreeSet<_> = g
            .items
            .iter()
            .filter(|i| matches!(i.action, PkgAction::Install | PkgAction::Upgrade | PkgAction::Downgrade))
            .map(|i| (i.name.clone(), i.to.clone().unwrap()))
            .collect();
        let alpm_add: BTreeSet<_> = sync.add.iter().map(|a| (a.name.clone(), a.version.clone())).collect();
        assert_eq!(planned_add, alpm_add);
        let planned_rm: BTreeSet<_> = g
            .items
            .iter()
            .filter(|i| i.action == PkgAction::Remove)
            .map(|i| i.name.clone())
            .collect();
        let alpm_rm: BTreeSet<_> = rm.remove.iter().map(|(n, _)| n.clone()).collect();
        assert_eq!(planned_rm, alpm_rm);
        assert_eq!(planned_rm, ["htop", "ncurses"].map(String::from).into());
        assert!(
            !planned_add.iter().any(|(n, _)| n == "ncurses"),
            "a package the removal takes isn't upgraded first"
        );
    }
}
