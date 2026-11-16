use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use alpm::{Alpm, Question, SigLevel, TransFlag, Usage};
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use super::conf::PacmanConf;

/// pacman's own process_siglevel over `tokens`, starting from `base`.
pub fn siglevel(tokens: &[&str], base: SigLevel) -> Result<SigLevel> {
    let mut level = base;
    for token in tokens.iter().flat_map(|t| t.split_whitespace()) {
        let (package, database, word) = if let Some(rest) = token.strip_prefix("Package") {
            (true, false, rest)
        } else if let Some(rest) = token.strip_prefix("Database") {
            (false, true, rest)
        } else {
            (true, true, token)
        };
        let set = |level: &mut SigLevel, pkg: SigLevel, db: SigLevel, on: bool| {
            for (want, flag) in [(package, pkg), (database, db)] {
                if want {
                    level.set(flag, on);
                }
            }
        };
        match word {
            "Never" => {
                set(&mut level, SigLevel::PACKAGE, SigLevel::DATABASE, false);
                set(
                    &mut level,
                    SigLevel::PACKAGE_OPTIONAL,
                    SigLevel::DATABASE_OPTIONAL,
                    true,
                );
            }
            "Optional" => {
                set(&mut level, SigLevel::PACKAGE, SigLevel::DATABASE, true);
                set(
                    &mut level,
                    SigLevel::PACKAGE_OPTIONAL,
                    SigLevel::DATABASE_OPTIONAL,
                    true,
                );
            }
            "Required" => {
                set(&mut level, SigLevel::PACKAGE, SigLevel::DATABASE, true);
                set(
                    &mut level,
                    SigLevel::PACKAGE_OPTIONAL,
                    SigLevel::DATABASE_OPTIONAL,
                    false,
                );
            }
            "TrustedOnly" => {
                set(
                    &mut level,
                    SigLevel::PACKAGE_MARGINAL_OK,
                    SigLevel::DATABASE_MARGINAL_OK,
                    false,
                );
                set(
                    &mut level,
                    SigLevel::PACKAGE_UNKNOWN_OK,
                    SigLevel::DATABASE_UNKNOWN_OK,
                    false,
                );
            }
            "TrustAll" => {
                set(
                    &mut level,
                    SigLevel::PACKAGE_MARGINAL_OK,
                    SigLevel::DATABASE_MARGINAL_OK,
                    true,
                );
                set(
                    &mut level,
                    SigLevel::PACKAGE_UNKNOWN_OK,
                    SigLevel::DATABASE_UNKNOWN_OK,
                    true,
                );
            }
            other => bail!("unknown SigLevel {other:?}"),
        }
    }
    Ok(level)
}

fn default_siglevel() -> SigLevel {
    SigLevel::PACKAGE | SigLevel::PACKAGE_OPTIONAL | SigLevel::DATABASE | SigLevel::DATABASE_OPTIONAL
}

fn usage(values: &[String]) -> Usage {
    if values.is_empty() {
        return Usage::ALL;
    }
    let mut usage = Usage::NONE;
    for word in values.iter().flat_map(|v| v.split_whitespace()) {
        usage |= match word {
            "Sync" => Usage::SYNC,
            "Search" => Usage::SEARCH,
            "Install" => Usage::INSTALL,
            "Upgrade" => Usage::UPGRADE,
            _ => Usage::ALL,
        };
    }
    usage
}

/// Where a user's diff keeps its sync dbs.
pub fn user_sync_dir() -> PathBuf {
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    cache.join("archix/sync")
}

/// Where root's plan keeps the sync dbs of the plan with `fingerprint`.
pub fn root_sync_dir(fingerprint: &str) -> PathBuf {
    PathBuf::from("/var/cache/archix/sync").join(fingerprint)
}

/// Makes `dir` a DBPath whose sync/ is the plan's and whose local is a link to the real local db, as checkupdates does.
pub fn plan_dbpath(dir: &Path, real_dbpath: &Path) -> Result<PathBuf> {
    fs::create_dir_all(dir.join("sync")).with_context(|| format!("creating {}", dir.display()))?;
    let local = dir.join("local");
    let target = real_dbpath.join("local");
    match fs::read_link(&local) {
        Ok(existing) if existing == target => {}
        Ok(_) => {
            fs::remove_file(&local)?;
            std::os::unix::fs::symlink(&target, &local)?;
        }
        Err(_) => std::os::unix::fs::symlink(&target, &local)
            .with_context(|| format!("linking {} to {}", local.display(), target.display()))?,
    }
    Ok(dir.to_path_buf())
}

