use std::cell::{OnceCell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::archive::Archive;
use crate::aur::git::AurGit;
use crate::aur::index::{Index, constraint, meets};
use crate::aur::srcinfo::{self, SrcInfo};
use crate::date::Date;
use crate::lock::{ARCHIVE_REPO, AUR_REPO, HostLock, LOCAL_REPO, Locked};
use crate::pacman::{SyncPkg, read_sync_db};
use crate::recipes::Recipes;
use crate::repos::{Repo, fetch_db, is_official};

/// Where AUR packages come from while resolving, and the previous lock whose AUR commits are kept.
pub struct Aur<'a> {
    pub git: &'a AurGit,
    pub previous: Option<&'a HostLock>,
    /// Commits to lock bases at, overriding the previous lock and the AUR's newest.
    pub commits: RefCell<BTreeMap<String, String>>,
    /// The config's own recipes, which win over the AUR and, for names they build, over the repos.
    pub recipes: Option<&'a Recipes>,
    index: OnceCell<Index>,
    load_index: Box<dyn Fn() -> Result<Index> + 'a>,
}

impl<'a> Aur<'a> {
    /// The AUR's package list is only loaded once a name turns out not to be in the official repos.
    pub fn new(git: &'a AurGit, previous: Option<&'a HostLock>, load_index: impl Fn() -> Result<Index> + 'a) -> Self {
        Aur {
            git,
            previous,
            commits: RefCell::new(BTreeMap::new()),
            recipes: None,
            index: OnceCell::new(),
            load_index: Box::new(load_index),
        }
    }

    pub fn with_recipes(mut self, recipes: &'a Recipes) -> Self {
        self.recipes = Some(recipes);
        self
    }

    pub fn index(&self) -> Result<&Index> {
        if let Some(index) = self.index.get() {
            return Ok(index);
        }
        let index = (self.load_index)()?;
        Ok(self.index.get_or_init(|| index))
    }
}

/// A pacman config and db directory for one archive snapshot; `local/` is empty, so it resolves as if from scratch.
pub fn snapshot_conf(archive: &Archive, cache: &Path, date: Date, repos: &[Repo]) -> Result<(PathBuf, PathBuf)> {
    let official: Vec<String> = repos
        .iter()
        .filter(|r| r.custom.is_none())
        .map(|r| r.name.clone())
        .collect();
    let db = archive.snapshot_db(cache, date, &official)?;
    for repo in repos.iter().filter(|r| r.custom.is_some()) {
        fetch_db(archive, repo, &db.join("sync").join(format!("{}.db", repo.name)))?;
    }
    let conf = db.join(format!("pacman.{}.conf", std::process::id()));
    fs::write(&conf, pacman_conf(archive, date, repos)).with_context(|| format!("writing {}", conf.display()))?;
    Ok((conf, db))
}

/// The pacman.conf serving one archive snapshot of the official repos, plus custom repos as they are now.
pub fn pacman_conf(archive: &Archive, date: Date, repos: &[Repo]) -> String {
    let mut text = format!(
        "[options]\nArchitecture = {}\nSigLevel = Required DatabaseOptional\nLocalFileSigLevel = Optional\n",
        crate::archive::architectures()
    );
    for repo in repos {
        let _ = write!(text, "\n[{}]\n", repo.name);
        match &repo.custom {
            None => {
                for url in archive.repo_urls(date, &repo.name) {
                    let _ = writeln!(text, "Server = {url}");
                }
            }
            Some(custom) => {
                let _ = writeln!(text, "SigLevel = {}", custom.sig_level);
                for server in &custom.servers {
                    let _ = writeln!(text, "Server = {server}");
                }
            }
        }
    }
    text
}

