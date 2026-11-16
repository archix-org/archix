use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::IsTerminal;
use std::os::unix::fs::{MetadataExt, lchown};
use std::path::Path;

use anyhow::{Context, Result, bail};

use archix::adopt::{Candidates, Fit, best_snapshot, fit};
use archix::archive::{ARCHIVE_URL, Archive};
use archix::aur::index::Index;
use archix::aur::review::review;
use archix::date::Date;
use archix::eval::{self, Evaluation};
use archix::installed;
use archix::lock::{HostLock, LOCK_FILE, Lockfile};
use archix::luagen::Lua;
use archix::machine::{self, Accounts};
use archix::pacman::{LocalPkg, read_local_db};
use archix::paths::Paths;
use archix::plan::{Change, plan};
use archix::recipes::Recipes;
use archix::repos::Custom;

use super::{Declared, aur_git, aur_source, confirm_or, count, declared, require_root, user_cache};

/// What init reads from the machine and writes into the config.
#[derive(Default)]
struct Facts {
    local: BTreeMap<String, LocalPkg>,
    build_dates: BTreeMap<String, u64>,
    foreign: BTreeSet<String>,
    official: Vec<String>,
    custom: Vec<(String, Custom)>,
    /// Explicit packages from the repos and the AUR.
    packages: Vec<String>,
    /// Explicit foreign packages the AUR doesn't have, which the config leaves out.
    unknown: Vec<String>,
    /// Orphans, which the lock keeps as dependencies through `prefer`.
    orphans: Vec<String>,
    /// Units whose enabled state differs from their preset.
    services: BTreeMap<String, bool>,
    accounts: Accounts,
}

/// Writes the minimal config for this machine as it stands (explicit packages, enabled services, users) and locks it to what's installed.
pub fn init(config_dir: &Path, host: &str) -> Result<()> {
    require_root("init")?;
    refuse_existing(config_dir)?;
    let paths = Paths::system();
    eprintln!("reading this system");
    let facts = gather(&paths)?;
    if !facts.unknown.is_empty() {
        eprintln!("left out, as no repo or the AUR has them: {}", facts.unknown.join(", "));
    }
    fs::create_dir_all(config_dir).with_context(|| format!("creating {}", config_dir.display()))?;
    write_config(config_dir, host, &facts, &[])?;
    let evaluation = evaluate(config_dir, host)?;
    let mut declared = declared(&evaluation.config)?;
    let mut lock = lock_as_installed(&declared, &facts, &paths, config_dir)?;
    let swapped = swapped_providers(&lock, &facts);
    if !swapped.is_empty() {
        eprintln!(
            "preferring {}, so the lock keeps the providers this machine has",
            swapped.join(", ")
        );
        write_config(config_dir, host, &facts, &swapped)?;
        declared = super::declared(&evaluate(config_dir, host)?.config)?;
        lock = lock_as_installed(&declared, &facts, &paths, config_dir)?;
    }
    let git = aur_git(&declared)?;
    review(
        &git,
        None,
        &lock,
        declared.aur_review && std::io::stdin().is_terminal(),
        &confirm_or,
    )?;
    let mut lockfile = Lockfile::default();
    lockfile.hosts.insert(host.to_string(), lock.clone());
    lockfile.save(&config_dir.join(LOCK_FILE))?;
    hand_over(config_dir)?;
    println!(
        "wrote {}: {} locked to the {} snapshot",
        config_dir.display(),
        count(lock.packages.len(), "package", "packages"),
        lock.date
    );
    if !lock.pinned.is_empty() {
        println!("held at their installed version instead: {}", lock.pinned.join(", "));
    }
    Ok(())
}

/// Reads everything init needs from the machine.
fn gather(paths: &Paths) -> Result<Facts> {
    let local = read_local_db(&paths.local_db())?;
    let foreign: BTreeSet<String> = machine::pacman_query("m")?.into_iter().collect();
    let explicit = machine::pacman_query("e")?;
    let conf =
        fs::read_to_string(&paths.pacman_conf).with_context(|| format!("reading {}", paths.pacman_conf.display()))?;
    let (official, custom) = machine::parse_pacman_conf(&conf, |path| fs::read_to_string(path).ok());
    let in_aur: BTreeSet<String> = match foreign.is_empty() {
        true => BTreeSet::new(),
        false => {
            let index = Index::load(
                &Archive::new(ARCHIVE_URL),
                archix::aur::DEFAULT_URL,
                &user_cache()?,
                false,
            )?;
            foreign
                .iter()
                .filter(|name| index.get(name).is_some())
                .cloned()
                .collect()
        }
    };
    let (packages, unknown): (Vec<String>, Vec<String>) = explicit
        .into_iter()
        .partition(|name| !foreign.contains(name) || in_aur.contains(name));
    let accounts = machine::parse_accounts(
        &fs::read_to_string(paths.root.join("etc/passwd")).context("reading /etc/passwd")?,
        &fs::read_to_string(paths.root.join("etc/group")).context("reading /etc/group")?,
        &machine::sysusers(&paths.root),
    );
    Ok(Facts {
        build_dates: machine::build_dates(&paths.local_db())?,
        local,
        orphans: machine::pacman_query("dtt")?
            .into_iter()
            .filter(|name| !foreign.contains(name) || in_aur.contains(name))
            .collect(),
        foreign,
        official,
        custom,
        packages,
        unknown,
        services: machine::unit_states()?.changed,
        accounts,
    })
}

