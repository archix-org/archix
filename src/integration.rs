use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::apply::TRANSACTION_ENV;
use crate::paths::Paths;
use crate::repos::{Repo, is_official};

const BEFORE_BEGIN: &str = "# BEGIN archix custom repos (before the official repos)";
const BEFORE_END: &str = "# END archix custom repos (before the official repos)";
const AFTER_BEGIN: &str = "# BEGIN archix custom repos (after the official repos)";
const AFTER_END: &str = "# END archix custom repos (after the official repos)";

/// Packages changed by one pacman or AUR-helper transaction that archix didn't run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutsideChange {
    pub at: u64,
    pub packages: Vec<String>,
}

/// The files archix installs so the rest of the system tells it (and you) about things.
pub fn files(paths: &Paths, archix: &Path) -> Vec<(PathBuf, String)> {
    let journal = paths.journal();
    let journal = journal.display();
    let hook = format!(
        "[Trigger]\nOperation = Install\nOperation = Upgrade\nOperation = Remove\nType = Package\nTarget = *\n\n\
         [Action]\nDescription = Recording package changes for archix...\nWhen = PostTransaction\n\
         Exec = {} hook-record\nNeedsTargets\n",
        archix.display()
    );
    let profile = format!(
        "if [ -e {journal} ]; then\n  echo \"archix: a switch was interrupted; run 'sudo archix recover'\" >&2\nfi\n"
    );
    let autostart = format!(
        "[Desktop Entry]\nType=Application\nName=archix status\nNoDisplay=true\n\
         Exec=sh -c '[ -e {journal} ] && command -v notify-send >/dev/null && notify-send -u critical archix \"A switch was interrupted. Run: sudo archix recover\"'\n"
    );
    vec![
        (paths.pacman_hooks.join("archix.hook"), hook),
        (paths.profile_d.join("archix.sh"), profile),
        (paths.autostart.join("archix-status.desktop"), autostart),
    ]
}

/// The statically linked archix the rescue initramfs hook copies; the package installs it.
pub const STATIC_BINARY: &str = "/usr/lib/archix/archix-static";

/// Files for managed booting: the early-boot generation check, and the rescue hook when the static binary exists.
pub fn boot_files(paths: &Paths, archix: &Path) -> Vec<(PathBuf, String)> {
    let root = paths
        .pacman_conf
        .parent()
        .and_then(Path::parent)
        .unwrap_or(Path::new("/"));
    let unit = format!(
        "[Unit]\nDescription=archix: match the booted generation\nDefaultDependencies=no\nAfter=local-fs.target\n\
         Before=sysinit.target systemd-udev-trigger.service systemd-modules-load.service\nConditionKernelCommandLine=archix.generation\n\n\
         [Service]\nType=oneshot\nExecStart={} boot-check\nStandardOutput=journal+console\n\n[Install]\nWantedBy=sysinit.target\n",
        archix.display()
    );
    let mut files = vec![(root.join("etc/systemd/system/archix-boot.service"), unit)];
    if root.join(STATIC_BINARY.trim_start_matches('/')).exists() {
        files.push((
            root.join("etc/initcpio/install/archix-rescue"),
            format!(
                "#!/bin/bash\n\nbuild() {{\n    add_binary {STATIC_BINARY} /usr/bin/archix\n    add_runscript\n    \
                 add_file /usr/lib/archix/archix-rescue.service /usr/lib/systemd/system/archix-rescue.service\n    \
                 add_symlink /usr/lib/systemd/system/initrd.target.wants/archix-rescue.service /usr/lib/systemd/system/archix-rescue.service\n}}\n\n\
                 help() {{\n    echo \"archix rescue: restores core packages from the cache when archix.rescue is on the kernel command line\"\n}}\n"
            ),
        ));
        files.push((
            root.join("etc/initcpio/hooks/archix-rescue"),
            "#!/usr/bin/ash\n\nrun_latehook() {\n    if grep -qw archix.rescue /proc/cmdline; then\n        \
             mount -o remount,rw /new_root\n        /usr/bin/archix rescue --root /new_root\n    fi\n}\n"
                .to_string(),
        ));
        files.push((
            root.join("usr/lib/archix/archix-rescue.service"),
            "[Unit]\nDescription=archix rescue: restore core packages\nDefaultDependencies=no\nConditionKernelCommandLine=archix.rescue\n\
             After=initrd-root-fs.target\nBefore=initrd-parse-etc.service initrd-switch-root.target\n\n\
             [Service]\nType=oneshot\nExecStartPre=/usr/bin/mount -o remount,rw /sysroot\nExecStart=/usr/bin/archix rescue --root /sysroot\n\
             StandardOutput=journal+console\n"
                .to_string(),
        ));
        files.push((
            root.join("etc/mkinitcpio.conf.d/archix-rescue.conf"),
            "HOOKS+=(archix-rescue)\n".to_string(),
        ));
    }
    files
}

