use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::git::AurGit;
use super::srcinfo::SrcInfo;
use crate::apply::TRANSACTION_ENV;
use crate::archive::Archive;
use crate::date::Date;
use crate::fetch::{Wanted, fetch_all};
use crate::pacman::read_sync_db;
use crate::paths::Paths;
use crate::repos::{Repo, is_official};
use crate::resolve::{pacman_conf, print_targets, snapshot_conf};

/// The unprivileged user AUR packages are built as; makepkg refuses to run as root.
pub const BUILD_USER: &str = "archix-build";

pub struct Builder<'a> {
    pub paths: &'a Paths,
    pub archive: &'a Archive,
    pub git: &'a AurGit,
    pub snapshot_cache: &'a Path,
}

/// One AUR base to build at a commit, against a snapshot, with these already-built AUR packages installed first.
pub struct Job<'a> {
    pub base: &'a str,
    /// The config's own recipe directory; None exports the AUR commit.
    pub dir: Option<&'a Path>,
    pub commit: &'a str,
    pub info: &'a SrcInfo,
    pub date: Date,
    pub repos: &'a [String],
    pub inputs: &'a [PathBuf],
    /// The dependencies (build and of `inputs`) that come from the repos rather than the AUR.
    pub repo_deps: &'a [String],
    pub on_host: bool,
}

/// The files one build produced, so the same commit and snapshot never needs building twice.
#[derive(Serialize, Deserialize)]
struct Record {
    files: Vec<(String, String)>,
}

