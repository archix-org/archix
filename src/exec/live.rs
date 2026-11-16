//! The executor's steps on the running system: libalpm on the plan's DBPath, system/apply for files and accounts, systemctl for units.
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use alpm::{Alpm, PackageReason};
use anyhow::{Context, Result, anyhow, bail};

use super::compare;
use super::dblock::{self, DbLock};
use super::output::{say, warn};
use super::snapshot;
use super::transaction;
use super::{AurBuilder, HomeManager, Refused, Steps};
use crate::alpm::{PacmanConf, Prepared};
use crate::flatpak::LockedApp;
use crate::journal::{ActionKind, JournalWriter, now};
use crate::paths::Paths;
use crate::plan::fingerprint::Fingerprint;
use crate::plan::record::LastApplied;
use crate::plan::{AccountAction, FileAction, PkgAction, Plan, ServiceAction, ServiceItem};
use crate::system::apply::{self, Accounts, SystemChange};
use crate::system::fstab::FstabLine;
use crate::system::{Rendered, credentials};
use crate::value::Value;

pub struct Options {
    pub no_snapshot: bool,
    pub snapper_configured: bool,
    pub entry_timeout: Duration,
}

/// Everything plan() saw that the steps need, so they apply exactly that plan.
pub struct Inputs {
    pub plan: Plan,
    pub fingerprint: Fingerprint,
    pub conf: PacmanConf,
    pub plan_dbpath: PathBuf,
    pub config: Value,
    pub rendered: Rendered,
    pub last: Option<LastApplied>,
    pub paths: Paths,
    pub record: PathBuf,
    pub flatpak: BTreeMap<String, LockedApp>,
    pub archive_url: String,
    pub secret_hmacs: BTreeMap<String, String>,
    pub fstab: Vec<FstabLine>,
    pub esp: String,
    pub options: Options,
}

pub struct Live {
    inputs: Inputs,
    aur: Box<dyn AurBuilder>,
    home: Box<dyn HomeManager>,
    handle: Option<Alpm>,
    lock: Option<DbLock>,
    built: Vec<PathBuf>,
    home_built: BTreeMap<String, PathBuf>,
    deferred: Vec<String>,
}

impl Live {
    pub fn new(inputs: Inputs, aur: Box<dyn AurBuilder>, home: Box<dyn HomeManager>) -> Live {
        Live {
            inputs,
            aur,
            home,
            handle: None,
            lock: None,
            built: Vec::new(),
            home_built: BTreeMap::new(),
            deferred: Vec::new(),
        }
    }

    fn root(&self) -> &Path {
        &self.inputs.paths.root
    }

    fn change(&self) -> SystemChange {
        SystemChange {
            from: self.inputs.last.as_ref().map(|l| l.system.clone()).unwrap_or_default(),
            to: self.inputs.rendered.state.clone(),
        }
    }

    fn handle(&mut self) -> Result<&mut Alpm> {
        if self.handle.is_none() {
            let handle = crate::alpm::open(&self.inputs.conf, &self.inputs.plan_dbpath)?;
            transaction::set_output(&handle);
            self.handle = Some(handle);
        }
        Ok(self.handle.as_mut().expect("opened above"))
    }

