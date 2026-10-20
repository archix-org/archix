use std::ffi::CString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::system::boot;

/// Each deployment's trees and where the booted one's go on the root.
pub const TREES: &[(&str, &str)] = &[
    ("usr", "usr"),
    ("etc", "etc"),
    ("opt", "opt"),
    ("boot", "boot"),
    ("pacman", "var/lib/pacman/local"),
    ("dkms", "var/lib/dkms"),
];

/// The generation a boot entry id belongs to, whatever its variant or boot counters.
pub fn generation_of_entry(id: &str) -> Option<u32> {
    boot::parse_entry(&boot::entry_id(id)).map(|(number, _)| number)
}

#[derive(Debug, PartialEq)]
pub struct Chosen {
    pub generation: u32,
    /// Whether it's the selected boot entry's deployment rather than `current`.
    pub selected: bool,
}

fn is_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir())
}

/// Whether `deploy/<generation>` is committed and has every tree; a `.staging-*` name never parses as one.
fn complete(archix: &Path, generation: u32) -> bool {
    let dir = archix.join("deploy").join(generation.to_string());
    is_dir(&dir) && TREES.iter().all(|(tree, _)| is_dir(&dir.join(tree)))
}

/// The generation `current` points to, None when there's no `current`.
fn current(archix: &Path) -> Result<Option<u32>> {
    let link = archix.join("current");
    let target = match fs::read_link(&link) {
        Ok(target) => target,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", link.display())),
    };
    let number = target
        .to_str()
        .and_then(|target| target.strip_prefix("deploy/"))
        .filter(|number| !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|number| number.parse().ok());
    match number {
        Some(number) => Ok(Some(number)),
        None => bail!(
            "{} points to {}, which isn't a committed deployment",
            link.display(),
            target.display()
        ),
    }
}

/// The deployment to boot: the selected entry's if it's committed, else `current`; None on a system without deployments.
pub fn choose(archix: &Path, selected: Option<&str>) -> Result<Option<Chosen>> {
    if let Some(generation) = selected.and_then(generation_of_entry)
        && complete(archix, generation)
    {
        return Ok(Some(Chosen {
            generation,
            selected: true,
        }));
    }
    match current(archix)? {
        Some(generation) if complete(archix, generation) => Ok(Some(Chosen {
            generation,
            selected: false,
        })),
        Some(generation) => bail!(
            "{}/current names deployment {generation}, which isn't complete",
            archix.display()
        ),
        None if archix.join("deploy").exists() => bail!("{} has deployments but no current one", archix.display()),
        None => Ok(None),
    }
}

fn c_path(path: &Path) -> Result<CString> {
    CString::new(path.as_os_str().as_bytes()).with_context(|| format!("{} has a NUL byte", path.display()))
}

fn sys_mount(source: Option<&Path>, target: &Path, flags: libc::c_ulong) -> Result<()> {
    let source = source.map(c_path).transpose()?;
    let target_c = c_path(target)?;
    let none = c"none";
    let done = unsafe {
        libc::mount(
            source.as_ref().map_or(none.as_ptr(), |s| s.as_ptr()),
            target_c.as_ptr(),
            std::ptr::null(),
            flags,
            std::ptr::null(),
        )
    };
    if done != 0 {
        return Err(io::Error::last_os_error()).with_context(|| format!("mounting {}", target.display()));
    }
    Ok(())
}

/// The per-mount flags of the mount holding `path`, which a bind mount of something below it starts with.
fn mount_flags(path: &Path) -> Result<libc::c_ulong> {
    let path_c = c_path(path)?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(path_c.as_ptr(), &mut stat) } != 0 {
        return Err(io::Error::last_os_error()).with_context(|| format!("statvfs {}", path.display()));
    }
    Ok(stat.f_flag)
}

fn bind(from: &Path, to: &Path) -> Result<()> {
    sys_mount(Some(from), to, libc::MS_BIND)?;
    let flags = mount_flags(to)?;
    // A bind keeps the root's read-only flag, which remounting the root read-write after switch_root doesn't clear.
    if flags & libc::ST_RDONLY != 0 {
        let keep = [
            (libc::ST_NOSUID, libc::MS_NOSUID),
            (libc::ST_NODEV, libc::MS_NODEV),
            (libc::ST_NOEXEC, libc::MS_NOEXEC),
            (libc::ST_NOATIME, libc::MS_NOATIME),
            (libc::ST_NODIRATIME, libc::MS_NODIRATIME),
            (libc::ST_RELATIME, libc::MS_RELATIME),
        ]
        .iter()
        .filter(|(st, _)| flags & st != 0)
        .fold(0, |all, (_, ms)| all | ms);
        sys_mount(None, to, libc::MS_REMOUNT | libc::MS_BIND | keep)?;
    }
    Ok(())
}

/// Creates a missing mountpoint, remounting a read-only root read-write first if it has to.
fn make_mountpoint(root: &Path, dir: &Path) -> Result<()> {
    match fs::create_dir_all(dir) {
        Err(e) if e.raw_os_error() == Some(libc::EROFS) => {
            sys_mount(None, root, libc::MS_REMOUNT)?;
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))
        }
        other => other.with_context(|| format!("creating {}", dir.display())),
    }
}