/// A libalpm handle with every setting pacman would apply from `conf`, on `dbpath` (the plan's).
pub fn open(conf: &PacmanConf, dbpath: &Path) -> Result<Alpm> {
    let root = conf.root_dir();
    let mut handle = Alpm::new(
        root.to_string_lossy().as_bytes().to_vec(),
        dbpath.to_string_lossy().as_bytes().to_vec(),
    )
    .map_err(|e| anyhow!("initialising libalpm on {}: {e}", dbpath.display()))?;
    let alpm_err = |what: &str, e: alpm::Error| anyhow!("setting {what}: {e}");
    let cachedirs = conf.values("CacheDir");
    let cachedirs = if cachedirs.is_empty() {
        vec!["/var/cache/pacman/pkg/"]
    } else {
        cachedirs
    };
    handle
        .set_cachedirs(cachedirs.iter())
        .map_err(|e| alpm_err("CacheDir", e))?;
    handle
        .set_hookdirs(conf.hook_dirs().iter().map(String::as_str))
        .map_err(|e| alpm_err("HookDir", e))?;
    if let Some(log) = conf.value("LogFile") {
        handle.set_logfile(log).map_err(|e| alpm_err("LogFile", e))?;
    }
    if let Some(gpg) = conf.value("GPGDir") {
        handle.set_gpgdir(gpg).map_err(|e| alpm_err("GPGDir", e))?;
    }
    // Switching to DownloadUser needs root; a user diff downloads as itself.
    if unsafe { libc::geteuid() } == 0 {
        handle
            .set_sandbox_user(conf.value("DownloadUser"))
            .map_err(|e| alpm_err("DownloadUser", e))?;
    }
    handle.set_disable_sandbox_filesystem(conf.flag("DisableSandbox") || conf.flag("DisableSandboxFilesystem"));
    handle.set_disable_sandbox_syscalls(conf.flag("DisableSandbox") || conf.flag("DisableSandboxSyscalls"));
    if let Some(n) = conf.value("ParallelDownloads").and_then(|n| n.parse().ok()) {
        handle.set_parallel_downloads(n);
    }
    handle.set_disable_dl_timeout(conf.flag("DisableDownloadTimeout"));
    handle.set_check_space(conf.flag("CheckSpace"));
    handle.set_use_syslog(conf.flag("UseSyslog"));
    let words = |key: &str| -> Vec<String> {
        conf.values(key)
            .iter()
            .flat_map(|v| v.split_whitespace())
            .map(str::to_string)
            .collect()
    };
    handle
        .set_noupgrades(words("NoUpgrade").iter().map(String::as_str))
        .map_err(|e| alpm_err("NoUpgrade", e))?;
    handle
        .set_noextracts(words("NoExtract").iter().map(String::as_str))
        .map_err(|e| alpm_err("NoExtract", e))?;
    handle
        .set_ignorepkgs(words("IgnorePkg").iter().map(String::as_str))
        .map_err(|e| alpm_err("IgnorePkg", e))?;
    handle
        .set_ignoregroups(words("IgnoreGroup").iter().map(String::as_str))
        .map_err(|e| alpm_err("IgnoreGroup", e))?;
    let arches = words("Architecture");
    let arches: Vec<String> = arches
        .into_iter()
        .map(|a| {
            if a == "auto" {
                std::env::consts::ARCH.to_string()
            } else {
                a
            }
        })
        .collect();
    handle
        .set_architectures(arches.iter().map(String::as_str))
        .map_err(|e| alpm_err("Architecture", e))?;
    let default = siglevel(&conf.values("SigLevel"), default_siglevel())?;
    handle
        .set_default_siglevel(default)
        .map_err(|e| alpm_err("SigLevel", e))?;
    let local = siglevel(&conf.values("LocalFileSigLevel"), default)?;
    handle
        .set_local_file_siglevel(local)
        .map_err(|e| alpm_err("LocalFileSigLevel", e))?;
    let remote = siglevel(&conf.values("RemoteFileSigLevel"), default)?;
    handle
        .set_remote_file_siglevel(remote)
        .map_err(|e| alpm_err("RemoteFileSigLevel", e))?;
    for repo in &conf.repos {
        let level = match repo.sig_level.is_empty() {
            true => SigLevel::USE_DEFAULT,
            false => siglevel(&repo.sig_level.iter().map(String::as_str).collect::<Vec<_>>(), default)?,
        };
        let db = handle
            .register_syncdb_mut(repo.name.as_str(), level)
            .map_err(|e| anyhow!("registering repo {}: {e}", repo.name))?;
        db.set_servers(repo.servers.iter().map(String::as_str))
            .map_err(|e| anyhow!("servers of {}: {e}", repo.name))?;
        db.set_usage(usage(&repo.usage))
            .map_err(|e| anyhow!("usage of {}: {e}", repo.name))?;
    }
    Ok(handle)
}

