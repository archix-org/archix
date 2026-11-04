//! A system archix v0.2.0 laid out as deployments (`/archix/deploy/N`, `/archix/current`), which this archix manages in place and has no migration from, so it refuses to run on one.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// The sentence the refusal opens with, which the self-upgrade VM test matches.
pub const REFUSAL: &str =
    "this system uses archix's deployments model, which this version no longer supports; it was left unchanged";

/// The subcommands only a deployments system calls: its initramfs, its boot units, its switch and its pacman hooks.
pub const ENTRY_POINTS: &[&str] = &[
    "deploy-mount",
    "deploy-activate",
    "deploy-integrate",
    "pacman-guard",
    "hook-record",
];

/// The initcpio hook `deploy-integrate` leaves in a v0.2.0 switch's staging root, whose build fails that switch's mkinitcpio before it commits.
pub const HOOK: &str = "etc/initcpio/install/archix-refuse-deployments";
pub const HOOK_CONF: &str = "etc/mkinitcpio.conf.d/archix-refuse-deployments.conf";

/// What marks `root` as laid out as deployments, if anything does.
pub fn found(root: &Path) -> Option<String> {
    let inside = |path: &str| root.join(path.trim_start_matches('/'));
    let current = fs::read_link(inside("/archix/current")).ok();
    if let Some(target) = current.filter(|target| target.starts_with("deploy")) {
        return Some(format!("/archix/current points at {}", target.display()));
    }
    if inside("/archix/deploy").is_dir() {
        return Some("/archix/deploy holds deployments".to_string());
    }
    if inside("/run/archix/deployment").is_file() {
        return Some("/run/archix/deployment names the booted deployment".to_string());
    }
    None
}

/// The subcommand `args` name, past the global options and their values.
fn subcommand<'a>(args: &[&'a OsStr]) -> Option<&'a OsStr> {
    let mut words = args.iter().copied();
    while let Some(arg) = words.next() {
        match arg.to_str() {
            Some("--config" | "--host") => {
                words.next();
            }
            Some(flag) if flag.starts_with('-') => {}
            _ => return Some(arg),
        }
    }
    None
}

/// Why this run refuses: the deployments found under `root`, or an entry point of theirs being called, unless it only asks for help or the version.
pub fn refusal(root: &Path, args: &[&OsStr]) -> Option<String> {
    let asks = |flag: &str| args.iter().any(|arg| *arg == flag);
    let named = subcommand(args);
    if asks("--help") || asks("-h") || asks("--version") || asks("-V") || named.is_some_and(|arg| arg == "help") {
        return None;
    }
    let called = named
        .and_then(|arg| arg.to_str())
        .filter(|arg| ENTRY_POINTS.contains(arg))
        .map(|arg| format!("`archix {arg}` is called only by archix v0.2.0's deployments under /archix/deploy"));
    let why = called.or_else(|| found(root))?;
    Some(format!(
        "{REFUSAL} ({why}). This archix applies configs in place and has no migration from deployments. \
         Keep the archix that set this system up: boot an entry from before archix was upgraded and keep archix at v0.2.0 \
         (build archix-git from the v0.2.0 tag); or reinstall Arch and set it up with this archix"
    ))
}

/// Whether `mountinfo` puts `/` at a v0.2.0 switch's staging view, `<root>/archix/deploy/.staging-N/.view`, where `deploy-integrate` runs.
pub fn in_staging_view(mountinfo: &str) -> bool {
    mountinfo.lines().any(|line| {
        let fields: Vec<&str> = line.split(' ').collect();
        fields.len() > 4
            && fields[4] == "/"
            && fields[3].contains("/archix/deploy/.staging-")
            && fields[3].ends_with("/.view")
    })
}

fn mountinfo() -> String {
    if cfg!(debug_assertions)
        && let Some(path) = std::env::var_os("ARCHIX_TEST_MOUNTINFO")
    {
        return fs::read_to_string(path).unwrap_or_default();
    }
    fs::read_to_string("/proc/self/mountinfo").unwrap_or_default()
}

/// The hook's script: mkinitcpio ignores what build returns, so it exits the sourcing mkinitcpio instead.
pub fn hook_script(message: &str) -> String {
    let quoted = message.replace('\'', "'\\''");
    format!(
        "#!/bin/bash\n\nbuild() {{\n    error '{quoted}'\n    exit 1\n}}\n\nhelp() {{\n    echo 'archix: refuses to build an initramfs for a deployment'\n}}\n"
    )
}

