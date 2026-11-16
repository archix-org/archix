use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow};

use archix::alpm::{self, PacmanConf, PinMode};
use archix::date::Date;
use archix::plan::build::{Environment, PlanInputs, plan};
use archix::plan::evaluate::evaluate_for_plan;
use archix::plan::fingerprint::{self, Fingerprint, SavedPlan};
use archix::plan::record::{self, LastApplied};
use archix::plan::{Header, exit, render, repostate};
use archix::value::Value;

pub struct DiffArgs {
    pub json: bool,
    pub stat: bool,
    pub save: Option<PathBuf>,
    pub cached: bool,
}

const SCREEN_LOCKERS: [&str; 9] = [
    "swaylock",
    "hyprlock",
    "i3lock",
    "kscreenlocker_g",
    "gnome-screensav",
    "xsecurelock",
    "xscreensaver",
    "physlock",
    "slock",
];

fn is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

fn is_active(unit: &str, user: bool) -> Option<bool> {
    if user {
        return None;
    }
    let status = Command::new("systemctl")
        .args(["is-active", "--quiet", "--", unit])
        .status()
        .ok()?;
    Some(status.success())
}

fn kernel_release() -> Option<String> {
    let mut name: libc::utsname = unsafe { std::mem::zeroed() };
    if unsafe { libc::uname(&mut name) } != 0 {
        return None;
    }
    let release = unsafe { std::ffi::CStr::from_ptr(name.release.as_ptr()) };
    Some(release.to_string_lossy().into_owned())
}

fn screen_locker_running() -> bool {
    fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| fs::read_to_string(e.path().join("comm")).ok())
        .any(|comm| SCREEN_LOCKERS.contains(&comm.trim()))
}

fn display_manager(root: &Path) -> Option<String> {
    let link = fs::read_link(root.join("etc/systemd/system/display-manager.service")).ok()?;
    Some(link.file_name()?.to_string_lossy().into_owned())
}

fn visudo(content: &[u8]) -> Option<bool> {
    use std::os::unix::fs::OpenOptionsExt;
    let path = std::env::temp_dir().join(format!("archix-sudoers-{}", std::process::id()));
    let _ = fs::remove_file(&path);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .ok()?;
    let written = file.write_all(content);
    let status = written
        .ok()
        .and_then(|()| Command::new("visudo").arg("-cf").arg(&path).output().ok());
    let _ = fs::remove_file(&path);
    Some(status?.status.success())
}

fn ldd(path: &Path) -> Option<String> {
    let out = Command::new("ldd").arg(path).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn secret_hmacs(state: &Path, rendered: &archix::system::Rendered) -> Option<BTreeMap<String, String>> {
    let Some(manifest) = rendered.state.secrets.as_ref() else {
        return Some(BTreeMap::new());
    };
    archix::secrets::crypto::hmac_key(state).ok()?;
    Some(archix::secrets::prepare::hmacs(state, manifest, &|sha| {
        rendered.ciphertext(sha)
    }))
}

fn environment(flatpak: bool) -> Environment {
    let root = PathBuf::from("/");
    let state = PathBuf::from("/var/lib/archix");
    Environment {
        running_kernel_release: kernel_release(),
        screen_locker_running: screen_locker_running(),
        ssh_session: std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some(),
        chwd: BTreeSet::new(),
        snapper_configured: root.join("etc/snapper/configs/root").exists(),
        display_manager: display_manager(&root),
        enabled: Box::new(|unit, user| Some(archix::system::apply::is_enabled(Path::new("/"), unit, user))),
        active: Box::new(is_active),
        visudo: Box::new(visudo),
        ldd: Box::new(ldd),
        secret_hmacs: Box::new(move |rendered| secret_hmacs(&state, rendered)),
        flatpak_installed: (flatpak && archix::flatpak::available())
            .then(|| archix::flatpak::installed().ok())
            .flatten(),
        root,
    }
}

fn str_option<'a>(config: &'a Value, key: &str) -> Option<&'a str> {
    match config {
        Value::Map(map) => match map.get(key) {
            Some(Value::Str(s)) => Some(s),
            _ => None,
        },
        _ => None,
    }
}

/// Opens the plan's handle on `conf` in `dir`, fetching its dbs unless `cached`.
fn prepared_handle(conf: &PacmanConf, dir: &Path, cached: bool) -> Result<(::alpm::Alpm, PathBuf)> {
    let dbpath = alpm::plan_dbpath(dir, &conf.db_path())?;
    let mut handle = alpm::open(conf, &dbpath)?;
    if cached {
        for repo in &conf.repos {
            let db = dbpath.join("sync").join(format!("{}.db", repo.name));
            if !db.exists() {
                return Err(anyhow!(
                    "--cached, but there is no cached db for {} in {}",
                    repo.name,
                    dbpath.display()
                ));
            }
        }
    } else {
        alpm::with_plan_dbpath_lock(&dbpath, || {
            alpm::fetch(&mut handle).context("fetching the sync dbs (offline?)")?;
            let _ = alpm::fetch_files(conf, &dbpath);
            Ok(())
        })?;
    }
    Ok((handle, dbpath))
}