/// Downloads the sync dbs into the handle's DBPath; plan() calls this once and a switch never again.
pub fn fetch(handle: &mut Alpm) -> Result<()> {
    if handle.syncdbs().is_empty() {
        return Ok(());
    }
    handle
        .syncdbs_mut()
        .update(false)
        .map_err(|e| anyhow!("fetching the sync dbs: {e}"))?;
    Ok(())
}

/// Downloads the `.files` dbs next to the `.db` ones, for the file lists of packages not installed yet.
pub fn fetch_files(conf: &PacmanConf, dbpath: &Path) -> Result<()> {
    let mut handle = open(conf, dbpath)?;
    handle.set_dbext(".files");
    fetch(&mut handle)
}

/// Each package's version in the handle's sync dbs, the first repo winning as pacman's order does.
pub fn sync_versions(handle: &Alpm) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for db in handle.syncdbs() {
        for pkg in db.pkgs() {
            out.entry(pkg.name().to_string())
                .or_insert_with(|| pkg.version().to_string());
        }
    }
    out
}

/// How a prepare answers libalpm's questions.
#[derive(Debug, Clone)]
pub enum Answers {
    /// plan(): accept replacements and conflict removals and record them as the plan's conflict replacements.
    Record,
    /// switch: accept only these replaced package names, refuse everything else.
    Only(BTreeSet<String>),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Added {
    pub name: String,
    pub version: String,
    pub repo: String,
    pub old_version: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Prepared {
    pub add: Vec<Added>,
    pub remove: Vec<(String, String)>,
    /// Installed package replaced → the package replacing it.
    pub replaced: BTreeMap<String, String>,
}

fn install_questions(handle: &Alpm, answers: Answers) -> Rc<RefCell<BTreeMap<String, String>>> {
    let replaced = Rc::new(RefCell::new(BTreeMap::new()));
    let record = replaced.clone();
    handle.set_question_cb((), move |question, _| {
        // A panic must never unwind into libalpm.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let accept = |old: &str| match &answers {
                Answers::Record => true,
                Answers::Only(planned) => planned.contains(old),
            };
            match question.question() {
                Question::Replace(q) => {
                    let (old, new) = (q.oldpkg().name().to_string(), q.newpkg().name().to_string());
                    let yes = accept(&old);
                    q.set_replace(yes);
                    if yes {
                        record.borrow_mut().insert(old, new);
                    }
                }
                Question::Conflict(mut q) => {
                    let (new, old) = (
                        q.conflict().package1().name().to_string(),
                        q.conflict().package2().name().to_string(),
                    );
                    let yes = accept(&old);
                    q.set_remove(yes);
                    if yes {
                        record.borrow_mut().insert(old, new);
                    }
                }
                Question::SelectProvider(mut q) => q.set_index(0),
                Question::InstallIgnorepkg(mut q) => q.set_install(false),
                Question::Corrupted(mut q) => q.set_remove(false),
                Question::RemovePkgs(mut q) => q.set_skip(false),
                Question::ImportKey(mut q) => q.set_import(false),
            }
        }));
    });
    replaced
}

fn prepare_error(e: alpm::PrepareError) -> anyhow::Error {
    let detail = match e.data() {
        Some(alpm::PrepareData::UnsatisfiedDeps(deps)) => deps
            .iter()
            .map(|d| format!("{} requires {}", d.target(), d.depend()))
            .collect::<Vec<_>>()
            .join("; "),
        Some(alpm::PrepareData::ConflictingDeps(conflicts)) => conflicts
            .iter()
            .map(|c| format!("{} conflicts with {}", c.package1().name(), c.package2().name()))
            .collect::<Vec<_>>()
            .join("; "),
        Some(alpm::PrepareData::PkgInvalidArch(pkgs)) => pkgs
            .iter()
            .map(|p| format!("{} has an invalid architecture", p.name()))
            .collect::<Vec<_>>()
            .join("; "),
        None => String::new(),
    };
    match detail.is_empty() {
        true => anyhow!("libalpm refused the transaction: {}", e.error()),
        false => anyhow!("libalpm refused the transaction: {}: {detail}", e.error()),
    }
}

fn collect(handle: &Alpm, replaced: BTreeMap<String, String>) -> Prepared {
    let local = handle.localdb();
    let mut add: Vec<Added> = handle
        .trans_add()
        .iter()
        .map(|pkg| Added {
            name: pkg.name().to_string(),
            version: pkg.version().to_string(),
            repo: pkg.db().map(|db| db.name().to_string()).unwrap_or_default(),
            old_version: local.pkg(pkg.name()).ok().map(|old| old.version().to_string()),
        })
        .collect();
    add.sort_by(|a, b| a.name.cmp(&b.name));
    let mut remove: Vec<(String, String)> = handle
        .trans_remove()
        .iter()
        .map(|pkg| (pkg.name().to_string(), pkg.version().to_string()))
        .collect();
    remove.sort();
    Prepared { add, remove, replaced }
}