/// Locks `declared` and everything it depends on at `date` into the exact closure, taking from the AUR what the repos lack; `prefer` picks providers and is installed as dependencies.
pub fn resolve(
    archive: &Archive,
    cache: &Path,
    date: Date,
    repos: &[Repo],
    declared: &[String],
    prefer: &[String],
    aur: Option<&Aur>,
) -> Result<HostLock> {
    let invalid: Vec<&String> = declared.iter().filter(|name| !valid_name(name)).collect();
    if !invalid.is_empty() {
        bail!("invalid package names: {invalid:?}");
    }
    let mut lock = HostLock {
        date,
        repos: repos.iter().map(|r| r.name.clone()).collect(),
        declared: declared.to_vec(),
        prefer: prefer.to_vec(),
        pinned: Vec::new(),
        packages: BTreeMap::new(),
        flatpak: BTreeMap::new(),
        recipes: BTreeMap::new(),
    };
    if declared.is_empty() {
        if let Some(recipes) = aur.and_then(|aur| aur.recipes) {
            lock.recipes = recipes.fingerprints(&BTreeSet::new())?;
        }
        return Ok(lock);
    }
    let (conf, db) = snapshot_conf(archive, cache, date, repos)?;

    let mut not_in_repos = unsatisfied(&conf, &db, declared)?;
    if let Some(recipes) = aur.and_then(|aur| aur.recipes) {
        not_in_repos.extend(
            declared
                .iter()
                .filter(|name| recipes.satisfier(name).is_some_and(|(_, pkg)| pkg.name == **name))
                .cloned(),
        );
    }
    let official: Vec<String> = declared
        .iter()
        .filter(|name| !not_in_repos.contains(*name))
        .cloned()
        .collect();
    let mut from_aur = AurClosure::default();
    if !not_in_repos.is_empty() && aur.is_none() {
        let names: Vec<&String> = not_in_repos.iter().collect();
        bail!("not in the configured repos for this snapshot: {names:?}");
    }
    let mut sync = BTreeMap::new();
    for repo in repos {
        let path = db.join("sync").join(format!("{}.db", repo.name));
        sync.insert(repo.name.clone(), read_sync_db(&path)?);
    }
    if let Some(aur) = aur.filter(|_| !not_in_repos.is_empty()) {
        let newer = Newer::new(archive, cache, date, repos);
        let in_repos: BTreeSet<&str> = sync
            .values()
            .flat_map(|packages| packages.keys())
            .map(String::as_str)
            .collect();
        from_aur = aur_closure(aur, &conf, &db, &not_in_repos, &newer, &in_repos)?;
    }
    let mut targets: Vec<String> = official.clone();
    targets.extend(from_aur.repo_deps.iter().map(|dep| provider(&sync, dep, prefer)));
    targets.extend(
        prefer
            .iter()
            .filter(|name| !targets.contains(*name))
            .cloned()
            .collect::<Vec<_>>(),
    );
    if !targets.is_empty() {
        let mut closure = print_targets(&conf, &db, &targets, false, &from_aur.provides)?;
        let chosen: Vec<String> = official.iter().chain(prefer).cloned().collect();
        for _ in 0..5 {
            let missing = shadowed(&sync, &closure, &from_aur, &chosen);
            if missing.is_empty() {
                break;
            }
            targets.extend(missing);
            // A set pacman can't resolve together keeps the providers it picked before.
            match print_targets(&conf, &db, &targets, false, &from_aur.provides) {
                Ok(fuller) => closure = fuller,
                Err(_) => break,
            }
        }
        let explicit: BTreeSet<String> = if official.is_empty() {
            BTreeSet::new()
        } else {
            print_targets(&conf, &db, &official, true, &[])?
                .into_iter()
                .map(|t| t.0)
                .collect()
        };
        for (name, version, repo) in closure {
            let Some(pkg) = sync.get(&repo).and_then(|packages| packages.get(&name)) else {
                bail!("pacman picked {name} {version} from {repo}, but it is missing from that snapshot's database");
            };
            if pkg.version != version {
                bail!(
                    "pacman picked {name} {version}, but the {repo} database has {}",
                    pkg.version
                );
            }
            let locked = Locked {
                version,
                repo,
                file: pkg.filename.clone(),
                sha256: pkg.sha256.clone(),
                explicit: explicit.contains(&name),
                ..Default::default()
            };
            lock.packages.insert(name, locked);
        }
    }
    for (name, mut locked) in from_aur.packages {
        locked.explicit = not_in_repos.contains(&name);
        lock.packages.insert(name, locked);
    }
    if let Some(recipes) = aur.and_then(|aur| aur.recipes) {
        apply_patches(recipes, &sync, &mut lock)?;
        lock.recipes = recipes.fingerprints(&lock.local_recipes())?;
    }
    Ok(lock)
}

