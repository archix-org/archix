//! The plan's home group for home-manager users (SCOPE 7): the home.nix diff, the build's file changes, nix-first-install deferral and linger.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;

use super::model::{LineItem, PkgAction, Plan};
use super::udiff;
use crate::home::hm::generate::{Entry, Rendered, Secrets, render};
use crate::home::hm::{self, Account, HmLock, Refusal, flake_dir, update::is_rev};
use crate::value::Value;

/// The packages that bring the tools a home-manager build needs.
pub const TOOLS: [&str; 2] = ["nix", "home-manager"];

pub type Build = Box<dyn Fn(&str, &Account, &Rendered) -> Result<PathBuf>>;
pub type Accounts = Box<dyn Fn(&str) -> Option<Account>>;

/// What the home group reads from the machine, injected so tests stay pure.
pub struct HomeEnv {
    pub lock: HmLock,
    /// `diff --no-build`: skip the build, which leaves the plan incomplete (exit 4).
    pub no_build: bool,
    /// nix and home-manager can run now.
    pub tools: bool,
    pub account: Accounts,
    /// Whether logind already lingers the user.
    pub linger_enabled: Box<dyn Fn(&str) -> bool>,
    /// Builds the rendered flake in a scratch flake dir of the user's and returns the generation's store path.
    pub build: Build,
}

fn line(line: String, reason: &str) -> LineItem {
    LineItem {
        line,
        reason: reason.into(),
        diff: None,
        risk: Vec::new(),
    }
}

fn short(rev: &str) -> &str {
    &rev[..rev.len().min(12)]
}

/// The generation home-manager activated last, from its own gcroot or the older profile.
pub fn current_generation(home: &Path) -> Option<PathBuf> {
    [
        ".local/state/home-manager/gcroots/current-home",
        ".local/state/nix/profiles/home-manager",
    ]
    .iter()
    .find_map(|rel| home.join(rel).canonicalize().ok())
}

fn tree(dir: &Path, rel: &str, out: &mut BTreeMap<String, Vec<u8>>, depth: usize) -> Result<()> {
    if depth > 64 {
        anyhow::bail!("{} nests too deep", dir.display());
    }
    let mut names: Vec<_> = fs::read_dir(dir)?
        .map(|e| e.map(|e| e.file_name()))
        .collect::<std::io::Result<_>>()?;
    names.sort();
    for name in names {
        let path = dir.join(&name);
        let sub = match rel.is_empty() {
            true => name.to_string_lossy().into_owned(),
            false => format!("{rel}/{}", name.to_string_lossy()),
        };
        let meta = fs::metadata(&path)?;
        if meta.is_dir() {
            tree(&path, &sub, out, depth + 1)?;
        } else {
            use std::os::unix::fs::PermissionsExt;
            let mut id = fs::read(&path)?;
            id.push((meta.permissions().mode() & 0o111 != 0) as u8);
            out.insert(sub, id);
        }
    }
    Ok(())
}

/// Home files a new generation creates, changes or removes against the current one, by path under the home.
pub fn changed_home_files(old: Option<&Path>, new: &Path) -> Result<Vec<(String, &'static str)>> {
    let mut before = BTreeMap::new();
    if let Some(old) = old
        && old.join("home-files").is_dir()
    {
        tree(&old.join("home-files"), "", &mut before, 0)?;
    }
    let mut after = BTreeMap::new();
    if new.join("home-files").is_dir() {
        tree(&new.join("home-files"), "", &mut after, 0)?;
    }
    let mut out = Vec::new();
    for (path, id) in &after {
        match before.get(path) {
            None => out.push((path.clone(), "create")),
            Some(old) if old != id => out.push((path.clone(), "modify")),
            Some(_) => {}
        }
    }
    out.extend(
        before
            .keys()
            .filter(|path| !after.contains_key(*path))
            .map(|path| (path.clone(), "delete")),
    );
    out.sort();
    Ok(out)
}

/// Copied files that differ from the flake dir's files/ now.
fn changed_copies(dir: &Path, files: &[Entry]) -> Vec<String> {
    use std::os::unix::fs::PermissionsExt;
    let mut wanted = BTreeSet::new();
    let mut out = Vec::new();
    for entry in files {
        let Entry::File { rel, data, executable } = entry else {
            continue;
        };
        wanted.insert(rel.clone());
        let path = dir.join("files").join(rel);
        let same = fs::read(&path).is_ok_and(|have| have == *data)
            && fs::metadata(&path).is_ok_and(|m| (m.permissions().mode() & 0o111 != 0) == *executable);
        if !same {
            out.push(rel.clone());
        }
    }
    let mut on_disk = BTreeMap::new();
    if tree(&dir.join("files"), "", &mut on_disk, 0).is_ok() {
        out.extend(on_disk.into_keys().filter(|rel| !wanted.contains(rel)));
    }
    out.sort();
    out.dedup();
    out
}

