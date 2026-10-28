//! What a machine carries from one deployment to the next without its config declaring it, which a machine built from the config won't have.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use anyhow::Result;

use crate::capture::{self, Installed};

/// Lines each section shows unless all are asked for.
pub const SHOWN: usize = 10;

/// What's carried, by section, each sorted.
#[derive(Debug, Default, PartialEq)]
pub struct Carried {
    /// Under /usr (but /usr/local) and /opt, no package's: a directory no package owns as one line with its file count.
    pub trees: Vec<String>,
    /// In /etc, no package's; a directory no package owns as one line with its file count.
    pub unowned_etc: Vec<String>,
    /// Package config files in /etc edited from what their package shipped.
    pub edited_etc: Vec<String>,
    /// Credentials, which no config carries: a secret directory as one line with its file count.
    pub secrets: Vec<String>,
    /// Package config files this user can't read, so not checked for edits.
    pub unreadable: usize,
}

pub struct Scan<'a> {
    /// The root whose usr and opt the next deployment is staged from: the current deployment's.
    pub trees: &'a Path,
    /// The live root, whose /etc the next deployment takes.
    pub root: &'a Path,
    pub installed: &'a BTreeMap<String, Installed>,
    /// Absolute paths the config, or archix itself, writes.
    pub declared: &'a BTreeSet<String>,
    /// Packages whose config files archix leaves alone.
    pub ignored: &'a BTreeSet<String>,
    /// The config directory as a path on the machine, which is never listed.
    pub config_dir: Option<String>,
}

pub fn find(scan: &Scan) -> Result<Carried> {
    let owned = capture::owned(scan.installed);
    let mut trees = Vec::new();
    for top in ["usr", "opt"] {
        unowned_tree(scan.trees, top, &owned, &mut trees);
    }
    trees.sort();
    let found = capture::config_files_owned(scan.root, scan.installed, &owned, scan.declared)?;
    let left_alone: BTreeSet<&str> = scan
        .installed
        .iter()
        .filter(|(name, _)| scan.ignored.contains(*name))
        .flat_map(|(_, pkg)| pkg.backups.iter().map(|(path, _)| path.as_str()))
        .collect();
    let listed = |path: &str| {
        !left_alone.contains(path)
            && !scan
                .config_dir
                .as_ref()
                .is_some_and(|dir| path.starts_with(&format!("{}/", dir.trim_end_matches('/'))))
    };
    let (edited, unowned): (Vec<_>, Vec<_>) = found
        .files
        .into_iter()
        .filter(|file| listed(&file.path))
        .partition(|file| file.edited);
    let secrets: Vec<String> = found.secrets.into_iter().filter(|path| listed(path)).collect();
    let mut secrets = capture::group_secrets(&secrets);
    secrets.extend(capture::secrets_elsewhere(scan.root));
    secrets.sort();
    let mut edited_etc: Vec<String> = edited.into_iter().map(|file| file.path).collect();
    edited_etc.sort();
    Ok(Carried {
        trees,
        unowned_etc: by_directory(unowned.into_iter().map(|file| file.path), &owned),
        edited_etc,
        secrets,
        unreadable: found.unreadable.iter().filter(|path| listed(path)).count(),
    })
}

/// Each path, or the outermost directory below its top one (/etc) that no package owns, with how many of the paths it holds.
fn by_directory(paths: impl Iterator<Item = String>, owned: &BTreeSet<String>) -> Vec<String> {
    let mut dirs: BTreeMap<String, usize> = BTreeMap::new();
    let mut out = Vec::new();
    for path in paths {
        let unowned = path
            .match_indices('/')
            .skip(2)
            .map(|(at, _)| &path[..=at])
            .find(|dir| !owned.contains(*dir));
        match unowned {
            Some(dir) => *dirs.entry(dir.to_string()).or_default() += 1,
            None => out.push(path),
        }
    }
    out.extend(dirs.into_iter().map(|(dir, n)| with_count(&dir, n)));
    out.sort();
    out
}

fn with_count(dir: &str, n: usize) -> String {
    match n {
        1 => format!("{dir} (1 file)"),
        n => format!("{dir} ({n} files)"),
    }
}

