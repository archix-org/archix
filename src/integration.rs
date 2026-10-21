use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::packages::TRANSACTION_ENV;
use crate::paths::Paths;
use crate::repos::{Repo, is_official};

const BEFORE_BEGIN: &str = "# BEGIN archix custom repos (before the official repos)";
const BEFORE_END: &str = "# END archix custom repos (before the official repos)";
const AFTER_BEGIN: &str = "# BEGIN archix custom repos (after the official repos)";
const AFTER_END: &str = "# END archix custom repos (after the official repos)";

/// The PreTransaction hook that stops plain pacman on a deployment; named to sort before every other hook.
const GUARD_HOOK: &str = "00-archix-deployment.hook";
/// What a deployment's pacman.conf has instead of CheckSpace, whose check fails on the read-only /usr before any PreTransaction hook runs.
const CHECK_SPACE_OFF: &str =
    "#CheckSpace   # off on archix deployments: pacman can't install here, and archix checks space itself";

/// What plain pacman, or an AUR helper running it, gets on a system running from a deployment.
pub const PACMAN_REFUSAL: &str = "archix: this system runs from a deployment whose /usr, /opt and /boot are read-only, so pacman, and AUR helpers like yay that run it, can't change its packages. Change them with archix, which builds a new deployment that boots at the next restart: `archix install <package>` (or `archix -S`) to add one, `archix remove <package>` (or `archix -R`) to take one out, `archix upgrade` (or `archix -Syu`) to update everything, and `archix switch` to apply your config. pacman -Q, -Ss, -Si, -F and -Sw work as before. See https://archix.dev/docs/concepts/pacman-and-yay/";

/// For the PreTransaction hook, which alpm runs chrooted into pacman's root: plain pacman may change the system under `root` unless it runs from a deployment with a read-only /usr; archix's own transactions always may.
pub fn pacman_allowed(root: &Path) -> bool {
    std::env::var_os(TRANSACTION_ENV).is_some()
        || !crate::system::deploy_boot::uses_deployments(root)
        || !crate::system::deploy_boot::read_only(&root.join("usr"))
}

/// pacman.conf with an active CheckSpace turned off, so the guard hook gets to explain; None when there's none.
pub fn without_check_space(conf: &str) -> Option<String> {
    if !conf.lines().any(|line| line.trim() == "CheckSpace") {
        return None;
    }
    let lines: Vec<&str> = conf
        .lines()
        .map(|line| {
            if line.trim() == "CheckSpace" {
                CHECK_SPACE_OFF
            } else {
                line
            }
        })
        .collect();
    Some(lines.join("\n") + "\n")
}

/// pacman.conf with the CheckSpace archix turned off back on, for a system without deployments.
fn with_check_space(conf: &str) -> Option<String> {
    if !conf.lines().any(|line| line == CHECK_SPACE_OFF) {
        return None;
    }
    let lines: Vec<&str> = conf
        .lines()
        .map(|line| if line == CHECK_SPACE_OFF { "CheckSpace" } else { line })
        .collect();
    Some(lines.join("\n") + "\n")
}

/// Rewrites a pacman.conf through `edit` when it changes it.
fn edit_pacman_conf(conf: &Path, edit: fn(&str) -> Option<String>) -> Result<()> {
    let Ok(current) = fs::read_to_string(conf) else {
        return Ok(());
    };
    if let Some(wanted) = edit(&current) {
        let tmp = conf.with_extension("conf.archix");
        fs::write(&tmp, wanted).with_context(|| format!("writing {}", tmp.display()))?;
        fs::rename(&tmp, conf).with_context(|| format!("writing {}", conf.display()))?;
    }
    Ok(())
}

/// Packages changed by one pacman or AUR-helper transaction that archix didn't run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutsideChange {
    pub at: u64,
    pub packages: Vec<String>,
}

