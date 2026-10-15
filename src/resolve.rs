use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::archive::{ARCH, Archive};
use crate::date::Date;
use crate::lock::{HostLock, Locked};
use crate::pacman::read_sync_db;

/// Resolves declared packages against one archive snapshot into the exact closure pacman would install.
pub fn resolve(archive: &Archive, cache: &Path, date: Date, repos: &[String], declared: &[String]) -> Result<HostLock> {
    let invalid: Vec<&String> = declared.iter().filter(|name| !valid_name(name)).collect();
    if !invalid.is_empty() {
        bail!("invalid package names: {invalid:?}");
    }
    let mut lock = HostLock {
        date,
        repos: repos.to_vec(),
        declared: declared.to_vec(),
        pinned: Vec::new(),
        packages: BTreeMap::new(),
    };
    if declared.is_empty() {
        return Ok(lock);
    }

    let db = archive.snapshot_db(cache, date, repos)?;
    let conf = db.join("pacman.conf");
    let mut text = format!("[options]\nArchitecture = {ARCH}\nSigLevel = Required DatabaseOptional\n");
    for repo in repos {
        let _ = write!(text, "\n[{repo}]\nServer = {}\n", archive.repo_url(date, repo));
    }
    fs::write(&conf, text).with_context(|| format!("writing {}", conf.display()))?;

    let closure = print_targets(&conf, &db, declared, false)?;
    let explicit: BTreeSet<String> = print_targets(&conf, &db, declared, true)?
        .into_iter()
        .map(|t| t.0)
        .collect();

    let mut sync = BTreeMap::new();
    for repo in repos {
        sync.insert(repo.clone(), read_sync_db(&db.join("sync").join(format!("{repo}.db")))?);
    }
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
        let explicit = explicit.contains(&name);
        let locked = Locked {
            version,
            repo,
            file: pkg.filename.clone(),
            sha256: pkg.sha256.clone(),
            explicit,
        };
        lock.packages.insert(name, locked);
    }
    Ok(lock)
}

/// Runs `pacman -Sp` against the snapshot db; with `targets_only`, dependencies are skipped (`-dd`).
fn print_targets(
    conf: &Path,
    db: &Path,
    declared: &[String],
    targets_only: bool,
) -> Result<Vec<(String, String, String)>> {
    let mut cmd = Command::new("pacman");
    cmd.arg("--config").arg(conf).arg("--dbpath").arg(db);
    cmd.args(["-Sp", "--noconfirm", "--print-format", "%n %v %r"]);
    if targets_only {
        cmd.args(["--nodeps", "--nodeps"]);
    }
    let output = match cmd.args(declared).output() {
        Ok(output) => output,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!("pacman not found; archix needs Arch Linux"),
        Err(e) => return Err(e).context("running pacman"),
    };
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
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
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