/// Writes the integration files that are missing or out of date, and boot ones only while archix manages booting.
pub fn ensure(paths: &Paths, boot: bool) -> Result<()> {
    let archix = std::env::current_exe().context("finding the archix binary")?;
    let root = paths
        .pacman_conf
        .parent()
        .and_then(Path::parent)
        .unwrap_or(Path::new("/"))
        .to_path_buf();
    let wants = root.join("etc/systemd/system/sysinit.target.wants/archix-boot.service");
    if boot {
        if let Some(dir) = wants.parent() {
            fs::create_dir_all(dir)?;
        }
        if fs::symlink_metadata(&wants).is_err() {
            std::os::unix::fs::symlink("/etc/systemd/system/archix-boot.service", &wants)?;
        }
    } else {
        let _ = fs::remove_file(&wants);
        for (path, _) in boot_files(paths, &archix) {
            let _ = fs::remove_file(path);
        }
    }
    let boot_files = if boot { boot_files(paths, &archix) } else { Vec::new() };
    for (path, content) in files(paths, &archix).into_iter().chain(boot_files) {
        if fs::read_to_string(&path).ok().as_deref() == Some(content.as_str()) {
            continue;
        }
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        fs::write(&path, content).with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(())
}

/// pacman.conf with archix's custom-repo blocks replaced by `repos`' custom ones, placed around the official repos.
pub fn with_custom_repos(conf: &str, repos: &[Repo]) -> String {
    let mut lines: Vec<&str> = Vec::new();
    let mut skipping = false;
    for line in conf.lines() {
        if line == BEFORE_BEGIN || line == AFTER_BEGIN {
            skipping = true;
        } else if line == BEFORE_END || line == AFTER_END {
            skipping = false;
        } else if !skipping {
            lines.push(line);
        }
    }
    let block = |begin: &str, end: &str, before: bool| -> Vec<String> {
        let mut out = Vec::new();
        for repo in repos {
            let Some(custom) = repo.custom.as_ref().filter(|c| c.before_official == before) else {
                continue;
            };
            out.push(format!("[{}]", repo.name));
            out.push(format!("SigLevel = {}", custom.sig_level));
            out.extend(custom.servers.iter().map(|server| format!("Server = {server}")));
            out.push(String::new());
        }
        if out.is_empty() {
            return out;
        }
        let mut wrapped = vec![begin.to_string()];
        wrapped.extend(out);
        wrapped.push(end.to_string());
        wrapped
    };
    let before = block(BEFORE_BEGIN, BEFORE_END, true);
    let after = block(AFTER_BEGIN, AFTER_END, false);
    let first_official = lines.iter().position(|line| {
        line.trim()
            .strip_prefix('[')
            .and_then(|l| l.strip_suffix(']'))
            .is_some_and(is_official)
    });
    let mut out: Vec<String> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if Some(i) == first_official {
            out.extend(before.iter().cloned());
        }
        out.push(line.to_string());
    }
    if first_official.is_none() {
        out.extend(before);
    }
    if !after.is_empty() {
        if out.last().is_some_and(|l| !l.is_empty()) {
            out.push(String::new());
        }
        out.extend(after);
    }
    let mut text = out.join("\n");
    text.push('\n');
    text
}