/// The files archix installs so the rest of the system tells it (and you) about things, at their absolute paths.
pub fn files(paths: &Paths, archix: &Path) -> Vec<(PathBuf, String)> {
    let notes = paths.notes();
    let notes = notes.display();
    let hook = format!(
        "[Trigger]\nOperation = Install\nOperation = Upgrade\nOperation = Remove\nType = Package\nTarget = *\n\n\
         [Action]\nDescription = Recording package changes for archix...\nWhen = PostTransaction\n\
         Exec = {} hook-record\nNeedsTargets\n",
        archix.display()
    );
    let guard = format!(
        "[Trigger]\nOperation = Install\nOperation = Upgrade\nOperation = Remove\nType = Package\nTarget = *\n\n\
         [Action]\nDescription = Checking that pacman can change this system...\nWhen = PreTransaction\n\
         Exec = /bin/sh -c 'test -n \"$ARCHIX_TRANSACTION\" || test ! -d /archix/deploy || test -w /usr || exec {} pacman-guard'\nAbortOnFail\n",
        archix.display()
    );
    let profile = format!("if [ -s {notes} ]; then\n  cat {notes} >&2\nfi\n");
    let autostart = format!(
        "[Desktop Entry]\nType=Application\nName=archix status\nNoDisplay=true\n\
         Exec=sh -c 'command -v notify-send >/dev/null || exit 0; \
         [ -s {notes} ] && notify-send -u critical archix \"$(cat {notes})\"; exit 0'\n"
    );
    let system = Paths::system();
    vec![
        (system.pacman_hooks.join("archix.hook"), hook),
        (system.pacman_hooks.join(GUARD_HOOK), guard),
        (system.profile_d.join("archix.sh"), profile),
        (system.autostart.join("archix-status.desktop"), autostart),
    ]
}

/// The statically linked archix the initramfs hook copies; the package installs it.
pub const STATIC_BINARY: &str = "/usr/lib/archix/archix-static";

/// Files for managed booting at their absolute paths: the early-boot generation check, the boot-complete promotion, and the deployment activation unit and initramfs hook.
pub fn boot_files(archix: &Path) -> Vec<(PathBuf, String)> {
    let unit = format!(
        "[Unit]\nDescription=archix: match the booted generation\nDefaultDependencies=no\nAfter=local-fs.target\n\
         Before=sysinit.target\n\n\
         [Service]\nType=oneshot\nExecStart={} boot-check\nStandardOutput=journal+console\n\n[Install]\nWantedBy=sysinit.target\n",
        archix.display()
    );
    let complete = format!(
        "[Unit]\nDescription=archix: make a generation that booted fine the default\nAfter=systemd-bless-boot.service\n\n\
         [Service]\nType=oneshot\nExecStart={} boot-complete\n\n[Install]\nWantedBy=multi-user.target\n",
        archix.display()
    );
    let flatpak = format!(
        "[Unit]\nDescription=archix: install the Flatpak apps left for this deployment's first boot\n\
         Wants=network-online.target\nAfter=network-online.target\nConditionDirectoryNotEmpty={}\n\n\
         [Service]\nType=oneshot\nExecStart={} deploy-flatpak\nStandardOutput=journal+console\n\n[Install]\nWantedBy=multi-user.target\n",
        crate::flatpak::PENDING_DIR,
        archix.display()
    );
    let etc_merge = format!(
        "[Unit]\nDescription=archix: carry /etc changes into the deployment that boots next\n{}\n\n\
         [Service]\nExecStart={} etc-merge\n{}\nStandardOutput=journal\n\n[Install]\nWantedBy=basic.target\n",
        crate::etcmerge::UNIT_ORDER.join("\n"),
        archix.display(),
        crate::etcmerge::STOP_TIMEOUT
    );
    let etc_merge_boot = format!(
        "[Unit]\nDescription=archix: carry what a power cut kept from the /etc merge\nDefaultDependencies=no\n\
         After=systemd-remount-fs.service\nBefore=systemd-sysusers.service archix-deploy-activate.service sysinit.target shutdown.target\n\
         Conflicts=shutdown.target\nConditionDirectoryNotEmpty={}/etc-merge\n\n\
         [Service]\nType=oneshot\nExecStart={} etc-merge --boot\nStandardOutput=journal+console\n\n[Install]\nWantedBy=sysinit.target\n",
        Paths::system().state.display(),
        archix.display()
    );
    let root = Path::new("/");
    let mut files = vec![
        (root.join("etc/systemd/system/archix-etc-merge.service"), etc_merge),
        (
            root.join("etc/systemd/system/archix-etc-merge-boot.service"),
            etc_merge_boot,
        ),
        (root.join("etc/systemd/system/archix-boot.service"), unit),
        (root.join("etc/systemd/system/archix-boot-complete.service"), complete),
        (root.join("etc/systemd/system/archix-flatpak.service"), flatpak),
    ];
    files.push(crate::system::deploy_boot::activate_file(root, archix));
    files.extend(crate::system::deploy_boot::hook_files(root));
    files
}