/// Swaps each patched package, and the rest of its base in the lock, for a build of Arch's recipe with the config's patches.
fn apply_patches(
    recipes: &Recipes,
    sync: &BTreeMap<String, BTreeMap<String, SyncPkg>>,
    lock: &mut HostLock,
) -> Result<()> {
    let official = |name: &str| {
        sync.iter()
            .filter(|(repo, _)| is_official(repo))
            .find_map(|(_, packages)| packages.get(name))
    };
    for name in recipes.patched_packages() {
        let Some(locked) = lock.packages.get(name) else {
            bail!("patches.{name}: {name} isn't among this config's packages or their dependencies");
        };
        let Some(arch) = official(name) else {
            bail!("patches.{name}: {name} isn't in Arch's official repos, and only Arch's recipes can be patched");
        };
        // An official or pinned package keeps its version; one a third-party repo (CachyOS) supplies is rebuilt at Arch's version.
        let version = if is_official(&locked.repo) || locked.repo == ARCHIVE_REPO {
            locked.version.clone()
        } else {
            arch.version.clone()
        };
        let base = arch.base.clone();
        let recipe = recipes.patched(name, &base, &version)?;
        let members: Vec<String> = lock
            .packages
            .keys()
            .filter(|member| official(member).is_some_and(|pkg| pkg.base == base))
            .cloned()
            .collect();
        for member in members {
            let Some(pkg) = recipe.info.package(&member) else {
                bail!("the patched recipe for {base} doesn't build {member}");
            };
            let old = &lock.packages[&member];
            let patched = Locked {
                version: recipe.info.version.clone(),
                repo: LOCAL_REPO.to_string(),
                file: recipe.info.file(pkg),
                explicit: old.explicit,
                base: Some(recipe.id.clone()),
                commit: Some(recipe.commit.clone()),
                ..Default::default()
            };
            lock.packages.insert(member, patched);
        }
    }
    Ok(())
}

/// A concrete package for a dependency that may be a virtual name: the one listed in `prefer`, if any.
fn provider(sync: &BTreeMap<String, BTreeMap<String, SyncPkg>>, dep: &str, prefer: &[String]) -> String {
    let wanted = dep.split(['<', '>', '=']).next().unwrap_or(dep);
    if sync.values().any(|packages| packages.contains_key(wanted)) {
        return dep.to_string();
    }
    let provides = |pkg: &SyncPkg| pkg.provides.iter().any(|p| p.split('=').next() == Some(wanted));
    let candidates: Vec<&String> = sync
        .values()
        .flat_map(|packages| packages.values())
        .filter(|pkg| provides(pkg))
        .map(|pkg| &pkg.name)
        .collect();
    prefer
        .iter()
        .find(|name| candidates.contains(name))
        .cloned()
        .unwrap_or_else(|| dep.to_string())
}

/// Dependencies on a real package that pacman met with a provider already in the transaction, like ca-certificates-utils (no certificates) for ca-certificates.
fn shadowed(
    sync: &BTreeMap<String, BTreeMap<String, SyncPkg>>,
    closure: &[(String, String, String)],
    aur: &AurClosure,
    chosen: &[String],
) -> Vec<String> {
    let picked: Vec<&SyncPkg> = closure
        .iter()
        .filter_map(|(name, _, repo)| sync.get(repo)?.get(name))
        .collect();
    let provides = |pkg: &SyncPkg, name: &str| pkg.provides.iter().any(|p| srcinfo::dep_name(p) == name);
    let clashes = |a: &SyncPkg, b: &SyncPkg| {
        a.conflicts
            .iter()
            .map(|c| srcinfo::dep_name(c))
            .any(|c| c == b.name || provides(b, c))
    };
    let mut missing = BTreeSet::new();
    for dep in picked.iter().flat_map(|pkg| &pkg.depends).chain(&aur.repo_deps) {
        let name = srcinfo::dep_name(dep);
        if picked.iter().any(|pkg| pkg.name == name) || aur.provides.iter().any(|p| srcinfo::dep_name(p) == name) {
            continue;
        }
        let Some(real) = sync.values().find_map(|packages| packages.get(name)) else {
            continue;
        };
        if picked
            .iter()
            .any(|pkg| provides(pkg, name) && chosen.contains(&pkg.name))
            || picked.iter().any(|pkg| clashes(real, pkg) || clashes(pkg, real))
        {
            continue;
        }
        missing.insert(name.to_string());
    }
    missing.into_iter().collect()
}

