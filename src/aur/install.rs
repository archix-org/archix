use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::path::PathBuf;

use anyhow::{Result, bail};

use super::build::{Builder, Job, SetupFailed};
use super::git::AurGit;
use super::srcinfo::{self, SrcInfo, dep_name};
use crate::lock::{AUR_REPO, HostLock, LOCAL_REPO};
use crate::recipes::Recipes;

/// One AUR base couldn't be built at its locked commit; switch offers the nearest version instead.
#[derive(Debug)]
pub struct BuildFailed {
    pub base: String,
    pub commit: String,
    pub version: String,
    pub error: anyhow::Error,
}

impl fmt::Display for BuildFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {} can't be built: {:#}", self.base, self.version, self.error)
    }
}

impl std::error::Error for BuildFailed {}

/// Builds the AUR packages `names` (and the AUR packages they need to build) from a lock.
pub struct Installer<'a> {
    builder: &'a Builder<'a>,
    lock: &'a HostLock,
    on_host: &'a BTreeSet<String>,
    infos: HashMap<String, (String, SrcInfo)>,
    /// Recipe directories of the config's own bases, which build from there instead of an AUR export.
    dirs: HashMap<String, PathBuf>,
    satisfiers: HashMap<String, String>,
    built: HashMap<String, Vec<(String, PathBuf)>>,
    building: BTreeSet<String>,
}

impl<'a> Installer<'a> {
    pub fn new(
        builder: &'a Builder<'a>,
        lock: &'a HostLock,
        on_host: &'a BTreeSet<String>,
        recipes: Option<&Recipes>,
    ) -> Result<Self> {
        let mut infos = HashMap::new();
        let mut dirs = HashMap::new();
        let mut satisfiers = HashMap::new();
        for (name, pkg) in lock.packages.iter().filter(|(_, pkg)| pkg.is_built()) {
            let (Some(base), Some(commit)) = (&pkg.base, &pkg.commit) else {
                bail!("{name} is locked as built without a base and commit");
            };
            if pkg.repo == LOCAL_REPO && !infos.contains_key(base) {
                let Some(recipes) = recipes else {
                    bail!("{name} is built from the config's recipes, which aren't available here");
                };
                let names: Vec<String> = lock
                    .packages
                    .iter()
                    .filter(|(_, other)| other.base.as_deref() == Some(base.as_str()))
                    .map(|(other, _)| other.clone())
                    .collect();
                let recipe = recipes.locked(base, commit, &pkg.version, &names)?;
                dirs.insert(base.clone(), recipe.dir);
                infos.insert(base.clone(), (commit.clone(), recipe.info));
            } else if pkg.repo == AUR_REPO && !infos.contains_key(base) {
                let info = read_srcinfo(builder.git, base, commit).map_err(|error| BuildFailed {
                    base: base.clone(),
                    commit: commit.clone(),
                    version: pkg.version.clone(),
                    error,
                })?;
                infos.insert(base.clone(), (commit.clone(), info));
            }
            satisfiers.insert(name.clone(), name.clone());
            if let Some(srcpkg) = infos[base].1.package(name) {
                for provided in &srcpkg.provides {
                    satisfiers
                        .entry(dep_name(provided).to_string())
                        .or_insert_with(|| name.clone());
                }
            }
        }
        Ok(Installer {
            builder,
            lock,
            on_host,
            infos,
            dirs,
            satisfiers,
            built: HashMap::new(),
            building: BTreeSet::new(),
        })
    }

    /// The built file of every requested package, building dependencies first.
    pub fn build(mut self, names: &[String]) -> Result<BTreeMap<String, PathBuf>> {
        for name in names {
            let base = self.base_of(name)?;
            self.build_base(&base)?;
        }
        Ok(self.built.into_values().flatten().collect())
    }

    fn base_of(&self, name: &str) -> Result<String> {
        match self.lock.packages.get(name).and_then(|pkg| pkg.base.clone()) {
            Some(base) => Ok(base),
            None => bail!("{name} is not built from a recipe in the lock"),
        }
    }

    /// The AUR package in the lock that satisfies a dependency string, if any.
    fn aur_satisfier(&self, dep: &str) -> Option<String> {
        self.satisfiers.get(dep_name(dep)).cloned()
    }