/// The links that start archix's boot units, and the units they name.
pub fn boot_wants() -> Vec<(PathBuf, String)> {
    let mut wants: Vec<(PathBuf, String)> = [
        ("sysinit.target.wants", "archix-boot.service"),
        ("multi-user.target.wants", "archix-boot-complete.service"),
        ("multi-user.target.wants", "archix-flatpak.service"),
        ("basic.target.wants", "archix-etc-merge.service"),
        ("sysinit.target.wants", "archix-etc-merge-boot.service"),
    ]
    .map(|(dir, unit)| {
        (
            Path::new("/etc/systemd/system").join(dir).join(unit),
            format!("/etc/systemd/system/{unit}"),
        )
    })
    .into();
    wants.push(crate::system::deploy_boot::activate_wants(Path::new("/")));
    wants
}

/// Where this archix was started from, also after a switch had pacman replace it, when Linux appends " (deleted)".
pub fn own_path() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("finding the archix binary")?;
    let text = exe.to_string_lossy();
    Ok(match text.strip_suffix(" (deleted)") {
        Some(path) => PathBuf::from(path),
        None => exe,
    })
}

/// Writes every integration file and boot link that's missing or out of date where `place` puts each absolute path: into a staging root, or onto the live root before its first layout.
pub fn write_all(paths: &Paths, archix: &Path, place: &dyn Fn(&Path) -> Result<PathBuf>) -> Result<()> {
    for (path, content) in files(paths, archix).into_iter().chain(boot_files(archix)) {
        let target = place(&path)?;
        if fs::read_to_string(&target).ok().as_deref() == Some(content.as_str()) {
            continue;
        }
        if let Some(dir) = target.parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let mode = if content.starts_with("#!") { 0o755 } else { 0o644 };
        fs::write(&target, &content).with_context(|| format!("writing {}", target.display()))?;
        fs::set_permissions(&target, std::os::unix::fs::PermissionsExt::from_mode(mode))?;
    }
    for (link, unit) in boot_wants() {
        let target = place(&link)?;
        if let Some(dir) = target.parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        if fs::symlink_metadata(&target).is_err() {
            std::os::unix::fs::symlink(unit, &target).with_context(|| format!("linking {}", target.display()))?;
        }
    }
    edit_pacman_conf(&place(&Paths::system().pacman_conf)?, without_check_space)
}

/// Where a staging view gets the running archix when the deployment has none that writes its own integration.
const RUNNING_IN_VIEW: &str = "/run/archix-running";

/// Writes the integration into a staging root through its view: by the staged static archix, so an archix the switch upgrades writes its own version, else by this one; the units run `archix`.
pub fn write_staged(staging: &crate::deploy::Staging, archix: &Path) -> Result<()> {
    let exec = archix.to_str().context("archix's path isn't UTF-8")?;
    if staging.read_path(Path::new(STATIC_BINARY))?.is_file() {
        let output = staging
            .transaction()?
            .output(&[STATIC_BINARY, "deploy-integrate", "--exec", exec])?;
        if output.status.success() {
            return Ok(());
        }
        eprintln!(
            "warning: the new deployment's archix couldn't write its boot integration ({}), so this archix does",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    staging
        .transaction_with(&[(archix.to_path_buf(), PathBuf::from(RUNNING_IN_VIEW))])?
        .run(&[RUNNING_IN_VIEW, "deploy-integrate", "--exec", exec])
}

/// Takes every integration file off a system without deployments, for `archix eject`.
pub fn remove(paths: &Paths) -> Result<()> {
    let root = &paths.root;
    crate::system::deploy_boot::refuse_without_hook(root, "taking archix's boot integration off")?;
    let inside = |path: &Path| root.join(path.strip_prefix("/").unwrap_or(path));
    let archix = Path::new("archix");
    let gone: Vec<PathBuf> = files(paths, archix)
        .into_iter()
        .chain(boot_files(archix))
        .map(|(path, _)| path)
        .chain(boot_wants().into_iter().map(|(link, _)| link))
        .map(|path| inside(&path))
        .collect();
    for path in gone {
        match fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(e).with_context(|| format!("removing {}", path.display()));
            }
            _ => {}
        }
    }
    edit_pacman_conf(&inside(&Paths::system().pacman_conf), with_check_space)
}

