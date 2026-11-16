//! SCOPE 2.5 [B4]: what each transaction must prepare to, from the Plan, and the comparison that aborts it before commit on any difference.
use std::collections::{BTreeMap, BTreeSet};

use alpm::Alpm;

use crate::alpm::Prepared;
use crate::plan::{PkgAction, PkgReason, Plan};
use crate::value::Value;

/// archix never upgrades itself inside its own switch (SCOPE 2.5).
pub const SELF: [&str; 2] = ["archix", "archix-git"];

/// The detail plan() gives a removal libalpm's -Rs adds, not a target.
const RECURSED: &str = "only needed by removed packages";

/// What one transaction is planned to do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Expected {
    /// Repo packages to add, with their versions.
    pub add: BTreeMap<String, String>,
    /// AUR packages to add, by name, as their version is known only after the build.
    pub aur: BTreeSet<String>,
    pub remove: BTreeSet<String>,
    /// Installed packages the transaction may replace (planned conflict replacements).
    pub replaced: BTreeSet<String>,
}

/// The install transaction's expectation and the packages it leaves to the removal (and to the next switch, for archix itself).
pub fn install(plan: &Plan) -> (Expected, Vec<String>) {
    let mut expected = Expected::default();
    let mut skip = Vec::new();
    for p in &plan.packages {
        match (p.action, p.reason) {
            (PkgAction::Upgrade | PkgAction::Downgrade, _) if SELF.contains(&p.name.as_str()) => {
                skip.push(p.name.clone())
            }
            (PkgAction::Install | PkgAction::Upgrade | PkgAction::Downgrade, _) => {
                expected.add.insert(p.name.clone(), p.to.clone().unwrap_or_default());
            }
            (PkgAction::Remove, PkgReason::ConflictReplacement) => {
                expected.remove.insert(p.name.clone());
                expected.replaced.insert(p.name.clone());
            }
            (PkgAction::Remove, _) => skip.push(p.name.clone()),
            _ => {}
        }
    }
    expected.aur = plan.aur.iter().map(|a| a.name.clone()).collect();
    (expected, skip)
}

/// The removal transaction's targets (`-Rs` adds the rest) and its expectation.
pub fn removal(plan: &Plan) -> (Vec<String>, Expected) {
    let mut expected = Expected::default();
    let mut targets = Vec::new();
    for p in plan
        .packages
        .iter()
        .filter(|p| p.action == PkgAction::Remove && p.reason != PkgReason::ConflictReplacement)
    {
        expected.remove.insert(p.name.clone());
        if p.detail.as_deref() != Some(RECURSED) {
            targets.push(p.name.clone());
        }
    }
    (targets, expected)
}

/// The declared packages libalpm gets as sync targets, as plan() gives them: in a repo and not installed.
pub fn sync_targets(handle: &Alpm, config: &Value) -> Vec<String> {
    let declared: BTreeSet<String> = match config {
        Value::Map(map) => match map.get("packages") {
            Some(Value::List(items)) => items
                .iter()
                .filter_map(|i| match i {
                    Value::Str(s) => Some(s.clone()),
                    _ => None,
                })
                .collect(),
            _ => BTreeSet::new(),
        },
        _ => BTreeSet::new(),
    };
    declared
        .into_iter()
        .filter(|name| handle.localdb().pkg(name.as_str()).is_err())
        .filter(|name| handle.syncdbs().iter().any(|db| db.pkg(name.as_str()).is_ok()))
        .collect()
}

