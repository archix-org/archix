//! What a keep does to /var/lib, which is @varlib and so in no snapshot of `@`: package-owned files go back to the snapshot's versions from its packages, and files of packages installed since are moved aside, all listed in a manifest so `archix rollback --cancel` can put them back.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Under /var/lib: one directory per keep, named after the snapshot kept, holding what it moved aside and its manifest.
pub const MOVED: &str = ".archix-moved";
const MANIFEST: &str = "manifest.json";

/// What a keep did to one path under /var/lib.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    /// Moved aside: a package installed since the snapshot owns it.
    Moved,
    /// Put back as the snapshot's package has it; `had` says whether a file was there, which was moved aside.
    Restored { had: bool },
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Manifest {
    #[serde(default)]
    pub format: u32,
    pub snapshot: u32,
    pub entries: Vec<(String, Action)>,
}

/// One file or link a package lists under var/lib.
#[derive(Debug, Clone, PartialEq)]
pub struct Owned {
    pub package: String,
    pub version: String,
    pub arch: String,
    pub content: Content,
}

/// What a path holds: a file's sha256 or a link's target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Content {
    File(Option<String>),
    Link(String),
}

/// What a keep did and left, for its report.
#[derive(Debug, Default)]
pub struct Report {
    pub dir: Option<PathBuf>,
    pub restored: Vec<String>,
    pub moved: Vec<String>,
    pub warnings: Vec<String>,
}

/// Files and links under var/lib in an mtree listing, relative to /var/lib, leaving out what rolls back with the root.
pub fn mtree_var_lib(text: &str) -> Vec<(String, Content)> {
    let mut default_type = "file".to_string();
    let mut found = Vec::new();
    for line in text.lines() {
        let mut words = line.split_whitespace();
        let Some(first) = words.next() else { continue };
        let keys: BTreeMap<&str, &str> = words.filter_map(|word| word.split_once('=')).collect();
        if first == "/set" {
            if let Some(kind) = keys.get("type") {
                default_type = kind.to_string();
            }
            continue;
        }
        let Some(path) = first.strip_prefix("./var/lib/") else {
            continue;
        };
        let path = crate::conflicts::unescape(path);
        if super::layout::ROLLED
            .iter()
            .any(|rolled| path == *rolled || path.starts_with(&format!("{rolled}/")))
        {
            continue;
        }
        let kind = keys.get("type").copied().unwrap_or(&default_type);
        let content = match kind {
            "file" => Content::File(keys.get("sha256digest").map(|digest| digest.to_string())),
            "link" => Content::Link(crate::conflicts::unescape(
                keys.get("link").copied().unwrap_or_default(),
            )),
            _ => continue,
        };
        found.push((path, content));
    }
    found
}

/// Everything installed packages own under /var/lib by pacman's local database `local_db`, by path relative to /var/lib.
pub fn owned(local_db: &Path) -> BTreeMap<String, Owned> {
    let mut owned = BTreeMap::new();
    for entry in fs::read_dir(local_db).into_iter().flatten().flatten() {
        let Ok(desc) = fs::read_to_string(entry.path().join("desc")) else {
            continue;
        };
        let desc = crate::pacman::parse_desc(&desc);
        let first = |key: &str| desc.get(key).and_then(|values| values.first()).cloned();
        let (Some(package), Some(version)) = (first("NAME"), first("VERSION")) else {
            continue;
        };
        let arch = first("ARCH").unwrap_or_else(|| "x86_64".to_string());
        let Ok(file) = fs::File::open(entry.path().join("mtree")) else {
            continue;
        };
        let mut text = String::new();
        if flate2::read::GzDecoder::new(file).read_to_string(&mut text).is_err() {
            continue;
        }
        for (path, content) in mtree_var_lib(&text) {
            owned.insert(
                path,
                Owned {
                    package: package.clone(),
                    version: version.clone(),
                    arch: arch.clone(),
                    content,
                },
            );
        }
    }
    owned
}

/// What's at `path` now, in the terms an mtree uses; None when nothing is.
fn content_of(path: &Path) -> Option<Content> {
    let meta = fs::symlink_metadata(path).ok()?;
    if meta.file_type().is_symlink() {
        return Some(Content::Link(fs::read_link(path).ok()?.to_string_lossy().into_owned()));
    }
    if meta.is_dir() {
        return None;
    }
    Some(Content::File(crate::fetch::sha256(path).ok()))
}

/// Whether what's at a path, `actual`, is what an mtree says, `listed`; a file the mtree gives no digest for counts as matching.
fn same(actual: &Content, listed: &Content) -> bool {
    match (actual, listed) {
        (Content::File(_), Content::File(None)) => true,
        (a, b) => a == b,
    }
}