#[derive(Default)]
struct AurClosure {
    packages: BTreeMap<String, Locked>,
    repo_deps: BTreeSet<String>,
    /// `name=version` for every AUR package and what it provides, so pacman treats those dependencies as met.
    provides: Vec<String>,
}

/// A snapshot's date with its pacman config and db directory.
type Snapshot = (Date, PathBuf, PathBuf);

/// A newer snapshot's official repos, which tell a name this snapshot lacks because it's too new from one only the AUR has.
struct Newer<'a> {
    archive: &'a Archive,
    cache: &'a Path,
    date: Date,
    repos: Vec<Repo>,
    snapshot: OnceCell<std::result::Result<Option<Snapshot>, String>>,
}

impl<'a> Newer<'a> {
    fn new(archive: &'a Archive, cache: &'a Path, date: Date, repos: &[Repo]) -> Self {
        Newer {
            archive,
            cache,
            date,
            repos: repos.iter().filter(|r| r.custom.is_none()).cloned().collect(),
            snapshot: OnceCell::new(),
        }
    }

    /// The newest snapshot, if it's newer than the lock's and its official repos satisfy `dep`; an error when that can't be checked, so nothing comes from the AUR unchecked.
    fn has(&self, dep: &str) -> Result<Option<Date>> {
        let snapshot = self.snapshot.get_or_init(|| {
            let latest = self.archive.latest().map_err(|e| format!("{e:#}"))?;
            if latest <= self.date {
                return Ok(None);
            }
            let (conf, db) =
                snapshot_conf(self.archive, self.cache, latest, &self.repos).map_err(|e| format!("{e:#}"))?;
            Ok(Some((latest, conf, db)))
        });
        let unverified = |why: &str| {
            anyhow::anyhow!(
                "couldn't verify against the archive that `{dep}` isn't simply too new for the {} snapshot ({why}), so it isn't taken from the AUR; nothing was changed",
                self.date
            )
        };
        let (date, conf, db) = match snapshot {
            Ok(Some(found)) => found,
            Ok(None) => return Ok(None),
            Err(why) => return Err(unverified(why)),
        };
        let missing = unsatisfied(conf, db, &[dep.to_string()]).map_err(|e| unverified(&format!("{e:#}")))?;
        Ok(missing.is_empty().then_some(*date))
    }
}

