//! Moving a root that is btrfs's top level (subvolid 5) into `@`, which a keep can swap: `schedule` marks it, and at the next boot the initramfs `finish`es it before the root is used, resumably, then mounts `@` as the root on every boot after.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

/// At the top level from `schedule` until the move is done.
pub const MARK: &str = ".archix-move-root";
/// At the top level once the root lives in `@`, so the initramfs mounts `@` though the command line names no subvolume.
pub const MOVED: &str = ".archix-root-in-at";
const NEW: &str = "@.archix-new";
/// At the top level once `@`'s fstab mounts it as the root, so a crash after doesn't rewrite it.
const FSTAB_DONE: &str = ".archix-move-root.fstab";

/// Whether the move into `@` is scheduled for the next boot on the root at `root`, which is the top level.
pub fn scheduled(root: &Path) -> bool {
    root.join(MARK).exists()
}

/// Schedules the move of the root at `root`, btrfs's top level, into `@` for the next boot.
pub fn schedule(root: &Path) -> Result<()> {
    if root.join(super::ROOT).exists() {
        bail!(
            "{} has a subvolume named @ already, so the root can't move into it; rename or remove it first",
            root.display()
        );
    }
    crate::durable::write(&root.join(MARK), b"")
}

fn btrfs(args: &[&str], path: &Path, what: &str) -> Result<()> {
    let output = Command::new("btrfs")
        .args(args)
        .arg(path)
        .output()
        .context("running btrfs")?;
    if !output.status.success() {
        bail!(
            "btrfs couldn't {what}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// The UUID of the btrfs filesystem mounted at `path`, from `btrfs filesystem show`.
fn filesystem_uuid(path: &Path) -> Option<String> {
    let output = Command::new("btrfs")
        .args(["filesystem", "show"])
        .arg(path)
        .output()
        .ok()?;
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .skip_while(|word| *word != "uuid:")
        .nth(1)
        .map(str::to_string)
}

/// A btrfs subvolume's root directory has inode 256.
fn is_subvolume(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir() && meta.ino() == 256)
}

/// Subvolumes whose parent is the top level, by path, from `btrfs subvolume list`'s output.
fn top_level_children(listing: &str) -> Vec<String> {
    listing
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let level = fields.iter().position(|field| *field == "level")?;
            let path = fields.iter().position(|field| *field == "path")?;
            (fields.get(level + 1) == Some(&"5")).then(|| fields.get(path + 1).map(|p| p.to_string()))?
        })
        .collect()
}

/// `fstab` for the root in `@`: `/` mounted as subvol=/@, `subvol=/X` of a subvolume moved into `@` as `/@/X`, and a line for each top-level subvolume in `mount` (name) that nothing mounts yet, at /name, on the root's device from fstab or else `device`.
pub fn moved_fstab(fstab: &str, moved: &[String], mount: &[String], device: Option<&str>) -> String {
    let mut text = String::new();
    for line in fstab.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if line.trim_start().starts_with('#') || fields.len() < 4 || fields[2] != "btrfs" {
            text.push_str(line);
            text.push('\n');
            continue;
        }
        let options: Vec<String> = fields[3]
            .split(',')
            .filter_map(|option| {
                if fields[1] == "/" {
                    return (!option.starts_with("subvol=") && !option.starts_with("subvolid="))
                        .then(|| option.to_string());
                }
                match option.strip_prefix("subvol=") {
                    Some(sub) if moved.iter().any(|m| m == sub.trim_start_matches('/')) => {
                        Some(format!("subvol=/@/{}", sub.trim_start_matches('/')))
                    }
                    _ => Some(option.to_string()),
                }
            })
            .chain((fields[1] == "/").then(|| format!("subvol=/{}", super::ROOT)))
            .collect();
        let mut out: Vec<String> = fields.iter().map(|f| f.to_string()).collect();
        out[3] = options.join(",");
        text.push_str(&out.join(" "));
        text.push('\n');
    }
    let root_device = fstab
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>())
        .find(|fields| fields.len() > 3 && fields[1] == "/")
        .map(|fields| fields[0].to_string())
        .or_else(|| device.map(str::to_string));
    if let Some(device) = root_device {
        for name in mount {
            let point = format!("/{name}");
            if fstab
                .lines()
                .any(|line| line.split_whitespace().nth(1) == Some(point.as_str()))
            {
                continue;
            }
            text.push_str(&format!("{device} {point} btrfs subvol=/{name} 0 0\n"));
        }
    }
    text
}

/// Removes `path` and everything under it but subvolumes, which stay with the directories leading to them; returns those kept.
fn remove_keeping_subvolumes(path: &Path, kept: &mut Vec<PathBuf>) -> Result<()> {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return Ok(());
    };
    if !meta.is_dir() {
        return fs::remove_file(path).with_context(|| format!("removing {}", path.display()));
    }
    if is_subvolume(path) {
        kept.push(path.to_path_buf());
        return Ok(());
    }
    for entry in fs::read_dir(path).with_context(|| format!("reading {}", path.display()))? {
        remove_keeping_subvolumes(&entry?.path(), kept)?;
    }
    match fs::remove_dir(path) {
        Err(e) if e.raw_os_error() == Some(libc::ENOTEMPTY) => Ok(()),
        other => other.with_context(|| format!("removing {}", path.display())),
    }
}

