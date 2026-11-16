use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use super::build::BUILD_USER;
use super::srcinfo::{SrcInfo, dep_name, parse};
use crate::plan::AurItem;

/// One external command, as a runner gets it.
#[derive(Debug, Clone, PartialEq)]
pub struct Cmd {
    pub program: String,
    pub args: Vec<OsString>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, OsString)>,
    pub env_remove: Vec<String>,
    /// Where stdout and stderr go instead of the terminal.
    pub log: Option<PathBuf>,
}

impl Cmd {
    pub fn new(program: &str) -> Cmd {
        Cmd {
            program: program.to_string(),
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
            env_remove: Vec::new(),
            log: None,
        }
    }

    pub fn arg(mut self, arg: impl AsRef<OsStr>) -> Cmd {
        self.args.push(arg.as_ref().to_os_string());
        self
    }

    pub fn args<I: IntoIterator<Item = S>, S: AsRef<OsStr>>(mut self, args: I) -> Cmd {
        self.args.extend(args.into_iter().map(|a| a.as_ref().to_os_string()));
        self
    }

    pub fn env(mut self, key: &str, value: impl AsRef<OsStr>) -> Cmd {
        self.env.push((key.to_string(), value.as_ref().to_os_string()));
        self
    }

    /// The program and its arguments, lossily as text.
    pub fn argv(&self) -> Vec<String> {
        std::iter::once(self.program.clone())
            .chain(self.args.iter().map(|a| a.to_string_lossy().into_owned()))
            .collect()
    }

    pub fn line(&self) -> String {
        self.argv().join(" ")
    }
}

/// Runs the builder's commands; tests swap in one that records them.
pub trait Runner {
    /// Runs `cmd`, failing unless it exits 0.
    fn run(&self, cmd: &Cmd) -> Result<()>;
    /// Runs `cmd` and says whether it exited 0.
    fn succeeds(&self, cmd: &Cmd) -> Result<bool>;
    fn is_root(&self) -> bool {
        unsafe { libc::geteuid() == 0 }
    }
}

pub struct SystemRunner;

impl SystemRunner {
    fn command(cmd: &Cmd) -> Result<Command> {
        let mut command = Command::new(&cmd.program);
        command.args(&cmd.args);
        if let Some(cwd) = &cmd.cwd {
            command.current_dir(cwd);
        }
        for key in &cmd.env_remove {
            command.env_remove(key);
        }
        command.envs(cmd.env.iter().map(|(k, v)| (k, v)));
        if let Some(log) = &cmd.log {
            let file = fs::File::create(log).with_context(|| format!("creating {}", log.display()))?;
            command.stdout(file.try_clone()?).stderr(file);
        }
        Ok(command)
    }

    fn status(cmd: &Cmd, quiet: bool) -> Result<std::process::ExitStatus> {
        let mut command = Self::command(cmd)?;
        if quiet {
            command.stdout(Stdio::null()).stderr(Stdio::null());
        }
        match command.status() {
            Ok(status) => Ok(status),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                bail!("{} not found; AUR builds need devtools and git", cmd.program)
            }
            Err(e) => Err(e).with_context(|| format!("running {}", cmd.program)),
        }
    }
}

impl Runner for SystemRunner {
    fn run(&self, cmd: &Cmd) -> Result<()> {
        let status = Self::status(cmd, false)?;
        if !status.success() {
            bail!("{} failed with {status}", cmd.line());
        }
        Ok(())
    }

    fn succeeds(&self, cmd: &Cmd) -> Result<bool> {
        Ok(Self::status(cmd, true)?.success())
    }
}

/// SCOPE 2.4: builds AUR packages as root with devtools in archix's one clean chroot, never through an AUR helper.
pub struct DevtoolsBuilder {
    pub aur_url: String,
    /// Checkouts at `<cache>/<base>`, sources at `<cache>/.srcdest/<base>`, built files at `<cache>/.pkgdest/<base>`.
    pub cache: PathBuf,
    /// makechrootpkg's -r: the clean chroot is `<chroot>/root`.
    pub chroot: PathBuf,
    /// Each base's build output goes to `<log_dir>/<base>.log`.
    pub log_dir: PathBuf,
    /// devtools' archroot.sh, whose CHROOT_VERSION a reused chroot has to match.
    pub archroot: PathBuf,
    pub runner: Box<dyn Runner>,
}

impl Default for DevtoolsBuilder {
    fn default() -> Self {
        DevtoolsBuilder {
            aur_url: super::DEFAULT_URL.to_string(),
            cache: PathBuf::from("/var/cache/archix/aur"),
            chroot: PathBuf::from("/var/lib/archix/chroot"),
            log_dir: PathBuf::from("/var/log/archix/aur"),
            archroot: PathBuf::from("/usr/share/devtools/lib/archroot.sh"),
            runner: Box::new(SystemRunner),
        }
    }
}

/// One AUR base to build and the item packages wanted from it.
#[derive(Debug, Clone, PartialEq)]
pub struct Recipe {
    pub info: SrcInfo,
    pub wanted: Vec<String>,
}

impl Recipe {
    fn needs(&self) -> impl Iterator<Item = &str> {
        self.info
            .makedepends
            .iter()
            .chain(&self.info.checkdepends)
            .chain(self.info.packages.iter().flat_map(|pkg| &pkg.depends))
            .map(|dep| dep_name(dep))
    }
}