/// The host module's fields; `prefer` keeps the providers and orphans the lock would otherwise swap or drop.
fn host_fields(facts: &Facts, prefer: &[String]) -> Vec<(String, Lua)> {
    let mut fields = Vec::new();
    if facts.official != ["core", "extra"] {
        fields.push(("repos".to_string(), Lua::strings(&facts.official)));
    }
    if !facts.custom.is_empty() {
        let repos = facts
            .custom
            .iter()
            .map(|(name, repo)| {
                let mut entry = vec![
                    ("name".to_string(), Lua::str(name)),
                    ("servers".to_string(), Lua::strings(&repo.servers)),
                    ("sig_level".to_string(), Lua::str(&repo.sig_level)),
                ];
                if repo.before_official {
                    entry.push(("before_official".into(), Lua::Bool(true)));
                }
                Lua::Table(entry)
            })
            .collect();
        fields.push(("custom_repos".to_string(), Lua::List(repos)));
    }
    let mut preferred: Vec<String> = facts.orphans.iter().chain(prefer).cloned().collect();
    preferred.sort();
    preferred.dedup();
    if !preferred.is_empty() {
        fields.push(("prefer".to_string(), Lua::strings(&preferred)));
    }
    if !facts.accounts.users.is_empty() {
        let users = facts
            .accounts
            .users
            .iter()
            .map(|(name, user)| {
                let mut entry = vec![("uid".to_string(), Lua::Int(user.uid.into()))];
                if user.system {
                    entry.push(("system".into(), Lua::Bool(true)));
                }
                if let Some(group) = &user.group {
                    entry.push(("group".into(), Lua::str(group)));
                }
                if !user.groups.is_empty() {
                    entry.push(("groups".into(), Lua::strings(&user.groups)));
                }
                if user.shell != "/bin/bash" {
                    entry.push(("shell".into(), Lua::str(&user.shell)));
                }
                if let Some(home) = &user.home {
                    entry.push(("home".into(), Lua::str(home)));
                }
                if !user.description.is_empty() {
                    entry.push(("description".into(), Lua::str(&user.description)));
                }
                (name.clone(), Lua::Table(entry))
            })
            .collect();
        fields.push(("users".to_string(), Lua::Table(users)));
    }
    if !facts.accounts.groups.is_empty() {
        let groups = facts
            .accounts
            .groups
            .iter()
            .map(|(name, gid)| {
                let mut entry = vec![("gid".to_string(), Lua::Int((*gid).into()))];
                if facts.accounts.system_groups.contains(name) {
                    entry.push(("system".into(), Lua::Bool(true)));
                }
                (name.clone(), Lua::Table(entry))
            })
            .collect();
        fields.push(("groups".to_string(), Lua::Table(groups)));
    }
    if !facts.services.is_empty() {
        let services = facts
            .services
            .iter()
            .map(|(unit, on)| (unit.clone(), Lua::Bool(*on)))
            .collect();
        fields.push(("services".to_string(), Lua::Table(services)));
    }
    fields
}

fn refuse_existing(dir: &Path) -> Result<()> {
    if dir.join("init.lua").exists() {
        bail!(
            "{} already has an init.lua; init only writes a new config",
            dir.display()
        );
    }
    Ok(())
}

fn write_config(dir: &Path, host: &str, facts: &Facts, prefer: &[String]) -> Result<()> {
    let host_dir = installed::default_dir(dir, host);
    fs::create_dir_all(&host_dir).with_context(|| format!("creating {}", host_dir.display()))?;
    archix::durable::write(
        &dir.join("init.lua"),
        b"return { imports = { \"hosts.\" .. archix.hostname } }\n",
    )?;
    let body = Lua::Table(host_fields(facts, prefer)).render(0);
    archix::durable::write(
        &host_dir.join("init.lua"),
        format!("-- Written by `archix init` from this machine; edit it freely.\nreturn {body}\n").as_bytes(),
    )?;
    archix::durable::write(
        &host_dir.join(installed::FILE),
        installed::render(&facts.packages).as_bytes(),
    )
}