    fn builds(&mut self, j: &mut JournalWriter) -> Result<String> {
        let change = self.change();
        let rendered = &self.inputs.rendered;
        crate::system::live::check_files(&change, &|path| {
            rendered
                .state
                .files
                .get(path)
                .and_then(|f| rendered.blobs.get(&f.sha256))
                .cloned()
        })
        .map_err(|e| Refused(format!("{e:#}")))?;
        let archive = crate::archive::Archive::new(&self.inputs.archive_url);
        crate::system::blobs::ensure(&self.inputs.paths, &archive, rendered)?;
        if !self.inputs.plan.aur.is_empty() {
            let conf = self.inputs.plan_dbpath.join("pacman.conf");
            crate::durable::write(&conf, self.inputs.conf.render().as_bytes())?;
            self.built = self
                .aur
                .build(&self.inputs.plan.aur, &conf, &self.inputs.plan_dbpath.join("sync"))
                .context("building the AUR packages")?;
        }
        for user in home_manager_users(&self.inputs.config) {
            let accounts = Accounts::read(&self.root().join("etc"));
            let (home, _) = accounts
                .home(&user)
                .ok_or_else(|| anyhow!("home-manager user {user} doesn't exist"))?;
            let flake = home.join(".config/archix/home-manager").join(&user);
            let store = self
                .home
                .build(&user, &flake)
                .with_context(|| format!("home-manager build for {user}"))?;
            let shown = store.display().to_string();
            j.update_meta(|m| {
                m.home.insert(user.clone(), shown);
            })?;
            self.home_built.insert(user, store);
        }
        Ok(format!(
            "{} AUR packages built, {} home generations built",
            self.built.len(),
            self.home_built.len()
        ))
    }

    fn lock(&mut self, j: &mut JournalWriter) -> Result<String> {
        let dbpath = self.inputs.conf.db_path();
        let lock = DbLock::take(&dbpath)?;
        j.update_meta(|m| m.alpm_window.start = Some(now()))?;
        let checked = dblock::recheck(&dbpath, &self.inputs.fingerprint.local_db)
            .and_then(|()| dblock::recheck_sync_dbs(&self.inputs.plan_dbpath, &self.inputs.fingerprint.sync_dbs));
        if let Err(e) = checked {
            let _ = j.update_meta(|m| m.alpm_window.end = Some(now()));
            lock.release()?;
            return Err(e);
        }
        let shown = lock.path().display().to_string();
        self.lock = Some(lock);
        Ok(format!("{shown} taken; the local db is the plan's"))
    }

    fn snapshot_pre(&mut self, j: &mut JournalWriter) -> Result<String> {
        if self.inputs.options.no_snapshot {
            return Ok("no pre snapshot (--no-snapshot)".into());
        }
        if !self.inputs.options.snapper_configured {
            return Err(
                Refused("snapper isn't set up; switch with --no-snapshot to go without a pre snapshot".into()).into(),
            );
        }
        let pre = snapshot::create_pre(j.journal().n)?;
        j.update_meta(|m| m.snapper_pre = Some(pre))?;
        say(&format!("snapper pre snapshot {pre}; waiting for its Limine entry"));
        snapshot::wait_for_entry(self.root(), pre, self.inputs.options.entry_timeout)?;
        Ok(format!("snapper pre {pre}, in limine.conf"))
    }

    fn install(&mut self, j: &mut JournalWriter) -> Result<String> {
        let plan = &self.inputs.plan;
        let (expected, skip) = compare::install(plan);
        let reasons: BTreeMap<String, PackageReason> = plan
            .packages
            .iter()
            .filter_map(|p| match p.action {
                PkgAction::MarkExplicit => Some((p.name.clone(), PackageReason::Explicit)),
                PkgAction::MarkDependency => Some((p.name.clone(), PackageReason::Depend)),
                _ => None,
            })
            .collect();
        let transaction = !(expected.add.is_empty() && expected.aur.is_empty() && expected.remove.is_empty());
        if !transaction && reasons.is_empty() {
            return Ok("no package transaction".into());
        }
        let built = self.built.clone();
        let config = self.inputs.config.clone();
        let repos: Vec<String> = self.inputs.conf.repos.iter().map(|r| r.name.clone()).collect();
        let (plan_dbpath, real_dbpath) = (self.inputs.plan_dbpath.clone(), self.inputs.conf.db_path());
        let handle = self.handle()?;
        let mut detail = String::new();
        if transaction {
            let targets = compare::sync_targets(handle, &config);
            let prepared = transaction::install(handle, &targets, &skip, &built, &expected, &mut |p| {
                journal_packages(j, "install", p)
            })?;
            detail = format!("{} added, {} replaced", prepared.add.len(), prepared.remove.len());
            transaction::replace_sync_dbs(&plan_dbpath, &real_dbpath, &repos, &mut |repo| {
                j.action("install", ActionKind::SyncDbReplace, repo)
            })?;
        }
        for (name, reason) in &reasons {
            let kind = match reason {
                PackageReason::Explicit => ActionKind::MarkExplicit,
                PackageReason::Depend => ActionKind::MarkDependency,
            };
            j.action("install", kind, name)?;
        }
        transaction::set_reasons(handle, &reasons)?;
        Ok(detail)
    }