/// Which recipe and package satisfy each name or provided name among the wanted packages.
fn providers(recipes: &[Recipe]) -> BTreeMap<&str, (usize, &str)> {
    let mut map = BTreeMap::new();
    for (i, recipe) in recipes.iter().enumerate() {
        for name in &recipe.wanted {
            let Some(pkg) = recipe.info.package(name) else { continue };
            map.entry(pkg.name.as_str()).or_insert((i, pkg.name.as_str()));
            for provided in &pkg.provides {
                map.entry(dep_name(provided)).or_insert((i, pkg.name.as_str()));
            }
        }
    }
    for (i, recipe) in recipes.iter().enumerate() {
        for name in &recipe.wanted {
            map.insert(name.as_str(), (i, name.as_str()));
        }
    }
    map
}

/// For each recipe, the other recipes whose wanted packages its depends, makedepends or checkdepends name.
pub fn dependencies(recipes: &[Recipe]) -> Vec<BTreeSet<usize>> {
    let providers = providers(recipes);
    recipes
        .iter()
        .enumerate()
        .map(|(i, recipe)| {
            recipe
                .needs()
                .filter_map(|dep| providers.get(dep).map(|(j, _)| *j))
                .filter(|j| *j != i)
                .collect()
        })
        .collect()
}

/// The recipes' indices in build order, dependencies first and otherwise as given; a cycle is an error naming its bases.
pub fn build_order(recipes: &[Recipe]) -> Result<Vec<usize>> {
    let deps = dependencies(recipes);
    let mut done = vec![false; recipes.len()];
    let mut order = Vec::new();
    while order.len() < recipes.len() {
        let next = (0..recipes.len()).find(|&i| !done[i] && deps[i].iter().all(|&j| done[j]));
        let Some(i) = next else {
            let stuck: Vec<&str> = (0..recipes.len())
                .filter(|&i| !done[i])
                .map(|i| recipes[i].info.base.as_str())
                .collect();
            bail!(
                "the AUR packages {} depend on each other in a cycle, so none can be built first",
                stuck.join(", ")
            );
        };
        done[i] = true;
        order.push(i);
    }
    Ok(order)
}

/// The wanted packages of other recipes that building `index` needs installed, with their runtime dependencies, as (recipe, package).
pub fn build_inputs(recipes: &[Recipe], index: usize) -> Vec<(usize, String)> {
    let providers = providers(recipes);
    let mut seen: BTreeSet<(usize, &str)> = BTreeSet::new();
    let mut queue: VecDeque<&str> = recipes[index].needs().collect();
    while let Some(dep) = queue.pop_front() {
        let Some(&(j, pkg)) = providers.get(dep) else { continue };
        if j == index || !seen.insert((j, pkg)) {
            continue;
        }
        if let Some(pkg) = recipes[j].info.package(pkg) {
            queue.extend(pkg.depends.iter().map(|d| dep_name(d)));
        }
    }
    let order = build_order(recipes).unwrap_or_else(|_| (0..recipes.len()).collect());
    let rank = |j: usize| order.iter().position(|&o| o == j).unwrap_or(usize::MAX);
    let mut inputs: Vec<(usize, String)> = seen.into_iter().map(|(j, p)| (j, p.to_string())).collect();
    inputs.sort_by_key(|(j, p)| (rank(*j), p.clone()));
    inputs
}

/// Whether the PKGBUILD has a pkgver() function, which sets the version while building.
pub fn is_vcs(pkgbuild: &str) -> bool {
    pkgbuild.lines().any(|line| {
        let line = line.trim_start();
        let line = line.strip_prefix("function ").unwrap_or(line).trim_start();
        line.strip_prefix("pkgver")
            .is_some_and(|rest| rest.trim_start().starts_with("()"))
    })
}

fn assignment<'a>(pkgbuild: &'a str, key: &str) -> Option<&'a str> {
    pkgbuild.lines().find_map(|line| {
        line.strip_prefix(key)?
            .strip_prefix('=')
            .map(|v| v.trim().trim_matches(['\'', '"']))
    })
}

/// The version the build made: .SRCINFO's, or for a VCS package what makepkg wrote back to the PKGBUILD from pkgver().
pub fn expected_version(info: &SrcInfo, pkgbuild_after: &str) -> String {
    if !is_vcs(pkgbuild_after) {
        return info.version.clone();
    }
    let (Some(pkgver), Some(pkgrel)) = (
        assignment(pkgbuild_after, "pkgver"),
        assignment(pkgbuild_after, "pkgrel"),
    ) else {
        return info.version.clone();
    };
    match assignment(pkgbuild_after, "epoch").filter(|e| *e != "0") {
        Some(epoch) => format!("{epoch}:{pkgver}-{pkgrel}"),
        None => format!("{pkgver}-{pkgrel}"),
    }
}

/// Name and version from a package file name such as `foo-1:2.0-1-x86_64.pkg.tar.zst`.
pub fn file_name_version(file: &str) -> Option<(String, String)> {
    let stem = &file[..file.find(".pkg.tar")?];
    let mut parts = stem.rsplitn(4, '-');
    let (_arch, rel, ver, name) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    Some((name.to_string(), format!("{ver}-{rel}")))
}