/// Gives `dir` and everything under it to the owner of `like`, so files in a user's repo stay theirs.
fn own_tree(dir: &Path, like: &Path) -> Result<()> {
    let Ok(meta) = fs::metadata(like) else { return Ok(()) };
    let mut stack = vec![dir.to_path_buf()];
    while let Some(path) = stack.pop() {
        lchown(&path, Some(meta.uid()), Some(meta.gid())).with_context(|| format!("chown {}", path.display()))?;
        if fs::symlink_metadata(&path)?.is_dir() {
            stack.extend(fs::read_dir(&path)?.flatten().map(|entry| entry.path()));
        }
    }
    Ok(())
}

/// A config in someone's own directory belongs to them, not to root who ran init.
fn hand_over(dir: &Path) -> Result<()> {
    let Some(parent) = dir.parent() else { return Ok(()) };
    if fs::metadata(parent).is_ok_and(|meta| meta.uid() != 0) {
        own_tree(dir, parent)?;
    }
    Ok(())
}

fn evaluate(dir: &Path, host: &str) -> Result<Evaluation> {
    eval::evaluate(&eval::Input {
        config_dir: dir.to_path_buf(),
        hostname: host.to_string(),
        ..Default::default()
    })
    .map_err(|errors| anyhow::anyhow!("the generated config doesn't evaluate; please report this:\n{errors}"))
}

/// Locks the snapshot the installed official packages were built for, then pins every version that differs from it.
fn lock_as_installed(declared: &Declared, facts: &Facts, paths: &Paths, dir: &Path) -> Result<HostLock> {
    let archive = Archive::new(ARCHIVE_URL);
    let newest = facts
        .build_dates
        .iter()
        .filter(|(name, _)| !facts.foreign.contains(*name))
        .map(|(_, date)| *date)
        .max();
    let day = newest.map_or_else(Date::today_utc, Date::from_unix);
    let official: Vec<String> = declared
        .repos
        .iter()
        .filter(|repo| repo.custom.is_none())
        .map(|repo| repo.name.clone())
        .collect();
    eprintln!("finding the archive snapshot closest to what's installed");
    let latest = archive.latest()?;
    let date = best_snapshot(&archive, &user_cache()?, day, latest, &official, &facts.local)?;
    let git = aur_git(declared)?;
    let recipes = Recipes::load(dir, &declared.patches, &user_cache()?)?;
    let aur = aur_source(&git, declared, &archive, None, false, &recipes);
    fit(
        &archive,
        &user_cache()?,
        paths,
        &declared.packages,
        &declared.prefer,
        &Candidates {
            dates: BTreeSet::from([date]),
            day,
        },
        None,
        &facts.local,
        &declared.repos,
        Some(&aur),
        Fit::Exact,
    )
}

/// Installed dependencies the lock would replace with another provider of the same thing.
fn swapped_providers(lock: &HostLock, facts: &Facts) -> Vec<String> {
    plan(&lock.packages, &facts.local)
        .into_iter()
        .filter_map(|change| match change {
            Change::Remove { name, .. } if !facts.orphans.contains(&name) => Some(name),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use archix::machine::Account;

    #[test]
    fn the_config_declares_explicit_packages_services_and_users_and_evaluates() {
        let mut facts = Facts {
            official: vec!["core".into(), "extra".into(), "multilib".into()],
            packages: vec!["base".into(), "openssh".into()],
            orphans: vec!["libfoo".into()],
            services: BTreeMap::from([("sshd.service".to_string(), true), ("cups.service".to_string(), false)]),
            ..Facts::default()
        };
        facts.accounts.users.insert(
            "me".into(),
            Account {
                uid: 1000,
                system: false,
                group: None,
                groups: vec!["wheel".into()],
                shell: "/usr/bin/zsh".into(),
                home: None,
                description: "Me".into(),
            },
        );
        facts.accounts.groups.insert("shared".into(), 1500);
        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path(), "box", &facts, &["provider".to_string()]).unwrap();
        let installed_lua = installed::default_dir(dir.path(), "box").join(installed::FILE);
        assert_eq!(installed::read(&installed_lua).unwrap(), ["base", "openssh"]);
        let config = evaluate(dir.path(), "box").unwrap().config;
        let declared = declared(&config).unwrap();
        assert_eq!(declared.packages, ["base", "openssh"]);
        assert_eq!(declared.prefer, ["libfoo", "provider"]);
        assert_eq!(declared.repo_names(), ["core", "extra", "multilib"]);
        let json = serde_json::to_value(&config).unwrap();
        assert_eq!(json["services"]["sshd.service"], true);
        assert_eq!(json["services"]["cups.service"], false);
        assert_eq!(json["users"]["me"]["uid"], 1000);
        assert_eq!(json["users"]["me"]["shell"], "/usr/bin/zsh");
        assert_eq!(json["groups"]["shared"]["gid"], 1500);
        assert!(refuse_existing(dir.path()).is_err(), "init never writes over a config");
    }
}
