use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use flate2::read::GzDecoder;

use crate::pacman::parse_desc;

/// What a package file is and installs, from the .PKGINFO and .MTREE at the start of the archive.
pub struct Contents {
    pub name: String,
    /// Packages it conflicts with or replaces, which pacman removes to install it.
    pub displaces: Vec<String>,
    /// Files and links, not directories.
    pub files: Vec<String>,
    /// Each file's size in bytes from .MTREE, in the order of `files`; links count 0.
    pub sizes: Vec<u64>,
    /// Installed size in bytes, `size` in .PKGINFO.
    pub size: u64,
    /// Config files (`backup` in .PKGINFO), which pacman installs as .pacnew instead of refusing when one already exists.
    pub backups: Vec<String>,
}

pub fn contents(path: &Path) -> Result<Contents> {
    let at = || format!("reading {}", path.display());
    let file = File::open(path).with_context(at)?;
    let decoder: Box<dyn Read> = if path.to_string_lossy().ends_with(".zst") {
        Box::new(zstd::Decoder::new(file).with_context(at)?)
    } else {
        bail!("{} isn't a .pkg.tar.zst", path.display());
    };
    let mut archive = tar::Archive::new(decoder);
    let (mut name, mut files, mut displaces, mut size, mut backups) = (None, None, Vec::new(), 0, Vec::new());
    for entry in archive.entries().with_context(at)? {
        let mut entry = entry.with_context(at)?;
        let entry_path = entry.path().with_context(at)?.to_string_lossy().into_owned();
        match entry_path.as_str() {
            ".PKGINFO" => {
                let mut text = String::new();
                entry.read_to_string(&mut text).with_context(at)?;
                name = text
                    .lines()
                    .find_map(|line| line.strip_prefix("pkgname = "))
                    .map(str::to_string);
                displaces = text
                    .lines()
                    .filter_map(|line| {
                        line.strip_prefix("conflict = ")
                            .or_else(|| line.strip_prefix("replaces = "))
                    })
                    .map(|dep| crate::aur::srcinfo::dep_name(dep).to_string())
                    .collect();
                backups = text
                    .lines()
                    .filter_map(|line| line.strip_prefix("backup = "))
                    .map(str::to_string)
                    .collect();
                size = text
                    .lines()
                    .find_map(|line| line.strip_prefix("size = "))
                    .and_then(|n| n.trim().parse().ok())
                    .unwrap_or(0);
            }
            ".MTREE" => {
                let mut text = String::new();
                GzDecoder::new(entry).read_to_string(&mut text).with_context(at)?;
                files = Some(mtree_entries(&text));
            }
            _ if !entry_path.starts_with('.') => break,
            _ => {}
        }
        if let (Some(name), Some(entries)) = (&name, &files) {
            let (files, sizes): (Vec<String>, Vec<u64>) = entries.iter().cloned().unzip();
            return Ok(Contents {
                name: name.clone(),
                displaces: displaces.clone(),
                files,
                sizes,
                size,
                backups: backups.clone(),
            });
        }
    }
    bail!("{} has no .PKGINFO and .MTREE", path.display())
}

/// Paths of everything but directories in an mtree listing, with its \ooo escapes decoded.
#[cfg(test)]
fn mtree_files(text: &str) -> Vec<String> {
    mtree_entries(text).into_iter().map(|(path, _)| path).collect()
}

/// Everything but directories in an mtree listing, with each one's `size`.
fn mtree_entries(text: &str) -> Vec<(String, u64)> {
    text.lines()
        .filter(|line| line.starts_with("./") && !line.contains(" type=dir"))
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let path = unescape(words.next()?.trim_start_matches("./"));
            let size = words
                .find_map(|word| word.strip_prefix("size="))
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
            Some((path, size))
        })
        .filter(|(path, _)| !path.starts_with('.'))
        .collect()
}