/// Walks AUR packages and their dependencies, installing runtime ones and only building make/check-only ones; `in_repos` names what the snapshot's repos ship, which an AUR package may stand in for only when declared.
fn aur_closure(
    aur: &Aur,
    conf: &Path,
    db: &Path,
    targets: &BTreeSet<String>,
    newer: &Newer,
    in_repos: &BTreeSet<&str>,
) -> Result<AurClosure> {
    let mut queue: VecDeque<(String, bool)> = targets.iter().map(|t| (t.clone(), true)).collect();
    let mut needed_by: HashMap<String, String> = HashMap::new();
    let mut bases: HashMap<String, (String, SrcInfo)> = HashMap::new();
    let mut runtime: BTreeMap<String, String> = BTreeMap::new();
    let mut build: BTreeMap<String, String> = BTreeMap::new();
    let mut closure = AurClosure::default();

    while let Some((dep, needed_at_runtime)) = queue.pop_front() {
        let local = aur.recipes.and_then(|recipes| recipes.satisfier(&dep));
        let (name, base) = match local {
            Some((recipe, pkg)) => {
                bases
                    .entry(recipe.id.clone())
                    .or_insert_with(|| (recipe.commit.clone(), recipe.info.clone()));
                (pkg.name.clone(), recipe.id.clone())
            }
            None => {
                if let Some(since) = newer.has(&dep)? {
                    let what = match needed_by.get(&dep) {
                        Some(by) => format!("{by} needs `{dep}`, which"),
                        None => format!("`{dep}`"),
                    };
                    bail!(
                        "{what} isn't in the {} snapshot, but the newest one ({since}) has it in the official repos, so it isn't taken from the AUR. Run `archix update` to lock a newer snapshot; nothing was changed",
                        newer.date
                    );
                }
                let index = aur.index()?;
                let declared = targets
                    .iter()
                    .filter_map(|target| index.get(target))
                    .find(|entry| Index::satisfies(entry, &dep));
                let pinned = match declared {
                    Some(_) => None,
                    None => match locked_satisfier(&bases, &dep, index.satisfier(&dep).is_some()) {
                        Some(found) => Some(found),
                        None => at_locked_commit(aur, &mut bases, index, &dep)?,
                    },
                };
                if let Some(found) = pinned {
                    found
                } else {
                    let Some(entry) = declared.or_else(|| index.satisfier(&dep)) else {
                        bail!("`{dep}` is in none of the configured repos, pkgbuilds/ or the AUR");
                    };
                    let wanted = srcinfo::dep_name(&dep);
                    if entry.name != wanted && in_repos.contains(wanted) && !targets.contains(&entry.name) {
                        let what = match needed_by.get(&dep) {
                            Some(by) => {
                                format!("{by} needs `{dep}`, which the configured repos' {wanted} can't satisfy;")
                            }
                            None => format!("`{dep}`:"),
                        };
                        bail!(
                            "{what} only {}, an AUR package standing in for {wanted}, would, and archix doesn't swap a repo package for an AUR one unasked; nothing was changed. Declare {} in packages if that's what you want",
                            entry.name,
                            entry.name
                        );
                    }
                    (entry.name.clone(), entry.package_base.clone())
                }
            }
        };
        let fresh = if needed_at_runtime {
            build.remove(&name);
            runtime.insert(name.clone(), base.clone()).is_none()
        } else {
            !runtime.contains_key(&name) && build.insert(name.clone(), base.clone()).is_none()
        };
        if !fresh {
            continue;
        }
        if !bases.contains_key(&base) {
            let commit = locked_commit(aur, &base)?;
            let info = srcinfo::parse(&aur.git.show(&base, &commit, ".SRCINFO")?)
                .with_context(|| format!("reading {base}'s .SRCINFO at {commit}"))?;
            bases.insert(base.clone(), (commit, info));
        }
        let (_, info) = &bases[&base];
        let Some(pkg) = info.package(&name) else {
            bail!("the AUR says {base} builds {name}, but its .SRCINFO doesn't list it");
        };
        let runtime_deps = pkg.depends.clone();
        let build_deps: Vec<String> = info.makedepends.iter().chain(&info.checkdepends).cloned().collect();
        let all: Vec<String> = runtime_deps.iter().chain(&build_deps).cloned().collect();
        let missing_from_repos = unsatisfied(conf, db, &all)?;
        for dep in runtime_deps {
            if !missing_from_repos.contains(&dep) {
                if needed_at_runtime {
                    closure.repo_deps.insert(dep);
                }
            } else {
                needed_by.entry(dep.clone()).or_insert_with(|| name.clone());
                queue.push_back((dep, needed_at_runtime));
            }
        }
        for dep in build_deps {
            if missing_from_repos.contains(&dep) {
                needed_by.entry(dep.clone()).or_insert_with(|| name.clone());
                queue.push_back((dep, false));
            }
        }
    }

    for (name, base) in &runtime {
        let (_, info) = &bases[base];
        let pkg = info.package(name).expect("checked while walking");
        closure.provides.push(format!("{name}={}", info.version));
        for provided in &pkg.provides {
            closure.provides.push(match provided.split_once('=') {
                Some(_) => provided.clone(),
                None => format!("{provided}={}", info.version),
            });
        }
    }
    let satisfied_by_aur = |dep: &str| {
        let wanted = dep.split(['<', '>', '=']).next().unwrap_or(dep);
        closure.provides.iter().any(|p| p.split('=').next() == Some(wanted))
    };
    closure.repo_deps = closure
        .repo_deps
        .iter()
        .filter(|dep| !satisfied_by_aur(dep))
        .cloned()
        .collect();
    for (name, base, build_only) in runtime
        .into_iter()
        .map(|(n, b)| (n, b, false))
        .chain(build.into_iter().map(|(n, b)| (n, b, true)))
    {
        let (commit, info) = &bases[&base];
        let pkg = info.package(&name).expect("checked while walking");
        let local = aur.recipes.is_some_and(|r| r.is_local(&base));
        let maintainer = match local {
            true => None,
            false => aur
                .index()?
                .get(&name)
                .map(|entry| entry.maintainer.clone().unwrap_or_default()),
        };
        closure.packages.insert(
            name,
            Locked {
                version: info.version.clone(),
                repo: if local {
                    LOCAL_REPO.to_string()
                } else {
                    AUR_REPO.to_string()
                },
                file: info.file(pkg),
                base: Some(base.clone()),
                commit: Some(commit.clone()),
                build_only,
                maintainer,
                ..Default::default()
            },
        );
    }
    Ok(closure)
}