/// pacman.conf with archix's custom-repo blocks replaced by `repos`' custom ones, placed around the official repos.
pub fn with_custom_repos(conf: &str, repos: &[Repo]) -> String {
    let mut lines: Vec<&str> = Vec::new();
    let mut skipping = false;
    let mut taken_over = false;
    for line in conf.lines() {
        if let Some(section) = line.trim().strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            taken_over = repos.iter().any(|repo| repo.custom.is_some() && repo.name == section);
        }
        if line == BEFORE_BEGIN || line == AFTER_BEGIN {
            skipping = true;
        } else if line == BEFORE_END || line == AFTER_END {
            skipping = false;
            taken_over = false;
        } else if !skipping && !taken_over {
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

/// Keeps a pacman.conf's archix-managed repo sections in step with the config.
pub fn sync_pacman_conf(conf: &Path, repos: &[Repo]) -> Result<()> {
    if !conf.exists() && repos.iter().all(|repo| repo.custom.is_none()) {
        return Ok(());
    }
    let current = fs::read_to_string(conf).with_context(|| format!("reading {}", conf.display()))?;
    let wanted = with_custom_repos(&current, repos);
    if wanted != current {
        let tmp = conf.with_extension("conf.archix");
        fs::write(&tmp, &wanted)?;
        fs::rename(&tmp, conf).with_context(|| format!("writing {}", conf.display()))?;
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
        let handwritten =
            "[options]\n\n[cachyos]\nServer = https://old/$repo\n\n[core]\nInclude = /etc/pacman.d/mirrorlist\n";
        let taken = with_custom_repos(handwritten, &repos);
        assert_eq!(taken.matches("[cachyos]").count(), 1, "{taken}");
        assert!(!taken.contains("https://old"), "{taken}");
    }

    #[test]
    fn a_root_using_deployments_keeps_its_boot_integration() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        fs::create_dir_all(dir.path().join("archix/deploy/1")).unwrap();
        let place = |path: &Path| Ok(dir.path().join(path.strip_prefix("/").unwrap()));
        write_all(&paths, Path::new("/usr/bin/archix"), &place).unwrap();
        let hook = dir.path().join("etc/initcpio/hooks/archix-deploy");
        assert!(hook.exists());
        assert!(dir.path().join("etc/mkinitcpio.conf.d/archix-deploy.conf").exists());
        assert!(
            dir.path()
                .join("etc/systemd/system/sysinit.target.wants/archix-deploy-activate.service")
                .is_symlink()
        );
        assert!(remove(&paths).is_err(), "eject can't take the hook off");
        assert!(hook.exists());
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
        assert!(
            files[1]
                .1
                .contains("test -w /usr || exec /opt/archix/bin/archix pacman-guard'\nAbortOnFail"),
            "the shell lets archix's own and writable roots through without needing archix in them: {}",
            files[1].1
        );
        assert!(files[1].0.ends_with("00-archix-deployment.hook"));
        assert!(files[2].1.contains("state/notes"));
    }

    #[test]
    fn a_deployments_pacman_conf_leaves_the_space_check_to_archix_and_eject_restores_it() {
        let conf =
            "[options]\nHoldPkg = pacman\nCheckSpace\n#VerbosePkgLists\n\n[core]\nInclude = /etc/pacman.d/mirrorlist\n";
        let off = without_check_space(conf).unwrap();
        assert!(!off.lines().any(|line| line.trim() == "CheckSpace"), "{off}");
        assert!(off.contains("#VerbosePkgLists\n\n[core]\n"), "the rest is kept: {off}");
        assert_eq!(without_check_space(&off), None, "turning it off is idempotent");
        assert_eq!(with_check_space(&off).as_deref(), Some(conf));
        assert_eq!(
            without_check_space("[options]\n#CheckSpace\n"),
            None,
            "a commented one stays as it is"
        );
        assert_eq!(with_check_space("[options]\n#CheckSpace\n"), None);

        let dir = tempfile::tempdir().unwrap();
        let etc = dir.path().join("etc");
        fs::create_dir_all(&etc).unwrap();
        fs::write(etc.join("pacman.conf"), conf).unwrap();
        let place = |path: &Path| Ok(dir.path().join(path.strip_prefix("/").unwrap()));
        write_all(&Paths::under(dir.path()), Path::new("/usr/bin/archix"), &place).unwrap();
        assert_eq!(fs::read_to_string(etc.join("pacman.conf")).unwrap(), off);
        remove(&Paths::under(dir.path())).unwrap();
        assert_eq!(fs::read_to_string(etc.join("pacman.conf")).unwrap(), conf);
    }

    #[test]
    fn plain_pacman_may_change_a_root_without_deployments_or_with_a_writable_usr() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("usr")).unwrap();
        assert!(pacman_allowed(dir.path()), "no deployments");
        fs::create_dir_all(dir.path().join("archix/deploy/1")).unwrap();
        assert!(pacman_allowed(dir.path()), "a writable /usr, as in a staging root");
    }
}
