use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use alpm::Alpm;
use anyhow::Result;

use super::hooks::{self, Change, Op};
use super::model::{FileAction, Header, LineItem, PkgAction, Plan};
use super::packages::{self, LocalPkg, PackageInputs};
use super::record::LastApplied;
use super::risk::{self, RiskContext};
use super::system::{self, SystemInputs};
use crate::alpm::PacmanConf;
use crate::flatpak::{InstalledApp, LockedApp};
use crate::value::Value;

const HELPERS: [&str; 6] = ["paru", "paru-bin", "paru-git", "yay", "yay-bin", "yay-git"];

pub type UnitState = Box<dyn Fn(&str, bool) -> Option<bool>>;
pub type Visudo = Box<dyn Fn(&[u8]) -> Option<bool>>;
pub type Ldd = Box<dyn Fn(&Path) -> Option<String>>;
pub type SecretHmacs = Box<dyn Fn(&crate::system::Rendered) -> Option<BTreeMap<String, String>>>;

/// What plan() reads from the machine beyond the dbs and files, injected so tests stay pure.
pub struct Environment {
    pub root: PathBuf,
    pub running_kernel_release: Option<String>,
    pub screen_locker_running: bool,
    pub ssh_session: bool,
    pub chwd: BTreeSet<String>,
    pub snapper_configured: bool,
    pub display_manager: Option<String>,
    pub enabled: UnitState,
    pub visudo: Visudo,
    /// `ldd` of a binary under the root; None when it isn't a dynamic executable or ldd can't run.
    pub ldd: Ldd,
    /// HMACs of the rendered secrets' values under the machine key; None without the key.
    pub secret_hmacs: SecretHmacs,
    pub flatpak_installed: Option<BTreeMap<String, InstalledApp>>,
}

pub struct PlanInputs<'a> {
    pub config: &'a Value,
    pub config_dir: &'a Path,
    pub conf: &'a PacmanConf,
    pub header: Header,
    pub moved_from: Option<BTreeMap<String, String>>,
    pub last: Option<&'a LastApplied>,
    pub aur_commits: BTreeMap<String, String>,
    pub flatpak: BTreeMap<String, LockedApp>,
    pub env: &'a Environment,
}

fn get<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    match value {
        Value::Map(map) => map.get(key),
        _ => None,
    }
}

fn strings(value: Option<&Value>) -> BTreeSet<String> {
    match value {
        Some(Value::List(items)) => items
            .iter()
            .filter_map(|i| match i {
                Value::Str(s) => Some(s.clone()),
                _ => None,
            })
            .collect(),
        _ => BTreeSet::new(),
    }
}

fn helper(config: &Value, local: &BTreeMap<String, LocalPkg>) -> Option<String> {
    if let Some(Value::Str(h)) = get(config, "aur").and_then(|a| get(a, "helper")) {
        return Some(h.clone());
    }
    HELPERS.iter().find(|h| local.contains_key(**h)).map(|h| h.to_string())
}

fn closure_binaries(shells: &[String], display_manager: Option<&str>) -> Vec<String> {
    let mut bins: Vec<String> = [
        "usr/bin/pacman",
        "usr/bin/sudo",
        "usr/lib/systemd/systemd",
        "usr/bin/login",
        "usr/bin/agetty",
        "usr/bin/mkinitcpio",
        "usr/bin/bash",
        "usr/bin/limine",
    ]
    .map(String::from)
    .to_vec();
    bins.extend(shells.iter().map(|s| s.trim_start_matches('/').to_string()));
    if let Some(dm) = display_manager {
        bins.push(format!("usr/bin/{}", dm.trim_end_matches(".service")));
    }
    bins.sort();
    bins.dedup();
    bins
}

fn new_files(conf: &PacmanConf, dbpath: &Path) -> BTreeMap<String, Vec<String>> {
    let mut out = BTreeMap::new();
    for repo in &conf.repos {
        let path = dbpath.join("sync").join(format!("{}.files", repo.name));
        if let Ok(lists) = crate::alpm::read_files_db(&path) {
            for (name, files) in lists {
                out.entry(name).or_insert(files);
            }
        }
    }
    out
}

