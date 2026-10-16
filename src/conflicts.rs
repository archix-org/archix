use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use flate2::read::GzDecoder;

use crate::pacman::parse_desc;

/// A package file's name and the files (not directories) it installs, read from the .PKGINFO and .MTREE at the start of the archive.
pub fn contents(path: &Path) -> Result<(String, Vec<String>)> {
    let file = File::open(path).with_context(|| format!("reading {}", path.display()))?;
    let decoder: Box<dyn Read> = if path.to_string_lossy().ends_with(".zst") {
        Box::new(zstd::Decoder::new(file)?)
    } else {
        bail!("{} isn't a .pkg.tar.zst", path.display());
    };
    let mut archive = tar::Archive::new(decoder);
    let (mut name, mut files) = (None, None);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let entry_path = entry.path()?.to_string_lossy().into_owned();
        match entry_path.as_str() {
            ".PKGINFO" => {
                let mut text = String::new();
                entry.read_to_string(&mut text)?;
                name = text
                    .lines()
                    .find_map(|line| line.strip_prefix("pkgname = "))
                    .map(str::to_string);
            }
            ".MTREE" => {
                let mut text = String::new();
                GzDecoder::new(entry).read_to_string(&mut text)?;
                files = Some(mtree_files(&text));
            }
            _ if !entry_path.starts_with('.') => break,
            _ => {}
        }
        if let (Some(name), Some(files)) = (&name, &files) {
            return Ok((name.clone(), files.clone()));
        }
    }
    bail!("{} has no .PKGINFO and .MTREE", path.display())
}

/// Paths of everything but directories in an mtree listing, with its \ooo escapes decoded.
fn mtree_files(text: &str) -> Vec<String> {
    text.lines()
        .filter(|line| line.starts_with("./") && !line.contains(" type=dir"))
        .filter_map(|line| line.split_whitespace().next())
        .map(|path| unescape(path.trim_start_matches("./")))
        .filter(|path| !path.starts_with('.'))
        .collect()
}

fn unescape(path: &str) -> String {
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() && bytes[i + 1..i + 4].iter().all(|b| (b'0'..=b'7').contains(b)) {
            let value = u8::from_str_radix(&path[i + 1..i + 4], 8).unwrap_or(b'?');
            out.push(value);
            i += 4;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Files installed packages that the transaction keeps own, from pacman's local database.
fn kept_files(local_db: &Path, replaced: &BTreeSet<String>) -> BTreeMap<String, String> {
    let mut owners = BTreeMap::new();
    for entry in fs::read_dir(local_db).into_iter().flatten().flatten() {
        let Ok(desc) = fs::read_to_string(entry.path().join("desc")) else {
            continue;
        };
        let Some(name) = parse_desc(&desc).get("NAME").and_then(|n| n.first()).cloned() else {
            continue;
        };
        if replaced.contains(&name) {
            continue;
        }
        let files = fs::read_to_string(entry.path().join("files"))
            .map(|text| parse_desc(&text).remove("FILES").unwrap_or_default())
            .unwrap_or_default();
        for file in files.into_iter().filter(|f| !f.ends_with('/')) {
            owners.insert(file, name.clone());
        }
    }
    owners
}

/// Two packages a switch would install, or one it installs and one it keeps, that ship the same file; pacman would refuse the whole transaction.
pub fn check(install: &[PathBuf], removed: &[String], local_db: &Path) -> Result<()> {
    let mut incoming = Vec::new();
    for path in install {
        incoming.push(contents(path)?);
    }
    let mut replaced: BTreeSet<String> = incoming.iter().map(|(name, _)| name.clone()).collect();
    replaced.extend(removed.iter().cloned());
    let mut owners = kept_files(local_db, &replaced);
    let mut clashes = BTreeSet::new();
    for (name, files) in &incoming {
        for file in files {
            match owners.get(file) {
                Some(other) if other != name => {
                    clashes.insert(format!("{file} is in both {other} and {name}"));
                }
                _ => {
                    owners.insert(file.clone(), name.clone());
                }
            }
        }
    }
    if clashes.is_empty() {
        return Ok(());
    }
    let shown: Vec<String> = clashes.iter().take(10).cloned().collect();
    bail!(
        "pacman would refuse these packages, which ship the same files; nothing was changed. Drop one of each pair from the config:\n  {}",
        shown.join("\n  ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mtree_lists_files_and_links_but_not_directories() {
        let mtree = "#mtree\n/set type=file uid=0\n./.BUILDINFO time=1 size=10\n./usr time=1 type=dir\n./usr/bin/foo time=1 size=3\n./usr/lib/libfoo.so time=1 type=link link=libfoo.so.1\n./usr/share/my\\040app/x time=1\n";
        assert_eq!(
            mtree_files(mtree),
            ["usr/bin/foo", "usr/lib/libfoo.so", "usr/share/my app/x"]
        );
    }
}
