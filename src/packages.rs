use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::pacman::read_local_db;
use crate::paths::Paths;

/// Set for every pacman run archix starts, so archix's own pacman hook can tell them apart from manual ones.
pub const TRANSACTION_ENV: &str = "ARCHIX_TRANSACTION";
/// pacman's `--ask` bit that answers "remove the conflicting package?" with yes (ALPM_QUESTION_CONFLICT_PKG).
const ASK_REMOVE_CONFLICTS: &str = "--ask=4";

/// The package changes of one switch or rollback, prepared before anything is staged.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Packages {
    pub install: Vec<PathBuf>,
    pub remove: Vec<String>,
    /// Version every touched or kept package must have afterwards.
    pub versions: BTreeMap<String, String>,
    /// Whether each package must end up explicitly installed (true) or as a dependency.
    pub reasons: BTreeMap<String, bool>,
    /// Some install is a rebuild at the installed version, so pacman mustn't skip up-to-date packages.
    pub reinstall: bool,
}

#[derive(Debug, PartialEq)]
pub enum PacmanLock {
    Free,
    Running,
    Stale,
}

pub fn pacman_lock(paths: &Paths) -> PacmanLock {
    let lock = paths.pacman_lock();
    if !lock.exists() {
        PacmanLock::Free
    } else if held_open(&lock) {
        PacmanLock::Running
    } else {
        PacmanLock::Stale
    }
}

/// Whether `path` was last changed before this boot began, so no process running now left it.
pub fn from_an_earlier_boot(path: &Path) -> bool {
    let boot = fs::read_to_string("/proc/stat").ok().and_then(|stat| {
        stat.lines()
            .find_map(|line| line.strip_prefix("btime "))
            .and_then(|seconds| seconds.trim().parse::<u64>().ok())
    });
    let changed = fs::symlink_metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok());
    matches!((boot, changed), (Some(boot), Some(changed)) if changed.as_secs() < boot)
}

/// Whether any process has `path` open: libalpm keeps db.lck open while it holds the lock, whichever front end (pacman, pamac, PackageKit) took it.
fn held_open(path: &Path) -> bool {
    let Ok(processes) = fs::read_dir("/proc") else {
        return true;
    };
    processes.flatten().any(|process| {
        fs::read_dir(process.path().join("fd"))
            .into_iter()
            .flatten()
            .flatten()
            .any(|fd| fs::read_link(fd.path()).is_ok_and(|target| target == path))
    })
}

/// Something that runs pacman against the root a transaction changes.
pub trait RunsPacman {
    fn pacman(&self) -> Result<Command>;
    /// The place pacman runs in, as errors name it.
    fn place(&self) -> String;
    /// The root pacman changes.
    fn root(&self) -> PathBuf;
    /// The pacman.conf that pacman reads.
    fn conf(&self) -> PathBuf {
        self.root().join("etc/pacman.conf")
    }
}

/// Runs the package changes against the root `runner` stands for, whose pacman database is `db`, then checks the result.
pub fn transact(runner: &dyn RunsPacman, packages: &Packages, db: &Path) -> Result<()> {
    let pacman = |args: &[&OsStr]| -> Result<()> {
        let status = runner
            .pacman()?
            .args(args)
            .env(TRANSACTION_ENV, "1")
            .status()
            .with_context(|| format!("running pacman in {}", runner.place()))?;
        if !status.success() {
            bail!("pacman exited with {status} in {}", runner.place());
        }
        Ok(())
    };
    let displaced = Displaced::find(db, packages)?;
    let installed = read_local_db(db)?;
    let leaving: Vec<&OsStr> = displaced
        .packages
        .iter()
        .filter(|package| installed.contains_key(&package.name))
        .map(|package| OsStr::new(&package.name))
        .collect();
    if !leaving.is_empty() {
        // On disk first: a transaction that stops after this leaves files no package owns, which the next one must know for its own.
        displaced.record(db)?;
        let conf = KeepFilesConf::new(&runner.conf())?;
        let mut args: Vec<&OsStr> = vec![OsStr::new("--config"), conf.0.as_os_str()];
        args.extend(["-Rdd", "--noconfirm"].map(OsStr::new));
        args.extend(leaving);
        pacman(&args)?;
    }
    let overwrite = displaced.overwrite_globs();
    let keyrings = incoming_keyrings(&packages.install);
    let rest: Vec<&PathBuf> = packages
        .install
        .iter()
        .filter(|path| !keyrings.contains(path))
        .collect();
    // Keyrings first, so pacman checks the rest against the keys they add; -dd as the lock is a complete set.
    for (files, conflicts) in [(keyrings.iter().collect::<Vec<_>>(), false), (rest, true)] {
        if files.is_empty() {
            continue;
        }
        let mut args: Vec<&OsStr> = ["-U", "--noconfirm", "--nodeps", "--nodeps"].map(OsStr::new).to_vec();
        if conflicts {
            args.push(OsStr::new(ASK_REMOVE_CONFLICTS));
        }
        if !packages.reinstall {
            args.push(OsStr::new("--needed"));
        }
        for glob in &overwrite {
            args.push(OsStr::new("--overwrite"));
            args.push(OsStr::new(glob));
        }
        args.extend(files.iter().map(|path| path.as_os_str()));
        pacman(&args)?;
    }
    displaced.remove_leftovers(db, &runner.root())?;
    let installed = read_local_db(db)?;
    let remove: Vec<&OsStr> = packages
        .remove
        .iter()
        .filter(|name| installed.contains_key(*name))
        .map(OsStr::new)
        .collect();
    if !remove.is_empty() {
        let mut args = vec![OsStr::new("-Rdd"), OsStr::new("--noconfirm")];
        args.extend(remove);
        pacman(&args)?;
    }
    let installed = read_local_db(db)?;
    for (flag, explicit) in [("--asexplicit", true), ("--asdeps", false)] {
        let names: Vec<&OsStr> = packages
            .reasons
            .iter()
            .filter(|(name, want)| **want == explicit && installed.get(*name).is_some_and(|p| p.explicit != explicit))
            .map(|(name, _)| OsStr::new(name))
            .collect();
        if !names.is_empty() {
            let mut args = vec![OsStr::new("-D"), OsStr::new(flag)];
            args.extend(names);
            pacman(&args)?;
        }
    }
    let problems = mismatches(db, packages)?;
    if !problems.is_empty() {
        bail!(
            "after pacman, the installed packages don't match the target:\n  {}",
            problems.join("\n  ")
        );
    }
    Ok(())
}