/// pkgname and pkgver from a built package's .PKGINFO.
pub fn pkginfo(path: &Path) -> Result<(String, String)> {
    let file = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let decoder = zstd::stream::read::Decoder::new(file)?;
    let mut archive = tar::Archive::new(decoder);
    for entry in archive.entries()? {
        let mut entry = entry?;
        if entry.path()?.as_os_str() != ".PKGINFO" {
            continue;
        }
        let mut text = String::new();
        entry.read_to_string(&mut text)?;
        let field = |key: &str| {
            text.lines().find_map(|l| {
                l.strip_prefix(key)?
                    .trim_start()
                    .strip_prefix('=')
                    .map(|v| v.trim().to_string())
            })
        };
        let (Some(name), Some(version)) = (field("pkgname"), field("pkgver")) else {
            bail!("{} has a .PKGINFO without pkgname or pkgver", path.display());
        };
        return Ok((name, version));
    }
    bail!("{} has no .PKGINFO", path.display())
}

/// The built file for each wanted package, checked by file name and .PKGINFO against the version the build should have made.
pub fn check_built(recipe: &Recipe, pkgbuild_after: &str, files: &[PathBuf]) -> Result<Vec<(String, PathBuf)>> {
    let base = &recipe.info.base;
    let version = expected_version(&recipe.info, pkgbuild_after);
    let mut out = Vec::new();
    for name in &recipe.wanted {
        let found: Vec<&PathBuf> = files
            .iter()
            .filter(|f| {
                f.file_name()
                    .and_then(|f| f.to_str())
                    .and_then(file_name_version)
                    .is_some_and(|(n, _)| &n == name)
            })
            .collect();
        let [file] = found.as_slice() else {
            bail!("building {base} made {} package files for {name}, not one", found.len());
        };
        let (_, made) = file_name_version(&file.file_name().unwrap_or_default().to_string_lossy()).unwrap_or_default();
        if made != version {
            bail!("building {base} made {name} {made}, but its recipe says {version}");
        }
        let (info_name, info_version) = pkginfo(file)?;
        if info_name != *name || info_version != version {
            bail!(
                "{} says it is {info_name} {info_version}, not {name} {version}",
                file.display()
            );
        }
        out.push((name.clone(), (*file).clone()));
    }
    Ok(out)
}

/// devtools' CHROOT_VERSION in archroot.sh.
fn chroot_version(archroot: &Path) -> Option<String> {
    fs::read_to_string(archroot)
        .ok()?
        .lines()
        .find_map(|line| line.trim().strip_prefix("CHROOT_VERSION="))
        .map(|value| value.trim_matches(['\'', '"']).to_string())
}

fn devtools(program: &str) -> Cmd {
    let mut cmd = Cmd::new(program).env("USER", "root");
    cmd.env_remove.push("SUDO_USER".into());
    cmd
}

fn git(dir: &Path) -> Cmd {
    Cmd::new("git").args(["-c", "safe.directory=*", "-C"]).arg(dir)
}

impl DevtoolsBuilder {
    fn checkout(&self, base: &str) -> PathBuf {
        self.cache.join(base)
    }

    fn srcdest(&self, base: &str) -> PathBuf {
        self.cache.join(".srcdest").join(base)
    }

    fn pkgdest(&self, base: &str) -> PathBuf {
        self.cache.join(".pkgdest").join(base)
    }

    fn log(&self, base: &str) -> PathBuf {
        self.log_dir.join(format!("{base}.log"))
    }

    /// The checkout of `base` at `commit`, with nothing else in it.
    fn fetch(&self, base: &str, commit: &str) -> Result<()> {
        let dir = self.checkout(base);
        if !dir.join(".git").is_dir() {
            if dir.exists() {
                fs::remove_dir_all(&dir)?;
            }
            fs::create_dir_all(&self.cache)?;
            let url = format!("{}/{base}.git", self.aur_url.trim_end_matches('/'));
            self.runner
                .run(
                    &Cmd::new("git")
                        .args(["clone", "--quiet", "--no-checkout"])
                        .arg(&url)
                        .arg(&dir),
                )
                .with_context(|| format!("fetching {base} from {url}"))?;
        } else {
            self.runner
                .run(&git(&dir).args(["fetch", "--quiet", "origin"]))
                .with_context(|| format!("fetching {base}"))?;
        }
        self.runner
            .run(&git(&dir).args(["checkout", "--quiet", "--force", "--detach", commit]))
            .with_context(|| format!("checking out {base} at its pinned commit {commit}"))?;
        self.runner.run(&git(&dir).args(["clean", "-ffdxq"]))
    }

    fn ensure_user(&self) -> Result<()> {
        if self.runner.succeeds(&Cmd::new("id").args(["-u", BUILD_USER]))? {
            return Ok(());
        }
        self.runner.run(&Cmd::new("useradd").args([
            "--system",
            "--no-create-home",
            "--shell",
            "/usr/bin/nologin",
            BUILD_USER,
        ]))
    }