    fn build_base(&mut self, base: &str) -> Result<()> {
        if self.built.contains_key(base) {
            return Ok(());
        }
        if !self.building.insert(base.to_string()) {
            bail!("AUR packages depend on each other in a cycle through {base}");
        }
        let (commit, info) = self.infos[base].clone();
        let deps: Vec<String> = info
            .makedepends
            .iter()
            .chain(&info.checkdepends)
            .chain(info.packages.iter().flat_map(|pkg| &pkg.depends))
            .cloned()
            .collect();
        let mut inputs = Vec::new();
        let mut repo_deps = Vec::new();
        for dep in deps {
            let Some(name) = self.aur_satisfier(&dep) else {
                repo_deps.push(dep);
                continue;
            };
            if self.base_of(&name)? == base {
                continue;
            }
            for needed in self.runtime_closure(&name)? {
                let needed_base = self.base_of(&needed)?;
                self.build_base(&needed_base)?;
                let file = self.built[&needed_base]
                    .iter()
                    .find(|(n, _)| *n == needed)
                    .map(|(_, f)| f.clone());
                inputs.extend(file);
                if let Some(pkg) = self.infos[&needed_base].1.package(&needed) {
                    repo_deps.extend(pkg.depends.iter().filter(|d| self.aur_satisfier(d).is_none()).cloned());
                }
            }
        }
        repo_deps.sort();
        repo_deps.dedup();
        let on_host = info.packages.iter().any(|pkg| self.on_host.contains(&pkg.name));
        let locked: BTreeMap<String, String> = info
            .packages
            .iter()
            .filter_map(|pkg| {
                let sha256 = &self.lock.packages.get(&pkg.name)?.sha256;
                (!sha256.is_empty()).then(|| (pkg.name.clone(), sha256.clone()))
            })
            .collect();
        let job = Job {
            base,
            dir: self.dirs.get(base).map(PathBuf::as_path),
            commit: &commit,
            info: &info,
            date: self.lock.date,
            repos: &self.lock.repos,
            inputs: &inputs,
            repo_deps: &repo_deps,
            on_host,
            locked: &locked,
        };
        let files = self.builder.build(job).map_err(|error| {
            if error.is::<SetupFailed>() {
                return error;
            }
            anyhow::Error::new(BuildFailed {
                base: base.to_string(),
                commit: commit.clone(),
                version: info.version.clone(),
                error,
            })
        })?;
        self.building.remove(base);
        self.built.insert(base.to_string(), files);
        Ok(())
    }

    /// A package plus the AUR packages it needs at runtime, which the build chroot must have installed too.
    fn runtime_closure(&self, name: &str) -> Result<Vec<String>> {
        let mut seen = BTreeSet::new();
        let mut stack = vec![name.to_string()];
        while let Some(name) = stack.pop() {
            if !seen.insert(name.clone()) {
                continue;
            }
            let base = self.base_of(&name)?;
            if let Some(pkg) = self.infos[&base].1.package(&name) {
                stack.extend(pkg.depends.iter().filter_map(|dep| self.aur_satisfier(dep)));
            }
        }
        Ok(seen.into_iter().collect())
    }
}

pub fn read_srcinfo(git: &AurGit, base: &str, commit: &str) -> Result<SrcInfo> {
    if !git.has_commit(base, commit) {
        git.sync(base)?;
        if !git.has_commit(base, commit) {
            bail!("commit {commit} is no longer in {base}'s AUR history");
        }
    }
    srcinfo::parse(&git.show(base, commit, ".SRCINFO")?)
}

/// A commit of a base together with what it builds.
pub type Candidate = (String, SrcInfo);

/// The nearest newer (the AUR's newest) and nearest older versions of a base around a commit that can't be built.
pub fn nearest(
    git: &AurGit,
    base: &str,
    commit: &str,
    version: &str,
) -> Result<(Option<Candidate>, Option<Candidate>)> {
    git.sync(base)?;
    let history = git.history(base)?;
    let head = history.first().cloned();
    let newer = match head {
        Some(head) if head != commit => {
            let info = read_srcinfo(git, base, &head)?;
            (info.version != version).then_some((head, info))
        }
        _ => None,
    };
    let after = history
        .iter()
        .position(|c| c == commit)
        .map_or(history.len(), |i| i + 1);
    let older = history[after..].iter().take(200).find_map(|c| {
        let info = read_srcinfo(git, base, c).ok()?;
        (info.version != version).then(|| (c.clone(), info))
    });
    Ok((newer, older))
}