/// An installed package an incoming one displaces, with what pacman's database says it owns.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct DisplacedPackage {
    name: String,
    /// Files, links and directories (ending in /), in the database's order.
    entries: Vec<String>,
    /// Its config files, each with the md5 it was installed with.
    backups: BTreeMap<String, String>,
}

/// The installed packages that go because incoming ones take their place: pacman would delete their files before the new ones are in, so they leave the database first with every file kept, the new packages overwrite what they share, and only then does the rest go.
struct Displaced {
    packages: Vec<DisplacedPackage>,
    /// Every file and link the incoming packages ship.
    incoming: std::collections::HashSet<String>,
    /// Where each symlink the incoming packages ship points.
    links: BTreeMap<String, String>,
}

impl Displaced {
    fn find(db: &Path, packages: &Packages) -> Result<Displaced> {
        let mut none = Displaced {
            packages: Vec::new(),
            incoming: std::collections::HashSet::new(),
            links: BTreeMap::new(),
        };
        let mut names = std::collections::HashSet::new();
        let mut displaces = std::collections::HashSet::new();
        for path in &packages.install {
            let contents = crate::conflicts::contents(path)?;
            names.insert(contents.name);
            displaces.extend(contents.displaces);
            none.incoming.extend(contents.files);
            none.links.extend(contents.links);
        }
        for entry in fs::read_dir(db).into_iter().flatten().flatten() {
            let desc = crate::pacman::parse_desc(&fs::read_to_string(entry.path().join("desc")).unwrap_or_default());
            let Some(name) = desc.get("NAME").and_then(|n| n.first()).cloned() else {
                continue;
            };
            // An installed version of an incoming package is an upgrade, which pacman does file by file itself.
            if names.contains(&name) {
                continue;
            }
            let conflicts = displaces.contains(&name)
                || ["CONFLICTS", "REPLACES"]
                    .iter()
                    .flat_map(|key| desc.get(*key).into_iter().flatten())
                    .any(|dep| names.contains(crate::aur::srcinfo::dep_name(dep)));
            if !conflicts && !packages.remove.contains(&name) {
                continue;
            }
            let mut files =
                crate::pacman::parse_desc(&fs::read_to_string(entry.path().join("files")).unwrap_or_default());
            let entries = files.remove("FILES").unwrap_or_default();
            if !entries.iter().any(|file| none.incoming.contains(file)) {
                continue;
            }
            let backups = files
                .remove("BACKUP")
                .unwrap_or_default()
                .iter()
                .filter_map(|line| line.split_once('\t'))
                .map(|(file, md5)| (file.to_string(), md5.to_string()))
                .collect();
            none.packages.push(DisplacedPackage { name, entries, backups });
        }
        // What a transaction that stopped part way already took out of the database, whose files are still there.
        for package in pending_displaced(db) {
            if !none.packages.iter().any(|found| found.name == package.name) {
                none.packages.push(package);
            }
        }
        none.packages.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(none)
    }

    fn record(&self, db: &Path) -> Result<()> {
        crate::durable::write(&displaced_record(db), serde_json::to_string(&self.packages)?.as_bytes())
    }

    /// pacman --overwrite globs for the files the incoming packages take over, each matching only its file.
    fn overwrite_globs(&self) -> Vec<String> {
        let globs: Vec<String> = self
            .packages
            .iter()
            .flat_map(|package| &package.entries)
            .filter(|file| self.incoming.contains(*file))
            .map(|file| glob_escape(file))
            .collect();
        // Past what one command line holds, everything may be overwritten: archix's own conflict check has passed by now.
        match globs.iter().map(|glob| glob.len() + 16).sum::<usize>() > 1 << 20 {
            true => vec!["*".to_string()],
            false => globs,
        }
    }