/// The commit a base is locked at: one set in `commits`, else the previous lock's if the repo still has it, else the AUR's newest.
fn locked_commit(aur: &Aur, base: &str) -> Result<String> {
    if let Some(commit) = aur.commits.borrow().get(base) {
        aur.git.sync(base)?;
        return Ok(commit.clone());
    }
    let previous = aur.previous.and_then(|lock| {
        lock.packages
            .values()
            .find(|pkg| pkg.base.as_deref() == Some(base))
            .and_then(|pkg| pkg.commit.clone())
    });
    aur.git.sync(base)?;
    match previous {
        Some(commit) if aur.git.has_commit(base, &commit) => Ok(commit),
        _ => aur.git.head(base),
    }
}

/// A package of a base already locked that satisfies `dep`, version included: one with its name, or, when the AUR has nothing for it, one providing it; a split package's siblings depend on each other at the locked version, not the AUR's newest.
fn locked_satisfier(bases: &HashMap<String, (String, SrcInfo)>, dep: &str, aur_has: bool) -> Option<(String, String)> {
    let name = srcinfo::dep_name(dep);
    let wanted = constraint(dep);
    let mut sorted: Vec<(&String, &SrcInfo)> = bases.iter().map(|(base, (_, info))| (base, info)).collect();
    sorted.sort_by_key(|(base, _)| *base);
    let named = sorted
        .iter()
        .find(|(_, info)| info.package(name).is_some_and(|_| meets(Some(&info.version), wanted)));
    let provided = || {
        sorted.iter().find_map(|(base, info)| {
            info.packages
                .iter()
                .find(|pkg| {
                    pkg.provides.iter().filter(|p| srcinfo::dep_name(p) == name).any(|p| {
                        let version = p.split_once('=').map_or(info.version.as_str(), |(_, v)| v);
                        meets(Some(version), wanted)
                    })
                })
                .map(|pkg| (pkg.name.clone(), (*base).clone()))
        })
    };
    match named {
        Some((base, _)) => Some((name.to_string(), (*base).clone())),
        None if !aur_has => provided(),
        None => None,
    }
}

/// The AUR package named in `dep` at the commit its base is locked at, when the AUR's newest version doesn't meet `dep` but that one does.
fn at_locked_commit(
    aur: &Aur,
    bases: &mut HashMap<String, (String, SrcInfo)>,
    index: &Index,
    dep: &str,
) -> Result<Option<(String, String)>> {
    let name = srcinfo::dep_name(dep);
    let Some(entry) = index.get(name) else { return Ok(None) };
    let base = entry.package_base.clone();
    let pinned = aur.commits.borrow().contains_key(&base)
        || aur.previous.is_some_and(|lock| {
            lock.packages
                .values()
                .any(|pkg| pkg.base.as_deref() == Some(base.as_str()))
        });
    if bases.contains_key(&base) || !pinned {
        return Ok(None);
    }
    let commit = locked_commit(aur, &base)?;
    let info = srcinfo::parse(&aur.git.show(&base, &commit, ".SRCINFO")?)
        .with_context(|| format!("reading {base}'s .SRCINFO at {commit}"))?;
    let meets_dep = info.package(name).is_some() && meets(Some(&info.version), constraint(dep));
    bases.insert(base.clone(), (commit, info));
    Ok(meets_dep.then(|| (name.to_string(), base)))
}