/// Mountpoints in the deployment's fstab that would cover the database and DKMS binds after switch_root.
fn separate_var(etc: &Path) -> Vec<String> {
    let covered = [
        "/var",
        "/var/lib",
        "/var/lib/pacman",
        "/var/lib/pacman/local",
        "/var/lib/dkms",
    ];
    fs::read_to_string(etc.join("fstab"))
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .filter_map(|line| line.split_whitespace().nth(1))
        .map(|at| at.trim_end_matches('/').to_string())
        .filter(|at| covered.contains(&at.as_str()))
        .collect()
}

/// In the initramfs: binds the chosen deployment's trees over `root` and records its generation in `run_dir`/deployment.
pub fn mount(root: &Path, efivar: &Path, run_dir: &Path) -> Result<Option<u32>> {
    if efivar == Path::new(boot::SELECTED_ENTRY_VAR) {
        boot::mount_efivars();
    }
    let selected = fs::read(efivar).ok().and_then(|bytes| boot::efivar_string(&bytes));
    let archix = root.join("archix");
    let Some(chosen) = choose(&archix, selected.as_deref())? else {
        return Ok(None);
    };
    let deployment = archix.join("deploy").join(chosen.generation.to_string());
    for at in separate_var(&deployment.join("etc")) {
        eprintln!(
            "archix: warning: {at} is a separate filesystem, which hides this deployment's pacman database and DKMS state"
        );
    }
    for (tree, at) in TREES {
        let target = root.join(at);
        if !is_dir(&target) {
            make_mountpoint(root, &target)?;
        }
        bind(&deployment.join(tree), &target)
            .with_context(|| format!("binding deployment {}'s {tree}", chosen.generation))?;
    }
    fs::create_dir_all(run_dir).with_context(|| format!("creating {}", run_dir.display()))?;
    fs::write(run_dir.join("deployment"), format!("{}\n", chosen.generation))
        .with_context(|| format!("writing {}", run_dir.join("deployment").display()))?;
    let why = match (&selected, chosen.selected) {
        (Some(entry), true) => format!("the boot entry {entry}"),
        (Some(entry), false) => format!("current; the boot entry {entry} names no committed deployment"),
        (None, _) => "current".to_string(),
    };
    println!("archix: booting deployment {} ({why})", chosen.generation);
    Ok(Some(chosen.generation))
}

/// The mkinitcpio hook for both initramfs types, the initrd unit, the first-boot activation unit and the conf.d line; `with_static` when the static binary the initramfs needs is installed.
pub fn files(root: &Path, archix: &Path, with_static: bool) -> Vec<(PathBuf, String)> {
    let static_binary = crate::integration::STATIC_BINARY;
    let mut files = vec![(
        root.join("etc/systemd/system/archix-deploy-activate.service"),
        activate_unit(archix),
    )];
    if with_static {
        files.extend([
            (
                root.join("etc/initcpio/install/archix-deploy"),
                format!(
                    "#!/bin/bash\n\nbuild() {{\n    add_binary {static_binary} /usr/bin/archix\n    add_runscript\n    \
                     add_file /usr/lib/archix/archix-deploy-mount.service /usr/lib/systemd/system/archix-deploy-mount.service\n    \
                     add_symlink /usr/lib/systemd/system/initrd-root-fs.target.requires/archix-deploy-mount.service /usr/lib/systemd/system/archix-deploy-mount.service\n}}\n\n\
                     help() {{\n    echo \"archix deployments: mounts the booted deployment's /usr, /etc, /opt, /boot, pacman database and DKMS state on the root\"\n}}\n"
                ),
            ),
            (
                root.join("etc/initcpio/hooks/archix-deploy"),
                "#!/usr/bin/ash\n\nrun_latehook() {\n    if ! /usr/bin/archix deploy-mount /sysroot; then\n        \
                 err \"archix couldn't mount this boot's deployment\"\n        launch_interactive_shell\n    fi\n}\n"
                    .to_string(),
            ),
            (root.join("usr/lib/archix/archix-deploy-mount.service"), MOUNT_UNIT.to_string()),
            (
                root.join("etc/mkinitcpio.conf.d/archix-deploy.conf"),
                "HOOKS+=(archix-deploy)\n".to_string(),
            ),
        ]);
    }
    files
}

/// The link that starts the activation unit at every boot, and the unit it names.
pub fn activate_wants(root: &Path) -> (PathBuf, String) {
    (
        root.join("etc/systemd/system/sysinit.target.wants/archix-deploy-activate.service"),
        "/etc/systemd/system/archix-deploy-activate.service".to_string(),
    )
}

/// Every file `files` writes plus the activation unit's link, for taking them off.
pub fn all_files(root: &Path) -> Vec<PathBuf> {
    let mut all: Vec<PathBuf> = files(root, Path::new("archix"), true)
        .into_iter()
        .map(|(path, _)| path)
        .collect();
    all.push(activate_wants(root).0);
    all
}