    /// Once the new packages are in: deletes what the displaced ones owned that no installed package owns now, as their removal would have, and puts a new package's version of a shared config file in place.
    fn remove_leftovers(&self, db: &Path, root: &Path) -> Result<()> {
        if self.packages.is_empty() {
            return Ok(());
        }
        let installed = read_local_db(db)?;
        let mut owned = std::collections::HashSet::new();
        for entry in fs::read_dir(db).into_iter().flatten().flatten() {
            let files = crate::pacman::parse_desc(&fs::read_to_string(entry.path().join("files")).unwrap_or_default());
            owned.extend(files.get("FILES").into_iter().flatten().cloned());
        }
        let gone = |result: std::io::Result<()>, path: &Path| match result {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(e).with_context(|| format!("removing {}", path.display()))
            }
            _ => Ok(()),
        };
        let leaving: Vec<&DisplacedPackage> = self
            .packages
            .iter()
            .filter(|package| !installed.contains_key(&package.name))
            .collect();
        // pacman's ldconfig may have pointed a taken-over link at a library of the old package, which goes below; each is put back, in one rename, to where its new package has it.
        for entry in leaving.iter().flat_map(|package| &package.entries) {
            let Some(target) = self.links.get(entry) else {
                continue;
            };
            let path = root.join(entry);
            if fs::read_link(&path).is_ok_and(|now| now == Path::new(target)) {
                continue;
            }
            let temporary = path.with_file_name(format!(
                ".{}.archix-link",
                path.file_name().unwrap_or_default().to_string_lossy()
            ));
            let _ = fs::remove_file(&temporary);
            std::os::unix::fs::symlink(target, &temporary)
                .and_then(|()| fs::rename(&temporary, &path))
                .with_context(|| format!("pointing {} at {target}", path.display()))?;
        }
        let mut libraries_gone = false;
        for package in leaving {
            for entry in package.entries.iter().rev() {
                let path = root.join(entry);
                let modified = || {
                    package
                        .backups
                        .get(entry)
                        .is_some_and(|md5| fs::read(&path).is_ok_and(|bytes| crate::capture::md5_bytes(&bytes) != *md5))
                };
                if entry.ends_with('/') {
                    // Like pacman, which leaves a directory that still holds something or that another package owns.
                    if !owned.contains(entry) && fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_dir()) {
                        let _ = fs::remove_dir(&path);
                    }
                } else if !owned.contains(entry) {
                    libraries_gone |= entry.contains(".so");
                    match modified() {
                        true => gone(fs::rename(&path, pacsave(&path)), &path)?,
                        false => gone(fs::remove_file(&path), &path)?,
                    }
                } else if package.backups.contains_key(entry) {
                    // pacman left the new owner's version beside the old package's file, as it does for a config file it finds in the way.
                    let new = path.with_file_name(format!(
                        "{}.pacnew",
                        path.file_name().unwrap_or_default().to_string_lossy()
                    ));
                    if fs::symlink_metadata(&new).is_ok() {
                        if modified() {
                            fs::copy(&path, pacsave(&path)).with_context(|| format!("keeping {}", path.display()))?;
                        }
                        fs::rename(&new, &path).with_context(|| format!("moving {} into place", new.display()))?;
                    }
                }
            }
        }
        // ldconfig is static, and links nothing ships may still name a library that went.
        if libraries_gone && root.join("etc/ld.so.conf").exists() {
            let mut ldconfig = Command::new("ldconfig");
            if root != Path::new("/") {
                ldconfig.arg("-r").arg(root);
            }
            if !ldconfig.status().is_ok_and(|status| status.success()) {
                eprintln!("warning: ldconfig failed after removing the files of packages that were replaced");
            }
        }
        match fs::remove_file(displaced_record(db)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(e).context("removing the record of displaced packages")
            }
            _ => Ok(()),
        }
    }
}

/// Finishes what a transaction a cut stopped after its packages were in left undone: the displaced packages' leftover files go, as when it runs to the end; with nothing pending, does nothing.
pub fn finish_displaced(runner: &dyn RunsPacman, db: &Path) -> Result<()> {
    let packages = pending_displaced(db);
    if packages.is_empty() {
        return match fs::remove_file(displaced_record(db)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(e).context("removing the record of displaced packages")
            }
            _ => Ok(()),
        };
    }
    let mut links = BTreeMap::new();
    for entry in fs::read_dir(db).into_iter().flatten().flatten() {
        let mut text = String::new();
        if fs::File::open(entry.path().join("mtree"))
            .and_then(|file| std::io::Read::read_to_string(&mut flate2::read::GzDecoder::new(file), &mut text))
            .is_ok()
        {
            links.extend(crate::conflicts::mtree_links(&text));
        }
    }
    Displaced {
        packages,
        incoming: std::collections::HashSet::new(),
        links,
    }
    .remove_leftovers(db, &runner.root())
}

/// Beside pacman's database, so a snapshot or rollback takes both or neither.
fn displaced_record(db: &Path) -> PathBuf {
    db.with_file_name("archix-displaced.json")
}

/// Packages a transaction that stopped part way took out of the database with their files left for the new ones, unless they're installed again.
fn pending_displaced(db: &Path) -> Vec<DisplacedPackage> {
    let recorded: Vec<DisplacedPackage> = fs::read_to_string(displaced_record(db))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    if recorded.is_empty() {
        return recorded;
    }
    let installed = read_local_db(db).unwrap_or_default();
    recorded
        .into_iter()
        .filter(|package| !installed.contains_key(&package.name))
        .collect()
}