impl Builder<'_> {
    fn chroot(&self) -> PathBuf {
        self.paths.state.join("chroot")
    }

    fn record(&self, base: &str, commit: &str, date: Date) -> PathBuf {
        self.paths
            .cache
            .join("aur-builds")
            .join(base)
            .join(format!("{commit}-{date}.json"))
    }

    /// The built package file for every package of `base`, from the build cache or a fresh build.
    pub fn build(&self, job: Job) -> Result<Vec<(String, PathBuf)>> {
        let Job {
            base,
            dir: recipe,
            commit,
            info,
            date,
            repos,
            inputs,
            repo_deps,
            on_host,
        } = job;
        let record = self.record(base, commit, date);
        if let Ok(text) = fs::read_to_string(&record)
            && let Ok(record) = serde_json::from_str::<Record>(&text)
        {
            let files: Vec<(String, PathBuf)> = record
                .files
                .into_iter()
                .map(|(name, file)| (name, self.paths.packages().join(file)))
                .collect();
            if files.iter().all(|(_, path)| path.is_file()) {
                return Ok(files);
            }
        }

        ensure_user()?;
        let dir = self
            .paths
            .cache
            .join("aur-build")
            .join(format!("{}-{}", base.replace(':', "-"), std::process::id()));
        if dir.exists() {
            fs::remove_dir_all(&dir)?;
        }
        match recipe {
            Some(recipe) => crate::recipes::copy_dir(recipe, &dir)?,
            None => self.git.export(base, commit, &dir)?,
        }
        chown(&dir)?;
        eprintln!(
            ":: building {base} {} ({})",
            info.version,
            if on_host { "on the host" } else { "in a clean chroot" }
        );
        let setup = self
            .import_keys(&info.validpgpkeys)
            .and_then(|()| {
                if on_host {
                    Ok(())
                } else {
                    self.ensure_chroot(date, repos)
                        .and_then(|()| self.prefetch(date, repos, repo_deps))
                }
            })
            .map_err(|e| anyhow::Error::new(SetupFailed(format!("{e:#}"))));
        let built = setup.and_then(|()| {
            if on_host {
                self.build_on_host(&dir, info, date, repos, inputs)
            } else {
                self.build_in_chroot(&dir, inputs)
            }
        });
        let result = built.and_then(|()| {
            fs::create_dir_all(self.paths.packages())?;
            let mut files = Vec::new();
            for pkg in &info.packages {
                let expected = info.file(pkg);
                // A VCS package (-git and the like) sets its version while building, so the file can differ from .SRCINFO's.
                let Some(produced) = Some(dir.join(&expected))
                    .filter(|path| path.is_file())
                    .or_else(|| find_built(&dir, &pkg.name))
                else {
                    bail!("building {base} did not produce {expected}");
                };
                let file = produced.file_name().unwrap_or_default().to_string_lossy().into_owned();
                // A changed local recipe can rebuild the same version, so its file name carries the recipe hash.
                let file = match recipe {
                    Some(_) => format!("{}-{file}", &crate::fetch::sha256_bytes(commit.as_bytes())[..12]),
                    None => file,
                };
                let dest = self.paths.packages().join(&file);
                fs::rename(&produced, &dest).or_else(|_| fs::copy(&produced, &dest).map(|_| ()))?;
                files.push((pkg.name.clone(), file));
            }
            fs::create_dir_all(record.parent().expect("record has a parent"))?;
            fs::write(&record, serde_json::to_string(&Record { files: files.clone() })?)?;
            Ok(files
                .into_iter()
                .map(|(name, file)| (name, self.paths.packages().join(file)))
                .collect())
        });
        let _ = fs::remove_dir_all(&dir);
        result
    }

    /// Creates the chroot on first use and moves it to `date`'s snapshot when the lock needs another one.
    fn ensure_chroot(&self, date: Date, repos: &[String]) -> Result<()> {
        let chroot = self.chroot();
        let root = chroot.join("root");
        let marker = chroot.join("snapshot");
        let conf = chroot.join("pacman.conf");
        fs::create_dir_all(&chroot)?;
        fs::write(&conf, pacman_conf(self.archive, date, &official(repos)))?;
        if !root.join(".arch-chroot").exists() {
            if root.exists() {
                fs::remove_dir_all(&root)?;
            }
            eprintln!(":: creating the build chroot at the {date} snapshot");
            self.prefetch(date, repos, &["base-devel".to_string()])?;
            with_retries("creating the build chroot", || {
                if root.exists() {
                    fs::remove_dir_all(&root)?;
                }
                run(
                    devtools("mkarchroot").arg("-C").arg(&conf).arg(&root).arg("base-devel"),
                    "mkarchroot",
                )
            })?;
        } else if fs::read_to_string(&marker).ok().as_deref() != Some(&date.to_string()) {
            eprintln!(":: moving the build chroot to the {date} snapshot");
            let installed = crate::pacman::read_local_db(&root.join("var/lib/pacman/local")).unwrap_or_default();
            self.prefetch(date, repos, &installed.into_keys().collect::<Vec<_>>())?;
            with_retries("moving the build chroot", || {
                run(
                    devtools("arch-nspawn")
                        .arg("-C")
                        .arg(&conf)
                        .arg(&root)
                        .args(["pacman", "-Syyuu", "--noconfirm"]),
                    "arch-nspawn",
                )
            })?;
        }
        fs::write(&marker, date.to_string())?;
        Ok(())
    }

    /// Downloads what mkarchroot or makepkg will install in the chroot into pacman's cache, which devtools uses, so a slow archive connection gets archix's retries.
    fn prefetch(&self, date: Date, repos: &[String], deps: &[String]) -> Result<()> {
        if deps.is_empty() {
            return Ok(());
        }
        let (conf, db) = snapshot_conf(self.archive, self.snapshot_cache, date, &official(repos))?;
        // A dependency the snapshot can't satisfy is the recipe's problem, which makepkg reports in the build itself.
        let Ok(targets) = print_targets(&conf, &db, deps, false, &[]) else {
            return Ok(());
        };
        let installed =
            crate::pacman::read_local_db(&self.chroot().join("root/var/lib/pacman/local")).unwrap_or_default();
        let mut syncs = std::collections::BTreeMap::new();
        let mut wanted = Vec::new();
        for (name, version, repo) in targets {
            if installed.get(&name).is_some_and(|pkg| pkg.version == version) {
                continue;
            }
            if !syncs.contains_key(&repo) {
                syncs.insert(repo.clone(), read_sync_db(&db.join("sync").join(format!("{repo}.db")))?);
            }
            let pkg = &syncs[&repo][&name];
            wanted.push(Wanted {
                name,
                file: pkg.filename.clone(),
                urls: vec![format!("{}/{}", self.archive.repo_url(date, &repo), pkg.filename)],
                sha256: Some(pkg.sha256.clone()),
            });
        }
        fetch_all(self.archive, &self.paths.pacman_cache, &[], &wanted).map(|_| ())
    }

    /// The keyring makepkg checks source signatures against, owned by the build user.
    fn keyring(&self) -> PathBuf {
        self.paths.state.join("aur-gnupg")
    }

    /// Fetches the recipe's `validpgpkeys` the keyring doesn't have yet, the way AUR helpers do.
    fn import_keys(&self, keys: &[String]) -> Result<()> {
        if keys.is_empty() {
            return Ok(());
        }
        let keyring = self.keyring();
        fs::create_dir_all(&keyring)?;
        fs::set_permissions(&keyring, fs::Permissions::from_mode(0o700))?;
        chown(&keyring)?;
        let gpg = |args: &[&str]| {
            let mut cmd = Command::new("runuser");
            cmd.args(["-u", BUILD_USER, "--", "env"])
                .arg(format!("GNUPGHOME={}", keyring.display()))
                .args(["gpg", "--batch", "--quiet"])
                .args(args);
            cmd
        };
        let missing: Vec<&String> = keys
            .iter()
            .filter(|key| !gpg(&["--list-keys", key]).output().is_ok_and(|o| o.status.success()))
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        eprintln!(
            ":: importing the source signing keys {}",
            missing.iter().map(|k| k.as_str()).collect::<Vec<_>>().join(" ")
        );
        let mut last = None;
        for server in ["hkps://keyserver.ubuntu.com", "hkps://keys.openpgp.org"] {
            let mut args = vec!["--keyserver", server, "--recv-keys"];
            args.extend(missing.iter().map(|k| k.as_str()));
            match run(&mut gpg(&args), "gpg --recv-keys") {
                Ok(()) => return Ok(()),
                Err(e) => last = Some(e),
            }
        }
        Err(last.expect("tried at least one keyserver"))
    }

    fn build_in_chroot(&self, dir: &Path, inputs: &[PathBuf]) -> Result<()> {
        let mut cmd = devtools("makechrootpkg");
        cmd.current_dir(dir)
            .env("GNUPGHOME", self.keyring())
            .arg("-c")
            .arg("-r")
            .arg(self.chroot())
            .args(["-U", BUILD_USER]);
        for input in inputs {
            cmd.arg("-I").arg(input);
        }
        run(&mut cmd, "makechrootpkg")
    }

    /// Installs missing build dependencies from the snapshot, runs makepkg as the build user, then removes them again.
    fn build_on_host(
        &self,
        dir: &Path,
        info: &SrcInfo,
        date: Date,
        repos: &[String],
        inputs: &[PathBuf],
    ) -> Result<()> {
        let mut deps: BTreeSet<String> = info.makedepends.iter().chain(&info.checkdepends).cloned().collect();
        deps.extend(info.packages.iter().flat_map(|pkg| pkg.depends.iter().cloned()));
        let output = Command::new("pacman")
            .arg("-T")
            .args(&deps)
            .output()
            .context("running pacman -T")?;
        let missing: Vec<String> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::to_string)
            .collect();

        let installed_before = crate::pacman::read_local_db(&self.paths.local_db())?;
        let mut files: Vec<PathBuf> = inputs.to_vec();
        if !missing.is_empty() {
            let (conf, db) = snapshot_conf(self.archive, self.snapshot_cache, date, &official(repos))?;
            let needed: Vec<(String, String, String)> = print_targets(&conf, &db, &missing, false, &[])?
                .into_iter()
                .filter(|(name, _, _)| !installed_before.contains_key(name))
                .collect();
            let mut wanted = Vec::new();
            for (name, _, repo) in &needed {
                let sync = read_sync_db(&db.join("sync").join(format!("{repo}.db")))?;
                let pkg = &sync[name];
                wanted.push(Wanted {
                    name: name.clone(),
                    file: pkg.filename.clone(),
                    urls: vec![format!("{}/{}", self.archive.repo_url(date, repo), pkg.filename)],
                    sha256: Some(pkg.sha256.clone()),
                });
            }
            files.extend(fetch_all(
                self.archive,
                &self.paths.packages(),
                std::slice::from_ref(&self.paths.pacman_cache),
                &wanted,
            )?);
        }
        if !files.is_empty() {
            let mut cmd = Command::new("pacman");
            cmd.args(["-U", "--noconfirm", "--needed", "--asdeps"])
                .args(&files)
                .env(TRANSACTION_ENV, "1");
            run(&mut cmd, "pacman -U")?;
        }
        let built = run(
            Command::new("runuser")
                .args(["-u", BUILD_USER, "--", "env"])
                .arg(format!("HOME={}", dir.display()))
                .arg(format!("GNUPGHOME={}", self.keyring().display()))
                .args(["makepkg", "--noconfirm", "--clean"])
                .current_dir(dir),
            "makepkg",
        );
        let installed_after = crate::pacman::read_local_db(&self.paths.local_db())?;
        let added: Vec<&String> = installed_after
            .keys()
            .filter(|name| !installed_before.contains_key(*name))
            .collect();
        if !added.is_empty() {
            let mut cmd = Command::new("pacman");
            cmd.args(["-R", "--noconfirm"]).args(&added).env(TRANSACTION_ENV, "1");
            run(&mut cmd, "pacman -R")?;
        }
        built
    }
}

