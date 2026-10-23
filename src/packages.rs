use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::deploy::Transaction;
use crate::pacman::{LocalPkg, read_local_db};
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

/// Runs the package changes in a staging root whose pacman database is `db`, then checks the result; pacman sees the package files at the same paths, under /var.
pub fn transact(tx: &Transaction, packages: &Packages, db: &Path) -> Result<()> {
    for file in &packages.install {
        if !file.starts_with("/var/") {
            bail!("{} is outside /var, so the staging root can't reach it", file.display());
        }
    }
    let pacman = |args: &[&OsStr]| -> Result<()> {
        let status = tx
            .pacman()?
            .args(args)
            .env(TRANSACTION_ENV, "1")
            .status()
            .context("running pacman in the staging root")?;
        if !status.success() {
            bail!("pacman exited with {status} in the staging root");
        }
        Ok(())
    };
    let installed = read_local_db(db)?;
    let taken_over = removals_taken_over(db, packages, &installed)?;
    if !taken_over.is_empty() {
        let mut args: Vec<&OsStr> = vec![OsStr::new("-Rdd"), OsStr::new("--noconfirm")];
        args.extend(taken_over.iter().map(OsStr::new));
        pacman(&args)?;
    }
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
        args.extend(files.iter().map(|path| path.as_os_str()));
        pacman(&args)?;
    }
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
            "the staged deployment doesn't match the target:\n  {}",
            problems.join("\n  ")
        );
    }
    Ok(())
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
        fs::set_permissions(&gnupg, fs::Permissions::from_mode(0o700))?;
        fs::create_dir_all(&import)?;
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
            for file in fs::read_dir(trial.0.join(crate::pacman::KEYRINGS))?.flatten() {
                let name = file.file_name().to_string_lossy().into_owned();
                if let Some(stem) = name.strip_suffix(".gpg") {
                    names.push(stem.to_string());
                }
                fs::rename(file.path(), import.join(&name))?;
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
    fs::create_dir_all(&dir)?;
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

/// Packages the changes remove that own a file an incoming package ships; pacman would refuse the install while they still hold it, so they go first.
fn removals_taken_over(db: &Path, packages: &Packages, installed: &BTreeMap<String, LocalPkg>) -> Result<Vec<String>> {
    let removing: Vec<&String> = packages
        .remove
        .iter()
        .filter(|name| installed.contains_key(*name))
        .collect();
    if removing.is_empty() {
        return Ok(Vec::new());
    }
    let mut incoming = std::collections::HashSet::new();
    for path in &packages.install {
        incoming.extend(crate::conflicts::contents(path)?.files);
    }
    let owners = crate::conflicts::owners(db);
    Ok(removing
        .into_iter()
        .filter(|name| {
            owners
                .get(name.as_str())
                .is_some_and(|files| files.iter().any(|file| incoming.contains(file)))
        })
        .cloned()
        .collect())
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