pub(crate) fn unescape(path: &str) -> String {
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

/// Files installed packages that the transaction keeps own, from pacman's local database; one that conflicts with an incoming package is removed by pacman, so it isn't kept.
fn kept_files(local_db: &Path, replaced: &BTreeSet<String>, incoming: &BTreeSet<String>) -> BTreeMap<String, String> {
    let mut owners = BTreeMap::new();
    for entry in fs::read_dir(local_db).into_iter().flatten().flatten() {
        let Ok(desc) = fs::read_to_string(entry.path().join("desc")) else {
            continue;
        };
        let desc = parse_desc(&desc);
        let Some(name) = desc.get("NAME").and_then(|n| n.first()).cloned() else {
            continue;
        };
        let displaced = ["CONFLICTS", "REPLACES"]
            .iter()
            .flat_map(|key| desc.get(*key).into_iter().flatten())
            .any(|dep| incoming.contains(crate::aur::srcinfo::dep_name(dep)));
        if replaced.contains(&name) || displaced {
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

/// Two packages a switch would install, or one it installs and one it keeps, that ship the same file, or a file no package owns in the way under `root`, which holds the trees the changes are staged from; pacman would refuse the whole transaction.
pub fn check(install: &[PathBuf], removed: &[String], local_db: &Path, root: &Path) -> Result<()> {
    let mut incoming = Vec::new();
    for path in install {
        incoming.push(contents(path)?);
    }
    let mut replaced: BTreeSet<String> = incoming.iter().map(|c| c.name.clone()).collect();
    replaced.extend(incoming.iter().flat_map(|c| c.displaces.iter().cloned()));
    replaced.extend(removed.iter().cloned());
    let names: BTreeSet<String> = incoming.iter().map(|c| c.name.clone()).collect();
    let mut owners = kept_files(local_db, &replaced, &names);
    let mut clashes = BTreeSet::new();
    for Contents { name, files, .. } in &incoming {
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
    let owned = owned_files(local_db);
    for Contents {
        name, files, backups, ..
    } in &incoming
    {
        for file in unowned_on_disk(root, files, &owned).filter(|file| !backups.contains(file)) {
            clashes.insert(format!("{file} from {name} exists on disk, and no package owns it"));
        }
    }
    if !clashes.is_empty() {
        let shown: Vec<String> = clashes.iter().take(10).cloned().collect();
        bail!(
            "pacman would refuse these packages over files that are already taken; nothing was changed. Drop one of each pair from the config, or move the stray files away:\n  {}",
            shown.join("\n  ")
        );
    }
    check_mounts(root, &incoming)?;
    check_inodes(root, local_db, &incoming)
}

/// Refuses a transaction when the root filesystem has fewer free inodes than staging a deployment from `root` takes: copies of etc, the database and DKMS state, new directories for the hardlinked usr, opt and boot and their overlay uppers, the incoming files, and a margin; btrfs allocates inodes as it goes and reports none.
pub fn check_inodes(root: &Path, local_db: &Path, incoming: &[Contents]) -> Result<()> {
    let Some((free, total, used)) = inodes(root) else {
        return Ok(());
    };
    let files: u64 = incoming.iter().map(|c| c.files.len() as u64).sum();
    // Before the first layout, deployment 1 is laid out from the live root before the next one is staged from it.
    let layouts = if local_db == root.join("var/lib/pacman/local") {
        2
    } else {
        1
    };
    let margin = |n: u64| n + n / 20 + INODE_SPARE.min(total / 20);
    if free >= margin(used * (layouts + 1) + files) {
        return Ok(());
    }
    let mut staged = files;
    for tree in ["etc", "dkms"] {
        staged += count_entries(&root.join(tree), false) * layouts;
    }
    staged += count_entries(local_db, false) * layouts;
    for tree in ["usr", "opt", "boot"] {
        staged += count_entries(&root.join(tree), true) * (layouts + 1);
    }
    let needed = margin(staged);
    if free < needed {
        bail!(
            "{} has {free} free inodes, but staging this deployment takes about {needed}; nothing was changed. Free some (`archix gc`, or delete many small files), or give the filesystem more inodes",
            root.display()
        );
    }
    Ok(())
}

/// Inodes a switch keeps free beyond its estimate, for pacman's temporary files and the initramfs build.
const INODE_SPARE: u64 = 4096;

/// Entries under `dir`, or only its directories (itself included) with `dirs_only`.
fn count_entries(dir: &Path, dirs_only: bool) -> u64 {
    let mut count = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        count += 1;
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                stack.push(entry.path());
            } else if !dirs_only {
                count += 1;
            }
        }
    }
    count
}

/// Free inodes for unprivileged users, the filesystem's total and how many are used, or None where it doesn't count them.
fn inodes(path: &Path) -> Option<(u64, u64, u64)> {
    let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } != 0 || stat.f_files == 0 {
        return None;
    }
    let (total, free) = (stat.f_files as u64, stat.f_favail as u64);
    Some((free, total, total - (stat.f_ffree as u64).min(total)))
}

/// Like pacman's disk space check: what each mount gains from the incoming files; the replaced versions free nothing, as the deployment staged from keeps them.
fn check_mounts(root: &Path, incoming: &[Contents]) -> Result<()> {
    let mounts = mount_points();
    let mount_of = |file: &str| {
        let path = root.join(file);
        mounts
            .iter()
            .find(|mount| path.starts_with(mount))
            .cloned()
            .unwrap_or_else(|| root.to_path_buf())
    };
    let mut blocks: BTreeMap<PathBuf, u64> = BTreeMap::new();
    let mut on_disk =
        |mount: &PathBuf, size: u64| allocated(size, *blocks.entry(mount.clone()).or_insert_with(|| block_size(mount)));
    let mut adds: BTreeMap<PathBuf, u64> = BTreeMap::new();
    // Staging copies /etc and the database and builds an initramfs even when nothing is installed.
    adds.insert(mount_of(""), 0);
    for package in incoming {
        for (file, size) in package.files.iter().zip(&package.sizes) {
            let mount = mount_of(file);
            *adds.entry(mount.clone()).or_default() += on_disk(&mount, *size);
        }
    }
    for (mount, add) in adds {
        crate::system::btrfs::ensure_metadata_room(&mount)?;
        check_space(&mount, add, 0)?;
    }
    Ok(())
}

/// Mount points from /proc/self/mounts, longest first so the first prefix match is the one a path lives on.
fn mount_points() -> Vec<PathBuf> {
    let mut mounts: Vec<PathBuf> = fs::read_to_string("/proc/self/mounts")
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.split_whitespace().nth(1))
        .map(|mount| PathBuf::from(unescape(mount)))
        .collect();
    mounts.sort_by_key(|mount| std::cmp::Reverse(mount.as_os_str().len()));
    mounts.dedup();
    mounts
}