/// Paths under `/top` of `base` no package owns, but what pacman hooks make anyway, /usr/local and other filesystems.
fn unowned_tree(base: &Path, top: &str, owned: &BTreeSet<String>, out: &mut Vec<String>) {
    let Ok(meta) = fs::symlink_metadata(base.join(top)) else {
        return;
    };
    if !meta.is_dir() {
        return;
    }
    let device = meta.dev();
    let mut stack = vec![format!("/{top}")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(base.join(dir.trim_start_matches('/'))) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else { continue };
            let path = format!("{dir}/{}", entry.file_name().to_string_lossy());
            if !kind.is_dir() {
                if !owned.contains(&path) && !capture::is_generated(&path) {
                    out.push(path);
                }
                continue;
            }
            let as_dir = format!("{path}/");
            if path == "/usr/local" || entry.metadata().is_ok_and(|meta| meta.dev() != device) {
                continue;
            }
            if owned.contains(&as_dir) {
                stack.push(path);
            } else if !capture::is_generated(&as_dir) {
                let n = files_under(base, &path, device);
                if n > 0 {
                    out.push(with_count(&as_dir, n));
                }
            }
        }
    }
}

/// Files and symlinks under `dir` but those pacman hooks make, on `device` only.
fn files_under(base: &Path, dir: &str, device: u64) -> usize {
    let mut n = 0;
    let mut stack = vec![dir.to_string()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(base.join(dir.trim_start_matches('/'))) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else { continue };
            let path = format!("{dir}/{}", entry.file_name().to_string_lossy());
            if kind.is_dir() {
                if !capture::is_generated(&format!("{path}/"))
                    && entry.metadata().is_ok_and(|meta| meta.dev() == device)
                {
                    stack.push(path);
                }
            } else if !capture::is_generated(&path) {
                n += 1;
            }
        }
    }
    n
}

impl Carried {
    pub fn is_empty(&self) -> bool {
        self.trees.is_empty() && self.unowned_etc.is_empty() && self.edited_etc.is_empty() && self.secrets.is_empty()
    }

    /// The report, each section cut to `SHOWN` lines unless `all`; `command` is what lists them all with --all.
    pub fn lines(&self, all: bool, command: &str) -> Vec<String> {
        let mut out = Vec::new();
        if self.is_empty() {
            return out;
        }
        out.push(
            "carried on this machine but not in the config (a machine built from this config won't have them):".into(),
        );
        let sections = [
            ("no package's, in /usr and /opt", &self.trees),
            ("no package's, in /etc", &self.unowned_etc),
            ("edited from what their package shipped, in /etc", &self.edited_etc),
            ("secrets, which no config carries", &self.secrets),
        ];
        let mut cut = false;
        for (title, lines) in sections {
            if lines.is_empty() {
                continue;
            }
            out.push(format!("  {title}:"));
            let shown = if all { lines.len() } else { lines.len().min(SHOWN) };
            out.extend(lines[..shown].iter().map(|line| format!("    {line}")));
            if shown < lines.len() {
                out.push(format!("    and {} more", lines.len() - shown));
                cut = true;
            }
        }
        if cut {
            out.push(format!("  `archix {command} --all` lists them all"));
        }
        if self.unreadable > 0 {
            out.push(match self.unreadable {
                1 => format!(
                    "  1 package config file only root can read wasn't checked for edits; `sudo archix {command}` checks it"
                ),
                n => format!(
                    "  {n} package config files only root can read weren't checked for edits; `sudo archix {command}` checks them"
                ),
            });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, path: &str) {
        let path = root.join(path.trim_start_matches('/'));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "x = 1\n").unwrap();
    }