/// Adds the home group's lines, refusals and unchecked items for every home.<user>.home_manager.
pub fn plan_home(config: &Value, config_dir: &Path, env: Option<&HomeEnv>, plan: &mut Plan) -> Result<()> {
    let users = hm::users(config);
    if users.is_empty() {
        return Ok(());
    }
    let Some(env) = env else {
        plan.unchecked
            .push("home: home-manager users aren't checked by this plan".into());
        return Ok(());
    };
    if !is_rev(&env.lock.home_manager) || !is_rev(&env.lock.nixpkgs) {
        plan.refusals
            .push("home: archix.lock has no home-manager and nixpkgs revs; run `archix update`".into());
        return Ok(());
    }
    let installs: BTreeSet<&str> = plan
        .packages
        .iter()
        .filter(|p| p.action == PkgAction::Install)
        .map(|p| p.name.as_str())
        .collect();
    let deferred = !env.tools && TOOLS.iter().any(|t| installs.contains(t));
    if deferred {
        for item in plan
            .packages
            .iter_mut()
            .filter(|p| p.name == "nix" && p.action == PkgAction::Install)
        {
            item.risk
                .push("first nix install: home-manager builds on the next switch".into());
        }
    }
    for user in users.keys() {
        let Some(account) = (env.account)(user) else {
            plan.home.push(line(
                format!("home-manager for {user}"),
                "builds once the user exists; home applies on the next switch",
            ));
            continue;
        };
        let dir = flake_dir(&account.home, user);
        let secrets = Secrets::declared(config, config_dir, None);
        let rendered = match render(user, &account.home, config, config_dir, &env.lock, &secrets) {
            Ok(rendered) => rendered,
            Err(e) if e.is::<Refusal>() => {
                plan.refusals.push(e.to_string());
                continue;
            }
            Err(e) => return Err(e),
        };
        let before = plan.home.len();
        let current = fs::read(dir.join("home.nix")).ok();
        let label = dir.join("home.nix").display().to_string();
        if let Some(diff) = udiff::unified(
            current.as_deref().unwrap_or_default(),
            rendered.home_nix.as_bytes(),
            &label,
        ) {
            let reason = match current {
                Some(_) => "generated home.nix changed",
                None => "home.nix generated for the first time",
            };
            plan.home.push(LineItem {
                diff: Some(diff),
                ..line(format!("home.nix for {user}"), reason)
            });
        }
        if fs::read(dir.join("flake.nix")).ok().as_deref() != Some(rendered.flake_nix.as_bytes()) {
            plan.home.push(line(
                format!(
                    "flake for {user}: home-manager {}, nixpkgs {}",
                    short(&env.lock.home_manager),
                    short(&env.lock.nixpkgs)
                ),
                "pinned by archix.lock",
            ));
        }
        for rel in changed_copies(&dir, &rendered.files) {
            plan.home.push(line(
                format!("copy files/{rel} for {user}"),
                "hm.path source in the config",
            ));
        }

        let mut new_generation = plan.home.len() > before;
        if deferred {
            plan.home.push(line(
                format!("home-manager for {user}"),
                "builds after nix is installed; home applies on the next switch",
            ));
        } else if !env.tools {
            plan.refusals.push(format!(
                "{}: nix and home-manager aren't installed; declare the nix and home-manager packages",
                hm::option_path(user)
            ));
            continue;
        } else if env.no_build {
            plan.unchecked
                .push(format!("home: home-manager build for {user} skipped (--no-build)"));
        } else {
            match (env.build)(user, &account, &rendered) {
                Ok(store) => {
                    let old = current_generation(&account.home);
                    new_generation |= old.as_deref() != Some(store.as_path());
                    let home = account.home.display().to_string();
                    for (rel, verb) in changed_home_files(old.as_deref(), &store)? {
                        plan.home
                            .push(line(format!("{verb} {home}/{rel}"), "home-manager generation"));
                    }
                }
                Err(e) => plan.refusals.push(format!(
                    "home: home-manager build for {user} failed, so the switch would refuse at step 2: {e:#}"
                )),
            }
        }
        if new_generation && !deferred {
            plan.home.push(line(
                format!(
                    "home-manager switch on {}@{}",
                    dir.display(),
                    short(&env.lock.home_manager)
                ),
                "activation scripts are code outside the plan contract",
            ));
        }
        let lingers = hm::linger(config, user);
        if lingers && !(env.linger_enabled)(user) {
            plan.home.push(line(
                format!("linger for {user}"),
                "declared (home.<user>.linger): keeps their systemd manager running for activation",
            ));
        } else if !lingers && new_generation {
            plan.notes.push(format!(
                "home-manager for {user} activates only while {user} is logged in; declare home.{user}.linger = true to switch while they're logged out"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use super::*;
    use crate::alpm::PinMode;
    use crate::plan::model::{Header, PackageItem, PkgReason, exit};

    fn plan() -> Plan {
        Plan::new(Header {
            mode: PinMode::Live,
            date: None,
            moved_from: None,
            fingerprint: String::new(),
            cached_dbs: false,
        })
    }

    fn config(linger: bool, value: Value) -> Value {
        let user = Value::Map(BTreeMap::from([
            ("home_manager".to_string(), value),
            ("linger".to_string(), Value::Bool(linger)),
        ]));
        Value::Map(BTreeMap::from([(
            "home".to_string(),
            Value::Map(BTreeMap::from([("al".to_string(), user)])),
        )]))
    }

    fn settings() -> Value {
        Value::Map(BTreeMap::from([(
            "programs".to_string(),
            Value::Map(BTreeMap::from([("git".to_string(), Value::Bool(true))])),
        )]))
    }

    struct Fixture {
        home: tempfile::TempDir,
        config_dir: tempfile::TempDir,
        builds: Rc<Cell<usize>>,
    }

    fn fixture() -> Fixture {
        Fixture {
            home: tempfile::tempdir().unwrap(),
            config_dir: tempfile::tempdir().unwrap(),
            builds: Rc::new(Cell::new(0)),
        }
    }

    fn env(f: &Fixture, tools: bool, no_build: bool, store: PathBuf) -> HomeEnv {
        let home = f.home.path().to_path_buf();
        let builds = f.builds.clone();
        HomeEnv {
            lock: HmLock {
                home_manager: "a".repeat(40),
                nixpkgs: "b".repeat(40),
            },
            no_build,
            tools,
            account: Box::new(move |_| {
                Some(Account {
                    uid: 1000,
                    gid: 1000,
                    home: home.clone(),
                })
            }),
            linger_enabled: Box::new(|_| false),
            build: Box::new(move |_, _, _| {
                builds.set(builds.get() + 1);
                Ok(store.clone())
            }),
        }
    }

    #[test]
    fn a_first_generation_shows_home_nix_the_build_files_and_the_switch_line() {
        let f = fixture();
        let store = tempfile::tempdir().unwrap();
        fs::create_dir_all(store.path().join("home-files/.config/git")).unwrap();
        fs::write(store.path().join("home-files/.config/git/config"), "x").unwrap();
        let mut plan = plan();
        let env = env(&f, true, false, store.path().to_path_buf());
        plan_home(&config(true, settings()), f.config_dir.path(), Some(&env), &mut plan).unwrap();
        let lines: Vec<&str> = plan.home.iter().map(|l| l.line.as_str()).collect();
        assert_eq!(lines[0], "home.nix for al");
        assert!(
            plan.home[0].diff.as_deref().unwrap().contains("+  programs = {"),
            "{:?}",
            plan.home[0].diff
        );
        assert!(lines[1].starts_with("flake for al: home-manager aaaaaaaaaaaa"));
        assert!(
            lines.contains(&format!("create {}/.config/git/config", f.home.path().display()).as_str()),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("home-manager switch on ") && l.ends_with("@aaaaaaaaaaaa"))
        );
        assert_eq!(*lines.last().unwrap(), "linger for al");
        assert_eq!(plan.exit_code(), exit::CHANGES);
    }

    #[test]
    fn no_build_leaves_the_plan_incomplete() {
        let f = fixture();
        let mut plan = plan();
        let env = env(&f, true, true, PathBuf::from("/nix/store/x"));
        plan_home(&config(false, settings()), f.config_dir.path(), Some(&env), &mut plan).unwrap();
        assert_eq!(f.builds.get(), 0);
        assert_eq!(plan.unchecked, ["home: home-manager build for al skipped (--no-build)"]);
        assert_eq!(plan.exit_code(), exit::INCOMPLETE);
    }

    #[test]
    fn the_switch_that_installs_nix_defers_home_and_marks_nix() {
        let f = fixture();
        let mut plan = plan();
        plan.packages.push(PackageItem {
            action: PkgAction::Install,
            name: "nix".into(),
            from: None,
            to: Some("2.30-1".into()),
            repo: Some("extra".into()),
            reason: PkgReason::Declared,
            detail: None,
            risk: Vec::new(),
        });
        let env = env(&f, false, false, PathBuf::from("/nix/store/x"));
        plan_home(&config(false, settings()), f.config_dir.path(), Some(&env), &mut plan).unwrap();
        assert_eq!(f.builds.get(), 0);
        assert!(!plan.packages[0].risk.is_empty());
        let deferred = plan.home.iter().find(|l| l.line == "home-manager for al").unwrap();
        assert_eq!(
            deferred.reason,
            "builds after nix is installed; home applies on the next switch"
        );
        assert!(plan.refusals.is_empty());
    }

    #[test]
    fn without_nix_installed_or_declared_the_switch_refuses() {
        let f = fixture();
        let mut plan = plan();
        let env = env(&f, false, false, PathBuf::from("/nix/store/x"));
        plan_home(&config(false, settings()), f.config_dir.path(), Some(&env), &mut plan).unwrap();
        assert_eq!(plan.exit_code(), exit::REFUSES);
    }

    #[test]
    fn a_secret_in_home_manager_is_a_refusal_naming_the_option() {
        let f = fixture();
        let mut plan = plan();
        let value = Value::Map(BTreeMap::from([(
            "token".to_string(),
            Value::Str(crate::secrets::placeholder("gh")),
        )]));
        let env = env(&f, true, false, PathBuf::from("/nix/store/x"));
        plan_home(&config(false, value), f.config_dir.path(), Some(&env), &mut plan).unwrap();
        assert_eq!(plan.refusals.len(), 1);
        assert!(
            plan.refusals[0].starts_with("home.al.home_manager.token: in v1 secrets"),
            "{}",
            plan.refusals[0]
        );
        assert_eq!(f.builds.get(), 0);
        assert_eq!(plan.exit_code(), exit::REFUSES);
    }

    #[test]
    fn an_unchanged_generation_plans_nothing() {
        let f = fixture();
        let store = tempfile::tempdir().unwrap();
        fs::create_dir_all(store.path().join("home-files")).unwrap();
        fs::write(store.path().join("home-files/.x"), "x").unwrap();
        let state = f.home.path().join(".local/state/home-manager/gcroots");
        fs::create_dir_all(&state).unwrap();
        std::os::unix::fs::symlink(store.path(), state.join("current-home")).unwrap();
        let env = env(&f, true, false, store.path().canonicalize().unwrap());
        let config = config(false, settings());
        let lock = env.lock.clone();
        let rendered = render(
            "al",
            f.home.path(),
            &config,
            f.config_dir.path(),
            &lock,
            &Secrets::default(),
        )
        .unwrap();
        let dir = flake_dir(f.home.path(), "al");
        fs::create_dir_all(dir.join("files")).unwrap();
        fs::write(dir.join("home.nix"), &rendered.home_nix).unwrap();
        fs::write(dir.join("flake.nix"), &rendered.flake_nix).unwrap();
        let mut plan = plan();
        plan_home(&config, f.config_dir.path(), Some(&env), &mut plan).unwrap();
        assert!(plan.home.is_empty(), "{:?}", plan.home);
        assert!(plan.notes.is_empty(), "{:?}", plan.notes);
        assert_eq!(plan.exit_code(), exit::NOTHING);
    }

    #[test]
    fn home_files_are_compared_by_content_and_exec_bit() {
        let old = tempfile::tempdir().unwrap();
        let new = tempfile::tempdir().unwrap();
        for (dir, files) in [(&old, [("a", "1"), ("b", "2")]), (&new, [("b", "3"), ("c", "4")])] {
            fs::create_dir_all(dir.path().join("home-files")).unwrap();
            for (name, text) in files {
                fs::write(dir.path().join("home-files").join(name), text).unwrap();
            }
        }
        assert_eq!(
            changed_home_files(Some(old.path()), new.path()).unwrap(),
            [
                ("a".to_string(), "delete"),
                ("b".to_string(), "modify"),
                ("c".to_string(), "create")
            ]
        );
        assert_eq!(changed_home_files(None, new.path()).unwrap().len(), 2);
    }
}