const MOUNT_UNIT: &str = "[Unit]\nDescription=archix: mount the deployment this boot belongs to\nDefaultDependencies=no\n\
     Requires=sysroot.mount\nAfter=sysroot.mount\nBefore=initrd-root-fs.target initrd-parse-etc.service\n\n\
     [Service]\nType=oneshot\nRemainAfterExit=yes\nExecStart=/usr/bin/archix deploy-mount /sysroot\nStandardOutput=journal+console\n";

fn activate_unit(archix: &Path) -> String {
    format!(
        "[Unit]\nDescription=archix: apply this deployment's /var changes on its first boot\nDefaultDependencies=no\n\
         ConditionPathExists=/run/archix/deployment\nAfter=local-fs.target systemd-sysusers.service\n\
         Before=sysinit.target systemd-tmpfiles-setup.service systemd-journal-catalog-update.service\n\
         Conflicts=shutdown.target\nBefore=shutdown.target\nOnFailure=emergency.target\nOnFailureJobMode=replace-irreversibly\n\n\
         [Service]\nType=oneshot\nRemainAfterExit=yes\nExecStart={} deploy-activate\nStandardOutput=journal+console\n\n\
         [Install]\nWantedBy=sysinit.target\n",
        archix.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_entry_id_names_its_generation_whatever_its_variant_or_counters() {
        assert_eq!(generation_of_entry("archix-7.conf"), Some(7));
        assert_eq!(generation_of_entry("archix-7+2.conf"), Some(7));
        assert_eq!(generation_of_entry("archix-7+0-3.conf"), Some(7));
        assert_eq!(generation_of_entry("archix-12-rescue.conf"), Some(12));
        assert_eq!(generation_of_entry("archix-12-protected.conf"), Some(12));
        assert_eq!(generation_of_entry("archix-3.efi"), Some(3));
        assert_eq!(generation_of_entry("arch-linux.efi"), None);
        assert_eq!(generation_of_entry("archix-.conf"), None);
        assert_eq!(generation_of_entry("archix-x.conf"), None);
        assert_eq!(generation_of_entry("auto-windows"), None);
    }

    fn deployment(archix: &Path, name: &str) {
        for (tree, _) in TREES {
            fs::create_dir_all(archix.join("deploy").join(name).join(tree)).unwrap();
        }
    }

    fn layout() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let archix = dir.path();
        deployment(archix, "1");
        deployment(archix, "2");
        deployment(archix, ".staging-3");
        fs::create_dir_all(archix.join("deploy/4/usr")).unwrap();
        std::os::unix::fs::symlink("deploy/1", archix.join("current")).unwrap();
        dir
    }

    fn chosen(generation: u32, selected: bool) -> Option<Chosen> {
        Some(Chosen { generation, selected })
    }

    #[test]
    fn the_selected_entry_boots_its_deployment_only_once_committed() {
        let dir = layout();
        let archix = dir.path();
        assert_eq!(choose(archix, Some("archix-2.conf")).unwrap(), chosen(2, true));
        assert_eq!(choose(archix, Some("archix-2+1-2.conf")).unwrap(), chosen(2, true));
        assert_eq!(choose(archix, Some("archix-1-rescue.conf")).unwrap(), chosen(1, true));
        assert_eq!(
            choose(archix, Some("archix-3.conf")).unwrap(),
            chosen(1, false),
            "a staging tree is never booted"
        );
        assert_eq!(
            choose(archix, Some("archix-4.conf")).unwrap(),
            chosen(1, false),
            "deployment 4 lacks trees"
        );
        assert_eq!(
            choose(archix, Some("archix-9.conf")).unwrap(),
            chosen(1, false),
            "no deployment 9"
        );
        assert_eq!(choose(archix, Some("arch-linux.efi")).unwrap(), chosen(1, false));
        assert_eq!(choose(archix, None).unwrap(), chosen(1, false));
    }

    #[test]
    fn current_must_name_a_complete_committed_deployment() {
        let dir = layout();
        let archix = dir.path();
        let point = |target: &str| {
            fs::remove_file(archix.join("current")).unwrap();
            std::os::unix::fs::symlink(target, archix.join("current")).unwrap();
        };
        point("deploy/.staging-3");
        assert!(choose(archix, None).is_err());
        assert_eq!(
            choose(archix, Some("archix-2.conf")).unwrap(),
            chosen(2, true),
            "a committed selected entry still boots"
        );
        point("deploy/4");
        assert!(choose(archix, None).is_err());
        point("deploy/+2");
        assert!(choose(archix, None).is_err());
        fs::remove_file(archix.join("current")).unwrap();
        assert!(choose(archix, None).is_err(), "deployments without a current one");
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(choose(empty.path(), None).unwrap(), None, "not a deployment system");
    }

    #[test]
    fn a_separate_var_is_named() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("fstab"),
            "UUID=a / ext4 rw 0 1\n# UUID=b /var ext4 rw 0 2\nUUID=c /var/ ext4 rw 0 2\nUUID=d /var/log ext4 rw 0 2\n",
        )
        .unwrap();
        assert_eq!(separate_var(dir.path()), ["/var"]);
    }
}