/// Every way `prepared` differs from `expected`; empty when the transaction is exactly the plan.
pub fn differences(expected: &Expected, prepared: &Prepared) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for added in &prepared.add {
        seen.insert(added.name.as_str());
        if expected.aur.contains(&added.name) {
            continue;
        }
        match expected.add.get(&added.name) {
            None => out.push(format!("libalpm would also install {} {}", added.name, added.version)),
            Some(version) if *version != added.version => out.push(format!(
                "libalpm would install {} {} where the plan has {version}",
                added.name, added.version
            )),
            Some(_) => {}
        }
    }
    for (name, version) in &expected.add {
        if !seen.contains(name.as_str()) {
            out.push(format!("the plan installs {name} {version}, which libalpm wouldn't"));
        }
    }
    for name in &expected.aur {
        if !seen.contains(name.as_str()) {
            out.push(format!(
                "the plan installs the AUR package {name}, which libalpm wouldn't"
            ));
        }
    }
    let removed: BTreeSet<&str> = prepared.remove.iter().map(|(n, _)| n.as_str()).collect();
    for name in removed.iter().filter(|n| !expected.remove.contains(**n)) {
        out.push(format!("libalpm would also remove {name}"));
    }
    for name in expected.remove.iter().filter(|n| !removed.contains(n.as_str())) {
        out.push(format!("the plan removes {name}, which libalpm wouldn't"));
    }
    for (old, new) in &prepared.replaced {
        if !expected.replaced.contains(old) {
            out.push(format!(
                "libalpm would replace {old} with {new}, which the plan doesn't"
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alpm::testdb::{TestRoot, pkg};
    use crate::alpm::{Answers, open, prepare_remove, prepare_sync_skipping};
    use crate::plan::{Header, PackageItem};

    fn plan(items: Vec<PackageItem>) -> Plan {
        let mut plan = Plan::new(Header {
            mode: crate::alpm::PinMode::Live,
            date: None,
            moved_from: None,
            fingerprint: String::new(),
            cached_dbs: false,
        });
        plan.packages = items;
        plan
    }

    fn item(action: PkgAction, name: &str, to: Option<&str>, reason: PkgReason, detail: Option<&str>) -> PackageItem {
        PackageItem {
            action,
            name: name.into(),
            from: None,
            to: to.map(String::from),
            repo: None,
            reason,
            detail: detail.map(String::from),
            risk: Vec::new(),
        }
    }

    fn system() -> TestRoot {
        let t = TestRoot::new();
        t.install(&[
            pkg("glibc", "2.40-1").dep(),
            pkg("zlib", "1.3-1").dep(),
            pkg("app", "1-1").deps(&["zlib"]),
            pkg("htop", "3-1").deps(&["ncurses"]),
            pkg("ncurses", "6-1").dep(),
        ]);
        t.repo(
            "core",
            &[
                pkg("glibc", "2.41-1"),
                pkg("ripgrep", "14-1").deps(&["pcre2"]),
                pkg("pcre2", "10-1"),
                pkg("zlib-ng-compat", "2.2-1")
                    .provides(&["zlib=1.3"])
                    .conflicts(&["zlib"])
                    .replaces(&["zlib"]),
            ],
        );
        t
    }

    fn the_plan() -> Plan {
        plan(vec![
            item(PkgAction::Install, "ripgrep", Some("14-1"), PkgReason::Declared, None),
            item(
                PkgAction::Install,
                "pcre2",
                Some("10-1"),
                PkgReason::Declared,
                Some("dependency"),
            ),
            item(PkgAction::Upgrade, "glibc", Some("2.41-1"), PkgReason::Declared, None),
            item(
                PkgAction::Install,
                "zlib-ng-compat",
                Some("2.2-1"),
                PkgReason::ConflictReplacement,
                Some("replaces zlib"),
            ),
            item(PkgAction::Remove, "zlib", None, PkgReason::ConflictReplacement, None),
            item(PkgAction::Remove, "htop", None, PkgReason::DriftInstalled, None),
            item(
                PkgAction::Remove,
                "ncurses",
                None,
                PkgReason::DriftInstalled,
                Some(RECURSED),
            ),
        ])
    }

    fn prepared(t: &TestRoot, plan: &Plan) -> (Prepared, Prepared) {
        let mut handle = open(&t.conf(&["core"]), &t.plan_dbpath()).unwrap();
        let (expected, skip) = install(plan);
        let sync = prepare_sync_skipping(
            &mut handle,
            &["ripgrep".to_string()],
            true,
            Answers::Only(expected.replaced.clone()),
            &skip,
        )
        .unwrap();
        let (targets, _) = removal(plan);
        let rm = prepare_remove(&mut handle, &targets).unwrap();
        (sync, rm)
    }

    #[test]
    fn a_transaction_that_is_the_plan_has_no_differences() {
        let t = system();
        let plan = the_plan();
        let (sync, rm) = prepared(&t, &plan);
        let (expected, skip) = install(&plan);
        assert_eq!(skip, ["htop", "ncurses"]);
        assert_eq!(differences(&expected, &sync), Vec::<String>::new());
        let (targets, expected) = removal(&plan);
        assert_eq!(targets, ["htop"], "-Rs takes ncurses with it");
        assert_eq!(differences(&expected, &rm), Vec::<String>::new());
    }

    #[test]
    fn any_extra_missing_or_other_version_package_is_a_difference() {
        let t = system();
        let (sync, _) = prepared(&t, &the_plan());
        let mut short = the_plan();
        short.packages.retain(|p| p.name != "pcre2");
        short.packages.iter_mut().find(|p| p.name == "glibc").unwrap().to = Some("2.42-1".into());
        short
            .packages
            .push(item(PkgAction::Install, "vim", Some("9-1"), PkgReason::Declared, None));
        let (expected, _) = install(&short);
        let diffs = differences(&expected, &sync);
        assert_eq!(
            diffs,
            [
                "libalpm would install glibc 2.41-1 where the plan has 2.42-1",
                "libalpm would also install pcre2 10-1",
                "the plan installs vim 9-1, which libalpm wouldn't",
            ]
        );
    }

    #[test]
    fn an_unplanned_removal_or_replacement_is_a_difference() {
        let t = system();
        let (sync, rm) = prepared(&t, &the_plan());
        let mut no_swap = the_plan();
        no_swap.packages.retain(|p| p.name != "zlib");
        let (expected, _) = install(&no_swap);
        let diffs = differences(&expected, &sync);
        assert!(
            diffs.contains(&"libalpm would also remove zlib".to_string()),
            "{diffs:?}"
        );
        assert!(
            diffs.contains(&"libalpm would replace zlib with zlib-ng-compat, which the plan doesn't".to_string()),
            "{diffs:?}"
        );
        let mut fewer = the_plan();
        fewer.packages.retain(|p| p.name != "ncurses");
        let (_, expected) = removal(&fewer);
        assert_eq!(differences(&expected, &rm), ["libalpm would also remove ncurses"]);
    }

    #[test]
    fn unplanned_replacements_are_answered_no() {
        let t = system();
        let mut handle = open(&t.conf(&["core"]), &t.plan_dbpath()).unwrap();
        let skipped = prepare_sync_skipping(&mut handle, &[], true, Answers::Only(BTreeSet::new()), &[]).unwrap();
        assert!(skipped.replaced.is_empty() && skipped.remove.is_empty(), "{skipped:?}");
        let (expected, _) = install(&the_plan());
        assert!(
            differences(&expected, &skipped).contains(&"the plan removes zlib, which libalpm wouldn't".to_string())
        );
        let target = ["zlib-ng-compat".to_string()];
        let refused = prepare_sync_skipping(&mut handle, &target, true, Answers::Only(BTreeSet::new()), &[]);
        assert!(
            refused.is_err(),
            "a conflict the plan didn't approve fails the prepare: {refused:?}"
        );
    }

    #[test]
    fn archix_itself_is_left_for_the_next_switch() {
        let p = plan(vec![item(
            PkgAction::Upgrade,
            "archix-git",
            Some("2-1"),
            PkgReason::Declared,
            None,
        )]);
        let (expected, skip) = install(&p);
        assert!(expected.add.is_empty());
        assert_eq!(skip, ["archix-git"]);
    }

    #[test]
    fn aur_packages_match_by_name() {
        let mut p = plan(Vec::new());
        p.aur.push(crate::plan::AurItem {
            base: None,
            name: "paru-bin".into(),
            commit: None,
            helper: None,
            reason: "declared".into(),
            risk: Vec::new(),
        });
        let (expected, _) = install(&p);
        let built = Prepared {
            add: vec![crate::alpm::Added {
                name: "paru-bin".into(),
                version: "2.0-1".into(),
                repo: String::new(),
                old_version: None,
            }],
            ..Prepared::default()
        };
        assert!(differences(&expected, &built).is_empty());
        assert_eq!(
            differences(&expected, &Prepared::default()),
            ["the plan installs the AUR package paru-bin, which libalpm wouldn't"]
        );
    }

    #[test]
    fn sync_targets_are_declared_uninstalled_repo_packages() {
        let t = system();
        let handle = open(&t.conf(&["core"]), &t.plan_dbpath()).unwrap();
        let config = Value::Map(BTreeMap::from([(
            "packages".to_string(),
            Value::List(["ripgrep", "app", "nonexistent"].map(|s| Value::Str(s.into())).to_vec()),
        )]));
        assert_eq!(sync_targets(&handle, &config), ["ripgrep"]);
    }
}