/// Keeps /etc/pacman.conf's archix-managed repo sections in step with the config.
pub fn sync_pacman_conf(paths: &Paths, repos: &[Repo]) -> Result<()> {
    let current =
        fs::read_to_string(&paths.pacman_conf).with_context(|| format!("reading {}", paths.pacman_conf.display()))?;
    let wanted = with_custom_repos(&current, repos);
    if wanted != current {
        let tmp = paths.pacman_conf.with_extension("conf.archix");
        fs::write(&tmp, &wanted)?;
        fs::rename(&tmp, &paths.pacman_conf).with_context(|| format!("writing {}", paths.pacman_conf.display()))?;
    }
    Ok(())
}

/// Called by the pacman hook with the transaction's package names; archix's own transactions are skipped.
pub fn record(paths: &Paths, packages: Vec<String>) -> Result<Option<OutsideChange>> {
    if std::env::var_os(TRANSACTION_ENV).is_some() || packages.is_empty() {
        return Ok(None);
    }
    let at = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let change = OutsideChange { at, packages };
    let mut changes = outside_changes(paths)?;
    changes.push(change.clone());
    let path = paths.outside_changes();
    fs::create_dir_all(&paths.state)?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_string_pretty(&changes)?)?;
    fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))?;
    Ok(Some(change))
}

pub fn outside_changes(paths: &Paths) -> Result<Vec<OutsideChange>> {
    let path = paths.outside_changes();
    match fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Forgets recorded outside changes once a switch has reconciled them with the lock.
pub fn clear_outside_changes(paths: &Paths) -> Result<()> {
    match fs::remove_file(paths.outside_changes()) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_paths(root: &Path) -> Paths {
        let mut paths = Paths::system();
        paths.state = root.join("state");
        paths.pacman_hooks = root.join("hooks");
        paths.profile_d = root.join("profile.d");
        paths.autostart = root.join("autostart");
        paths
    }

    #[test]
    fn records_accumulate_until_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let paths = temp_paths(dir.path());
        record(&paths, vec!["zlib".into()]).unwrap();
        record(&paths, vec!["lemon".into(), "sqlite".into()]).unwrap();
        let changes = outside_changes(&paths).unwrap();
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[1].packages, ["lemon", "sqlite"]);
        clear_outside_changes(&paths).unwrap();
        assert!(outside_changes(&paths).unwrap().is_empty());
    }

    #[test]
    fn custom_repo_blocks_go_around_the_official_repos_and_are_replaced() {
        use crate::repos::Custom;
        let custom = |before: bool| Custom {
            servers: vec!["https://example/$repo".into()],
            keys: vec![],
            keyserver: String::new(),
            sig_level: "Optional".into(),
            before_official: before,
        };
        let repos = vec![
            Repo {
                name: "cachyos".into(),
                custom: Some(custom(true)),
            },
            Repo::official("core"),
            Repo {
                name: "late".into(),
                custom: Some(custom(false)),
            },
        ];
        let conf = "[options]\nHoldPkg = pacman\n\n[core]\nInclude = /etc/pacman.d/mirrorlist\n";
        let once = with_custom_repos(conf, &repos);
        let cachyos = once.find("[cachyos]").unwrap();
        assert!(cachyos < once.find("[core]").unwrap() && once.find("[late]").unwrap() > once.find("[core]").unwrap());
        assert_eq!(with_custom_repos(&once, &repos), once, "rewriting is idempotent");
        assert_eq!(
            with_custom_repos(&once, &repos[1..2]).trim_end(),
            conf.trim_end(),
            "removing repos removes blocks"
        );
    }

    #[test]
    fn hook_runs_this_binary() {
        let dir = tempfile::tempdir().unwrap();
        let paths = temp_paths(dir.path());
        let files = files(&paths, Path::new("/opt/archix/bin/archix"));
        assert!(
            files[0]
                .1
                .contains("Exec = /opt/archix/bin/archix hook-record\nNeedsTargets")
        );
        assert!(files[1].1.contains("state/journal.json"));
    }
}