/// SCOPE 3's one plan(): the config and lock against the system, with libalpm prepared on `handle` (the plan's DBPath).
pub fn plan(handle: &mut Alpm, dbpath: &Path, inputs: &PlanInputs) -> Result<Plan> {
    let env = inputs.env;
    let config = inputs.config;
    let rendered = crate::system::render(config, inputs.config_dir)?;
    let local = packages::local_packages(handle);
    let helper = helper(config, &local);

    let shells: Vec<(String, String)> = rendered
        .state
        .users
        .iter()
        .filter(|(_, u)| !u.shell.is_empty())
        .map(|(n, u)| (n.clone(), u.shell.clone()))
        .collect();
    let etc_shells = match rendered.state.files.get("/etc/shells") {
        Some(f) => rendered
            .blobs
            .get(&f.sha256)
            .map(|b| String::from_utf8_lossy(b).into_owned()),
        None => fs::read_to_string(env.root.join("etc/shells")).ok(),
    };
    let owners: BTreeMap<&str, &str> = local
        .iter()
        .flat_map(|(name, p)| p.files.iter().map(move |f| (f.as_str(), name.as_str())))
        .collect();
    let shell_paths: Vec<String> = shells.iter().map(|(_, s)| s.clone()).collect();
    let binaries: Vec<(String, String)> = closure_binaries(&shell_paths, env.display_manager.as_deref())
        .into_iter()
        .filter(|bin| owners.contains_key(bin.as_str()))
        .map(|bin| {
            let ldd = (env.ldd)(&env.root.join(&bin)).unwrap_or_default();
            (bin, ldd)
        })
        .collect();
    let closure = risk::closure(&binaries, &|path| owners.get(path).map(|s| s.to_string()));
    let risk_ctx = RiskContext {
        closure,
        chwd: env.chwd.clone(),
        running_kernel: packages::running_kernel(&local, env.running_kernel_release.as_deref()),
        helper: helper.clone(),
        screen_locker_running: env.screen_locker_running,
    };

    let package_inputs = PackageInputs {
        declared: strings(get(config, "packages")),
        ignored: strings(get(config, "ignored_packages")),
        remove_undeclared: get(config, "undeclared_packages") != Some(&Value::Str("warn".into())),
        last: inputs.last,
        moved_from: inputs.moved_from.clone(),
        new_files: new_files(inputs.conf, dbpath),
        running_kernel_release: env.running_kernel_release.clone(),
        risk: risk_ctx,
        aur_commits: inputs.aur_commits.clone(),
        shells,
        etc_shells,
    };
    let group = packages::build(handle, &package_inputs)?;

    let mut plan = Plan::new(inputs.header.clone());
    plan.packages = group.items;
    plan.aur = group.aur;
    plan.refusals.extend(group.refusals);
    plan.notes.extend(group.notes);
    for name in ["archix", "archix-git"] {
        if plan
            .packages
            .iter()
            .any(|p| p.name == name && p.action == PkgAction::Upgrade)
        {
            plan.notes.push(format!(
                "{name} upgrades on the next switch or with pacman, never inside its own switch"
            ));
        }
    }

    let changes: Vec<Change> = plan
        .packages
        .iter()
        .filter_map(|p| {
            let op = match p.action {
                PkgAction::Install => Op::Install,
                PkgAction::Upgrade | PkgAction::Downgrade => Op::Upgrade,
                PkgAction::Remove => Op::Remove,
                _ => return None,
            };
            let files = group
                .final_files
                .get(&p.name)
                .or_else(|| local.get(&p.name).map(|l| &l.files))
                .cloned()
                .unwrap_or_default();
            Some(Change {
                op,
                name: p.name.clone(),
                files,
            })
        })
        .collect();
    plan.hooks = hooks::items(&hooks::load(&inputs.conf.hook_dirs()), &changes);

    let from = inputs.last.map(|l| &l.system);
    let empty = BTreeMap::new();
    let hmacs = (env.secret_hmacs)(&rendered);
    let sys = system::build(&SystemInputs {
        root: &env.root,
        to: &rendered.state,
        blobs: &rendered.blobs,
        from,
        enabled: &*env.enabled,
        secret_hmacs: hmacs.as_ref(),
        last_hmacs: inputs.last.map_or(&empty, |l| &l.secret_hmacs),
        ssh_session: env.ssh_session,
        display_manager: env.display_manager.as_deref(),
        visudo: &*env.visudo,
    });
    let home_dirs: Vec<String> = ["/home/", "/root/"].map(String::from).to_vec();
    for item in sys.files {
        if home_dirs.iter().any(|d| item.path.starts_with(d)) {
            let verb = match item.action {
                FileAction::Create => "create",
                FileAction::Modify => "modify",
                FileAction::Delete => "delete",
                FileAction::Mode => "mode",
                FileAction::Owner => "owner",
                FileAction::Unmanage => "unmanage",
            };
            plan.home.push(LineItem {
                line: format!("{verb} {}", item.path),
                reason: item.reason,
                risk: item.risk,
            });
        } else {
            plan.files.push(item);
        }
    }
    plan.services = sys.services;
    plan.users = sys.users;
    plan.refusals.extend(sys.refusals);
    plan.unchecked.extend(sys.unchecked);
    plan.notes.extend(rendered.warnings);

    let fstab = crate::system::fstab::render(config, crate::system::fstab::esp_of(config))?;
    for line in crate::system::fstab::describe_at(&env.root, config, &fstab.lines) {
        plan.files.push(super::model::FileItem {
            action: FileAction::Modify,
            path: "/etc/fstab".into(),
            reason: line.trim_start_matches("fstab").trim().to_string(),
            diff: None,
            secret: None,
            hand_edited: false,
            risk: risk::file_risks("/etc/fstab"),
        });
    }

    if let Some(Value::Map(users)) = get(config, "home")
        && users.values().any(|u| get(u, "home_manager").is_some())
    {
        plan.unchecked
            .push("home: home-manager build isn't run by this diff yet".into());
    }

    if !inputs.flatpak.is_empty() {
        match &env.flatpak_installed {
            Some(installed) => {
                let previous = BTreeMap::new();
                let (lines, _) = crate::flatpak::describe(&inputs.flatpak, &previous, installed);
                for line in lines {
                    plan.flatpak.push(LineItem {
                        line: line.trim_start_matches("flatpak").trim().to_string(),
                        reason: "declared, from a live remote".into(),
                        risk: Vec::new(),
                    });
                }
            }
            None => plan.unchecked.push("flatpak: installed apps unknown".into()),
        }
    }

    if plan.has_changes() {
        plan.snapshot = Some(match env.snapper_configured {
            true => LineItem {
                line: "snapper pre and post snapshots".into(),
                reason: "snapper is set up".into(),
                risk: Vec::new(),
            },
            false => LineItem {
                line: "no pre-snapshot".into(),
                reason: "snapper isn't set up; the switch needs --no-snapshot".into(),
                risk: vec!["no pre-snapshot".into()],
            },
        });
    }
    plan.unchecked.sort();
    plan.unchecked.dedup();
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alpm::testdb::{TestRoot, pkg};
    use crate::alpm::{PinMode, open};
    use crate::plan::model::{PkgReason, exit};

    fn env(root: PathBuf) -> Environment {
        Environment {
            root,
            running_kernel_release: Some("6.1-arch1".into()),
            screen_locker_running: false,
            ssh_session: false,
            chwd: BTreeSet::new(),
            snapper_configured: false,
            display_manager: None,
            enabled: Box::new(|_, _| Some(false)),
            visudo: Box::new(|_| Some(true)),
            ldd: Box::new(|path| {
                path.ends_with("usr/bin/pacman")
                    .then(|| "\tlibz.so.1 => /usr/lib/libz.so.1 (0x1)\n".to_string())
            }),
            secret_hmacs: Box::new(|_| None),
            flatpak_installed: None,
        }
    }

    fn config(packages: &[&str]) -> Value {
        let mut map = BTreeMap::new();
        map.insert(
            "packages".to_string(),
            Value::List(packages.iter().map(|p| Value::Str(p.to_string())).collect()),
        );
        let mut files = BTreeMap::new();
        let mut spec = BTreeMap::new();
        spec.insert("text".to_string(), Value::Str("hello\n".into()));
        spec.insert("mode".to_string(), Value::Str("0644".into()));
        files.insert("/etc/motd".to_string(), Value::Map(spec));
        map.insert("files".to_string(), Value::Map(files));
        Value::Map(map)
    }

    fn header() -> Header {
        Header {
            mode: PinMode::Live,
            date: None,
            moved_from: None,
            fingerprint: String::new(),
            cached_dbs: false,
        }
    }

    #[test]
    fn plan_builds_every_group_from_a_synthetic_system() {
        let t = TestRoot::new();
        t.install(&[
            pkg("linux", "6.1-1").files(&["usr/lib/modules/6.1-arch1/vmlinuz"]),
            pkg("pacman", "7.1-1").files(&["usr/bin/pacman"]),
            pkg("zlib", "1.3-1").dep().files(&["usr/lib/libz.so.1"]),
            pkg("htop", "3-1"),
        ]);
        t.repo(
            "core",
            &[
                pkg("linux", "6.1-1"),
                pkg("pacman", "7.1-1"),
                pkg("zlib", "1.3.1-1").files(&["usr/lib/libz.so.1"]),
                pkg("ripgrep", "14-1").files(&["usr/bin/rg"]),
            ],
        );
        let hooks = t.root().join("etc/pacman.d/hooks");
        fs::create_dir_all(&hooks).unwrap();
        fs::write(
            hooks.join("ldconfig.hook"),
            "[Trigger]\nOperation = Upgrade\nType = Path\nTarget = usr/lib/*.so*\n[Action]\nDescription = ldconfig\nWhen = PostTransaction\n",
        )
        .unwrap();
        let conf = t.conf(&["core"]);
        let dbpath = t.plan_dbpath();
        let mut handle = open(&conf, &dbpath).unwrap();
        let config = config(&["linux", "ripgrep"]);
        let cfgdir = tempfile::tempdir().unwrap();
        let env = env(t.root());
        let plan = plan(
            &mut handle,
            &dbpath,
            &PlanInputs {
                config: &config,
                config_dir: cfgdir.path(),
                conf: &conf,
                header: header(),
                moved_from: None,
                last: None,
                aur_commits: BTreeMap::new(),
                flatpak: BTreeMap::new(),
                env: &env,
            },
        )
        .unwrap();
        let find = |n: &str| plan.packages.iter().find(|p| p.name == n).unwrap();
        assert_eq!(find("ripgrep").action, PkgAction::Install);
        assert_eq!(find("htop").reason, PkgReason::DriftInstalled);
        let zlib = find("zlib");
        assert!(zlib.risk.iter().any(|r| r.contains("closure")), "{zlib:?}");
        assert!(plan.packages.iter().all(|p| p.name != "pacman"));
        assert_eq!(plan.hooks.len(), 1);
        assert_eq!(plan.hooks[0].reason, "zlib");
        assert_eq!(plan.files[0].path, "/etc/motd");
        assert_eq!(plan.files[0].action, FileAction::Create);
        assert_eq!(plan.snapshot.as_ref().unwrap().risk, ["no pre-snapshot"]);
        assert_eq!(plan.exit_code(), exit::CHANGES, "{plan:#?}");
        let text = super::super::render::human(&plan);
        assert!(text.contains("! no pre-snapshot"), "{text}");
    }
}