/// The dependency strings no package in the snapshot's repos satisfies, per pacman itself.
fn unsatisfied(conf: &Path, db: &Path, deps: &[String]) -> Result<BTreeSet<String>> {
    if deps.is_empty() {
        return Ok(BTreeSet::new());
    }
    let output = pacman_print(conf, db, deps, true, &[])?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    let mut missing = BTreeSet::new();
    let mut other = Vec::new();
    for line in stderr.lines().filter_map(|line| line.strip_prefix("error: ")) {
        match line.strip_prefix("target not found: ") {
            Some(dep) => {
                missing.insert(dep.to_string());
            }
            None => other.push(line.to_string()),
        }
    }
    if !other.is_empty() {
        bail!("pacman failed:\n  {}", other.join("\n  "));
    }
    Ok(missing)
}

fn pacman_print(
    conf: &Path,
    db: &Path,
    targets: &[String],
    targets_only: bool,
    assume: &[String],
) -> Result<std::process::Output> {
    let mut cmd = Command::new("pacman");
    cmd.arg("--config").arg(conf).arg("--dbpath").arg(db);
    cmd.args(["-Sp", "--noconfirm", "--print-format", "%n %v %r"]);
    for provided in assume {
        cmd.arg("--assume-installed").arg(provided);
    }
    if targets_only {
        cmd.args(["--nodeps", "--nodeps"]);
    }
    match cmd.args(targets).output() {
        Ok(output) => Ok(output),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!("pacman not found; archix needs Arch Linux"),
        Err(e) => Err(e).context("running pacman"),
    }
}

/// Runs `pacman -Sp` against a snapshot db; with `targets_only`, dependencies are skipped (`-dd`); `assume` counts as installed.
pub fn print_targets(
    conf: &Path,
    db: &Path,
    targets: &[String],
    targets_only: bool,
    assume: &[String],
) -> Result<Vec<(String, String, String)>> {
    let output = pacman_print(conf, db, targets, targets_only, assume)?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    let errors: Vec<String> = stderr
        .lines()
        .filter_map(|line| line.strip_prefix("error: "))
        .map(|line| match line.strip_prefix("target not found: ") {
            Some(name) => format!("package `{name}` is not in the configured repos for this snapshot"),
            None => line.to_string(),
        })
        .collect();
    if !output.status.success() || !errors.is_empty() {
        if errors.is_empty() {
            bail!("pacman failed: {}", stderr.trim());
        }
        bail!("could not resolve packages:\n  {}", errors.join("\n  "));
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| match line.split(' ').collect::<Vec<_>>()[..] {
            [name, version, repo] => Ok((name.to_string(), version.to_string(), repo.to_string())),
            _ => bail!("unexpected pacman output: {line:?}"),
        })
        .collect()
}

/// pacman package names: lowercase alphanumerics and `@._+-`, not starting with `-` or `.`.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(['-', '.'])
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "@._+-".contains(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_split_package_needs_its_sibling_at_the_locked_version() {
        let info = srcinfo::parse(
            "pkgbase = gpu-utils\n\tpkgver = 580.1\n\tpkgrel = 1\n\tarch = x86_64\n\n\
             pkgname = gpu-utils\n\tprovides = gpu-libgl\n\npkgname = gpu-dkms\n\tdepends = gpu-utils=580.1\n",
        )
        .unwrap();
        let bases = HashMap::from([("gpu-utils".to_string(), ("c0ffee".to_string(), info))]);
        let found = |dep: &str, aur_has: bool| locked_satisfier(&bases, dep, aur_has);
        assert_eq!(
            found("gpu-utils=580.1", true),
            Some(("gpu-utils".to_string(), "gpu-utils".to_string()))
        );
        assert_eq!(found("gpu-utils=581", true), None, "the version is checked");
        assert_eq!(
            found("gpu-utils>=500", false).map(|f| f.0),
            Some("gpu-utils".to_string())
        );
        assert_eq!(
            found("gpu-libgl", false),
            Some(("gpu-utils".to_string(), "gpu-utils".to_string()))
        );
        assert_eq!(
            found("gpu-libgl", true),
            None,
            "a provider only when the AUR has nothing"
        );
    }

    #[test]
    fn package_names() {
        for good in ["gtk+", "lib32-glibc", "python3.12", "a@b", "0ad"] {
            assert!(valid_name(good), "{good}");
        }
        for bad in ["", "-rf", ".hidden", "Kitty", "a b", "a/b"] {
            assert!(!valid_name(bad), "{bad}");
        }
    }
}