/// The files such packages left, which the next transaction's packages may overwrite.
pub fn pending_displaced_files(db: &Path) -> Vec<String> {
    pending_displaced(db)
        .into_iter()
        .flat_map(|package| package.entries)
        .filter(|entry| !entry.ends_with('/'))
        .collect()
}

fn pacsave(path: &Path) -> PathBuf {
    path.with_file_name(format!(
        "{}.pacsave",
        path.file_name().unwrap_or_default().to_string_lossy()
    ))
}

/// A copy of a pacman.conf under which a removal takes packages out of the database, with their scriptlets and hooks, and deletes none of their files (NoUpgrade also holds for removals); removed when dropped.
struct KeepFilesConf(PathBuf);

impl KeepFilesConf {
    fn new(conf: &Path) -> Result<KeepFilesConf> {
        let text = fs::read_to_string(conf).with_context(|| format!("reading {}", conf.display()))?;
        let dir = std::env::temp_dir();
        for attempt in 0..100 {
            let path = dir.join(format!("archix-pacman-{}-{attempt}.conf", std::process::id()));
            let file = fs::OpenOptions::new().write(true).create_new(true).open(&path);
            match file {
                Ok(mut file) => {
                    use std::io::Write;
                    file.write_all(format!("[options]\nNoUpgrade = *\n{text}").as_bytes())
                        .with_context(|| format!("writing {}", path.display()))?;
                    return Ok(KeepFilesConf(path));
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e).with_context(|| format!("creating {}", path.display())),
            }
        }
        bail!("couldn't create a pacman.conf in {}", dir.display())
    }
}