/// The package file a build produced for `name`, whatever version it gave itself.
fn find_built(dir: &Path, name: &str) -> Option<PathBuf> {
    fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).find(|path| {
        path.file_name()
            .and_then(|f| f.to_str())
            .and_then(|f| f.strip_prefix(name)?.strip_prefix('-'))
            .and_then(|rest| rest.strip_suffix(".pkg.tar.zst"))
            .is_some_and(|rest| rest.split('-').count() == 3)
    })
}

/// The version in a built package's file name, past archix's recipe-hash prefix if there is one.
pub fn built_version(name: &str, file: &str) -> Option<String> {
    let file = match file.split_once('-') {
        Some((hash, rest))
            if hash.len() == 12 && hash.chars().all(|c| c.is_ascii_hexdigit()) && rest.starts_with(name) =>
        {
            rest
        }
        _ => file,
    };
    let rest = file
        .strip_prefix(name)?
        .strip_prefix('-')?
        .strip_suffix(".pkg.tar.zst")?;
    let mut parts = rest.rsplitn(3, '-');
    let (_arch, rel, ver) = (parts.next()?, parts.next()?, parts.next()?);
    (!ver.contains('-')).then(|| format!("{ver}-{rel}"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn versions_come_from_built_file_names() {
        assert_eq!(
            super::built_version("chatmixd-git", "chatmixd-git-0.2.0.r23.g5e00a5b-1-x86_64.pkg.tar.zst").as_deref(),
            Some("0.2.0.r23.g5e00a5b-1")
        );
        assert_eq!(
            super::built_version("tree", "0123456789ab-tree-1:2.3.2-1.1-x86_64.pkg.tar.zst").as_deref(),
            Some("1:2.3.2-1.1")
        );
        assert_eq!(
            super::built_version("tree", "tree-debug-2.3-1-x86_64.pkg.tar.zst"),
            None
        );
    }
}

/// The chroot only gets the official repos; custom repos would need their signing keys inside it too.
fn official(repos: &[String]) -> Vec<Repo> {
    repos
        .iter()
        .filter(|name| is_official(name))
        .map(|name| Repo::official(name))
        .collect()
}

fn devtools(program: &str) -> Command {
    let mut cmd = Command::new(program);
    if std::env::var_os("USER").is_none_or(|user| user.is_empty()) {
        cmd.env("USER", "root");
    }
    cmd
}

fn run(cmd: &mut Command, what: &str) -> Result<()> {
    let status = match cmd.status() {
        Ok(status) => status,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!("{what} not found; AUR builds need devtools and git (pacman -S devtools git)")
        }
        Err(e) => return Err(e).with_context(|| format!("running {what}")),
    };
    if !status.success() {
        bail!("{what} failed with {status}");
    }
    Ok(())
}

/// Creates the build user the first time a package is built.
pub(crate) fn ensure_user() -> Result<()> {
    let exists = Command::new("id")
        .arg("-u")
        .arg(BUILD_USER)
        .output()
        .is_ok_and(|o| o.status.success());
    if exists {
        return Ok(());
    }
    run(
        Command::new("useradd").args([
            "--system",
            "--no-create-home",
            "--shell",
            "/usr/bin/nologin",
            BUILD_USER,
        ]),
        "useradd",
    )
}

fn chown(dir: &Path) -> Result<()> {
    run(
        Command::new("chown").arg("-R").arg(format!("{BUILD_USER}:")).arg(dir),
        "chown",
    )
}

/// Preparing the build (the chroot, the signing keys) failed, which says nothing about the recipe, so no other version is offered.
#[derive(Debug)]
pub struct SetupFailed(pub String);

impl std::fmt::Display for SetupFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "couldn't prepare the AUR build (is the network up?): {}", self.0)
    }
}

impl std::error::Error for SetupFailed {}

/// Runs `attempt` up to three times, since chroot setup downloads from the archive and can hit a slow mirror.
fn with_retries(what: &str, mut attempt: impl FnMut() -> Result<()>) -> Result<()> {
    for wait in [5, 20] {
        match attempt() {
            Ok(()) => return Ok(()),
            Err(e) => {
                eprintln!(":: {what} failed ({e:#}); trying again in {wait} seconds");
                std::thread::sleep(std::time::Duration::from_secs(wait));
            }
        }
    }
    attempt()
}
