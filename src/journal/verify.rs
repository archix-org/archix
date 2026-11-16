//! `archix log --verify N` (SCOPE 4): checks a switch against observations the executor doesn't produce.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::read::{self, Meta, parse_time};
use super::writemap;
use crate::plan::{AccountAction, PkgAction, Plan, ServiceAction};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Output {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Every external command verify runs goes through this, so tests feed recorded output.
pub trait Runner {
    fn run(&self, program: &str, args: &[&str]) -> Result<Output>;
}

pub struct SystemRunner;

impl Runner for SystemRunner {
    fn run(&self, program: &str, args: &[&str]) -> Result<Output> {
        let output = std::process::Command::new(program)
            .args(args)
            .env("LC_ALL", "C")
            .output()
            .with_context(|| format!("running {program}"))?;
        Ok(Output {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// Where verify reads from; the defaults are the system's, tests point them at fixtures.
#[derive(Debug, Clone)]
pub struct Sources {
    pub journal_root: PathBuf,
    pub pacman_log: Option<PathBuf>,
    pub dbpath: Option<PathBuf>,
    pub snapshots: PathBuf,
}

impl Default for Sources {
    fn default() -> Sources {
        Sources {
            journal_root: PathBuf::from("/"),
            pacman_log: None,
            dbpath: None,
            snapshots: PathBuf::from("/.snapshots"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub name: String,
    pub pass: bool,
    pub items: Vec<String>,
    pub notes: Vec<String>,
}

impl Check {
    fn new(name: impl Into<String>) -> Check {
        Check {
            name: name.into(),
            pass: true,
            items: Vec::new(),
            notes: Vec::new(),
        }
    }

    fn fail(&mut self, item: impl Into<String>) {
        self.pass = false;
        self.items.push(item.into());
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub n: u32,
    pub checks: Vec<Check>,
}

impl Report {
    pub fn pass(&self) -> bool {
        self.checks.iter().all(|c| c.pass)
    }

    pub fn render(&self) -> String {
        let mut out = format!("verify switch {}\n", self.n);
        for check in &self.checks {
            let _ = writeln!(out, "{}: {}", check.name, if check.pass { "pass" } else { "FAIL" });
            for item in &check.items {
                let _ = writeln!(out, "    {item}");
            }
            for note in &check.notes {
                let _ = writeln!(out, "    note: {note}");
            }
        }
        let _ = writeln!(
            out,
            "{}",
            if self.pass() {
                "all checks pass"
            } else {
                "verify failed"
            }
        );
        out
    }
}

pub fn verify(n: u32, sources: &Sources, runner: &dyn Runner) -> Result<Report> {
    let dir = read::switch_dir(&sources.journal_root, n);
    let meta = read::read_meta(&dir)?;
    let plan = read::read_plan(&dir)?;
    let log_path = match &sources.pacman_log {
        Some(path) => path.clone(),
        None => pacman_conf(runner, "LogFile")?,
    };
    let dbpath = match &sources.dbpath {
        Some(path) => path.clone(),
        None => pacman_conf(runner, "DBPath")?,
    };
    let log = fs::read_to_string(&log_path).with_context(|| format!("reading {}", log_path.display()))?;
    let in_window = window(&meta).map(|w| lines_in(&parse_log(&log), w));
    let mut checks = Vec::new();
    checks.push(match &in_window {
        Some(lines) => check_packages(&plan, lines),
        None => missing_window("(a) pacman.log packages"),
    });
    checks.push(match &in_window {
        Some(lines) => {
            let owned = Owned::load(&plan, &meta, &dbpath, sources);
            check_files(&plan, &meta, lines, &owned, runner)
        }
        None => missing_window("(b) snapper status"),
    });
    checks.push(check_units(&plan, runner));
    checks.push(check_flatpak_home(&plan, &meta, runner));
    Ok(Report { n, checks })
}

fn missing_window(name: &str) -> Check {
    let mut check = Check::new(name);
    check.fail("no [ALPM] window in meta.json (the switch never took the pacman lock, or was cut before recording it)");
    check
}

fn pacman_conf(runner: &dyn Runner, key: &str) -> Result<PathBuf> {
    let out = runner.run("pacman-conf", &[key])?;
    let value = out.stdout.lines().next().unwrap_or("").trim();
    if out.code != 0 || value.is_empty() {
        bail!("pacman-conf {key} failed: {}", out.stderr.trim());
    }
    Ok(PathBuf::from(value))
}

/// The lock window as whole seconds, the resolution pacman.log has.
fn window(meta: &Meta) -> Option<(i64, i64)> {
    let start = parse_time(meta.alpm_window.start.as_deref()?)?;
    let end = parse_time(meta.alpm_window.end.as_deref()?)?;
    Some((start, end))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogEvent {
    Package { op: String, name: String, versions: String },
    Hook(String),
    Scriptlet(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    pub t: i64,
    pub event: LogEvent,
}

const PACKAGE_OPS: [&str; 5] = ["installed", "upgraded", "downgraded", "removed", "reinstalled"];

/// pacman.log's [ALPM] package and hook lines and [ALPM-SCRIPTLET] output; anything else is skipped.
pub fn parse_log(text: &str) -> Vec<LogLine> {
    let mut lines = Vec::new();
    for raw in text.lines() {
        let Some(rest) = raw.strip_prefix('[') else { continue };
        let Some((time, rest)) = rest.split_once("] ") else {
            continue;
        };
        let Some(t) = parse_time(time) else { continue };
        let event = if let Some(body) = rest.strip_prefix("[ALPM] ") {
            if let Some(hook) = body.strip_prefix("running '").and_then(|h| h.strip_suffix("'...")) {
                LogEvent::Hook(hook.to_string())
            } else {
                let Some((op, pkg)) = body.split_once(' ') else {
                    continue;
                };
                if !PACKAGE_OPS.contains(&op) {
                    continue;
                }
                let Some((name, versions)) = pkg.split_once(" (") else {
                    continue;
                };
                let Some(versions) = versions.strip_suffix(')') else {
                    continue;
                };
                LogEvent::Package {
                    op: op.to_string(),
                    name: name.to_string(),
                    versions: versions.to_string(),
                }
            }
        } else if let Some(body) = rest.strip_prefix("[ALPM-SCRIPTLET] ") {
            LogEvent::Scriptlet(body.to_string())
        } else {
            continue;
        };
        lines.push(LogLine { t, event });
    }
    lines
}

fn lines_in(lines: &[LogLine], (start, end): (i64, i64)) -> Vec<LogLine> {
    lines.iter().filter(|l| l.t >= start && l.t <= end).cloned().collect()
}

/// (a): the window's package lines equal the plan's, by name and versions; AUR packages by name.
pub fn check_packages(plan: &Plan, lines: &[LogLine]) -> Check {
    let mut check = Check::new("(a) pacman.log packages");
    let mut expected: Vec<(String, String, Option<String>)> = Vec::new();
    for item in &plan.packages {
        let (op, versions) = match item.action {
            PkgAction::Install => ("installed", item.to.clone()),
            PkgAction::Upgrade => (
                "upgraded",
                item.from
                    .as_ref()
                    .zip(item.to.as_ref())
                    .map(|(f, t)| format!("{f} -> {t}")),
            ),
            PkgAction::Downgrade => (
                "downgraded",
                item.from
                    .as_ref()
                    .zip(item.to.as_ref())
                    .map(|(f, t)| format!("{f} -> {t}")),
            ),
            PkgAction::Remove => ("removed", item.from.clone()),
            PkgAction::MarkExplicit | PkgAction::MarkDependency => continue,
        };
        expected.push((op.to_string(), item.name.clone(), versions));
    }
    let aur: BTreeSet<&str> = plan.aur.iter().map(|a| a.name.as_str()).collect();
    let mut aur_seen: BTreeSet<&str> = BTreeSet::new();
    for line in lines {
        let LogEvent::Package { op, name, versions } = &line.event else {
            continue;
        };
        let position = expected
            .iter()
            .position(|(o, n, v)| o == op && n == name && v.as_ref().is_none_or(|v| v == versions));
        if let Some(i) = position {
            expected.remove(i);
            continue;
        }
        if op != "removed" && aur.contains(name.as_str()) && aur_seen.insert(name.as_str()) {
            continue;
        }
        check.fail(format!("unplanned: {op} {name} ({versions})"));
    }
    for (op, name, versions) in expected {
        let versions = versions.map_or_else(String::new, |v| format!(" ({v})"));
        check.fail(format!("missing: {op} {name}{versions}"));
    }
    for name in aur.difference(&aur_seen) {
        check.fail(format!("missing: AUR {name} installed or upgraded"));
    }
    check
}

/// One line of `snapper status`: its flag column and the path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub flags: String,
    pub path: String,
}

pub fn parse_snapper(text: &str) -> Vec<Change> {
    text.lines()
        .filter_map(|line| {
            let (flags, path) = line.split_once(' ')?;
            let valid = !flags.is_empty() && flags.chars().all(|c| "+-ctpugxa.".contains(c));
            let path = path.trim_start();
            (valid && path.starts_with('/')).then(|| Change {
                flags: flags.to_string(),
                path: path.trim_end_matches('/').to_string(),
            })
        })
        .collect()
}

/// Planned packages' file lists: `new` after the switch, `old` before it (upgrades, downgrades, removals).
#[derive(Debug, Clone, Default)]
pub struct Owned {
    pub new: BTreeMap<String, BTreeSet<String>>,
    pub old: BTreeMap<String, BTreeSet<String>>,
    pub missing: Vec<String>,
}

impl Owned {
    pub fn load(plan: &Plan, meta: &Meta, dbpath: &Path, sources: &Sources) -> Owned {
        let mut owned = Owned::default();
        let pre_db = meta.snapper_pre.map(|pre| {
            sources
                .snapshots
                .join(pre.to_string())
                .join("snapshot")
                .join(dbpath.strip_prefix("/").unwrap_or(dbpath))
        });
        for item in &plan.packages {
            if matches!(
                item.action,
                PkgAction::Install | PkgAction::Upgrade | PkgAction::Downgrade
            ) {
                match local_files(dbpath, &item.name, item.to.as_deref()) {
                    Some(files) => {
                        owned.new.insert(item.name.clone(), files);
                    }
                    None => owned.missing.push(format!(
                        "{} {}: not in the local db",
                        item.name,
                        item.to.as_deref().unwrap_or("?")
                    )),
                }
            }
            if matches!(
                item.action,
                PkgAction::Upgrade | PkgAction::Downgrade | PkgAction::Remove
            ) {
                let journaled = meta
                    .removed_files
                    .get(&item.name)
                    .map(|list| list.iter().map(|p| normalize(p)).collect());
                let files = journaled.or_else(|| {
                    pre_db
                        .as_deref()
                        .and_then(|db| local_files(db, &item.name, item.from.as_deref()))
                });
                match files {
                    Some(files) => {
                        owned.old.insert(item.name.clone(), files);
                    }
                    None => owned.missing.push(format!(
                        "{} {}: no earlier file list (journal or pre-snapshot db)",
                        item.name,
                        item.from.as_deref().unwrap_or("?")
                    )),
                }
            }
        }
        for item in &plan.aur {
            match local_files(dbpath, &item.name, None) {
                Some(files) => {
                    owned.new.insert(item.name.clone(), files);
                }
                None => owned.missing.push(format!("{} (AUR): not in the local db", item.name)),
            }
        }
        owned
    }

    fn owns(&self, path: &str) -> bool {
        self.new
            .values()
            .chain(self.old.values())
            .any(|files| files.contains(path))
    }

    fn owns_dir_above(&self, path: &str) -> bool {
        let mut dir = Path::new(path).parent();
        while let Some(d) = dir {
            let text = d.to_string_lossy();
            if text.matches('/').count() < 3 {
                return false;
            }
            if self.owns(&text) {
                return true;
            }
            dir = d.parent();
        }
        false
    }
}

fn normalize(path: &str) -> String {
    format!("/{}", path.trim_start_matches('/').trim_end_matches('/'))
}

/// A local db entry's %FILES% as absolute paths; with no version, the one entry of that name.
pub fn local_files(dbpath: &Path, name: &str, version: Option<&str>) -> Option<BTreeSet<String>> {
    let local = dbpath.join("local");
    let dir = match version {
        Some(version) => Some(local.join(format!("{name}-{version}"))).filter(|d| d.is_dir()),
        None => fs::read_dir(&local)
            .ok()?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .find(|d| {
                fs::read_to_string(d.join("desc"))
                    .is_ok_and(|desc| section(&desc, "NAME").first().map(String::as_str) == Some(name))
            }),
    }?;
    let files = fs::read_to_string(dir.join("files")).ok()?;
    Some(section(&files, "FILES").iter().map(|p| normalize(p)).collect())
}

fn section(text: &str, name: &str) -> Vec<String> {
    let header = format!("%{name}%");
    text.lines()
        .skip_while(|l| l.trim() != header)
        .skip(1)
        .take_while(|l| !l.trim().is_empty())
        .map(|l| l.trim().to_string())
        .collect()
}

const CHECKED_TREES: [&str; 3] = ["/etc", "/usr", "/opt"];

fn under(path: &str, tree: &str) -> bool {
    path == tree || path.strip_prefix(tree).is_some_and(|rest| rest.starts_with('/'))
}

/// (b): every change snapper saw under /etc, /usr, /opt (and the checked part of /var/lib) has an explanation.
pub fn check_files(plan: &Plan, meta: &Meta, lines: &[LogLine], owned: &Owned, runner: &dyn Runner) -> Check {
    let mut check = Check::new("(b) snapper status");
    let (Some(pre), Some(post)) = (meta.snapper_pre, meta.snapper_post) else {
        check.fail("no snapper pre/post pair in meta.json, so the switch's file changes can't be checked");
        return check;
    };
    check.name = format!("(b) snapper status {pre}..{post}");
    let range = format!("{pre}..{post}");
    let status = match runner.run("snapper", &["-c", "root", "status", &range]) {
        Ok(out) if out.code == 0 => out.stdout,
        Ok(out) => {
            check.fail(format!("snapper status {range} failed: {}", out.stderr.trim()));
            return check;
        }
        Err(e) => {
            check.fail(format!("{e:#}"));
            return check;
        }
    };
    for missing in &owned.missing {
        check.notes.push(format!("file list unavailable: {missing}"));
    }
    let explain = Explainer::new(plan, lines, owned);
    let mut ignored = 0usize;
    for change in parse_snapper(&status) {
        let path = change.path.as_str();
        let in_scope = CHECKED_TREES.iter().any(|tree| under(path, tree))
            || (under(path, "/var/lib")
                && !writemap::UNCHECKED.iter().any(|glob| writemap::matches(glob, path))
                && (owned.owns(path) || owned.owns_dir_above(path) || !writemap::sources_matching(path).is_empty()));
        if !in_scope {
            ignored += 1;
            continue;
        }
        if explain.explains(&change).is_none() {
            let mut item = format!("unexplained: {} {path}", change.flags);
            let mapped = writemap::sources_matching(path);
            if !mapped.is_empty() {
                let _ = write!(item, " (mapped to {}, which didn't run)", mapped.join(", "));
            }
            check.fail(item);
        }
    }
    if ignored > 0 {
        check.notes.push(format!("{ignored} changes outside the checked trees"));
    }
    check
}

struct Explainer<'a> {
    owned: &'a Owned,
    declared: BTreeSet<String>,
    declared_dirs: BTreeSet<String>,
    units: BTreeSet<String>,
    accounts: bool,
    ran: BTreeSet<String>,
}

impl<'a> Explainer<'a> {
    fn new(plan: &Plan, lines: &[LogLine], owned: &'a Owned) -> Explainer<'a> {
        let declared: BTreeSet<String> = plan.files.iter().map(|f| normalize(&f.path)).collect();
        let mut declared_dirs = BTreeSet::new();
        for path in &declared {
            let mut parent = Path::new(path).parent();
            while let Some(dir) = parent.filter(|d| d.as_os_str().len() > 1) {
                declared_dirs.insert(dir.to_string_lossy().into_owned());
                parent = dir.parent();
            }
        }
        let units = plan
            .services
            .iter()
            .filter(|s| {
                matches!(
                    s.action,
                    ServiceAction::Enable | ServiceAction::Disable | ServiceAction::Mask | ServiceAction::Unmask
                )
            })
            .map(|s| s.unit.clone())
            .collect();
        let accounts = plan.users.iter().any(|u| {
            matches!(
                u.action,
                AccountAction::AddUser
                    | AccountAction::ModifyUser
                    | AccountAction::RemoveUser
                    | AccountAction::AddGroup
                    | AccountAction::ModifyGroup
                    | AccountAction::RemoveGroup
            )
        });
        let mut ran: BTreeSet<String> = lines
            .iter()
            .filter_map(|l| match &l.event {
                LogEvent::Hook(name) => Some(name.clone()),
                _ => None,
            })
            .collect();
        let planned = plan
            .packages
            .iter()
            .filter(|p| !matches!(p.action, PkgAction::MarkExplicit | PkgAction::MarkDependency))
            .map(|p| p.name.as_str())
            .chain(plan.aur.iter().map(|a| a.name.as_str()));
        ran.extend(planned.map(|name| format!("scriptlet:{name}")));
        Explainer {
            owned,
            declared,
            declared_dirs,
            units,
            accounts,
            ran,
        }
    }

    fn explains(&self, change: &Change) -> Option<&'static str> {
        let path = change.path.as_str();
        if self.owned.owns(path) {
            return Some("planned package");
        }
        for suffix in [".pacnew", ".pacsave"] {
            if path.strip_suffix(suffix).is_some_and(|base| self.owned.owns(base)) {
                return Some("planned package's pacnew/pacsave");
            }
        }
        if self.declared.contains(path)
            || path
                .strip_suffix(".archix-save")
                .is_some_and(|base| self.declared.contains(base))
        {
            return Some("declared file");
        }
        if change.flags.starts_with('+') && self.declared_dirs.contains(path) {
            return Some("declared file's directory");
        }
        if self.accounts && writemap::matches("/etc/{passwd,group,shadow,gshadow,subuid,subgid}{,-}", path) {
            return Some("planned users and groups");
        }
        let unit_link = ["/etc/systemd/system", "/etc/systemd/user"]
            .iter()
            .any(|d| under(path, d))
            && Path::new(path)
                .file_name()
                .is_some_and(|name| self.units.contains(name.to_string_lossy().as_ref()));
        if unit_link {
            return Some("planned unit");
        }
        if writemap::allowlisted(path) {
            return Some("daemon allowlist");
        }
        let mapped = writemap::sources_matching(path);
        if mapped.iter().any(|source| self.ran.contains(*source)) {
            return Some("write map");
        }
        None
    }
}

/// (c): each unit the plan names is enabled, disabled, masked or active as planned.
pub fn check_units(plan: &Plan, runner: &dyn Runner) -> Check {
    let mut check = Check::new("(c) units");
    for item in &plan.services {
        let scope: &[&str] = if item.user { &["--global"] } else { &["--system"] };
        let query = |verb: &str| -> Result<String> {
            let mut args = scope.to_vec();
            args.extend([verb, item.unit.as_str()]);
            let out = runner.run("systemctl", &args)?;
            Ok(out.stdout.lines().next().unwrap_or("").trim().to_string())
        };
        let (verb, ok): (&str, fn(&str) -> bool) = match item.action {
            ServiceAction::Enable => ("is-enabled", |s| matches!(s, "enabled" | "enabled-runtime" | "alias")),
            ServiceAction::Disable => ("is-enabled", |s| matches!(s, "disabled" | "static" | "indirect")),
            ServiceAction::Mask => ("is-enabled", |s| matches!(s, "masked" | "masked-runtime")),
            ServiceAction::Unmask => ("is-enabled", |s| !s.starts_with("masked") && !s.is_empty()),
            ServiceAction::Start | ServiceAction::Restart | ServiceAction::Reload => ("is-active", |s| s == "active"),
        };
        let who = if item.user { "user " } else { "" };
        let action = action_text(item.action);
        if verb == "is-active" && (item.at_reboot || item.user) {
            let why = if item.at_reboot {
                "applies at reboot"
            } else {
                "a user unit's state is per user"
            };
            check
                .notes
                .push(format!("{action} {who}{}: not checked, {why}", item.unit));
            continue;
        }
        match query(verb) {
            Ok(state) if ok(&state) => {}
            Ok(state) => check.fail(format!(
                "{action} {who}{}: systemctl {verb} says {:?}",
                item.unit, state
            )),
            Err(e) => check.fail(format!("{action} {who}{}: {e:#}", item.unit)),
        }
    }
    check
}

fn action_text(action: ServiceAction) -> &'static str {
    match action {
        ServiceAction::Enable => "enable",
        ServiceAction::Disable => "disable",
        ServiceAction::Mask => "mask",
        ServiceAction::Unmask => "unmask",
        ServiceAction::Start => "start",
        ServiceAction::Restart => "restart",
        ServiceAction::Reload => "reload",
    }
}

/// (d): flatpak's installed apps match the plan's flatpak lines, and each user's home-manager generation is the one built in step 2.
pub fn check_flatpak_home(plan: &Plan, meta: &Meta, runner: &dyn Runner) -> Check {
    let mut check = Check::new("(d) flatpak and home");
    if !plan.flatpak.is_empty() {
        match runner.run(
            "flatpak",
            &["list", "--system", "--app", "--columns=application,origin,active"],
        ) {
            Ok(out) if out.code == 0 => {
                let installed = crate::flatpak::parse_list(&out.stdout);
                for item in &plan.flatpak {
                    let words: Vec<&str> = item.line.split_whitespace().collect();
                    let commit_matches = |id: &str, commit: &str| {
                        installed.get(id).is_some_and(|app| {
                            !app.commit.is_empty()
                                && (app.commit.starts_with(commit) || commit.starts_with(&app.commit))
                        })
                    };
                    let ok = match words[..] {
                        ["install", id, commit] => commit_matches(id, commit.trim_matches(|c| c == '(' || c == ')')),
                        ["remove", id] => !installed.contains_key(id),
                        [id, _, "->", commit] => commit_matches(id, commit),
                        _ => {
                            check.notes.push(format!("flatpak line not understood: {}", item.line));
                            true
                        }
                    };
                    if !ok {
                        let now = words
                            .get(if words[0] == "install" || words[0] == "remove" {
                                1
                            } else {
                                0
                            })
                            .and_then(|id| installed.get(*id))
                            .map_or("not installed".to_string(), |app| format!("active {}", app.commit));
                        check.fail(format!("flatpak {}: {now}", item.line));
                    }
                }
            }
            Ok(out) => check.fail(format!("flatpak list failed: {}", out.stderr.trim())),
            Err(e) => check.fail(format!("{e:#}")),
        }
    }
    if meta.home.is_empty() && plan.home.iter().any(|h| h.line.contains("home-manager")) {
        check.fail("the plan activates home-manager but meta.json records no store path");
    }
    for (user, store) in &meta.home {
        match activated(runner, user) {
            Ok(Some(path)) if path.trim_end_matches('/') == store.trim_end_matches('/') => {}
            Ok(Some(path)) => check.fail(format!("home {user}: activated {path}, built {store}")),
            Ok(None) => check.fail(format!("home {user}: no home-manager profile, built {store}")),
            Err(e) => check.fail(format!("home {user}: {e:#}")),
        }
    }
    check
}

/// The store path a user's home-manager profile resolves to, from the XDG profile or the per-user one.
fn activated(runner: &dyn Runner, user: &str) -> Result<Option<String>> {
    let out = runner.run("getent", &["passwd", user])?;
    let home = out
        .stdout
        .lines()
        .next()
        .and_then(|l| l.split(':').nth(5))
        .map(str::to_string);
    let Some(home) = home.filter(|h| out.code == 0 && !h.is_empty()) else {
        bail!("no passwd entry");
    };
    let candidates = [
        format!("{home}/.local/state/nix/profiles/home-manager"),
        format!("/nix/var/nix/profiles/per-user/{user}/home-manager"),
    ];
    for candidate in &candidates {
        let out = runner.run("readlink", &["-e", candidate])?;
        let target = out.stdout.trim();
        if out.code == 0 && !target.is_empty() {
            return Ok(Some(target.to_string()));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alpm::PinMode;
    use crate::journal::read::tests::meta;
    use crate::plan::{AurItem, FileAction, FileItem, Header, LineItem, PackageItem, PkgReason, ServiceItem};
    use std::cell::RefCell;

    #[derive(Default)]
    struct Fake {
        replies: BTreeMap<String, Output>,
        calls: RefCell<Vec<String>>,
    }

    impl Fake {
        fn reply(mut self, command: &str, code: i32, stdout: &str) -> Fake {
            self.replies.insert(
                command.to_string(),
                Output {
                    code,
                    stdout: stdout.to_string(),
                    stderr: String::new(),
                },
            );
            self
        }
    }

    impl Runner for Fake {
        fn run(&self, program: &str, args: &[&str]) -> Result<Output> {
            let line = std::iter::once(program)
                .chain(args.iter().copied())
                .collect::<Vec<_>>()
                .join(" ");
            self.calls.borrow_mut().push(line.clone());
            self.replies
                .get(&line)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("unexpected command: {line}"))
        }
    }

    fn pkg(action: PkgAction, name: &str, from: Option<&str>, to: Option<&str>) -> PackageItem {
        PackageItem {
            action,
            name: name.into(),
            from: from.map(Into::into),
            to: to.map(Into::into),
            repo: None,
            reason: PkgReason::Declared,
            detail: None,
            risk: Vec::new(),
        }
    }

    fn plan() -> Plan {
        let mut plan = Plan::new(Header {
            mode: PinMode::Live,
            date: None,
            moved_from: None,
            fingerprint: "f".into(),
            cached_dbs: false,
        });
        plan.packages = vec![
            pkg(PkgAction::Install, "foo", None, Some("1.0-1")),
            pkg(PkgAction::Upgrade, "glibc", Some("2.41-1"), Some("2.42-1")),
            pkg(PkgAction::Remove, "oldpkg", Some("3.0-1"), None),
            pkg(PkgAction::MarkExplicit, "vim", None, None),
        ];
        plan.aur = vec![AurItem {
            name: "paru-git".into(),
            commit: Some("abc".into()),
            helper: None,
            reason: "declared".into(),
            risk: Vec::new(),
        }];
        plan.files = vec![FileItem {
            action: FileAction::Create,
            path: "/etc/archix-test/app.conf".into(),
            reason: "declared".into(),
            diff: None,
            secret: None,
            hand_edited: false,
            risk: Vec::new(),
        }];
        plan.services = vec![
            ServiceItem {
                action: ServiceAction::Enable,
                unit: "foo.service".into(),
                user: false,
                at_reboot: false,
                reason: "declared".into(),
                risk: Vec::new(),
            },
            ServiceItem {
                action: ServiceAction::Restart,
                unit: "systemd-journald.service".into(),
                user: false,
                at_reboot: true,
                reason: "upgraded".into(),
                risk: Vec::new(),
            },
        ];
        plan
    }

    // The window in meta() is 09:00:10.250Z to 09:02:00.900Z, i.e. 12:00:10 to 12:02:00 at +0300.
    const LOG: &str = "\
[2026-11-16T12:00:09+0300] [ALPM] installed early (1-1)
[2026-11-16T12:00:10+0300] [ALPM] transaction started
[2026-11-16T12:00:11+0300] [ALPM] installed foo (1.0-1)
[2026-11-16T12:00:11+0300] [ALPM-SCRIPTLET] ==> post-install note
[2026-11-16T12:00:12+0300] [ALPM] upgraded glibc (2.41-1 -> 2.42-1)
[2026-11-16T12:00:13+0300] [ALPM] installed paru-git (2.1.0.r12.gabc-1)
[2026-11-16T12:00:14+0300] [ALPM] transaction completed
[2026-11-16T12:00:14+0300] [ALPM] running '20-systemd-sysusers.hook'...
[2026-11-16T12:00:15+0300] [ALPM] running '11-glibc-ldconfig.hook'...
[2026-11-16T12:01:30+0300] [ALPM] removed oldpkg (3.0-1)
[2026-11-16T12:02:01+0300] [ALPM] installed late (1-1)
[2026-11-16T12:02:02+0300] [ALPM] running 'dconf-update.hook'...
";

    const STATUS: &str = "\
+..... /etc/archix-test
+..... /etc/archix-test/app.conf
+..... /usr/bin/foo
+..... /usr/share/foo
c..... /usr/lib/libc.so.6
+..... /usr/lib/libnew.so
-..... /usr/lib/libold.so
-..... /usr/bin/oldpkg
c..... /etc/passwd
c..... /etc/ld.so.cache
c..... /etc/adjtime
+..... /etc/systemd/system/multi-user.target.wants/foo.service
+..... /var/lib/foo/data
.....x /usr/bin/paru
c..... /var/lib/systemd/random-seed
c..... /var/log/journal/x/system.journal
+..... /home/alice/.bashrc
";

    fn db(root: &Path, name: &str, version: &str, files: &[&str]) {
        let dir = root.join("local").join(format!("{name}-{version}"));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("desc"), format!("%NAME%\n{name}\n\n%VERSION%\n{version}\n\n")).unwrap();
        fs::write(
            dir.join("files"),
            format!("%FILES%\n{}\n\n%BACKUP%\netc/foo.conf\tabc\n", files.join("\n")),
        )
        .unwrap();
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        sources: Sources,
        dbpath: PathBuf,
        plan: Plan,
        meta: Meta,
        log: String,
        status: String,
    }

    impl Fixture {
        fn new() -> Fixture {
            let dir = tempfile::tempdir().unwrap();
            let dbpath = dir.path().join("db");
            db(
                &dbpath,
                "foo",
                "1.0-1",
                &[
                    "usr/",
                    "usr/bin/",
                    "usr/bin/foo",
                    "usr/share/foo/",
                    "var/lib/foo/",
                    "var/lib/foo/data",
                ],
            );
            db(&dbpath, "glibc", "2.42-1", &["usr/lib/libc.so.6", "usr/lib/libnew.so"]);
            db(&dbpath, "paru-git", "2.1.0.r12.gabc-1", &["usr/bin/paru"]);
            let snapshots = dir.path().join("snapshots");
            let pre_db = snapshots.join("41/snapshot").join(dbpath.strip_prefix("/").unwrap());
            db(&pre_db, "glibc", "2.41-1", &["usr/lib/libc.so.6", "usr/lib/libold.so"]);
            db(&pre_db, "oldpkg", "3.0-1", &["usr/bin/oldpkg"]);
            let sources = Sources {
                journal_root: dir.path().join("journal"),
                pacman_log: Some(dir.path().join("pacman.log")),
                dbpath: Some(dbpath.clone()),
                snapshots,
            };
            Fixture {
                _dir: dir,
                sources,
                dbpath,
                plan: plan(),
                meta: meta(7),
                log: LOG.to_string(),
                status: STATUS.to_string(),
            }
        }

        fn runner(&self) -> Fake {
            Fake::default()
                .reply("snapper -c root status 41..42", 0, &self.status)
                .reply("systemctl --system is-enabled foo.service", 0, "enabled\n")
        }

        fn run(&self) -> Report {
            let dir = read::switch_dir(&self.sources.journal_root, 7);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("meta.json"), serde_json::to_string(&self.meta).unwrap()).unwrap();
            fs::write(dir.join("plan.json"), serde_json::to_string(&self.plan).unwrap()).unwrap();
            fs::write(self.sources.pacman_log.as_ref().unwrap(), &self.log).unwrap();
            verify(7, &self.sources, &self.runner()).unwrap()
        }
    }

    fn named<'a>(report: &'a Report, prefix: &str) -> &'a Check {
        report.checks.iter().find(|c| c.name.starts_with(prefix)).unwrap()
    }

    #[test]
    fn a_faithful_switch_passes_every_check() {
        let fixture = Fixture::new();
        let report = fixture.run();
        assert!(report.pass(), "{}", report.render());
        let files = named(&report, "(b)");
        assert!(
            files.notes.iter().any(|n| n == "3 changes outside the checked trees"),
            "{files:?}"
        );
        let units = named(&report, "(c)");
        assert!(units.notes[0].contains("systemd-journald.service: not checked, applies at reboot"));
    }

    #[test]
    fn alpm_lines_outside_the_window_are_ignored() {
        let fixture = Fixture::new();
        let lines = lines_in(&parse_log(&fixture.log), window(&fixture.meta).unwrap());
        let names: Vec<&str> = lines
            .iter()
            .filter_map(|l| match &l.event {
                LogEvent::Package { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(names, ["foo", "glibc", "paru-git", "oldpkg"]);
        assert!(
            !lines
                .iter()
                .any(|l| l.event == LogEvent::Hook("dconf-update.hook".into()))
        );
    }

    #[test]
    fn mutation_an_extra_package_installed_fails() {
        let mut fixture = Fixture::new();
        fixture.log = fixture.log.replace(
            "[2026-11-16T12:00:14+0300] [ALPM] transaction completed",
            "[2026-11-16T12:00:13+0300] [ALPM] installed sneaky (1-1)\n[2026-11-16T12:00:14+0300] [ALPM] transaction completed",
        );
        let report = fixture.run();
        assert!(!report.pass());
        assert_eq!(named(&report, "(a)").items, ["unplanned: installed sneaky (1-1)"]);
    }

    #[test]
    fn a_planned_package_at_another_version_or_missing_fails() {
        let mut fixture = Fixture::new();
        fixture.log = fixture
            .log
            .replace("2.41-1 -> 2.42-1", "2.41-1 -> 2.43-1")
            .replace("[ALPM] removed oldpkg (3.0-1)", "[ALPM] transaction completed");
        let report = fixture.run();
        assert_eq!(
            named(&report, "(a)").items,
            [
                "unplanned: upgraded glibc (2.41-1 -> 2.43-1)",
                "missing: upgraded glibc (2.41-1 -> 2.42-1)",
                "missing: removed oldpkg (3.0-1)"
            ]
        );
    }

    #[test]
    fn mutation_an_extra_file_under_etc_fails() {
        let mut fixture = Fixture::new();
        fixture.status.push_str("+..... /etc/sneaky.conf\n");
        let report = fixture.run();
        assert!(!report.pass());
        assert_eq!(named(&report, "(b)").items, ["unexplained: +..... /etc/sneaky.conf"]);
    }

    #[test]
    fn mutation_a_write_under_a_planned_packages_var_lib_outside_its_file_list_fails() {
        let mut fixture = Fixture::new();
        fixture
            .status
            .push_str("+..... /var/lib/foo/extra\n+..... /var/lib/unrelated/state\n");
        let report = fixture.run();
        assert_eq!(named(&report, "(b)").items, ["unexplained: +..... /var/lib/foo/extra"]);
    }

    #[test]
    fn mutation_a_hook_writing_an_unmapped_file_fails() {
        let mut fixture = Fixture::new();
        fixture
            .log
            .push_str("[2026-11-16T12:00:16+0300] [ALPM] running '99-unmapped.hook'...\n");
        fixture
            .status
            .push_str("+..... /usr/lib/unmapped.cache\nc..... /etc/subuid\n");
        let report = fixture.run();
        assert_eq!(
            named(&report, "(b)").items,
            [
                "unexplained: +..... /usr/lib/unmapped.cache",
                "unexplained: c..... /etc/subuid (mapped to 21-systemd-tmpfiles.hook, which didn't run)"
            ]
        );
    }

    #[test]
    fn a_mapped_write_whose_hook_ran_outside_the_window_fails() {
        let mut fixture = Fixture::new();
        fixture.status.push_str("c..... /etc/dconf/db/local\n");
        let report = fixture.run();
        assert_eq!(
            named(&report, "(b)").items,
            ["unexplained: c..... /etc/dconf/db/local (mapped to dconf-update.hook, which didn't run)"]
        );
    }

    #[test]
    fn mutation_an_alpm_line_just_outside_the_window_is_ignored() {
        let mut fixture = Fixture::new();
        fixture.log = fixture.log.replace(
            "[2026-11-16T12:00:09+0300] [ALPM] installed early (1-1)",
            "[2026-11-16T12:00:09+0300] [ALPM] installed foo (1.0-1)",
        );
        assert!(fixture.run().pass());
        fixture.log = fixture
            .log
            .replace("[2026-11-16T12:00:11+0300] [ALPM] installed foo (1.0-1)\n", "");
        let report = fixture.run();
        assert_eq!(named(&report, "(a)").items, ["missing: installed foo (1.0-1)"]);
    }

    #[test]
    fn the_window_compares_across_timezone_offsets() {
        let mut fixture = Fixture::new();
        fixture.log = fixture.log.replace(
            "[2026-11-16T12:02:01+0300] [ALPM] installed late (1-1)",
            "[2026-11-16T04:02:00-0500] [ALPM] installed late (1-1)",
        );
        let report = fixture.run();
        assert_eq!(named(&report, "(a)").items, ["unplanned: installed late (1-1)"]);
    }

    #[test]
    fn removed_file_lists_come_from_the_journal_before_the_snapshot() {
        let mut fixture = Fixture::new();
        fixture
            .meta
            .removed_files
            .insert("oldpkg".into(), vec!["usr/bin/other".into()]);
        let report = fixture.run();
        assert_eq!(named(&report, "(b)").items, ["unexplained: -..... /usr/bin/oldpkg"]);
    }

    #[test]
    fn units_follow_the_plans_intent() {
        let fixture = Fixture::new();
        let mut plan = fixture.plan.clone();
        plan.services.push(ServiceItem {
            action: ServiceAction::Start,
            unit: "bar.service".into(),
            user: false,
            at_reboot: false,
            reason: "declared".into(),
            risk: Vec::new(),
        });
        let runner = Fake::default()
            .reply("systemctl --system is-enabled foo.service", 1, "disabled\n")
            .reply("systemctl --system is-active bar.service", 3, "inactive\n");
        let check = check_units(&plan, &runner);
        assert_eq!(
            check.items,
            [
                "enable foo.service: systemctl is-enabled says \"disabled\"",
                "start bar.service: systemctl is-active says \"inactive\""
            ]
        );
    }

    #[test]
    fn flatpak_and_home_manager_match_the_plan() {
        let mut plan = plan();
        plan.flatpak = [
            "install org.example.App (0123456789ab)",
            "org.example.Up 111111111111 -> 222222222222",
            "remove org.example.Gone",
        ]
        .iter()
        .map(|l| LineItem {
            line: l.to_string(),
            reason: "declared".into(),
            risk: Vec::new(),
        })
        .collect();
        let mut meta = meta(1);
        meta.home
            .insert("alice".into(), "/nix/store/aaa-home-manager-generation".into());
        let good = Fake::default()
            .reply(
                "flatpak list --system --app --columns=application,origin,active",
                0,
                "org.example.App\tflathub\t0123456789ab\norg.example.Up\tflathub\t222222222222\n",
            )
            .reply("getent passwd alice", 0, "alice:x:1000:1000::/home/alice:/bin/bash\n")
            .reply(
                "readlink -e /home/alice/.local/state/nix/profiles/home-manager",
                0,
                "/nix/store/aaa-home-manager-generation\n",
            );
        let check = check_flatpak_home(&plan, &meta, &good);
        assert!(check.pass, "{check:?}");
        let bad = Fake::default()
            .reply(
                "flatpak list --system --app --columns=application,origin,active",
                0,
                "org.example.Up\tflathub\t111111111111\norg.example.Gone\tflathub\t333333333333\n",
            )
            .reply("getent passwd alice", 0, "alice:x:1000:1000::/home/alice:/bin/bash\n")
            .reply("readlink -e /home/alice/.local/state/nix/profiles/home-manager", 1, "")
            .reply(
                "readlink -e /nix/var/nix/profiles/per-user/alice/home-manager",
                0,
                "/nix/store/bbb-home-manager-generation\n",
            );
        let check = check_flatpak_home(&plan, &meta, &bad);
        assert_eq!(
            check.items,
            [
                "flatpak install org.example.App (0123456789ab): not installed",
                "flatpak org.example.Up 111111111111 -> 222222222222: active 111111111111",
                "flatpak remove org.example.Gone: active 333333333333",
                "home alice: activated /nix/store/bbb-home-manager-generation, built /nix/store/aaa-home-manager-generation"
            ]
        );
    }

    #[test]
    fn missing_window_or_snapshots_fail_rather_than_pass() {
        let mut fixture = Fixture::new();
        fixture.meta.snapper_post = None;
        let report = fixture.run();
        assert!(named(&report, "(b)").items[0].contains("no snapper pre/post pair"));
        fixture.meta.alpm_window.end = None;
        let report = fixture.run();
        assert!(!named(&report, "(a)").pass);
    }

    #[test]
    fn snapper_status_lines_parse() {
        let changes = parse_snapper("+..... /etc/a b\nc.ug.. /usr/x/\nnoise\n-..... /opt/y\n");
        assert_eq!(
            changes,
            [
                Change {
                    flags: "+.....".into(),
                    path: "/etc/a b".into()
                },
                Change {
                    flags: "c.ug..".into(),
                    path: "/usr/x".into()
                },
                Change {
                    flags: "-.....".into(),
                    path: "/opt/y".into()
                },
            ]
        );
    }

    #[test]
    fn pacman_conf_paths_are_read_through_the_runner() {
        let fixture = Fixture::new();
        let runner = Fake::default()
            .reply("pacman-conf LogFile", 0, "/var/log/pacman.log\n")
            .reply("pacman-conf DBPath", 0, &format!("{}/\n", fixture.dbpath.display()));
        assert_eq!(
            pacman_conf(&runner, "LogFile").unwrap(),
            PathBuf::from("/var/log/pacman.log")
        );
        assert!(pacman_conf(&Fake::default().reply("pacman-conf LogFile", 1, ""), "LogFile").is_err());
        assert_eq!(runner.calls.borrow().len(), 1);
    }
}