impl Drop for KeepFilesConf {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Incoming packages that ship a pacman keyring; they're installed on their own first, so pacman checks the rest against the keys they add.
fn incoming_keyrings(files: &[PathBuf]) -> Vec<PathBuf> {
    files
        .iter()
        .filter(|file| {
            file.file_name()
                .is_some_and(|name| name.to_string_lossy().contains("-keyring-"))
        })
        .filter(|file| {
            crate::conflicts::contents(file).is_ok_and(|contents| {
                contents
                    .files
                    .iter()
                    .any(|path| path.starts_with(crate::pacman::KEYRINGS) && path.ends_with(".gpg"))
            })
        })
        .cloned()
        .collect()
}

/// A copy of pacman's keyring that also trusts what incoming keyring packages add, removed with any gpg-agent it started when dropped.
struct TrialKeyring(PathBuf);

impl TrialKeyring {
    fn new(keyrings: &[PathBuf]) -> Result<TrialKeyring> {
        let trial = TrialKeyring(std::env::temp_dir().join(format!("archix-keyring-{}", std::process::id())));
        let gnupg = trial.gnupg();
        let import = trial.0.join("keyrings");
        copy_regular(Path::new("/etc/pacman.d/gnupg"), &gnupg)?;
        fs::set_permissions(&gnupg, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("chmod {}", gnupg.display()))?;
        fs::create_dir_all(&import).with_context(|| format!("creating {}", import.display()))?;
        let mut names = Vec::new();
        for keyring in keyrings {
            let status = Command::new("bsdtar")
                .args(["-xf"])
                .arg(keyring)
                .arg("-C")
                .arg(&trial.0)
                .arg(crate::pacman::KEYRINGS)
                .status()
                .context("running bsdtar")?;
            if !status.success() {
                bail!("couldn't read the keyring in {}", keyring.display());
            }
            let unpacked = trial.0.join(crate::pacman::KEYRINGS);
            for file in fs::read_dir(&unpacked)
                .with_context(|| format!("reading {}", unpacked.display()))?
                .flatten()
            {
                let name = file.file_name().to_string_lossy().into_owned();
                if let Some(stem) = name.strip_suffix(".gpg") {
                    names.push(stem.to_string());
                }
                crate::durable::rename(file.path(), import.join(&name))
                    .with_context(|| format!("moving {} to {}", file.path().display(), import.display()))?;
            }
        }
        let output = Command::new("pacman-key")
            .arg("--gpgdir")
            .arg(&gnupg)
            .arg("--populate-from")
            .arg(&import)
            .arg("--populate")
            .args(&names)
            .output()
            .context("running pacman-key")?;
        if !output.status.success() {
            bail!(
                "pacman-key couldn't load the incoming keyrings: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(trial)
    }

    fn gnupg(&self) -> PathBuf {
        self.0.join("gnupg")
    }
}

impl Drop for TrialKeyring {
    fn drop(&mut self) {
        let _ = Command::new("gpgconf")
            .arg("--homedir")
            .arg(self.gnupg())
            .args(["--kill", "all"])
            .output();
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Copies a directory's files and subdirectories, leaving out sockets such as a running gpg-agent's.
fn copy_regular(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to).with_context(|| format!("creating {}", to.display()))?;
    for entry in fs::read_dir(from)
        .with_context(|| format!("reading {}", from.display()))?
        .flatten()
    {
        let kind = entry.file_type()?;
        let target = to.join(entry.file_name());
        if kind.is_dir() {
            copy_regular(&entry.path(), &target)?;
        } else if kind.is_file() {
            fs::copy(entry.path(), &target).with_context(|| format!("copying {}", entry.path().display()))?;
        }
    }
    Ok(())
}

/// Has pacman load every incoming package without installing it, so a bad signature or unknown key fails before anything is staged.
pub fn check_packages(packages: &Packages) -> Result<()> {
    if packages.install.is_empty() {
        return Ok(());
    }
    let keyrings = incoming_keyrings(&packages.install);
    let trial = match keyrings.is_empty() {
        true => None,
        false => TrialKeyring::new(&keyrings)
            .inspect_err(|e| eprintln!("warning: {e:#}; checking signatures against the installed keyring instead"))
            .ok(),
    };
    let mut pacman = Command::new("pacman");
    pacman.args([
        "-U",
        "--print",
        "--noconfirm",
        ASK_REMOVE_CONFLICTS,
        "--nodeps",
        "--nodeps",
    ]);
    if let Some(trial) = &trial {
        pacman.arg("--gpgdir").arg(trial.gnupg());
    }
    let output = pacman
        .args(&packages.install)
        .env(TRANSACTION_ENV, "1")
        .output()
        .context("running pacman")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let mut errors: Vec<String> = stderr
            .lines()
            .filter(|line| line.starts_with("error:"))
            .map(str::to_string)
            .collect();
        if errors.is_empty() {
            errors.push(stderr.trim().to_string());
        }
        let gnupg = trial
            .as_ref()
            .map_or_else(|| PathBuf::from("/etc/pacman.d/gnupg"), TrialKeyring::gnupg);
        errors.extend(packages.install.iter().filter_map(|file| signer_problem(file, &gnupg)));
        bail!(
            "pacman refuses these packages, so nothing was changed:\n  {}",
            errors.join("\n  ")
        );
    }
    Ok(())
}

/// What's wrong with the key that signed `file`, in words, since pacman says only "required key missing from keyring" for an expired one.
fn signer_problem(file: &Path, gnupg: &Path) -> Option<String> {
    let mut sig = file.as_os_str().to_os_string();
    sig.push(".sig");
    if !Path::new(&sig).is_file() {
        return None;
    }
    let output = Command::new("gpg")
        .arg("--homedir")
        .arg(gnupg)
        .args([
            "--batch",
            "--no-auto-key-retrieve",
            "--no-auto-check-trustdb",
            "--status-fd",
            "1",
            "--verify",
        ])
        .arg(&sig)
        .arg(file)
        .output()
        .ok()?;
    let name = file.file_name()?.to_string_lossy();
    signer_problem_in(&name, &String::from_utf8_lossy(&output.stdout))
}

fn signer_problem_in(name: &str, status: &str) -> Option<String> {
    let mut expired_at = None;
    for line in status.lines() {
        let Some(line) = line.strip_prefix("[GNUPG:] ") else {
            continue;
        };
        let (word, rest) = line.split_once(' ').unwrap_or((line, ""));
        let signer = || rest.split_once(' ').map_or(rest, |(_, uid)| uid).to_string();
        match word {
            "KEYEXPIRED" => expired_at = rest.trim().parse::<u64>().ok(),
            "EXPKEYSIG" => {
                let when = expired_at.map_or_else(
                    || "already".to_string(),
                    |at| crate::date::Date::from_unix(at).to_string(),
                );
                return Some(format!(
                    "{name} is signed by {}, whose key expired on {when}; a newer keyring package from its repo (archlinux-keyring for Arch's) renews it",
                    signer()
                ));
            }
            "REVKEYSIG" => return Some(format!("{name} is signed by {}, whose key was revoked", signer())),
            "BADSIG" => return Some(format!("{name} doesn't match its signature by {}", signer())),
            "NO_PUBKEY" => {
                return Some(format!(
                    "{name} is signed by key {}, which pacman's keyring doesn't have",
                    rest.trim()
                ));
            }
            _ => {}
        }
    }
    None
}

/// Hard links to package files without the .sig beside them, for a rollback: each was verified by pacman when first installed and is checked against its pinned sha256, while today's keyring may have revoked its packager since.
pub fn without_signatures(paths: &Paths, files: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let dir = paths.cache.join("verified");
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut out = Vec::new();
    for file in files {
        let has_sig = file
            .with_file_name(format!(
                "{}.sig",
                file.file_name().unwrap_or_default().to_string_lossy()
            ))
            .exists();
        if !has_sig {
            out.push(file.clone());
            continue;
        }
        let link = dir.join(file.file_name().unwrap_or_default());
        if !link.exists() && fs::hard_link(file, &link).is_err() {
            fs::copy(file, &link).with_context(|| format!("staging {}", file.display()))?;
        }
        out.push(link);
    }
    Ok(out)
}

/// `path` as a pacman --overwrite glob that matches only itself, or, with a comma, which separates globs, itself and its like.
fn glob_escape(path: &str) -> String {
    let mut escaped = String::with_capacity(path.len());
    for c in path.chars() {
        match c {
            ',' => escaped.push('?'),
            '*' | '?' | '[' | ']' | '\\' | '!' => {
                escaped.push('\\');
                escaped.push(c);
            }
            c => escaped.push(c),
        }
    }
    escaped
}

/// Every way the packages in database `db` differ from the changes' end state.
pub fn mismatches(db: &Path, packages: &Packages) -> Result<Vec<String>> {
    let installed = read_local_db(db)?;
    let keyrings = crate::pacman::keyring_packages(db);
    let mut problems = Vec::new();
    for (name, version) in &packages.versions {
        match installed.get(name) {
            None => problems.push(format!("{name} {version} is not installed")),
            Some(pkg)
                if keyrings.contains(name)
                    && crate::pacman::vercmp(&pkg.version, version) == std::cmp::Ordering::Greater => {}
            Some(pkg) if pkg.version != *version => {
                problems.push(format!("{name} is {} instead of {version}", pkg.version))
            }
            Some(_) => {}
        }
    }
    for name in &packages.remove {
        if installed.contains_key(name) {
            problems.push(format!("{name} is still installed"));
        }
    }
    for (name, explicit) in &packages.reasons {
        if installed.get(name).is_some_and(|pkg| pkg.explicit != *explicit) {
            let reason = if *explicit {
                "explicitly installed"
            } else {
                "a dependency"
            };
            problems.push(format!("{name} should be marked as {reason}"));
        }
    }
    Ok(problems)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpg_status_names_an_expired_or_revoked_signer() {
        let expired = "[GNUPG:] NEWSIG\n[GNUPG:] KEYEXPIRED 1792571807\n[GNUPG:] EXPKEYSIG A0052EA5F41EF1EE t <t@t>\n[GNUPG:] VALIDSIG E764\n";
        assert_eq!(
            signer_problem_in("x.pkg.tar.zst", expired).unwrap(),
            "x.pkg.tar.zst is signed by t <t@t>, whose key expired on 2026-10-21; a newer keyring package from its repo (archlinux-keyring for Arch's) renews it"
        );
        let revoked = "[GNUPG:] REVKEYSIG A0052EA5F41EF1EE t <t@t>\n";
        assert_eq!(
            signer_problem_in("x", revoked).unwrap(),
            "x is signed by t <t@t>, whose key was revoked"
        );
        assert_eq!(signer_problem_in("x", "[GNUPG:] GOODSIG A t <t@t>\n"), None);
    }

    /// A package file with only what `conflicts::contents` reads.
    fn package_file(dir: &Path, name: &str, conflicts: &[&str], files: &[&str]) -> PathBuf {
        use std::io::Write;
        let mut info = format!("pkgname = {name}\npkgver = 2-1\n");
        for conflict in conflicts {
            info.push_str(&format!("conflict = {conflict}\n"));
        }
        let mut mtree = String::from("#mtree\n/set type=file uid=0 gid=0 mode=644\n");
        for file in files {
            match file.split_once(" -> ") {
                Some((link, target)) => mtree.push_str(&format!("./{link} time=1 type=link link={target}\n")),
                None => mtree.push_str(&format!("./{file} time=1 size=1\n")),
            }
        }
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(mtree.as_bytes()).unwrap();
        let mtree = gz.finish().unwrap();
        let path = dir.join(format!("{name}-2-1-any.pkg.tar.zst"));
        let mut archive = tar::Builder::new(zstd::Encoder::new(fs::File::create(&path).unwrap(), 1).unwrap());
        for (name, bytes) in [(".PKGINFO", info.as_bytes()), (".MTREE", &mtree[..])] {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            archive.append_data(&mut header, name, bytes).unwrap();
        }
        archive.into_inner().unwrap().finish().unwrap();
        path
    }

    /// An installed package in a pacman database under `root`, its files written too.
    fn install(root: &Path, name: &str, conflicts: &[&str], files: &[&str], backups: &[(&str, &str)]) {
        let dir = root.join(format!("var/lib/pacman/local/{name}-1-1"));
        fs::create_dir_all(&dir).unwrap();
        let mut desc = format!("%NAME%\n{name}\n\n%VERSION%\n1-1\n\n");
        if !conflicts.is_empty() {
            desc.push_str(&format!("%CONFLICTS%\n{}\n\n", conflicts.join("\n")));
        }
        fs::write(dir.join("desc"), desc).unwrap();
        let mut list = format!("%FILES%\n{}\n\n", files.join("\n"));
        if !backups.is_empty() {
            let lines: Vec<String> = backups.iter().map(|(file, md5)| format!("{file}\t{md5}")).collect();
            list.push_str(&format!("%BACKUP%\n{}\n\n", lines.join("\n")));
        }
        fs::write(dir.join("files"), list).unwrap();
        for file in files {
            let path = root.join(file);
            match file.ends_with('/') {
                true => fs::create_dir_all(&path).unwrap(),
                false => {
                    fs::create_dir_all(path.parent().unwrap()).unwrap();
                    fs::write(&path, name).unwrap();
                }
            }
        }
    }

    #[test]
    fn packages_whose_files_an_incoming_one_takes_are_displaced_removed_or_in_conflict() {
        let root = tempfile::tempdir().unwrap();
        let db = root.path().join("var/lib/pacman/local");
        // As CachyOS's zlib-ng-compat, which conflicts with zlib and ships the same libz.so.1.
        install(
            root.path(),
            "zlib-ng-compat",
            &["zlib"],
            &["usr/", "usr/lib/", "usr/lib/libz.so.1"],
            &[],
        );
        install(
            root.path(),
            "renamed-old",
            &[],
            &["usr/bin/tool", "usr/share/odd,name"],
            &[],
        );
        install(root.path(), "zlib-doc", &["zlib"], &["usr/share/doc/zlib-ng"], &[]);
        install(root.path(), "removed-apart", &[], &["usr/bin/apart"], &[]);
        install(root.path(), "upgraded", &[], &["usr/bin/upgraded"], &[]);
        let install_files = vec![
            package_file(root.path(), "zlib", &[], &["usr/lib/libz.so.1", "usr/lib/libz.so.1.3"]),
            package_file(root.path(), "renamed-new", &[], &["usr/bin/tool", "usr/share/odd,name"]),
            package_file(root.path(), "upgraded", &[], &["usr/bin/upgraded"]),
        ];
        let changes = Packages {
            install: install_files,
            remove: ["renamed-old", "removed-apart"].map(str::to_string).to_vec(),
            ..Packages::default()
        };
        let displaced = Displaced::find(&db, &changes).unwrap();
        let names: Vec<&str> = displaced.packages.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            names,
            ["renamed-old", "zlib-ng-compat"],
            "one the config dropped and one pacman removes for its conflict; not what shares no file, nor an upgrade"
        );
        assert_eq!(
            displaced.overwrite_globs(),
            ["usr/bin/tool", "usr/share/odd?name", "usr/lib/libz.so.1"]
        );
        assert!(Displaced::find(&db, &Packages::default()).unwrap().packages.is_empty());
        // A transaction that stopped after they left the database: the next one still knows their files.
        displaced.record(&db).unwrap();
        fs::remove_dir_all(db.join("zlib-ng-compat-1-1")).unwrap();
        assert_eq!(pending_displaced_files(&db), ["usr/lib/libz.so.1"]);
        let again = Displaced::find(&db, &changes).unwrap();
        let names: Vec<&str> = again.packages.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["renamed-old", "zlib-ng-compat"]);
        assert!(again.overwrite_globs().contains(&"usr/lib/libz.so.1".to_string()));
    }