fn sync_dir(conf: &PacmanConf) -> PathBuf {
    let key = &fingerprint::hash_bytes(conf.render().as_bytes())[..16];
    match is_root() {
        true => alpm::root_sync_dir(key),
        false => alpm::user_sync_dir().join(key),
    }
}

fn out(text: &str) {
    let _ = std::io::stdout().write_all(text.as_bytes());
}

/// `archix diff`: plan(), printed; returns SCOPE 3.1's exit code.
pub fn diff(config_dir: &Path, host: &str, args: &DiffArgs) -> Result<u8> {
    let evaluation = evaluate_for_plan(config_dir, host)?;
    let config = &evaluation.config;
    let tree = evaluation.tree.as_path();
    let system_conf = PacmanConf::read(None)?;
    let lock_path = evaluation.lock_path(config_dir)?;
    let lock = match &lock_path {
        Some(path) => archix::lock::Lockfile::load(path)?.hosts.remove(host),
        None => None,
    };
    let mode = system_conf.pin_mode();
    let archive = str_option(config, "archive_url")
        .unwrap_or(archix::archive::ARCHIVE_URL)
        .to_string();
    let lock_date = lock.as_ref().map(|l| l.date).filter(|_| mode == PinMode::Archive);
    let conf_at = |date: Option<Date>| match date {
        Some(date) => system_conf.pinned(&archive, date),
        None => system_conf.clone(),
    };

    let mut date = lock_date;
    let mut conf = conf_at(date);
    let (mut handle, mut dbpath) = prepared_handle(&conf, &sync_dir(&conf), args.cached)?;
    let mut moved_from = None;
    if let Some(start) = lock_date {
        let installed: BTreeMap<String, String> = archix::plan::packages::local_packages(&handle)
            .into_iter()
            .map(|(n, p)| (n, p.version))
            .collect();
        let at_lock = alpm::sync_versions(&handle);
        if !repostate::newer_than(&installed, &at_lock).is_empty() {
            let (found, _) = repostate::earliest_not_older(start, 60, &installed, |d| {
                let c = conf_at(Some(d));
                let (h, _) = prepared_handle(&c, &sync_dir(&c), args.cached)?;
                Ok(alpm::sync_versions(&h))
            })?;
            date = Some(found);
            conf = conf_at(date);
            (handle, dbpath) = prepared_handle(&conf, &sync_dir(&conf), true)?;
            moved_from = Some(at_lock);
        }
    }

    let last = LastApplied::load(&record::default_path())?;
    let aur_commits = lock
        .as_ref()
        .map(|l| {
            l.packages
                .iter()
                .filter(|(_, p)| p.repo == archix::lock::AUR_REPO)
                .filter_map(|(n, p)| p.commit.clone().map(|c| (n.clone(), c)))
                .collect()
        })
        .unwrap_or_default();
    let flatpak = lock.as_ref().map(|l| l.flatpak.clone()).unwrap_or_default();
    let env = environment(!flatpak.is_empty());
    let header = Header {
        mode,
        date,
        moved_from: moved_from.as_ref().and(lock_date),
        fingerprint: String::new(),
        cached_dbs: args.cached,
    };
    let mut plan = plan(
        &mut handle,
        &dbpath,
        &PlanInputs {
            config,
            config_dir: tree,
            conf: &conf,
            header,
            moved_from,
            last: last.as_ref(),
            aur_commits,
            flatpak,
            env: &env,
        },
    )?;

    let rendered = archix::system::render(config, tree)?;
    let print = Fingerprint {
        config_tree: fingerprint::hash_config_tree(tree)?,
        lock: lock_path
            .as_deref()
            .map_or_else(|| "missing".into(), fingerprint::hash_file),
        pacman_conf: fingerprint::hash_bytes(conf.render().as_bytes()),
        sync_dbs: fingerprint::hash_sync_dbs(&dbpath, &conf.repos.iter().map(|r| r.name.clone()).collect::<Vec<_>>()),
        local_db: fingerprint::hash_local_db(&conf.db_path())?,
        managed_files: rendered
            .state
            .files
            .keys()
            .map(|p| {
                (
                    p.clone(),
                    fingerprint::hash_file(&env.root.join(p.trim_start_matches('/'))),
                )
            })
            .collect(),
        archix_version: env!("CARGO_PKG_VERSION").to_string(),
        unchecked: plan.unchecked.clone(),
    };
    plan.header.fingerprint = print.digest();

    if args.json {
        out(&render::json(&plan));
    } else if args.stat {
        out(&render::stat(&plan));
    } else {
        out(&render::human(&plan));
    }
    if let Some(path) = &args.save {
        SavedPlan {
            plan: plan.clone(),
            fingerprint: print,
        }
        .save(path)?;
    }
    Ok(plan.exit_code())
}

/// The exit code for an error: SCOPE 3.1's 3, offline included.
pub fn error_code() -> u8 {
    exit::ERROR
}