    fn write_file(&self, path: &str, change: &SystemChange) -> Result<()> {
        let root = self.root().to_path_buf();
        let locate = |p: &str| (p == path).then(|| root.join(p.trim_start_matches('/')));
        apply::files(&self.inputs.paths, change, &locate, &Accounts::read(&root.join("etc")))
    }

    fn files(&mut self, j: &mut JournalWriter) -> Result<String> {
        let change = self.change();
        let root = self.root().to_path_buf();
        let items: Vec<_> = self
            .inputs
            .plan
            .files
            .iter()
            .filter(|f| f.path != "/etc/fstab" && f.action != FileAction::Unmanage)
            .cloned()
            .collect();
        if items.is_empty() && !self.inputs.plan.files.iter().any(|f| f.path == "/etc/fstab") {
            return Ok("no file changes".into());
        }
        let planned: BTreeSet<&str> = items.iter().map(|f| f.path.as_str()).collect();
        let locate = |p: &str| planned.contains(p).then(|| root.join(p.trim_start_matches('/')));
        apply::check(&self.inputs.paths, &change, &locate)?;
        let accounts = Accounts::read(&root.join("etc"));
        let added: BTreeSet<&str> = self
            .inputs
            .plan
            .users
            .iter()
            .filter(|u| matches!(u.action, AccountAction::AddUser | AccountAction::AddGroup))
            .map(|u| u.name.as_str())
            .collect();
        let mut failures = Vec::new();
        let mut written = BTreeSet::new();
        for item in &items {
            if let Some(want) = change.to.files.get(&item.path) {
                let missing = |name: &str, known: bool| !known && name != "root" && added.contains(name);
                if missing(&want.owner, accounts.uid(&want.owner).is_some())
                    || missing(&want.group, accounts.gid(&want.group).is_some())
                {
                    self.deferred.push(item.path.clone());
                    continue;
                }
            }
            let kind = match item.action {
                FileAction::Delete => ActionKind::FileDelete,
                _ => ActionKind::FileWrite,
            };
            j.action("files", kind, &item.path)?;
            crate::testhooks::fail_at(&format!("file {}", item.path))?;
            match self.write_file(&item.path, &change) {
                Ok(()) => {
                    written.insert(item.path.clone());
                }
                Err(e) => failures.push(format!("{e:#}")),
            }
        }
        if self.inputs.plan.files.iter().any(|f| f.path == "/etc/fstab") {
            j.action("files", ActionKind::FileWrite, "/etc/fstab")?;
            if let Err(e) = crate::system::fstab::write_in(&root.join("etc"), &self.inputs.esp, &self.inputs.fstab) {
                failures.push(format!("writing /etc/fstab: {e:#}"));
            }
        }
        if items.iter().any(|f| f.secret.is_some()) {
            match crate::secrets::activate::activate(&root) {
                Ok(report) => {
                    for (name, why) in report.missing {
                        warn(&format!(
                            "warning: secret {name} wasn't decrypted, so what uses it fails: {why}"
                        ));
                    }
                }
                Err(e) => failures.push(format!("decrypting the secrets: {e:#}")),
            }
        }
        let due = crate::system::hooks::due(&change, &written);
        failures.extend(crate::system::hooks::run(&root, &due, &BTreeMap::new()));
        if !failures.is_empty() {
            bail!("{}", failures.join("\n"));
        }
        Ok(format!(
            "{} files changed, {} wait for their owner",
            written.len(),
            self.deferred.len()
        ))
    }