    #[test]
    fn what_a_displaced_package_leaves_goes_once_the_new_ones_are_in() {
        let root = tempfile::tempdir().unwrap();
        let db = root.path().join("var/lib/pacman/local");
        let installed_md5 = crate::capture::md5_bytes(b"old");
        install(
            root.path(),
            "old",
            &[],
            &[
                "etc/",
                "etc/old.conf",
                "etc/edited.conf",
                "etc/shared.conf",
                "usr/",
                "usr/lib/",
                "usr/lib/libshared.so",
                "usr/share/",
                "usr/share/old/",
                "usr/share/old/data",
                "usr/share/kept/",
                "usr/share/kept/data",
            ],
            &[
                ("etc/old.conf", &installed_md5),
                ("etc/edited.conf", &installed_md5),
                ("etc/shared.conf", &installed_md5),
            ],
        );
        let changes = Packages {
            install: vec![package_file(
                root.path(),
                "new",
                &[],
                &["usr/lib/libshared.so", "etc/shared.conf"],
            )],
            remove: vec!["old".to_string()],
            ..Packages::default()
        };
        let displaced = Displaced::find(&db, &changes).unwrap();
        assert_eq!(displaced.packages.len(), 1);
        // What pacman's runs leave: old out of the database with its files kept, new in, its config file beside the one in the way.
        fs::remove_dir_all(db.join("old-1-1")).unwrap();
        install(
            root.path(),
            "new",
            &[],
            &["usr/", "usr/lib/", "usr/lib/libshared.so", "etc/", "etc/shared.conf"],
            &[],
        );
        fs::write(root.path().join("etc/shared.conf"), "old").unwrap();
        fs::write(root.path().join("etc/shared.conf.pacnew"), "new").unwrap();
        fs::write(root.path().join("etc/edited.conf"), "mine").unwrap();
        fs::write(root.path().join("usr/share/kept/unowned"), "someone's").unwrap();
        displaced.remove_leftovers(&db, root.path()).unwrap();
        let at = |path: &str| root.path().join(path);
        assert_eq!(fs::read_to_string(at("usr/lib/libshared.so")).unwrap(), "new");
        assert!(
            !at("usr/share/old").exists(),
            "its own files and their directory are gone"
        );
        assert!(!at("usr/share/kept/data").exists());
        assert!(
            at("usr/share/kept/unowned").exists(),
            "a directory that still holds something stays"
        );
        assert!(!at("etc/old.conf").exists(), "an untouched config file goes");
        assert!(!at("etc/edited.conf").exists());
        assert_eq!(fs::read_to_string(at("etc/edited.conf.pacsave")).unwrap(), "mine");
        assert_eq!(
            fs::read_to_string(at("etc/shared.conf")).unwrap(),
            "new",
            "the new owner's config file replaces the old package's untouched one"
        );
        assert!(!at("etc/shared.conf.pacnew").exists());
        assert!(at("etc").is_dir() && at("usr/lib").is_dir());
    }

