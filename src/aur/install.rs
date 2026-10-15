use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::path::PathBuf;

use anyhow::{Result, bail};

use super::build::{Builder, Job};
use super::git::AurGit;
use super::srcinfo::{self, SrcInfo, dep_name};
use crate::lock::{AUR_REPO, HostLock};

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
    satisfiers: HashMap<String, String>,
    built: HashMap<String, Vec<(String, PathBuf)>>,
    building: BTreeSet<String>,
}

impl<'a> Installer<'a> {
    pub fn new(builder: &'a Builder<'a>, lock: &'a HostLock, on_host: &'a BTreeSet<String>) -> Result<Self> {
        let mut infos = HashMap::new();
        let mut satisfiers = HashMap::new();
        for (name, pkg) in lock.packages.iter().filter(|(_, pkg)| pkg.repo == AUR_REPO) {
            let (Some(base), Some(commit)) = (&pkg.base, &pkg.commit) else {
                bail!("{name} is locked from the AUR without a base and commit");
            };
            if !infos.contains_key(base) {
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
            None => bail!("{name} is not an AUR package in the lock"),
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
        for dep in deps {
            let Some(name) = self.aur_satisfier(&dep) else { continue };
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
            }
        }
        let on_host = info.packages.iter().any(|pkg| self.on_host.contains(&pkg.name));
        let job = Job {
            base,
            commit: &commit,
            info: &info,
            date: self.lock.date,
            repos: &self.lock.repos,
            inputs: &inputs,
            on_host,
        };
        let files = self.builder.build(job).map_err(|error| BuildFailed {
            base: base.to_string(),
            commit: commit.clone(),
            version: info.version.clone(),
            error,
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