/// A directory for this keep's moved files under `var_lib`: `.archix-moved/<number>`, or with a suffix if an earlier keep of the same snapshot left one.
fn new_dir(var_lib: &Path, number: u32) -> PathBuf {
    let base = var_lib.join(MOVED);
    let mut dir = base.join(number.to_string());
    let mut n = 1;
    while dir.exists() {
        dir = base.join(format!("{number}.{n}"));
        n += 1;
    }
    dir
}

fn save(dir: &Path, manifest: &Manifest) -> Result<()> {
    crate::durable::write(&dir.join(MANIFEST), serde_json::to_string_pretty(manifest)?.as_bytes())
}

fn move_into(from: &Path, to: &Path) -> Result<()> {
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    crate::durable::rename(from, to).with_context(|| format!("moving {} to {}", from.display(), to.display()))
}

/// Puts /var/lib, at `var_lib`, in step with snapshot `number`, whose packages pacman's database `then` lists, from the system whose database `now` lists: each file a package of the snapshot owns there goes back to its version from the snapshot's package, which `archive` finds by name, version and arch, if the file is unchanged since the current package installed it (else it's left with a warning, as pacman does with a changed backup file); each file only the current packages own is moved aside into `.archix-moved/<number>`, so reinstalling its package doesn't find it in the way. Every step is in that directory's manifest before it's made.
pub fn restore(
    var_lib: &Path,
    then: &Path,
    now: &Path,
    number: u32,
    archive: &mut dyn FnMut(&str, &str, &str) -> Result<PathBuf>,
) -> Result<Report> {
    let (then, now) = (owned(then), owned(now));
    let mut report = Report::default();
    let dir = new_dir(var_lib, number);
    let mut manifest = Manifest {
        format: 1,
        snapshot: number,
        entries: Vec::new(),
    };
    let mut started = false;
    let mut start = |manifest: &Manifest| -> Result<()> {
        if !started {
            fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
            started = true;
        }
        save(&dir, manifest)
    };
    for (path, _) in now.iter().filter(|(path, _)| !then.contains_key(*path)) {
        let live = var_lib.join(path);
        if content_of(&live).is_none() {
            continue;
        }
        manifest.entries.push((path.clone(), Action::Moved));
        start(&manifest)?;
        move_into(&live, &dir.join(path))?;
        report.moved.push(path.clone());
    }
    let mut wanted: BTreeMap<(String, String, String), BTreeSet<String>> = BTreeMap::new();
    for (path, listed) in &then {
        let actual = content_of(&var_lib.join(path));
        if actual.as_ref().is_some_and(|actual| same(actual, &listed.content)) {
            continue;
        }
        let unchanged = match (&actual, now.get(path)) {
            (None, _) => true,
            (Some(actual), Some(current)) => same(actual, &current.content),
            (Some(_), None) => false,
        };
        if !unchanged {
            report.warnings.push(format!(
                "/var/lib/{path} changed since its package installed it, or no package owns it now, so it's left as it is rather than put back to {} {}'s",
                listed.package, listed.version
            ));
            continue;
        }
        wanted
            .entry((listed.package.clone(), listed.version.clone(), listed.arch.clone()))
            .or_default()
            .insert(path.clone());
    }
    for ((package, version, arch), paths) in wanted {
        let file = match archive(&package, &version, &arch) {
            Ok(file) => file,
            Err(e) => {
                for path in &paths {
                    report.warnings.push(format!(
                        "/var/lib/{path} is left as it is: {package} {version}'s package isn't at hand: {e:#}"
                    ));
                }
                continue;
            }
        };
        if let Err(e) = unpack(&file, var_lib, &paths, &dir, &mut manifest, &mut start, &mut report) {
            report.warnings.push(format!(
                "putting back {package} {version}'s files under /var/lib stopped: {e:#}"
            ));
        }
    }
    if started {
        crate::durable::sync_filesystems_checked(&[var_lib])?;
        report.dir = Some(dir);
    }
    Ok(report)
}