/// Prepares, never commits, a sync transaction for `targets` (plus a full upgrade if asked), without taking the lock.
pub fn prepare_sync(handle: &mut Alpm, targets: &[String], sysupgrade: bool, answers: Answers) -> Result<Prepared> {
    prepare_sync_skipping(handle, targets, sysupgrade, answers, &[])
}

/// As prepare_sync, with `skip` (what the removal transaction takes) left out of the full upgrade as IgnorePkg would.
pub fn prepare_sync_skipping(
    handle: &mut Alpm,
    targets: &[String],
    sysupgrade: bool,
    answers: Answers,
    skip: &[String],
) -> Result<Prepared> {
    let added: Vec<&String> = skip
        .iter()
        .filter(|name| !handle.ignorepkgs().iter().any(|i| i == name.as_str()))
        .collect();
    for name in &added {
        handle
            .add_ignorepkg(name.as_str())
            .map_err(|e| anyhow!("ignoring {name}: {e}"))?;
    }
    let result = prepare_sync_inner(handle, targets, sysupgrade, answers);
    for name in added {
        let _ = handle.remove_ignorepkg(name.as_str());
    }
    result
}

fn prepare_sync_inner(handle: &mut Alpm, targets: &[String], sysupgrade: bool, answers: Answers) -> Result<Prepared> {
    let replaced = install_questions(handle, answers);
    handle
        .trans_init(TransFlag::NO_LOCK)
        .map_err(|e| anyhow!("starting a libalpm transaction: {e}"))?;
    let result = (|| {
        if sysupgrade {
            handle
                .sync_sysupgrade(false)
                .map_err(|e| anyhow!("full upgrade: {e}"))?;
        }
        for target in targets {
            let pkg = handle
                .syncdbs()
                .iter()
                .find_map(|db| db.pkg(target.as_str()).ok())
                .ok_or_else(|| anyhow!("target not found in any repo: {target}"))?;
            match handle.trans_add_pkg(pkg) {
                Ok(()) => {}
                Err(e) if e.error == alpm::Error::TransDupTarget => {}
                Err(e) => return Err(anyhow!("adding {target}: {}", e.error)),
            }
        }
        handle.trans_prepare().map_err(prepare_error)?;
        Ok(collect(handle, replaced.borrow().clone()))
    })();
    let _ = handle.trans_release();
    result
}

/// Prepares, never commits, a removal of `targets` as `pacman -Rs`, without taking the lock.
pub fn prepare_remove(handle: &mut Alpm, targets: &[String]) -> Result<Prepared> {
    handle
        .trans_init(TransFlag::NO_LOCK | TransFlag::RECURSE)
        .map_err(|e| anyhow!("starting a libalpm transaction: {e}"))?;
    let result = (|| {
        for target in targets {
            let pkg = handle
                .localdb()
                .pkg(target.as_str())
                .map_err(|_| anyhow!("{target} is not installed"))?;
            handle
                .trans_remove_pkg(pkg)
                .map_err(|e| anyhow!("removing {target}: {e}"))?;
        }
        handle.trans_prepare().map_err(prepare_error)?;
        Ok(collect(handle, BTreeMap::new()))
    })();
    let _ = handle.trans_release();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn siglevel_follows_pacman() {
        let level = siglevel(&["Required DatabaseOptional"], SigLevel::NONE).unwrap();
        assert!(level.contains(SigLevel::PACKAGE | SigLevel::DATABASE | SigLevel::DATABASE_OPTIONAL));
        assert!(!level.contains(SigLevel::PACKAGE_OPTIONAL));
        let never = siglevel(&["Never"], default_siglevel()).unwrap();
        assert!(!never.intersects(SigLevel::PACKAGE | SigLevel::DATABASE));
        let trust = siglevel(&["PackageTrustAll"], SigLevel::NONE).unwrap();
        assert!(trust.contains(SigLevel::PACKAGE_MARGINAL_OK | SigLevel::PACKAGE_UNKNOWN_OK));
        assert!(!trust.contains(SigLevel::DATABASE_MARGINAL_OK));
        assert!(siglevel(&["Sometimes"], SigLevel::NONE).is_err());
    }

    #[test]
    fn plan_dbpath_links_the_real_local_db() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        fs::create_dir_all(real.join("local")).unwrap();
        let dir = plan_dbpath(&tmp.path().join("plan"), &real).unwrap();
        assert_eq!(fs::read_link(dir.join("local")).unwrap(), real.join("local"));
        assert!(dir.join("sync").is_dir());
        plan_dbpath(&dir, &real).unwrap();
    }
}