/// Each installed package's files (not directories), from pacman's local database.
pub fn owners(local_db: &Path) -> BTreeMap<String, Vec<String>> {
    let mut owners = BTreeMap::new();
    for entry in fs::read_dir(local_db).into_iter().flatten().flatten() {
        let Some(name) = parse_desc(&fs::read_to_string(entry.path().join("desc")).unwrap_or_default())
            .remove("NAME")
            .and_then(|names| names.into_iter().next())
        else {
            continue;
        };
        let files = fs::read_to_string(entry.path().join("files"))
            .map(|text| parse_desc(&text).remove("FILES").unwrap_or_default())
            .unwrap_or_default();
        owners.insert(name, files.into_iter().filter(|f| !f.ends_with('/')).collect());
    }
    owners
}

/// Every file some installed package owns, from pacman's local database.
pub fn owned_files(local_db: &Path) -> std::collections::HashSet<String> {
    let mut owned = std::collections::HashSet::new();
    for entry in fs::read_dir(local_db).into_iter().flatten().flatten() {
        let files = fs::read_to_string(entry.path().join("files"))
            .map(|text| parse_desc(&text).remove("FILES").unwrap_or_default())
            .unwrap_or_default();
        owned.extend(files.into_iter().filter(|f| !f.ends_with('/')));
    }
    owned
}