    #[test]
    fn unowned_trees_edited_and_secret_files_are_listed_but_declared_generated_and_usr_local_ones() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        for path in [
            "/opt/foo/bin/foo",
            "/opt/foo/lib/libfoo.so",
            "/opt/foo/share/data",
            "/opt/owned/plugin.so",
            "/opt/owned/bin",
            "/usr/bin/handmade",
            "/usr/bin/owned",
            "/usr/lib/python3/site-packages/pipped/__init__.py",
            "/usr/lib/python3/site-packages/pipped/__pycache__/__init__.pyc",
            "/usr/lib/modules/7.0-arch1/modules.dep",
            "/usr/lib/modules/7.0-arch1/updates/dkms/vbox.ko.zst",
            "/usr/share/icons/hicolor/icon-theme.cache",
            "/usr/local/bin/mine",
            "/etc/app/config.yml",
            "/etc/app/extra.yml",
            "/etc/declared.conf",
            "/etc/edited.conf",
            "/etc/ignored.conf",
            "/etc/shipped.conf",
            "/etc/wireguard/wg0.conf",
            "/etc/ssh/ssh_host_ed25519_key",
            "/etc/archix/init.lua",
        ] {
            write(root, path);
        }
        let package = |files: &[&str], backups: &[(&str, &str)]| Installed {
            files: files.iter().map(|f| f.to_string()).collect(),
            backups: backups.iter().map(|(p, s)| (p.to_string(), s.to_string())).collect(),
            ..Default::default()
        };
        let unchanged = capture::md5_bytes(b"x = 1\n");
        let installed = BTreeMap::from([
            (
                "pkg".to_string(),
                package(
                    &[
                        "etc/",
                        "etc/edited.conf",
                        "etc/shipped.conf",
                        "opt/",
                        "opt/owned/",
                        "opt/owned/bin",
                        "usr/",
                        "usr/bin/",
                        "usr/bin/owned",
                        "usr/lib/",
                        "usr/lib/modules/",
                        "usr/lib/modules/7.0-arch1/",
                        "usr/lib/python3/",
                        "usr/lib/python3/site-packages/",
                        "usr/share/",
                        "usr/share/icons/",
                        "usr/share/icons/hicolor/",
                    ],
                    &[("/etc/edited.conf", "0"), ("/etc/shipped.conf", &unchanged)],
                ),
            ),
            (
                "left-alone".to_string(),
                package(&["etc/ignored.conf"], &[("/etc/ignored.conf", "0")]),
            ),
        ]);
        let carried = find(&Scan {
            trees: root,
            root,
            installed: &installed,
            declared: &BTreeSet::from(["/etc/declared.conf".to_string()]),
            ignored: &BTreeSet::from(["left-alone".to_string()]),
            config_dir: None,
        })
        .unwrap();
        assert_eq!(
            carried,
            Carried {
                trees: vec![
                    "/opt/foo/ (3 files)".into(),
                    "/opt/owned/plugin.so".into(),
                    "/usr/bin/handmade".into(),
                    "/usr/lib/python3/site-packages/pipped/ (1 file)".into(),
                ],
                unowned_etc: vec!["/etc/app/ (2 files)".into()],
                edited_etc: vec!["/etc/edited.conf".into()],
                secrets: vec![
                    "/etc/ssh/ssh_host_ed25519_key".into(),
                    "/etc/wireguard/ (1 file)".into()
                ],
                unreadable: 0,
            }
        );
    }

    #[test]
    fn each_section_is_cut_short_unless_all_are_asked_for() {
        let carried = Carried {
            trees: (0..12).map(|n| format!("/opt/t{n:02}/ (1 file)")).collect(),
            secrets: vec!["/etc/wireguard/ (1 file)".into()],
            ..Default::default()
        };
        let lines = carried.lines(false, "diff");
        assert_eq!(lines.len(), 1 + 1 + SHOWN + 1 + 2 + 1, "{lines:#?}");
        assert!(lines.contains(&"    and 2 more".to_string()), "{lines:#?}");
        assert_eq!(lines.last().unwrap(), "  `archix diff --all` lists them all");
        let all = carried.lines(true, "diff");
        assert!(all.contains(&"    /opt/t11/ (1 file)".to_string()), "{all:#?}");
        assert!(!all.iter().any(|line| line.contains("more")), "{all:#?}");
        assert!(Carried::default().lines(false, "diff").is_empty());
    }

    #[test]
    fn globs_match_any_run_of_characters() {
        assert!(capture::glob(
            "/usr/lib/modules/*/modules.*",
            "/usr/lib/modules/7.0/modules.dep.bin"
        ));
        assert!(capture::glob("*/__pycache__/*", "/usr/lib/x/__pycache__/"));
        assert!(!capture::glob(
            "/usr/lib/modules/*/modules.*",
            "/usr/lib/modules/7.0/kernel/a.ko"
        ));
        assert!(capture::glob("/usr/lib/jvm/default", "/usr/lib/jvm/default"));
        assert!(!capture::glob("/usr/lib/jvm/default", "/usr/lib/jvm/default-runtime"));
    }
}