    /// Makes `<chroot>/root` once, again only when devtools' chroot format changed.
    fn ensure_chroot(&self, conf: &Path) -> Result<()> {
        let root = self.chroot.join("root");
        let have = fs::read_to_string(root.join(".arch-chroot")).ok();
        let current = match (&have, chroot_version(&self.archroot)) {
            (Some(have), Some(want)) => have.trim() == want,
            (have, None) => have.is_some(),
            (None, Some(_)) => false,
        };
        if current {
            return Ok(());
        }
        if root.exists() {
            fs::remove_dir_all(&root).with_context(|| format!("removing the outdated chroot {}", root.display()))?;
        }
        fs::create_dir_all(&self.chroot)?;
        self.runner
            .run(&devtools("mkarchroot").arg("-C").arg(conf).arg(&root).arg("base-devel"))
            .context("creating the AUR build chroot")
    }

    /// Puts the plan's pacman.conf and sync dbs into the clean chroot, so its builds resolve at the plan's repo state.
    fn sync_chroot(&self, conf: &Path, sync_dbs: &Path) -> Result<()> {
        let root = self.chroot.join("root");
        let mut dbs: Vec<PathBuf> = fs::read_dir(sync_dbs)
            .with_context(|| format!("reading the plan's sync dbs in {}", sync_dbs.display()))?
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.is_file()
                    && p.file_name()
                        .and_then(|f| f.to_str())
                        .is_some_and(|f| f.ends_with(".db") || f.ends_with(".db.sig"))
            })
            .collect();
        dbs.sort();
        if dbs.is_empty() {
            bail!("{} has no sync dbs to build against", sync_dbs.display());
        }
        self.runner.run(
            &Cmd::new("cp")
                .args(["-f", "--preserve=timestamps", "--"])
                .arg(conf)
                .arg(root.join("etc/pacman.conf")),
        )?;
        self.runner.run(
            &Cmd::new("cp")
                .args(["-f", "--preserve=timestamps", "-t"])
                .arg(root.join("var/lib/pacman/sync"))
                .arg("--")
                .args(&dbs),
        )
    }

    /// SCOPE 2.4's AUR builds: the built package files of `items`, in install order.
    pub fn build(&self, items: &[AurItem], conf: &Path, sync_dbs: &Path) -> Result<Vec<PathBuf>> {
        if items.is_empty() {
            return Ok(Vec::new());
        }
        if !self.runner.is_root() {
            bail!("AUR packages are built as root, which archix isn't running as");
        }
        let mut bases: Vec<(String, String, Vec<String>)> = Vec::new();
        for item in items {
            let base = item.base.clone().unwrap_or_else(|| item.name.clone());
            let Some(commit) = item.commit.clone() else {
                bail!("{} has no pinned AUR commit to build", item.name);
            };
            match bases.iter_mut().find(|(b, _, _)| *b == base) {
                Some((_, c, wanted)) if *c == commit => wanted.push(item.name.clone()),
                Some((_, c, _)) => bail!("{base} is pinned at both {c} and {commit}"),
                None => bases.push((base, commit, vec![item.name.clone()])),
            }
        }
        let mut recipes = Vec::new();
        for (base, commit, wanted) in &bases {
            self.fetch(base, commit)?;
            let path = self.checkout(base).join(".SRCINFO");
            let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
            let info = parse(&text).with_context(|| format!("parsing {}", path.display()))?;
            if info.base != *base {
                bail!("{base} at {commit} is the package base {}", info.base);
            }
            for name in wanted {
                if info.package(name).is_none() {
                    bail!("{base} at {commit} doesn't build {name}");
                }
            }
            recipes.push(Recipe {
                info,
                wanted: wanted.clone(),
            });
        }
        let order = build_order(&recipes)?;
        self.ensure_user()?;
        self.ensure_chroot(conf)?;
        fs::create_dir_all(&self.log_dir)?;
        let mut built: BTreeMap<(usize, String), PathBuf> = BTreeMap::new();
        let mut out = Vec::new();
        for &i in &order {
            let recipe = &recipes[i];
            let base = recipe.info.base.as_str();
            let inputs: Vec<PathBuf> = build_inputs(&recipes, i)
                .into_iter()
                .map(|key| built[&key].clone())
                .collect();
            let files = self.build_one(recipe, &inputs, conf, sync_dbs)?;
            for (name, file) in files {
                built.insert((i, name), file.clone());
                out.push(file);
            }
            eprintln!(":: built {base}");
        }
        Ok(out)
    }

    fn build_one(
        &self,
        recipe: &Recipe,
        inputs: &[PathBuf],
        conf: &Path,
        sync_dbs: &Path,
    ) -> Result<Vec<(String, PathBuf)>> {
        let base = recipe.info.base.as_str();
        let (dir, srcdest, pkgdest, log) = (
            self.checkout(base),
            self.srcdest(base),
            self.pkgdest(base),
            self.log(base),
        );
        if pkgdest.exists() {
            fs::remove_dir_all(&pkgdest)?;
        }
        fs::create_dir_all(&pkgdest)?;
        fs::create_dir_all(&srcdest)?;
        self.runner.run(
            &Cmd::new("chown")
                .args(["-R", &format!("{BUILD_USER}:"), "--"])
                .arg(&dir)
                .arg(&srcdest),
        )?;
        self.sync_chroot(conf, sync_dbs)?;
        eprintln!(":: building {base} in a clean chroot (log: {})", log.display());
        let mut cmd = devtools("makechrootpkg")
            .arg("-c")
            .arg("-r")
            .arg(&self.chroot)
            .args(["-U", BUILD_USER]);
        for input in inputs {
            cmd = cmd.arg("-I").arg(input);
        }
        cmd = cmd
            .env("SRCDEST", &srcdest)
            .env("PKGDEST", &pkgdest)
            .env("LOGDEST", &pkgdest);
        cmd.cwd = Some(dir.clone());
        cmd.log = Some(log.clone());
        if let Err(e) = self.runner.run(&cmd) {
            bail!(
                "building the AUR package {} failed ({e:#}); its log is {}",
                recipe.wanted.join(", "),
                log.display()
            );
        }
        let pkgbuild = fs::read_to_string(dir.join("PKGBUILD")).unwrap_or_default();
        let mut files: Vec<PathBuf> = fs::read_dir(&pkgdest)?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.to_string_lossy().contains(".pkg.tar") && !p.to_string_lossy().ends_with(".sig"))
            .collect();
        files.sort();
        check_built(recipe, &pkgbuild, &files)
            .with_context(|| format!("checking what building {base} made; its log is {}", log.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::io::Write;
    use std::rc::Rc;
    use tempfile::TempDir;

    fn srcinfo(base: &str, version: &str, pkgs: &[(&str, &[&str], &[&str])], makedepends: &[&str]) -> String {
        let (ver, rel) = version.rsplit_once('-').unwrap();
        let mut s = format!("pkgbase = {base}\n\tpkgver = {ver}\n\tpkgrel = {rel}\n\tarch = any\n");
        for dep in makedepends {
            s.push_str(&format!("\tmakedepends = {dep}\n"));
        }
        for (name, depends, provides) in pkgs {
            s.push_str(&format!("\npkgname = {name}\n"));
            for dep in *depends {
                s.push_str(&format!("\tdepends = {dep}\n"));
            }
            for p in *provides {
                s.push_str(&format!("\tprovides = {p}\n"));
            }
        }
        s
    }

    fn recipe(base: &str, depends: &[&str], makedepends: &[&str]) -> Recipe {
        Recipe {
            info: parse(&srcinfo(base, "1-1", &[(base, depends, &[])], makedepends)).unwrap(),
            wanted: vec![base.to_string()],
        }
    }

    fn bases(recipes: &[Recipe], order: &[usize]) -> Vec<String> {
        order.iter().map(|&i| recipes[i].info.base.clone()).collect()
    }

    #[test]
    fn dependencies_build_first_and_the_rest_keep_their_order() {
        let recipes = vec![
            recipe("a", &["b>=1"], &[]),
            recipe("z", &[], &[]),
            recipe("b", &[], &["c"]),
            recipe("c", &["glibc"], &[]),
        ];
        assert_eq!(bases(&recipes, &build_order(&recipes).unwrap()), ["z", "c", "b", "a"]);
    }

    #[test]
    fn a_provided_name_and_a_split_sibling_order_too() {
        let mut lib = Recipe {
            info: parse(&srcinfo(
                "lib",
                "1-1",
                &[("lib", &[], &["libfoo.so=1"]), ("lib-docs", &["lib"], &[])],
                &[],
            ))
            .unwrap(),
            wanted: vec!["lib".into()],
        };
        let app = recipe("app", &["libfoo.so=1"], &[]);
        let recipes = vec![app.clone(), lib.clone()];
        assert_eq!(bases(&recipes, &build_order(&recipes).unwrap()), ["lib", "app"]);
        lib.wanted.push("lib-docs".into());
        let recipes = vec![lib, app];
        assert_eq!(
            dependencies(&recipes),
            vec![BTreeSet::new(), BTreeSet::from([0])],
            "a sibling isn't a dependency"
        );
    }

    #[test]
    fn a_cycle_fails_naming_its_bases() {
        let recipes = vec![
            recipe("ok", &[], &[]),
            recipe("x", &[], &["y"]),
            recipe("y", &["x"], &[]),
        ];
        let err = build_order(&recipes).unwrap_err().to_string();
        assert!(
            err.contains("x, y") && err.contains("cycle") && !err.contains("ok"),
            "{err}"
        );
    }

    #[test]
    fn inputs_are_every_aur_dependency_with_its_runtime_dependencies() {
        let recipes = vec![
            recipe("top", &[], &["mid"]),
            recipe("mid", &["leaf"], &[]),
            recipe("leaf", &[], &["tool"]),
            recipe("tool", &[], &[]),
        ];
        let names = |i| -> Vec<String> { build_inputs(&recipes, i).into_iter().map(|(_, n)| n).collect() };
        assert_eq!(names(0), ["leaf", "mid"], "tool is only leaf's makedepend");
        assert_eq!(names(2), ["tool"]);
        assert!(names(3).is_empty());
    }

    #[test]
    fn vcs_versions_come_from_the_pkgbuild_makepkg_rewrote() {
        let info = parse(&srcinfo("x-git", "1.0-3", &[("x-git", &[], &[])], &[])).unwrap();
        assert_eq!(expected_version(&info, "pkgver=1.0\npkgrel=3\n"), "1.0-3");
        let after = "pkgver=1.0.r7.gabc\npkgrel=1\nepoch=2\npkgver() {\n  git describe\n}\n";
        assert!(is_vcs(after));
        assert_eq!(expected_version(&info, after), "2:1.0.r7.gabc-1");
        assert!(is_vcs("function pkgver () {"));
        assert!(!is_vcs("pkgver=1\n_pkgver() {"));
        assert_eq!(
            file_name_version("x-git-2:1.0.r7.gabc-1-x86_64.pkg.tar.zst"),
            Some(("x-git".into(), "2:1.0.r7.gabc-1".into()))
        );
    }

    fn write_pkg(dir: &Path, name: &str, version: &str, info_version: &str) -> PathBuf {
        let path = dir.join(format!("{name}-{version}-any.pkg.tar.zst"));
        let mut tar = tar::Builder::new(Vec::new());
        let text = format!("pkgname = {name}\npkgver = {info_version}\n");
        let mut header = tar::Header::new_gnu();
        header.set_size(text.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, ".PKGINFO", text.as_bytes()).unwrap();
        let bytes = zstd::encode_all(tar.into_inner().unwrap().as_slice(), 0).unwrap();
        fs::File::create(&path).unwrap().write_all(&bytes).unwrap();
        path
    }

    #[test]
    fn built_files_must_match_the_recipe() {
        let tmp = TempDir::new().unwrap();
        let r = recipe("foo", &[], &[]);
        let good = write_pkg(tmp.path(), "foo", "1-1", "1-1");
        let debug = write_pkg(tmp.path(), "foo-debug", "1-1", "1-1");
        assert_eq!(
            check_built(&r, "pkgver=1\n", &[debug.clone(), good.clone()]).unwrap(),
            [("foo".to_string(), good)]
        );

        let old = write_pkg(tmp.path(), "foo", "0.9-1", "0.9-1");
        let err = check_built(&r, "", &[old]).unwrap_err().to_string();
        assert!(err.contains("made foo 0.9-1, but its recipe says 1-1"), "{err}");

        let lying = TempDir::new().unwrap();
        let file = write_pkg(lying.path(), "foo", "1-1", "2-1");
        let err = check_built(&r, "", &[file]).unwrap_err().to_string();
        assert!(err.contains("says it is foo 2-1"), "{err}");

        let err = check_built(&r, "", &[debug]).unwrap_err().to_string();
        assert!(err.contains("0 package files for foo"), "{err}");
    }

    struct Fake {
        dir: PathBuf,
        recipes: BTreeMap<String, (String, String)>,
        fail: Option<String>,
        /// What a VCS base's pkgver() gives, written back to the PKGBUILD as makepkg does.
        vcs: BTreeMap<String, String>,
        log: RefCell<Vec<Cmd>>,
    }

    impl Fake {
        fn env<'a>(cmd: &'a Cmd, key: &str) -> &'a OsStr {
            &cmd.env.iter().find(|(k, _)| k == key).unwrap().1
        }
    }

    impl Runner for Fake {
        fn run(&self, cmd: &Cmd) -> Result<()> {
            self.log.borrow_mut().push(cmd.clone());
            let argv = cmd.argv();
            match argv[0].as_str() {
                "git" if argv[1] == "clone" => {
                    let dest = PathBuf::from(argv.last().unwrap());
                    let base = dest.file_name().unwrap().to_str().unwrap();
                    let (srcinfo, pkgbuild) = &self.recipes[base];
                    fs::create_dir_all(dest.join(".git")).unwrap();
                    fs::write(dest.join(".SRCINFO"), srcinfo).unwrap();
                    fs::write(dest.join("PKGBUILD"), pkgbuild).unwrap();
                }
                "mkarchroot" => {
                    let root = PathBuf::from(&argv[3]);
                    fs::create_dir_all(root.join("var/lib/pacman/sync")).unwrap();
                    fs::write(root.join(".arch-chroot"), "v5\n").unwrap();
                }
                "makechrootpkg" => {
                    let dir = cmd.cwd.clone().unwrap();
                    let base = dir.file_name().unwrap().to_str().unwrap().to_string();
                    if self.fail.as_deref() == Some(base.as_str()) {
                        fs::write(cmd.log.as_ref().unwrap(), "==> ERROR: A failure occurred in build().\n").unwrap();
                        bail!("makechrootpkg failed with exit status: 1");
                    }
                    let info = parse(&self.recipes[&base].0).unwrap();
                    let version = match self.vcs.get(&base) {
                        Some(v) => {
                            let text = fs::read_to_string(dir.join("PKGBUILD")).unwrap();
                            let (ver, rel) = v.rsplit_once('-').unwrap();
                            let text = text
                                .lines()
                                .map(|l| match l {
                                    l if l.starts_with("pkgver=") => format!("pkgver={ver}"),
                                    l if l.starts_with("pkgrel=") => format!("pkgrel={rel}"),
                                    l => l.to_string(),
                                })
                                .collect::<Vec<_>>()
                                .join("\n");
                            fs::write(dir.join("PKGBUILD"), text).unwrap();
                            v.clone()
                        }
                        None => info.version.clone(),
                    };
                    let pkgdest = PathBuf::from(Self::env(cmd, "PKGDEST"));
                    for pkg in &info.packages {
                        write_pkg(&pkgdest, &pkg.name, &version, &version);
                    }
                }
                _ => {}
            }
            Ok(())
        }

        fn succeeds(&self, cmd: &Cmd) -> Result<bool> {
            self.log.borrow_mut().push(cmd.clone());
            Ok(true)
        }

        fn is_root(&self) -> bool {
            true
        }
    }

    struct Shared(Rc<Fake>);

    impl Runner for Shared {
        fn run(&self, cmd: &Cmd) -> Result<()> {
            self.0.run(cmd)
        }
        fn succeeds(&self, cmd: &Cmd) -> Result<bool> {
            self.0.succeeds(cmd)
        }
        fn is_root(&self) -> bool {
            true
        }
    }

    struct Env {
        tmp: TempDir,
        fake: Rc<Fake>,
        builder: DevtoolsBuilder,
    }

    impl Env {
        fn new(recipes: &[(&str, String)], fail: Option<&str>, vcs: &[(&str, &str)]) -> Env {
            let tmp = TempDir::new().unwrap();
            let p = tmp.path();
            fs::create_dir_all(p.join("dbs")).unwrap();
            for f in ["core.db", "extra.db", "extra.db.sig", "extra.files"] {
                fs::write(p.join("dbs").join(f), f).unwrap();
            }
            fs::write(p.join("pacman.conf"), "[options]\n").unwrap();
            fs::write(p.join("archroot.sh"), "CHROOT_VERSION='v5'\n").unwrap();
            let fake = Fake {
                dir: p.to_path_buf(),
                recipes: recipes
                    .iter()
                    .map(|(base, srcinfo)| {
                        let pkgbuild = match vcs.iter().any(|(b, _)| b == base) {
                            true => "pkgver=0\npkgrel=1\npkgver() {\n  echo\n}\n",
                            false => "pkgver=1\npkgrel=1\n",
                        };
                        (base.to_string(), (srcinfo.clone(), pkgbuild.to_string()))
                    })
                    .collect(),
                fail: fail.map(str::to_string),
                vcs: vcs.iter().map(|(b, v)| (b.to_string(), v.to_string())).collect(),
                log: RefCell::new(Vec::new()),
            };
            let fake = Rc::new(fake);
            let builder = DevtoolsBuilder {
                aur_url: "https://aur.example/".into(),
                cache: p.join("cache"),
                chroot: p.join("chroot"),
                log_dir: p.join("logs"),
                archroot: p.join("archroot.sh"),
                runner: Box::new(Shared(fake.clone())),
            };
            Env { tmp, fake, builder }
        }

        fn build(&self, items: &[AurItem]) -> Result<Vec<PathBuf>> {
            let p = self.tmp.path();
            self.builder.build(items, &p.join("pacman.conf"), &p.join("dbs"))
        }

        fn fake(&self) -> &Fake {
            &self.fake
        }

        fn argvs(&self) -> Vec<String> {
            let root = self.fake().dir.to_string_lossy().into_owned();
            self.fake()
                .log
                .borrow()
                .iter()
                .map(|c| c.line().replace(&root, "T"))
                .collect()
        }
    }

    fn item(name: &str) -> AurItem {
        AurItem {
            name: name.into(),
            base: None,
            commit: Some(format!("c0ffee-{name}")),
            helper: Some("paru".into()),
            reason: "declared".into(),
            risk: Vec::new(),
        }
    }

    #[test]
    fn builds_run_devtools_in_dependency_order_with_the_exact_commands() {
        let env = Env::new(
            &[
                ("app", srcinfo("app", "2-1", &[("app", &["lib"], &[])], &["cmake"])),
                ("lib", srcinfo("lib", "1-1", &[("lib", &[], &[])], &[])),
            ],
            None,
            &[],
        );
        let files = env.build(&[item("app"), item("lib")]).unwrap();
        let names: Vec<String> = files
            .iter()
            .map(|f| f.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["lib-1-1-any.pkg.tar.zst", "app-2-1-any.pkg.tar.zst"]);
        let g = "git -c safe.directory=* -C";
        let cp_dbs = "cp -f --preserve=timestamps -t T/chroot/root/var/lib/pacman/sync -- T/dbs/core.db T/dbs/extra.db T/dbs/extra.db.sig";
        let cp_conf = "cp -f --preserve=timestamps -- T/pacman.conf T/chroot/root/etc/pacman.conf";
        assert_eq!(
            env.argvs(),
            [
                "git clone --quiet --no-checkout https://aur.example/app.git T/cache/app".to_string(),
                format!("{g} T/cache/app checkout --quiet --force --detach c0ffee-app"),
                format!("{g} T/cache/app clean -ffdxq"),
                "git clone --quiet --no-checkout https://aur.example/lib.git T/cache/lib".into(),
                format!("{g} T/cache/lib checkout --quiet --force --detach c0ffee-lib"),
                format!("{g} T/cache/lib clean -ffdxq"),
                "id -u archix-build".into(),
                "mkarchroot -C T/pacman.conf T/chroot/root base-devel".into(),
                "chown -R archix-build: -- T/cache/lib T/cache/.srcdest/lib".into(),
                cp_conf.into(),
                cp_dbs.into(),
                "makechrootpkg -c -r T/chroot -U archix-build".into(),
                "chown -R archix-build: -- T/cache/app T/cache/.srcdest/app".into(),
                cp_conf.into(),
                cp_dbs.into(),
                "makechrootpkg -c -r T/chroot -U archix-build -I T/cache/.pkgdest/lib/lib-1-1-any.pkg.tar.zst".into(),
            ]
        );
        let log = env.fake().log.borrow();
        let make = log.iter().find(|c| c.program == "makechrootpkg").unwrap();
        assert_eq!(make.cwd.as_deref(), Some(env.tmp.path().join("cache/lib").as_path()));
        assert_eq!(make.log.as_deref(), Some(env.tmp.path().join("logs/lib.log").as_path()));
        assert!(make.env_remove.contains(&"SUDO_USER".to_string()));
        for cmd in log.iter() {
            let argv = cmd.argv();
            assert!(
                !["sudo", "paru", "yay", "pacman"].contains(&argv[0].as_str()),
                "{argv:?}"
            );
            assert!(!argv.iter().any(|a| a.contains("-Sy")), "{argv:?}");
            if argv[0] == "makechrootpkg" {
                assert!(!argv.iter().any(|a| a == "-u"), "{argv:?}");
            }
        }
        drop(log);

        env.fake().log.borrow_mut().clear();
        env.build(&[item("lib")]).unwrap();
        let again = env.argvs();
        assert_eq!(
            again[0],
            format!("{g} T/cache/lib fetch --quiet origin"),
            "a checkout is fetched, not cloned again"
        );
        assert!(
            !again.iter().any(|l| l.starts_with("mkarchroot")),
            "the chroot is made once: {again:?}"
        );
    }

    #[test]
    fn a_chroot_of_another_devtools_format_is_made_again() {
        let env = Env::new(&[("lib", srcinfo("lib", "1-1", &[("lib", &[], &[])], &[]))], None, &[]);
        let root = env.tmp.path().join("chroot/root");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join(".arch-chroot"), "v4\n").unwrap();
        fs::write(root.join("stale"), "").unwrap();
        env.build(&[item("lib")]).unwrap();
        assert!(env.argvs().iter().any(|l| l.starts_with("mkarchroot")));
        assert!(!root.join("stale").exists());
    }

    #[test]
    fn inputs_accumulate_across_a_chain() {
        let env = Env::new(
            &[
                ("top", srcinfo("top", "1-1", &[("top", &[], &[])], &["mid"])),
                ("mid", srcinfo("mid", "1-1", &[("mid", &["leaf"], &[])], &[])),
                ("leaf", srcinfo("leaf", "1-1", &[("leaf", &[], &[])], &[])),
            ],
            None,
            &[],
        );
        let files = env.build(&[item("top"), item("mid"), item("leaf")]).unwrap();
        assert_eq!(files.len(), 3);
        let makes: Vec<String> = env
            .argvs()
            .into_iter()
            .filter(|l| l.starts_with("makechrootpkg"))
            .collect();
        let i = |b: &str| format!("-I T/cache/.pkgdest/{b}/{b}-1-1-any.pkg.tar.zst");
        assert_eq!(
            makes,
            [
                "makechrootpkg -c -r T/chroot -U archix-build".to_string(),
                format!("makechrootpkg -c -r T/chroot -U archix-build {}", i("leaf")),
                format!(
                    "makechrootpkg -c -r T/chroot -U archix-build {} {}",
                    i("leaf"),
                    i("mid")
                ),
            ]
        );
    }

    #[test]
    fn a_failed_build_stops_everything_naming_the_package_and_its_log() {
        let env = Env::new(
            &[
                ("app", srcinfo("app", "1-1", &[("app", &["lib"], &[])], &[])),
                ("lib", srcinfo("lib", "1-1", &[("lib", &[], &[])], &[])),
            ],
            Some("lib"),
            &[],
        );
        let err = format!("{:#}", env.build(&[item("app"), item("lib")]).unwrap_err());
        let log = env.tmp.path().join("logs/lib.log");
        assert!(err.contains("building the AUR package lib failed"), "{err}");
        assert!(err.contains(&format!("its log is {}", log.display())), "{err}");
        assert!(fs::read_to_string(&log).unwrap().contains("ERROR"));
        let makes = env
            .argvs()
            .into_iter()
            .filter(|l| l.starts_with("makechrootpkg"))
            .count();
        assert_eq!(makes, 1, "app isn't built after lib failed");
    }

    #[test]
    fn a_vcs_package_takes_the_version_pkgver_gave() {
        let env = Env::new(
            &[("x-git", srcinfo("x-git", "0-1", &[("x-git", &[], &[])], &[]))],
            None,
            &[("x-git", "0.r12.gdeadbee-1")],
        );
        let files = env.build(&[item("x-git")]).unwrap();
        assert!(
            files[0].ends_with("x-git-0.r12.gdeadbee-1-any.pkg.tar.zst"),
            "{files:?}"
        );
    }

    #[test]
    fn an_item_needs_a_pinned_commit_and_a_package_its_base_builds() {
        let env = Env::new(&[("lib", srcinfo("lib", "1-1", &[("lib", &[], &[])], &[]))], None, &[]);
        let mut unpinned = item("lib");
        unpinned.commit = None;
        assert!(
            env.build(&[unpinned])
                .unwrap_err()
                .to_string()
                .contains("lib has no pinned AUR commit")
        );
        let mut other = item("lib-extra");
        other.base = Some("lib".into());
        let err = env.build(&[other]).unwrap_err().to_string();
        assert!(err.contains("doesn't build lib-extra"), "{err}");
        assert!(env.build(&[]).unwrap().is_empty());
    }
}