    #[test]
    fn a_link_ldconfig_pointed_at_the_old_packages_library_points_at_the_new_ones_before_that_goes() {
        let root = tempfile::tempdir().unwrap();
        let db = root.path().join("var/lib/pacman/local");
        let lib = root.path().join("usr/lib");
        install(
            root.path(),
            "zlib",
            &[],
            &["usr/", "usr/lib/", "usr/lib/libz.so.1", "usr/lib/libz.so.1.4"],
            &[],
        );
        let changes = Packages {
            install: vec![package_file(
                root.path(),
                "zlib-ng-compat",
                &["zlib"],
                &[
                    "usr/lib/libz.so.1 -> libz.so.1.3.1.zlib-ng",
                    "usr/lib/libz.so.1.3.1.zlib-ng",
                ],
            )],
            remove: vec!["zlib".to_string()],
            ..Packages::default()
        };
        let displaced = Displaced::find(&db, &changes).unwrap();
        // What pacman leaves, its ldconfig having picked the higher version, the old package's.
        fs::remove_dir_all(db.join("zlib-1-1")).unwrap();
        install(
            root.path(),
            "zlib-ng-compat",
            &[],
            &["usr/", "usr/lib/", "usr/lib/libz.so.1", "usr/lib/libz.so.1.3.1.zlib-ng"],
            &[],
        );
        fs::remove_file(lib.join("libz.so.1")).unwrap();
        std::os::unix::fs::symlink("libz.so.1.4", lib.join("libz.so.1")).unwrap();
        displaced.remove_leftovers(&db, root.path()).unwrap();
        assert!(!lib.join("libz.so.1.4").exists(), "the old package's library goes");
        assert_eq!(
            fs::read_link(lib.join("libz.so.1")).unwrap(),
            Path::new("libz.so.1.3.1.zlib-ng")
        );
        assert_eq!(fs::read_to_string(lib.join("libz.so.1")).unwrap(), "zlib-ng-compat");
    }