fn unpack(
    file: &Path,
    var_lib: &Path,
    paths: &BTreeSet<String>,
    dir: &Path,
    manifest: &mut Manifest,
    start: &mut dyn FnMut(&Manifest) -> Result<()>,
    report: &mut Report,
) -> Result<()> {
    let at = || format!("reading {}", file.display());
    let reader = fs::File::open(file).with_context(at)?;
    if !file.to_string_lossy().ends_with(".zst") {
        anyhow::bail!("{} isn't a .pkg.tar.zst", file.display());
    }
    let mut archive = tar::Archive::new(zstd::Decoder::new(reader).with_context(at)?);
    archive.set_preserve_permissions(true);
    archive.set_preserve_ownerships(true);
    archive.set_preserve_mtime(true);
    archive.set_unpack_xattrs(true);
    let mut left: BTreeSet<&String> = paths.iter().collect();
    for entry in archive.entries().with_context(at)? {
        let mut entry = entry.with_context(at)?;
        let name = entry.path().with_context(at)?.to_string_lossy().into_owned();
        let Some(path) = name.strip_prefix("var/lib/").map(str::to_string) else {
            continue;
        };
        if !left.remove(&path) {
            continue;
        }
        let live = var_lib.join(&path);
        let had = fs::symlink_metadata(&live).is_ok();
        manifest.entries.push((path.clone(), Action::Restored { had }));
        start(manifest)?;
        if had {
            move_into(&live, &dir.join(&path))?;
        }
        if let Some(parent) = live.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        entry
            .unpack(&live)
            .with_context(|| format!("writing {}", live.display()))?;
        report.restored.push(path);
        if left.is_empty() {
            break;
        }
    }
    for path in left {
        report.warnings.push(format!(
            "/var/lib/{path} isn't in {}, so it's left as it is",
            file.display()
        ));
    }
    Ok(())
}

/// Undoes the keep whose moved files are in `dir` under `var_lib`, newest step first, and removes `dir`.
pub fn undo(var_lib: &Path, dir: &Path) -> Result<()> {
    let text = match fs::read_to_string(dir.join(MANIFEST)) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.join(MANIFEST).display())),
    };
    let manifest: Manifest = serde_json::from_str(&text).with_context(|| format!("parsing {}", dir.display()))?;
    for (path, action) in manifest.entries.iter().rev() {
        let (live, aside) = (var_lib.join(path), dir.join(path));
        if matches!(action, Action::Restored { .. }) && fs::symlink_metadata(&aside).is_err() {
            let _ = fs::remove_file(&live);
        }
        if fs::symlink_metadata(&aside).is_ok() {
            move_into(&aside, &live)?;
        }
    }
    crate::durable::sync_filesystems_checked(&[var_lib])?;
    fs::remove_dir_all(dir).with_context(|| format!("removing {}", dir.display()))
}

/// The keeps' directories under `var_lib`/.archix-moved, by the snapshot each kept.
pub fn dirs(var_lib: &Path) -> Vec<(u32, PathBuf)> {
    let mut found: Vec<(u32, PathBuf)> = fs::read_dir(var_lib.join(MOVED))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let number = name.split('.').next()?.parse().ok()?;
            Some((number, entry.path()))
        })
        .collect();
    found.sort();
    found
}

/// Removes the moved files of keeps whose snapshot is gone, so they follow snapshot retention; returns the snapshots they were for.
pub fn prune(var_lib: &Path, existing: &BTreeSet<u32>) -> Vec<u32> {
    let mut removed = Vec::new();
    for (number, dir) in dirs(var_lib) {
        if existing.contains(&number) {
            continue;
        }
        if fs::remove_dir_all(&dir).is_ok() {
            removed.push(number);
        }
    }
    removed
}

/// Bytes the keeps' moved files take, as `du` counts them.
pub fn size(var_lib: &Path) -> u64 {
    fn walk(path: &Path) -> u64 {
        use std::os::unix::fs::MetadataExt;
        let Ok(meta) = fs::symlink_metadata(path) else { return 0 };
        let own = meta.blocks() * 512;
        if !meta.is_dir() {
            return own;
        }
        own + fs::read_dir(path)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| walk(&entry.path()))
            .sum::<u64>()
    }
    walk(&var_lib.join(MOVED))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_mtree_lists_var_lib_files_and_links_but_not_directories_or_what_rolls_back() {
        let mtree = "#mtree\n/set type=file uid=0 gid=0 mode=644\n./.PKGINFO time=1 size=10\n./var/lib/foo time=1 type=dir\n./var/lib/foo/state time=1 size=3 sha256digest=abc\n./var/lib/foo/link time=1 type=link link=state\n./var/lib/pacman/x time=1 sha256digest=def\n./var/lib/my\\040app/y time=1\n./usr/bin/foo time=1 sha256digest=0\n";
        assert_eq!(
            mtree_var_lib(mtree),
            vec![
                ("foo/state".to_string(), Content::File(Some("abc".into()))),
                ("foo/link".to_string(), Content::Link("state".into())),
                ("my app/y".to_string(), Content::File(None)),
            ]
        );
    }
}