/// `deploy-integrate` inside a v0.2.0 switch's staging view: that switch writes its own integration when this fails, then commits, so this succeeds and leaves a hook that fails its mkinitcpio, which runs next, before anything reaches the ESP or /archix/current. Anywhere else it only refuses.
pub fn integrate(root: &Path, message: &str) -> Result<bool> {
    if !in_staging_view(&mountinfo()) {
        return Ok(false);
    }
    let write = |rel: &str, text: &str, mode: u32| -> Result<PathBuf> {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap())
            .with_context(|| format!("creating {}", path.parent().unwrap().display()))?;
        match mode {
            0o755 => crate::durable::write_executable(&path, text.as_bytes())?,
            _ => crate::durable::write(&path, text.as_bytes())?,
        }
        Ok(path)
    };
    write(HOOK, &hook_script(message), 0o755)?;
    // First, so mkinitcpio exits before any other hook adds a module and warns, and the refusal is all it says.
    write(HOOK_CONF, "HOOKS=(archix-refuse-deployments \"${HOOKS[@]}\")\n", 0o644)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_current_link_into_deploy_or_a_deploy_directory_marks_deployments() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(found(dir.path()), None);
        fs::create_dir_all(dir.path().join("archix")).unwrap();
        std::os::unix::fs::symlink("elsewhere", dir.path().join("archix/current")).unwrap();
        assert_eq!(found(dir.path()), None);
        fs::create_dir_all(dir.path().join("archix/deploy")).unwrap();
        assert!(found(dir.path()).unwrap().contains("/archix/deploy"));
    }

    #[test]
    fn help_and_version_are_answered_and_entry_points_refused_anywhere() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("archix/deploy/1")).unwrap();
        let args = |list: &[&'static str]| list.iter().map(|arg| OsStr::new(*arg)).collect::<Vec<&'static OsStr>>();
        assert_eq!(refusal(dir.path(), &args(&["--version"])), None);
        assert_eq!(refusal(dir.path(), &args(&["switch", "--help"])), None);
        assert_eq!(refusal(dir.path(), &args(&["help", "switch"])), None);
        assert!(refusal(dir.path(), &args(&["switch"])).unwrap().starts_with(REFUSAL));
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(refusal(empty.path(), &args(&["status"])), None);
        assert!(refusal(empty.path(), &args(&["deploy-mount", "/sysroot"])).is_some());
        assert!(refusal(empty.path(), &args(&["--host", "vm", "hook-record"])).is_some());
        assert_eq!(refusal(empty.path(), &args(&["--host", "hook-record", "status"])), None);
    }

    #[test]
    fn only_a_root_pivoted_into_a_staging_view_is_one() {
        let btrfs = "812 811 0:31 /@/archix/deploy/.staging-7/.view / rw,relatime shared:1 - btrfs /dev/vda2 rw\n813 812 0:5 / /proc rw - proc proc rw\n";
        let ext4 = "40 39 254:1 /archix/deploy/.staging-3/.view / rw - ext4 /dev/vda2 rw\n";
        assert!(in_staging_view(btrfs));
        assert!(in_staging_view(ext4));
        assert!(!in_staging_view("29 1 0:31 /@ / rw - btrfs /dev/vda2 rw\n"));
        assert!(!in_staging_view(
            "30 29 0:31 /@/archix/deploy/.staging-7/.view /mnt rw - btrfs /dev/vda2 rw\n"
        ));
    }

    #[test]
    fn the_hook_fails_mkinitcpio_with_the_refusal_even_with_quotes_in_it() {
        let dir = tempfile::tempdir().unwrap();
        let hook = dir.path().join("hook");
        fs::write(&hook, hook_script("it's refused")).unwrap();
        let out = std::process::Command::new("bash")
            .arg("-c")
            .arg(r#"error() { echo "ERROR: $1" >&2; }; . "$1"; build; echo after"#)
            .arg("bash")
            .arg(&hook)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(1));
        assert_eq!(String::from_utf8_lossy(&out.stderr), "ERROR: it's refused\n");
        assert!(out.stdout.is_empty());
    }
}