    fn users(&mut self, j: &mut JournalWriter) -> Result<String> {
        if self.inputs.plan.users.is_empty() {
            return Ok("no account changes".into());
        }
        let change = self.change();
        let root = self.root().to_path_buf();
        let etc = root.join("etc");
        for item in &self.inputs.plan.users {
            let kind = match item.action {
                AccountAction::AddUser | AccountAction::ModifyUser | AccountAction::RemoveUser => ActionKind::User,
                _ => ActionKind::Group,
            };
            j.action("users", kind, &item.name)?;
        }
        let before = Accounts::read(&etc);
        let existed: BTreeSet<String> = change
            .to
            .users
            .keys()
            .filter(|n| before.uid(n).is_some())
            .cloned()
            .collect();
        let created = apply::users(&change, &etc, &run_command, &self.inputs.paths.state)?;
        let passwords = credentials::resolve(&change.to.users, &self.inputs.paths.state);
        let kept = credentials::apply(&etc, &etc, &passwords, &|name| existed.contains(name))?;
        for line in kept {
            warn(&format!("warning: {line}"));
        }
        apply::create_homes(
            &created,
            &Accounts::read(&etc),
            &etc.join("skel"),
            &root,
            &credentials::with_password(&change.to.users),
        )?;
        let deferred = std::mem::take(&mut self.deferred);
        for path in &deferred {
            j.action("users", ActionKind::FileWrite, path)?;
            self.write_file(path, &change)?;
        }
        Ok(format!(
            "{} account changes, {} created, {} files written for new owners",
            self.inputs.plan.users.len(),
            created.len(),
            deferred.len()
        ))
    }

    fn services(&mut self, j: &mut JournalWriter) -> Result<String> {
        let units_written = self
            .inputs
            .plan
            .files
            .iter()
            .any(|f| f.path.starts_with("/etc/systemd/"));
        if self.inputs.plan.services.is_empty() && !units_written {
            return Ok("no unit changes".into());
        }
        systemctl(&["daemon-reload".to_string()])?;
        let mut failures = Vec::new();
        let mut ran = 0;
        for item in &self.inputs.plan.services {
            let Some((args, kind)) = systemctl_args(item) else {
                continue;
            };
            j.action("services", kind, &item.unit)?;
            ran += 1;
            if let Err(e) = systemctl(&args) {
                failures.push(format!("{e:#}"));
            }
        }
        if !failures.is_empty() {
            bail!("{}", failures.join("\n"));
        }
        Ok(format!("{ran} unit actions"))
    }

    fn remove(&mut self, j: &mut JournalWriter) -> Result<String> {
        let (targets, expected) = compare::removal(&self.inputs.plan);
        if targets.is_empty() && expected.remove.is_empty() {
            return Ok("nothing to remove".into());
        }
        let handle = self.handle()?;
        let prepared = transaction::remove(handle, &targets, &expected, &mut |p| journal_packages(j, "remove", p))?;
        Ok(format!("{} removed", prepared.remove.len()))
    }

    fn snapshot_post(&mut self, j: &mut JournalWriter) -> Result<String> {
        let Some(pre) = j.meta().snapper_pre else {
            return Ok("no snapshot pair".into());
        };
        let post = snapshot::create_post(j.journal().n, pre)?;
        j.update_meta(|m| m.snapper_post = Some(post))?;
        Ok(format!("snapper post {post}"))
    }

    fn unlock(&mut self, j: &mut JournalWriter) -> Result<String> {
        self.handle = None;
        let Some(lock) = self.lock.take() else {
            return Ok("no lock held".into());
        };
        j.update_meta(|m| m.alpm_window.end = Some(now()))?;
        let shown = lock.path().display().to_string();
        lock.release()?;
        Ok(format!("{shown} released"))
    }

