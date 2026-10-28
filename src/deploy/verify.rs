use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::Store;
use crate::paths::Paths;
use crate::repack::{Kind, parse_mtree};

/// The trees ext4 deployments share through hardlinks, so a change through one path reaches the others; etc and pacman are copies.
pub const SHARED: &[&str] = &["usr/", "opt/", "boot/"];

/// One file of a deployment that differs from what its package installed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Problem {
    pub package: String,
    /// The file name pacman's cache holds the package under, without the extension.
    pub file: String,
    pub path: String,
    pub what: String,
    /// The mode, owner and group the package gives the file.
    #[serde(default)]
    pub mode: u32,
    #[serde(default)]
    pub uid: u64,
    #[serde(default)]
    pub gid: u64,
}

struct Package {
    name: String,
    file: String,
    backups: BTreeSet<String>,
    entries: Vec<crate::repack::Entry>,
}

fn packages(db: &Path) -> Result<Vec<Package>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(db).with_context(|| format!("reading {}", db.display()))? {
        let dir = entry.with_context(|| format!("reading {}", db.display()))?.path();
        let Ok(desc) = fs::read_to_string(dir.join("desc")) else {
            continue;
        };
        let desc = crate::pacman::parse_desc(&desc);
        let field = |key: &str| desc.get(key).and_then(|v| v.first()).cloned().unwrap_or_default();
        let files = crate::pacman::parse_desc(&fs::read_to_string(dir.join("files")).unwrap_or_default());
        let backups = files
            .get("BACKUP")
            .into_iter()
            .flatten()
            .filter_map(|line| line.split_once('\t').map(|(path, _)| path.to_string()))
            .collect();
        let mut mtree = String::new();
        if fs::File::open(dir.join("mtree"))
            .and_then(|file| flate2::read::GzDecoder::new(file).read_to_string(&mut mtree))
            .is_err()
        {
            continue;
        }
        out.push(Package {
            file: format!("{}-{}-{}", field("NAME"), field("VERSION"), field("ARCH")),
            name: field("NAME"),
            backups,
            entries: parse_mtree(&mtree),
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// What differs in one entry, or None when it matches its mtree line: type, mode, owner, and for files size and content, for symlinks the target.
fn differs(path: &Path, entry: &crate::repack::Entry) -> Result<Option<String>> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Some("missing".into())),
        Err(e) => return Err(e).with_context(|| format!("stat {}", path.display())),
    };
    let kind_ok = match &entry.kind {
        Kind::Dir => meta.is_dir(),
        Kind::Link(_) => meta.file_type().is_symlink(),
        Kind::File { .. } => meta.is_file(),
    };
    if !kind_ok {
        return Ok(Some("type".into()));
    }
    if let Kind::Link(target) = &entry.kind {
        return Ok((fs::read_link(path)? != Path::new(target)).then(|| "symlink target".into()));
    }
    if meta.mode() & 0o7777 != entry.mode & 0o7777 {
        return Ok(Some(format!(
            "mode {:o}, not {:o}",
            meta.mode() & 0o7777,
            entry.mode & 0o7777
        )));
    }
    if (meta.uid() as u64, meta.gid() as u64) != (entry.uid, entry.gid) {
        return Ok(Some(format!(
            "owner {}:{}, not {}:{}",
            meta.uid(),
            meta.gid(),
            entry.uid,
            entry.gid
        )));
    }
    if let Kind::File { size, sha256 } = &entry.kind {
        if meta.len() != *size {
            return Ok(Some("size".into()));
        }
        if let Some(sha256) = sha256
            && crate::fetch::sha256(path)? != *sha256
        {
            return Ok(Some("content".into()));
        }
    }
    Ok(None)
}

/// Files changed in place, which through a shared inode reach other deployments too; a missing file, as NoExtract leaves, only lacks in this one.
pub fn changed(problems: Vec<Problem>) -> (Vec<Problem>, usize) {
    let (missing, changed): (Vec<Problem>, Vec<Problem>) = problems.into_iter().partition(|p| p.what == "missing");
    (changed, missing.len())
}

/// Checks every file the packages of `deployment` installed under usr, opt and boot against their mtree, as `pacman -Qkk` does, backup files and /usr/local, which the boot covers with the shared one, aside.
pub fn check(deployment: &Path) -> Result<Vec<Problem>> {
    let mut problems = Vec::new();
    for package in packages(&deployment.join("pacman"))? {
        for entry in &package.entries {
            if !SHARED.iter().any(|tree| entry.path.starts_with(tree))
                || entry.path.starts_with("usr/local/")
                || package.backups.contains(&entry.path)
            {
                continue;
            }
            if let Some(what) = differs(&deployment.join(&entry.path), entry)? {
                problems.push(Problem {
                    package: package.name.clone(),
                    file: package.file.clone(),
                    path: entry.path.clone(),
                    what,
                    mode: entry.mode & 0o7777,
                    uid: entry.uid,
                    gid: entry.gid,
                });
            }
        }
    }
    Ok(problems)
}