    struct At(PathBuf);

    impl RunsPacman for At {
        fn pacman(&self) -> Result<Command> {
            bail!("no pacman here")
        }
        fn place(&self) -> String {
            self.0.display().to_string()
        }
        fn root(&self) -> PathBuf {
            self.0.clone()
        }
    }

    #[test]
    fn a_switch_after_a_cut_that_stopped_a_swap_once_its_packages_were_in_deletes_what_the_old_one_left() {
        use std::io::Write;
        let root = tempfile::tempdir().unwrap();
        let db = root.path().join("var/lib/pacman/local");
        let lib = root.path().join("usr/lib");
        install(
            root.path(),
            "zlib-ng-compat",
            &[],
            &["usr/", "usr/lib/", "usr/lib/libz.so.1", "usr/lib/libz.so.1.3.1.zlib-ng"],
            &[],
        );
        let changes = Packages {
            install: vec![package_file(
                root.path(),
                "zlib",
                &[],
                &["usr/lib/libz.so.1 -> libz.so.1.4", "usr/lib/libz.so.1.4"],
            )],
            remove: vec!["zlib-ng-compat".to_string()],
            ..Packages::default()
        };
        Displaced::find(&db, &changes).unwrap().record(&db).unwrap();
        // The cut came after pacman put zlib in and its ldconfig pointed libz.so.1 at the old library.
        fs::remove_dir_all(db.join("zlib-ng-compat-1-1")).unwrap();
        install(
            root.path(),
            "zlib",
            &[],
            &["usr/", "usr/lib/", "usr/lib/libz.so.1", "usr/lib/libz.so.1.4"],
            &[],
        );
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(b"#mtree\n./usr/lib/libz.so.1 time=1 type=link link=libz.so.1.4\n")
            .unwrap();
        fs::write(db.join("zlib-1-1/mtree"), gz.finish().unwrap()).unwrap();
        fs::remove_file(lib.join("libz.so.1")).unwrap();
        std::os::unix::fs::symlink("libz.so.1.3.1.zlib-ng", lib.join("libz.so.1")).unwrap();
        assert!(!pending_displaced_files(&db).is_empty());
        finish_displaced(&At(root.path().to_path_buf()), &db).unwrap();
        assert!(
            !lib.join("libz.so.1.3.1.zlib-ng").exists(),
            "the old package's library goes"
        );
        assert_eq!(fs::read_link(lib.join("libz.so.1")).unwrap(), Path::new("libz.so.1.4"));
        assert!(!displaced_record(&db).exists(), "nothing is left pending");
        finish_displaced(&At(root.path().to_path_buf()), &db).unwrap();
    }

    #[test]
    fn a_pacman_lock_changed_before_this_boot_is_from_an_earlier_one() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("db.lck");
        fs::write(&lock, "").unwrap();
        assert!(
            !from_an_earlier_boot(&lock),
            "one made now may be a pacman's of this boot"
        );
        fs::File::options()
            .write(true)
            .open(&lock)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1))
            .unwrap();
        assert!(from_an_earlier_boot(&lock));
        assert!(!from_an_earlier_boot(&dir.path().join("missing")));
    }

    #[test]
    fn a_lock_counts_as_held_only_while_a_process_has_it_open() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("db.lck");
        let file = fs::File::create(&lock).unwrap();
        assert!(held_open(&lock));
        drop(file);
        // Another test forking can hold every fd of this process until its child execs.
        let stale = (0..100).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(10));
            !held_open(&lock)
        });
        assert!(stale, "a lock left by a killed pacman is stale");
    }
}