    fn home(&mut self, j: &mut JournalWriter) -> Result<String> {
        let change = self.change();
        let mut failures = Vec::new();
        let mut files = 0;
        for line in &self.inputs.plan.home {
            let Some((verb, path)) = line.line.split_once(' ') else {
                continue;
            };
            if !path.starts_with('/') || verb == "unmanage" {
                continue;
            }
            let kind = match verb {
                "delete" => ActionKind::FileDelete,
                _ => ActionKind::FileWrite,
            };
            j.action("home", kind, path)?;
            files += 1;
            if let Err(e) = self.write_file(path, &change) {
                failures.push(format!("{e:#}"));
            }
        }
        for (user, store) in &self.home_built {
            j.action("home", ActionKind::HomeActivate, user)?;
            if let Err(e) = self.home.activate(user, store) {
                failures.push(format!("home-manager activation for {user}: {e:#}"));
            }
        }
        if !failures.is_empty() {
            bail!("{}", failures.join("\n"));
        }
        Ok(format!(
            "{files} home files, {} home-manager generations",
            self.home_built.len()
        ))
    }

    fn flatpak(&mut self, j: &mut JournalWriter) -> Result<String> {
        if self.inputs.plan.flatpak.is_empty() {
            return Ok("no flatpak changes".into());
        }
        for id in self.inputs.flatpak.keys() {
            j.action("flatpak", ActionKind::FlatpakInstall, id)?;
        }
        let failures = crate::flatpak::sync(&self.inputs.flatpak, &BTreeMap::new());
        if !failures.is_empty() {
            bail!("{}", failures.join("\n"));
        }
        Ok(format!("{} apps", self.inputs.flatpak.len()))
    }

    fn finish(&mut self) -> Result<String> {
        let installed = crate::pacman::read_local_db(&self.inputs.conf.db_path().join("local"))?
            .into_keys()
            .collect();
        let record = LastApplied {
            schema_version: 1,
            declared: declared(&self.inputs.config),
            installed,
            system: self.inputs.rendered.state.clone(),
            secret_hmacs: self.inputs.secret_hmacs.clone(),
        };
        crate::durable::write(&self.inputs.record, serde_json::to_string_pretty(&record)?.as_bytes())?;
        Ok(format!("{} written", self.inputs.record.display()))
    }
}

impl Steps for Live {
    fn run(&mut self, step: &str, j: &mut JournalWriter) -> Result<String> {
        crate::testhooks::fail_at(step)?;
        match step {
            "plan" => Ok(format!("fingerprint {}", self.inputs.plan.header.fingerprint)),
            "builds" => self.builds(j),
            "lock" => self.lock(j),
            "snapshot-pre" => self.snapshot_pre(j),
            "install" => self.install(j),
            "files" => self.files(j),
            "users" => self.users(j),
            "services" => self.services(j),
            "remove" => self.remove(j),
            "snapshot-post" => self.snapshot_post(j),
            "unlock" => self.unlock(j),
            "home" => self.home(j),
            "flatpak" => self.flatpak(j),
            "finish" => self.finish(),
            other => bail!("no step {other}"),
        }
    }

    fn holds_lock(&self) -> bool {
        self.lock.is_some()
    }
}

fn journal_packages(j: &mut JournalWriter, step: &str, prepared: &Prepared) -> Result<()> {
    for added in &prepared.add {
        let kind = match &added.old_version {
            None => ActionKind::Install,
            Some(old) if alpm::vercmp(added.version.as_str(), old.as_str()).is_lt() => ActionKind::Downgrade,
            Some(_) => ActionKind::Upgrade,
        };
        j.action(step, kind, &format!("{} {}", added.name, added.version))?;
    }
    for (name, version) in &prepared.remove {
        j.action(step, ActionKind::Remove, &format!("{name} {version}"))?;
    }
    Ok(())
}

fn declared(config: &Value) -> BTreeSet<String> {
    match config {
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
    }
}

/// Users whose home the config declares through home-manager.
pub fn home_manager_users(config: &Value) -> Vec<String> {
    let Value::Map(map) = config else { return Vec::new() };
    let Some(Value::Map(home)) = map.get("home") else {
        return Vec::new();
    };
    home.iter()
        .filter(|(_, spec)| matches!(spec, Value::Map(s) if s.contains_key("home_manager")))
        .map(|(user, _)| user.clone())
        .collect()
}

