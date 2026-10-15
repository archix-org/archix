use std::cell::{OnceCell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::archive::{ARCH, Archive};
use crate::aur::git::AurGit;
use crate::aur::index::Index;
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
        "[options]\nArchitecture = {ARCH}\nSigLevel = Required DatabaseOptional\nLocalFileSigLevel = Optional\n"
    );
    for repo in repos {
        let _ = write!(text, "\n[{}]\n", repo.name);
        match &repo.custom {
            None => {
                let _ = writeln!(text, "Server = {}", archive.repo_url(date, &repo.name));
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

/// Resolves declared packages against one snapshot into the exact closure, taking from the AUR what the repos lack.
/// Locks `declared` and everything it depends on at `date`; `prefer` picks providers and is installed as dependencies.
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
    if !not_in_repos.is_empty() {
        let Some(aur) = aur else {
            let names: Vec<&String> = not_in_repos.iter().collect();
            bail!("not in the configured repos for this snapshot: {names:?}");
        };
        from_aur = aur_closure(aur, &conf, &db, &not_in_repos)?;
    }

    let mut sync = BTreeMap::new();
    for repo in repos {
        let path = db.join("sync").join(format!("{}.db", repo.name));
        sync.insert(repo.name.clone(), read_sync_db(&path)?);
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
        let closure = print_targets(&conf, &db, &targets, false, &from_aur.provides)?;
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
    // A pinned package (repo "archive", e.g. adopted after a pacman -Syu) keeps the pkgbase the snapshot's repos list for it.
    let base_of = |name: &str, locked: &Locked| {
        if !is_official(&locked.repo) && locked.repo != ARCHIVE_REPO {
            return None;
        }
        sync.iter()
            .filter(|(repo, _)| is_official(repo))
            .find_map(|(_, packages)| packages.get(name))
            .map(|pkg| pkg.base.clone())
    };
    for name in recipes.patched_packages() {
        let Some(locked) = lock.packages.get(name) else {
            bail!("patches.{name}: {name} isn't among this config's packages or their dependencies");
        };
        let Some(base) = base_of(name, locked) else {
            bail!(
                "patches.{name}: {name} comes from {}, but only packages from Arch's official repos can be patched",
                locked.repo
            );
        };
        let recipe = recipes.patched(name, &base, &locked.version)?;
        let members: Vec<String> = lock
            .packages
            .iter()
            .filter(|(member, locked)| base_of(member, locked).as_deref() == Some(base.as_str()))
            .map(|(member, _)| member.clone())
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

#[derive(Default)]
struct AurClosure {
    packages: BTreeMap<String, Locked>,
    repo_deps: BTreeSet<String>,
    /// `name=version` for every AUR package and what it provides, so pacman treats those dependencies as met.
    provides: Vec<String>,
}

/// Walks AUR packages and their dependencies; runtime ones get installed, make/check-only ones are just built.
fn aur_closure(aur: &Aur, conf: &Path, db: &Path, targets: &BTreeSet<String>) -> Result<AurClosure> {
    let mut queue: VecDeque<(String, bool)> = targets.iter().map(|t| (t.clone(), true)).collect();
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
                let Some(entry) = aur.index()?.satisfier(&dep) else {
                    bail!("`{dep}` is in none of the configured repos, pkgbuilds/ or the AUR");
                };
                (entry.name.clone(), entry.package_base.clone())
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
                queue.push_back((dep, needed_at_runtime));
            }
        }
        for dep in build_deps {
            if missing_from_repos.contains(&dep) {
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
        closure.packages.insert(
            name,
            Locked {
                version: info.version.clone(),
                repo: if aur.recipes.is_some_and(|r| r.is_local(&base)) {
                    LOCAL_REPO.to_string()
                } else {
                    AUR_REPO.to_string()
                },
                file: info.file(pkg),
                base: Some(base.clone()),
                commit: Some(commit.clone()),
                build_only,
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
    fn package_names() {
        for good in ["gtk+", "lib32-glibc", "python3.12", "a@b", "0ad"] {
            assert!(valid_name(good), "{good}");
        }
        for bad in ["", "-rf", ".hidden", "Kitty", "a b", "a/b"] {
            assert!(!valid_name(bad), "{bad}");
        }
    }
}