/// What `finish` did.
#[derive(Debug, PartialEq)]
pub enum Finished {
    /// No move scheduled or done: the root stays the top level.
    NotMoved,
    /// The root lives in `@`; the subvolumes left at the top level that the old root's files led to, if any.
    InAt { kept: Vec<PathBuf> },
}

/// With btrfs's top level mounted read-write at `top`: does what's left of a scheduled move of the root into `@`: a snapshot of the top level becomes `@`, its fstab mounts `@` as the root, subvolumes nested in the old root's directories move to the same place in `@`, top-level subvolumes the old root used by path get fstab lines, and the old root's files go from the top level; every top-level subvolume stays untouched. Each step is safe to run again after a crash.
pub fn finish(top: &Path) -> Result<Finished> {
    let at = top.join(super::ROOT);
    if !top.join(MARK).exists() {
        return Ok(match top.join(MOVED).exists() && at.is_dir() {
            true => Finished::InAt { kept: Vec::new() },
            false => Finished::NotMoved,
        });
    }
    if !at.exists() {
        let new = top.join(NEW);
        if new.exists() {
            btrfs(
                &["-q", "subvolume", "delete"],
                &new,
                "delete a cut-short copy of the root",
            )?;
        }
        let output = Command::new("btrfs")
            .args(["-q", "subvolume", "snapshot"])
            .arg(top)
            .arg(&new)
            .output()
            .context("running btrfs")?;
        if !output.status.success() {
            bail!(
                "btrfs couldn't copy the root into {}: {}",
                new.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        crate::durable::sync_filesystems_checked(&[top])?;
        crate::durable::rename(&new, &at).with_context(|| format!("renaming {} to {}", new.display(), at.display()))?;
        crate::durable::sync_filesystems_checked(&[top])?;
    }
    crate::testhooks::mark("move-root-snapshotted");
    let listing = Command::new("btrfs")
        .args(["subvolume", "list"])
        .arg(top)
        .output()
        .context("running btrfs")?;
    let children = top_level_children(&String::from_utf8_lossy(&listing.stdout));
    let nested: Vec<String> = children.iter().filter(|path| path.contains('/')).cloned().collect();
    let siblings: Vec<String> = children
        .iter()
        .filter(|path| !path.contains('/') && !path.starts_with(['@', '.']) && at.join(path).is_dir())
        .cloned()
        .collect();
    let fstab = at.join("etc/fstab");
    if let Ok(text) = fs::read_to_string(&fstab)
        && !top.join(FSTAB_DONE).exists()
    {
        let device = filesystem_uuid(top).map(|uuid| format!("UUID={uuid}"));
        crate::durable::write(
            &fstab,
            moved_fstab(&text, &nested, &siblings, device.as_deref()).as_bytes(),
        )?;
        crate::durable::write(&top.join(FSTAB_DONE), b"")?;
        crate::testhooks::mark("move-root-fstab");
    }
    for name in [MARK, MOVED, FSTAB_DONE, NEW] {
        let _ = fs::remove_file(at.join(name));
        let _ = fs::remove_dir(at.join(name));
    }
    for name in children
        .iter()
        .filter(|path| !path.contains('/') && path.starts_with(['@', '.']))
    {
        let _ = fs::remove_dir(at.join(name));
    }
    for path in &nested {
        let (from, to) = (top.join(path), at.join(path));
        if !is_subvolume(&from) {
            continue;
        }
        if to.is_dir() && !is_subvolume(&to) {
            let _ = fs::remove_dir(&to);
        }
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        crate::durable::rename(&from, &to).with_context(|| format!("moving subvolume {} into @", from.display()))?;
    }
    crate::durable::sync_filesystems_checked(&[top])?;
    crate::testhooks::mark("move-root-nested-moved");
    let mut kept = Vec::new();
    for entry in fs::read_dir(top).with_context(|| format!("reading {}", top.display()))? {
        let path = entry?.path();
        let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        if [MARK, MOVED, FSTAB_DONE].contains(&name.as_str()) || is_subvolume(&path) {
            continue;
        }
        remove_keeping_subvolumes(&path, &mut kept)?;
    }
    crate::testhooks::mark("move-root-cleaned");
    crate::durable::write(&top.join(MOVED), b"")?;
    let _ = fs::remove_file(top.join(FSTAB_DONE));
    fs::remove_file(top.join(MARK)).context("removing the move's mark")?;
    crate::durable::sync_filesystems_checked(&[top])?;
    Ok(Finished::InAt { kept })
}

#[cfg(test)]
mod tests {
    use super::moved_fstab;

    #[test]
    fn a_subvolume_beside_the_root_gets_a_line_also_when_fstab_has_no_root_line() {
        let fstab = "UUID=AB-CD /efi vfat defaults 0 2\n";
        let text = moved_fstab(fstab, &[], &["beside".to_string()], Some("UUID=1234"));
        assert!(text.contains("UUID=1234 /beside btrfs subvol=/beside 0 0"), "{text}");
        let with_root = "UUID=1234 / btrfs rw 0 0\n";
        let text = moved_fstab(with_root, &[], &["beside".to_string()], None);
        assert!(
            text.contains("UUID=1234 / btrfs rw,subvol=/@ 0 0")
                && text.contains("UUID=1234 /beside btrfs subvol=/beside 0 0"),
            "{text}"
        );
    }
}