fn run_command(argv: &[String]) -> Result<()> {
    let output = Command::new(&argv[0])
        .args(&argv[1..])
        .output()
        .with_context(|| format!("running {}", argv[0]))?;
    if !output.status.success() {
        bail!(
            "{} failed: {}",
            argv.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn systemctl(args: &[String]) -> Result<()> {
    let argv: Vec<String> = ["systemctl".to_string(), "--no-ask-password".to_string()]
        .into_iter()
        .chain(args.iter().cloned())
        .collect();
    run_command(&argv)
}

/// The systemctl call for one service line, or None for a restart that applies at reboot; enable and disable also start and stop a system unit unless it applies at reboot.
pub fn systemctl_args(item: &ServiceItem) -> Option<(Vec<String>, ActionKind)> {
    let unit = item.unit.clone();
    let (verb, kind) = match item.action {
        ServiceAction::Enable => ("enable", ActionKind::UnitEnable),
        ServiceAction::Disable => ("disable", ActionKind::UnitDisable),
        ServiceAction::Mask => ("mask", ActionKind::UnitMask),
        ServiceAction::Unmask => ("unmask", ActionKind::UnitUnmask),
        ServiceAction::Start => ("start", ActionKind::UnitStart),
        ServiceAction::Restart => ("restart", ActionKind::UnitRestart),
        ServiceAction::Reload => ("reload-or-restart", ActionKind::UnitReload),
    };
    let live = matches!(
        item.action,
        ServiceAction::Start | ServiceAction::Restart | ServiceAction::Reload
    );
    if live && (item.at_reboot || item.user) {
        return None;
    }
    let mut args = Vec::new();
    if item.user {
        args.push("--global".to_string());
    }
    args.push(verb.to_string());
    let now = matches!(item.action, ServiceAction::Enable | ServiceAction::Disable) && !item.user && !item.at_reboot;
    if now {
        args.push("--now".to_string());
    }
    args.push(unit);
    Some((args, kind))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(action: ServiceAction, user: bool, at_reboot: bool) -> ServiceItem {
        ServiceItem {
            action,
            unit: "x.service".into(),
            user,
            at_reboot,
            reason: "declared".into(),
            risk: Vec::new(),
        }
    }

    fn args(i: &ServiceItem) -> Option<String> {
        systemctl_args(i).map(|(a, _)| a.join(" "))
    }

    #[test]
    fn service_lines_map_to_systemctl_and_reboot_lines_never_restart_live() {
        assert_eq!(
            args(&item(ServiceAction::Enable, false, false)).as_deref(),
            Some("enable --now x.service")
        );
        assert_eq!(
            args(&item(ServiceAction::Enable, false, true)).as_deref(),
            Some("enable x.service")
        );
        assert_eq!(
            args(&item(ServiceAction::Disable, false, false)).as_deref(),
            Some("disable --now x.service")
        );
        assert_eq!(
            args(&item(ServiceAction::Enable, true, false)).as_deref(),
            Some("--global enable x.service")
        );
        assert_eq!(
            args(&item(ServiceAction::Restart, false, false)).as_deref(),
            Some("restart x.service")
        );
        assert_eq!(
            args(&item(ServiceAction::Reload, false, false)).as_deref(),
            Some("reload-or-restart x.service")
        );
        assert_eq!(args(&item(ServiceAction::Restart, false, true)), None);
        assert_eq!(args(&item(ServiceAction::Start, true, false)), None);
        assert_eq!(
            args(&item(ServiceAction::Mask, false, false)).as_deref(),
            Some("mask x.service")
        );
    }

    #[test]
    fn home_manager_users_are_those_with_a_home_manager_table() {
        let table = |key: &str| Value::Map(BTreeMap::from([(key.to_string(), Value::Map(BTreeMap::new()))]));
        let home = BTreeMap::from([
            ("alice".to_string(), table("home_manager")),
            ("bob".to_string(), table("files")),
        ]);
        let config = Value::Map(BTreeMap::from([("home".to_string(), Value::Map(home))]));
        assert_eq!(home_manager_users(&config), ["alice"]);
        assert!(home_manager_users(&Value::Null).is_empty());
    }
}