/// Of `files`, those present under `root` that no package owns; archix's own copies of kernel modules don't count.
pub fn unowned_on_disk<'a>(
    root: &'a Path,
    files: &'a [String],
    owned: &'a std::collections::HashSet<String>,
) -> impl Iterator<Item = &'a String> + 'a {
    files.iter().filter(move |file| {
        !file.starts_with("usr/lib/modules/")
            && !owned.contains(*file)
            && fs::symlink_metadata(root.join(file)).is_ok_and(|meta| !meta.is_dir())
    })
}

/// pacman extracts file by file and the boot step writes initramfs images, so a switch keeps this much free beyond what it adds.
const SPARE: u64 = 128 << 20;

/// Refuses a transaction that adds `adds` bytes and frees `frees` when `root`'s filesystem can't hold the difference plus some spare.
pub fn check_space(root: &Path, adds: u64, frees: u64) -> Result<()> {
    let Some((mut free, total)) = space(root) else {
        return Ok(());
    };
    // pacman keeps 5% of the filesystem as a cushion; a small ESP still takes a ucode update.
    let needed = adds.saturating_sub(frees) + SPARE.min(total / 20);
    // btrfs hands back a deleted file's space only at its next commit, which can take two.
    for _ in 0..2 {
        if free >= needed {
            break;
        }
        if let Ok(dir) = File::open(root) {
            unsafe { libc::syncfs(std::os::fd::AsRawFd::as_raw_fd(&dir)) };
        }
        free = space(root).map_or(free, |(free, _)| free);
    }
    if free < needed {
        let mib = |bytes: u64| bytes.div_ceil(1 << 20);
        bail!(
            "{} has {} MiB free, but this needs about {} MiB; nothing was changed. Free some space (`archix gc`, `pacman -Sc`) and try again",
            root.display(),
            mib(free),
            mib(needed)
        );
    }
    Ok(())
}

/// The space a file of `size` bytes takes on a filesystem that allocates `block` bytes at a time.
fn allocated(size: u64, block: u64) -> u64 {
    match block {
        0 => size,
        block => size.div_ceil(block) * block,
    }
}

/// The filesystem's allocation unit at `path`, 0 when it can't be read.
fn block_size(path: &Path) -> u64 {
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) else {
        return 0;
    };
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    match unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } {
        0 => stat.f_frsize as u64,
        _ => 0,
    }
}

/// Bytes free on the filesystem holding `path`.
pub fn free_space(path: &Path) -> Option<u64> {
    space(path).map(|(free, _)| free)
}

/// Bytes available to unprivileged users, as pacman counts them, and the filesystem's size.
fn space(path: &Path) -> Option<(u64, u64)> {
    let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    (unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } == 0).then(|| {
        let block = stat.f_frsize as u64;
        (stat.f_bavail as u64 * block, stat.f_blocks as u64 * block)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_takes_whole_blocks() {
        assert_eq!(allocated(0, 4096), 0);
        assert_eq!(allocated(3, 4096), 4096);
        assert_eq!(allocated(4096, 4096), 4096);
        assert_eq!(allocated(4097, 4096), 8192);
        assert_eq!(allocated(3, 0), 3);
    }

    #[test]
    fn mtree_lists_files_and_links_but_not_directories() {
        let mtree = "#mtree\n/set type=file uid=0\n./.BUILDINFO time=1 size=10\n./usr time=1 type=dir\n./usr/bin/foo time=1 size=3\n./usr/lib/libfoo.so time=1 type=link link=libfoo.so.1\n./usr/share/my\\040app/x time=1\n";
        assert_eq!(
            mtree_files(mtree),
            ["usr/bin/foo", "usr/lib/libfoo.so", "usr/share/my app/x"]
        );
    }

    #[test]
    fn space_counts_what_replaced_packages_free() {
        let dir = tempfile::tempdir().unwrap();
        let free = free_space(dir.path()).unwrap();
        assert!(check_space(dir.path(), free * 2, 0).is_err());
        assert!(check_space(dir.path(), free * 2, free * 2).is_ok() || free < SPARE);
        let err = check_space(dir.path(), free + SPARE, 0).unwrap_err().to_string();
        assert!(err.contains("nothing was changed"), "{err}");
    }
}