/// The cached package file for `file` (name-version-arch) in archix's and pacman's caches.
fn cached(paths: &Paths, file: &str) -> Option<PathBuf> {
    let mut dirs = vec![paths.packages(), paths.pacman_cache.clone()];
    dirs.extend(
        fs::read_dir(paths.cache.join("aur-builds"))
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path()),
    );
    dirs.into_iter().find_map(|dir| {
        [".pkg.tar.zst", ".pkg.tar.xz", ".pkg.tar.gz", ".pkg.tar"]
            .iter()
            .map(|ext| dir.join(format!("{file}{ext}")))
            .find(|path| path.is_file())
    })
}

/// Puts back what `problems` lists in `deployment` from the cached packages, each file through a new name renamed over the old, so no other deployment sharing its inode changes; refuses before touching anything when a package isn't cached.
pub fn repair(paths: &Paths, deployment: &Path, problems: &[Problem]) -> Result<usize> {
    let mut by_file: BTreeMap<&str, Vec<&Problem>> = BTreeMap::new();
    for problem in problems {
        by_file.entry(&problem.file).or_default().push(problem);
    }
    let missing: Vec<&str> = by_file
        .keys()
        .copied()
        .filter(|file| cached(paths, file).is_none())
        .collect();
    if !missing.is_empty() {
        bail!(
            "{} isn't in the package cache, so it can't be repaired; download it into {} and try again",
            missing.join(", "),
            paths.packages().display()
        );
    }
    let mtrees: BTreeMap<String, Vec<crate::repack::Entry>> = packages(&deployment.join("pacman"))?
        .into_iter()
        .map(|package| (package.file, package.entries))
        .collect();
    let scratch = std::env::temp_dir().join(format!("archix-repair-{}", std::process::id()));
    let mut repaired = 0;
    for (file, problems) in by_file {
        let _ = fs::remove_dir_all(&scratch);
        fs::create_dir_all(&scratch)?;
        let package = cached(paths, file).expect("checked above");
        let mut extract = Command::new("bsdtar");
        extract
            .args(["-xpf"])
            .arg(&package)
            .args(["--numeric-owner", "--xattrs", "-C"])
            .arg(&scratch);
        for problem in &problems {
            extract.arg(&problem.path);
        }
        let output = extract.output().context("running bsdtar")?;
        if !output.status.success() {
            let _ = fs::remove_dir_all(&scratch);
            bail!(
                "extracting {} failed: {}",
                package.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let entries = mtrees.get(file).map(Vec::as_slice).unwrap_or_default();
        for problem in problems {
            let Some(entry) = entries.iter().find(|entry| entry.path == problem.path) else {
                continue;
            };
            put_back(&scratch.join(&problem.path), &deployment.join(&problem.path), entry)
                .with_context(|| format!("repairing {}", problem.path))?;
            repaired += 1;
        }
    }
    let _ = fs::remove_dir_all(&scratch);
    crate::durable::sync_filesystems_checked(&[deployment])?;
    Ok(repaired)
}

/// Puts back the file at `rel` under `deployment`'s etc as its package installed it, from the package cache, through a new name renamed over the old; returns the package's name.
pub fn restore_etc(paths: &Paths, deployment: &Path, rel: &str) -> Result<String> {
    let path = format!("etc/{}", rel.trim_start_matches('/'));
    let packages = packages(&deployment.join("pacman"))?;
    let Some((package, entry)) = packages
        .iter()
        .find_map(|package| Some((package, package.entries.iter().find(|entry| entry.path == path)?)))
    else {
        bail!("no package of {} installs /{path}", deployment.display());
    };
    let Some(file) = cached(paths, &package.file) else {
        bail!(
            "{} isn't in the package cache, so /{path} can't be restored; copy it into {}",
            package.file,
            paths.packages().display()
        );
    };
    let scratch = std::env::temp_dir().join(format!("archix-restore-{}", std::process::id()));
    let _ = fs::remove_dir_all(&scratch);
    fs::create_dir_all(&scratch)?;
    let output = Command::new("bsdtar")
        .args(["-xpf"])
        .arg(&file)
        .args(["--numeric-owner", "--xattrs", "-C"])
        .arg(&scratch)
        .arg(&path)
        .output()
        .context("running bsdtar")?;
    let restored = match output.status.success() {
        true => put_back(&scratch.join(&path), &deployment.join(&path), entry),
        false => Err(anyhow::anyhow!(
            "extracting /{path} from {} failed: {}",
            file.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        )),
    };
    let _ = fs::remove_dir_all(&scratch);
    restored?;
    crate::durable::sync_filesystems_checked(&[deployment])?;
    Ok(package.name.clone())
}

fn put_back(from: &Path, to: &Path, entry: &crate::repack::Entry) -> Result<()> {
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    let owner = |path: &Path| std::os::unix::fs::lchown(path, Some(entry.uid as u32), Some(entry.gid as u32));
    if entry.kind == Kind::Dir {
        if !to.is_dir() {
            if fs::symlink_metadata(to).is_ok() {
                fs::remove_file(to)?;
            }
            fs::create_dir(to)?;
        }
        owner(to)?;
        return Ok(fs::set_permissions(
            to,
            fs::Permissions::from_mode(entry.mode & 0o7777),
        )?);
    }
    let mut name = to.file_name().unwrap_or_default().to_os_string();
    name.push(".archix-repair");
    let tmp = to.with_file_name(name);
    let _ = fs::remove_file(&tmp);
    match &entry.kind {
        Kind::Link(target) => std::os::unix::fs::symlink(target, &tmp)?,
        _ => {
            fs::copy(from, &tmp)?;
            for name in super::xattr::list(from).unwrap_or_default() {
                if let Ok(Some(value)) = super::xattr::get(from, &name) {
                    super::xattr::set(&tmp, &name, &value)?;
                }
            }
        }
    }
    owner(&tmp)?;
    if !matches!(entry.kind, Kind::Link(_)) {
        fs::set_permissions(&tmp, fs::Permissions::from_mode(entry.mode & 0o7777))?;
        fs::File::open(&tmp)?.sync_all()?;
    }
    if fs::symlink_metadata(to).is_ok_and(|meta| meta.is_dir()) {
        fs::remove_dir_all(to)?;
    }
    fs::rename(&tmp, to)?;
    Ok(())
}

/// The last check of each deployment, which `archix status` shows.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Record {
    pub checked: BTreeMap<u32, Vec<Problem>>,
}

fn record_path(paths: &Paths) -> PathBuf {
    paths.state.join("verify.json")
}

pub fn read_record(paths: &Paths) -> Record {
    fs::read_to_string(record_path(paths))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

pub fn save(paths: &Paths, number: u32, problems: &[Problem]) {
    let mut record = read_record(paths);
    record.checked.insert(number, problems.to_vec());
    if let Ok(text) = serde_json::to_string_pretty(&record) {
        let _ = crate::durable::write(&record_path(paths), text.as_bytes());
    }
}

/// Describes problems in one line per file, at most `limit`.
pub fn describe(problems: &[Problem], limit: usize) -> String {
    let mut lines: Vec<String> = problems
        .iter()
        .take(limit)
        .map(|p| format!("  /{} ({}): {}", p.path, p.package, p.what))
        .collect();
    if problems.len() > limit {
        lines.push(format!("  and {} more", problems.len() - limit));
    }
    lines.join("\n")
}

/// For `archix status`: kept deployments the last `archix verify` found changed.
pub fn print_status(paths: &Paths) {
    let Ok(store) = Store::open(&paths.root) else { return };
    let kept: BTreeSet<u32> = store.deployments().unwrap_or_default().into_iter().collect();
    for (number, problems) in read_record(paths).checked {
        if kept.contains(&number) && !problems.is_empty() {
            println!(
                "deployment {number} differs from its packages in {} files at the last check; `sudo archix verify --repair` restores them",
                problems.len()
            );
        }
    }
}

/// Before a rollback boots kept deployment `number`: checks it, and puts back what changed from the package cache, or refuses when that isn't possible.
pub fn ensure_intact(paths: &Paths, store: &Store, number: u32) -> Result<()> {
    eprintln!(":: checking deployment {number}'s files against its packages");
    let deployment = store.path(number);
    let (problems, _) = changed(check(&deployment)?);
    if problems.is_empty() {
        save(paths, number, &problems);
        return Ok(());
    }
    eprintln!(
        "deployment {number} differs from its packages in {} files:\n{}",
        problems.len(),
        describe(&problems, 10)
    );
    let repaired = repair(paths, &deployment, &problems)
        .with_context(|| format!("deployment {number} was changed outside archix; nothing was rolled back"))?;
    let (left, _) = changed(check(&deployment)?);
    save(paths, number, &left);
    if !left.is_empty() {
        bail!(
            "deployment {number} still differs from its packages after the repair; nothing was rolled back:\n{}",
            describe(&left, 10)
        );
    }
    eprintln!("repaired {repaired} files of deployment {number} from the package cache");
    Ok(())
}
