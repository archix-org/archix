use std::ffi::CString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::paths::Paths;
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

/// Trees mounted read-only: on ext4 they share inodes with older deployments, so a write would change a rollback target; staging writes them instead.
pub const READ_ONLY: &[&str] = &["usr", "opt", "boot"];

/// Where the busybox initramfs has the root mounted when late hooks run.
pub const BUSYBOX_ROOT: &str = "/new_root";

/// Whether the root under `root` boots through deployments, and so needs the initramfs hook.
pub fn uses_deployments(root: &Path) -> bool {
    root.join("archix/deploy").exists()
}

/// Stops `what` from taking the initramfs hook off a root that can't boot without it.
pub fn refuse_without_hook(root: &Path, what: &str) -> Result<()> {
    if uses_deployments(root) {
        bail!(
            "{} holds deployments, which boot only through archix's initramfs hook, so {what} would leave the machine unbootable",
            root.join("archix/deploy").display()
        );
    }
    Ok(())
}

/// The generation a boot entry id belongs to, whatever its variant or boot counters, or the fallback UKI's.
pub fn generation_of_entry(id: &str) -> Option<u32> {
    boot::parse_entry(&boot::entry_id(id))
        .map(|(number, _)| number)
        .or_else(|| boot::parse_fallback(id))
}

/// systemd-stub's StubImageIdentifier, the path of the UKI it started, beside LoaderEntrySelected.
const STUB_IMAGE_VAR: &str = "StubImageIdentifier-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f";

/// A UKI path from an entry or from systemd-stub, compared as FAT does: case-blind, either slash.
fn esp_path(path: &str) -> String {
    path.trim().replace('\\', "/").trim_start_matches('/').to_lowercase()
}

/// Whether the UKI systemd-stub reports starting is the one archix recorded for entry `id`; true when the stub reports none.
fn started_recorded(root: &Path, id: &str, stub: Option<&str>) -> bool {
    let Some(stub) = stub else {
        return true;
    };
    let record =
        fs::read_to_string(boot::entry_records(&Paths::under(root)).join(boot::entry_id(id))).unwrap_or_default();
    record
        .lines()
        .find_map(|line| match line.split_once(char::is_whitespace) {
            Some(("efi" | "uki", path)) => Some(esp_path(path)),
            _ => None,
        })
        .is_some_and(|recorded| recorded == esp_path(stub))
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

/// Whether archix wrote the boot entry systemd-boot reports as `id`, going by the records it keeps on the root, as the ESP is unsigned.
fn recorded(root: &Path, id: &str) -> bool {
    boot::entry_records(&Paths::under(root))
        .join(boot::entry_id(id))
        .is_file()
}

/// The deployment to boot from `root`: the selected entry's if archix recorded that entry and its deployment is committed, else `current`; None on a root without deployments.
pub fn choose(root: &Path, selected: Option<&str>) -> Result<Option<Chosen>> {
    choose_started(root, selected, None)
}

/// `choose`, where the selected entry also has to have started the UKI archix recorded for it when systemd-stub says which it started.
pub fn choose_started(root: &Path, selected: Option<&str>, stub: Option<&str>) -> Result<Option<Chosen>> {
    let archix = root.join("archix");
    if let Some(entry) = selected
        && let Some(generation) = generation_of_entry(entry)
        && recorded(root, entry)
        && started_recorded(root, entry, stub)
        && complete(&archix, generation)
    {
        return Ok(Some(Chosen {
            generation,
            selected: true,
        }));
    }
    match current(&archix)? {
        Some(generation) if complete(&archix, generation) => Ok(Some(Chosen {
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

/// Whether the mount holding `path` is read-only; an unreadable one counts as writable.
pub fn read_only(path: &Path) -> bool {
    mount_flags(path).is_ok_and(|flags| flags & libc::ST_RDONLY != 0)
}

/// The kernel's statvfs flag for relatime, which the libc crate defines only for glibc and the static binary is musl.
const ST_RELATIME: libc::c_ulong = 0x1000;

fn bind(from: &Path, to: &Path, read_only: bool) -> Result<()> {
    sys_mount(Some(from), to, libc::MS_BIND)?;
    // A bind keeps the root's read-only flag, which remounting the root read-write after switch_root doesn't clear.
    if read_only || mount_flags(to)? & libc::ST_RDONLY != 0 {
        remount_bind(to, read_only)?;
    }
    Ok(())
}

/// Remounts the bind mount at `to` read-only or read-write, keeping its other per-mount flags.
pub fn remount_bind(to: &Path, read_only: bool) -> Result<()> {
    let flags = mount_flags(to)?;
    let keep = [
        (libc::ST_NOSUID, libc::MS_NOSUID),
        (libc::ST_NODEV, libc::MS_NODEV),
        (libc::ST_NOEXEC, libc::MS_NOEXEC),
        (libc::ST_NOATIME, libc::MS_NOATIME),
        (libc::ST_NODIRATIME, libc::MS_NODIRATIME),
        (ST_RELATIME, libc::MS_RELATIME),
    ]
    .iter()
    .filter(|(st, _)| flags & st != 0)
    .fold(0, |all, (_, ms)| all | ms);
    let read_only = if read_only { libc::MS_RDONLY } else { 0 };
    sys_mount(None, to, libc::MS_REMOUNT | libc::MS_BIND | keep | read_only)
}

/// Whether `path` is itself a mount point that is mounted read-only.
pub fn read_only_mount_point(path: &Path) -> Result<bool> {
    let Ok(canonical) = fs::canonicalize(path) else {
        return Ok(false);
    };
    // A chroot without /proc, as pacman runs its hooks in, has no mounts of its own to look at.
    let Ok(mountinfo) = fs::read("/proc/self/mountinfo") else {
        return Ok(false);
    };
    if !crate::deploy::view::mount_points(&mountinfo).contains(&canonical) {
        return Ok(false);
    }
    Ok(mount_flags(&canonical)? & libc::ST_RDONLY != 0)
}

/// Binds `root`/archix over itself read-only, so no path through it can chmod, chown or write a file that hardlinks share with other deployments; archix writes it from a mount namespace of its own.
pub fn protect_store(root: &Path) -> Result<()> {
    let archix = root.join("archix");
    bind(&archix, &archix, true).with_context(|| format!("binding {} read-only", archix.display()))
}

/// Makes `root`/archix writable for this process and its children alone when the boot bound it read-only: the process gets a mount namespace of its own, whose copy of the bind is remounted read-write; mount propagation stays as it was.
pub fn writable_store(root: &Path) -> Result<()> {
    let archix = root.join("archix");
    if !read_only_mount_point(&archix)? {
        return Ok(());
    }
    if unsafe { libc::unshare(libc::CLONE_NEWNS) } != 0 {
        return Err(io::Error::last_os_error()).context("making a mount namespace to write the deployments in");
    }
    remount_bind(&archix, false).with_context(|| format!("making {} writable for archix", archix.display()))
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

/// Mountpoints in the deployment's fstab that would cover its binds after switch_root, with what each hides.
fn separate_mounts(etc: &Path) -> Vec<(String, &'static str)> {
    let hidden = [
        ("/usr", "/usr"),
        ("/opt", "/opt"),
        ("/boot", "/boot"),
        ("/var", "pacman database and DKMS state"),
        ("/var/lib", "pacman database and DKMS state"),
        ("/var/lib/pacman", "pacman database"),
        ("/var/lib/pacman/local", "pacman database"),
        ("/var/lib/dkms", "DKMS state"),
    ];
    fs::read_to_string(etc.join("fstab"))
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .filter_map(|line| line.split_whitespace().nth(1))
        .map(|at| at.trim_end_matches('/'))
        .filter_map(|at| hidden.iter().find(|(mountpoint, _)| *mountpoint == at))
        .map(|(at, what)| (at.to_string(), *what))
        .collect()
}

/// Binds a deployment's trees over `root`'s usr, etc, opt, boot, pacman database and DKMS state, usr, opt and boot read-only.
pub fn bind_trees(root: &Path, deployment: &Path) -> Result<()> {
    for (tree, at) in TREES {
        let target = root.join(at);
        if !is_dir(&target) {
            make_mountpoint(root, &target)?;
        }
        bind(&deployment.join(tree), &target, READ_ONLY.contains(tree)).with_context(|| format!("binding {tree}"))?;
    }
    Ok(())
}

/// In the initramfs: binds the chosen deployment's trees over `root` and records its generation in `run_dir`/deployment.
pub fn mount(root: &Path, efivar: &Path, run_dir: &Path) -> Result<u32> {
    let archix = root.join("archix");
    if !is_dir(&archix) {
        bail!(
            "{} doesn't exist, so either the root isn't mounted at {} or it doesn't use deployments",
            archix.display(),
            root.display()
        );
    }
    if efivar == Path::new(boot::SELECTED_ENTRY_VAR) {
        boot::mount_efivars();
    }
    let selected = fs::read(efivar).ok().and_then(|bytes| boot::efivar_string(&bytes));
    let stub = efivar
        .parent()
        .and_then(|dir| fs::read(dir.join(STUB_IMAGE_VAR)).ok())
        .and_then(|bytes| boot::efivar_string(&bytes));
    let Some(chosen) = choose_started(root, selected.as_deref(), stub.as_deref())? else {
        bail!("{} has no deployments", archix.display());
    };
    let deployment = archix.join("deploy").join(chosen.generation.to_string());
    for (at, what) in separate_mounts(&deployment.join("etc")) {
        eprintln!("archix: warning: fstab mounts {at} separately, which hides this deployment's {what}");
    }
    bind_trees(root, &deployment).with_context(|| format!("mounting deployment {}", chosen.generation))?;
    protect_store(root)?;
    fs::create_dir_all(run_dir).with_context(|| format!("creating {}", run_dir.display()))?;
    fs::write(run_dir.join("deployment"), format!("{}\n", chosen.generation))
        .with_context(|| format!("writing {}", run_dir.join("deployment").display()))?;
    let why = match (&selected, chosen.selected) {
        (Some(entry), true) => format!("the boot entry {entry}"),
        (Some(entry), false) => {
            format!(
                "current; the boot entry {entry} isn't one archix recorded for a committed deployment, or it started another UKI ({}) than archix recorded for it",
                stub.as_deref().unwrap_or("unknown")
            )
        }
        (None, _) => "current".to_string(),
    };
    println!("archix: booting deployment {} ({why})", chosen.generation);
    Ok(chosen.generation)
}

/// The activation unit.
pub fn activate_file(root: &Path, archix: &Path) -> (PathBuf, String) {
    (
        root.join("etc/systemd/system/archix-deploy-activate.service"),
        activate_unit(archix),
    )
}

/// The mkinitcpio hook for both initramfs types, the initrd unit and the conf.d line.
pub fn hook_files(root: &Path) -> Vec<(PathBuf, String)> {
    let static_binary = crate::integration::STATIC_BINARY;
    vec![
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
            format!(
                "#!/usr/bin/ash\n\nrun_latehook() {{\n    if ! /usr/bin/archix deploy-mount {BUSYBOX_ROOT}; then\n        \
                 err \"archix couldn't mount this boot's deployment; the machine halts when this shell exits\"\n        \
                 launch_interactive_shell\n        \
                 err \"archix: halting rather than booting the root without its deployment; pick another boot entry\"\n        \
                 halt -f\n    fi\n}}\n"
            ),
        ),
        (
            root.join("usr/lib/archix/archix-deploy-mount.service"),
            MOUNT_UNIT.to_string(),
        ),
        (
            root.join("etc/mkinitcpio.conf.d/archix-deploy.conf"),
            "HOOKS+=(archix-deploy)\n".to_string(),
        ),
    ]
}

/// The link that starts the activation unit at every boot, and the unit it names.
pub fn activate_wants(root: &Path) -> (PathBuf, String) {
    (
        root.join("etc/systemd/system/sysinit.target.wants/archix-deploy-activate.service"),
        "/etc/systemd/system/archix-deploy-activate.service".to_string(),
    )
}

const MOUNT_UNIT: &str = "[Unit]\nDescription=archix: mount the deployment this boot belongs to\nDefaultDependencies=no\n\
     Requires=sysroot.mount\nAfter=sysroot.mount\nBefore=initrd-root-fs.target initrd-parse-etc.service\n\n\
     [Service]\nType=oneshot\nRemainAfterExit=yes\nExecStart=/usr/bin/archix deploy-mount /sysroot\nStandardOutput=journal+console\n";

fn activate_unit(archix: &Path) -> String {
    format!(
        "[Unit]\nDescription=archix: apply this deployment's /var changes on its first boot and run its hooks\nDefaultDependencies=no\n\
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
        assert_eq!(generation_of_entry("archix-12-protected.conf"), Some(12));
        assert_eq!(generation_of_entry("archix-3.efi"), None);
        assert_eq!(generation_of_entry("arch-linux.efi"), None);
        assert_eq!(generation_of_entry("archix-.conf"), None);
        assert_eq!(generation_of_entry("archix-x.conf"), None);
        assert_eq!(generation_of_entry("auto-windows"), None);
    }

    fn deployment(root: &Path, name: &str) {
        for (tree, _) in TREES {
            fs::create_dir_all(root.join("archix/deploy").join(name).join(tree)).unwrap();
        }
    }

    fn record(root: &Path, entry: &str) {
        let records = boot::entry_records(&Paths::under(root));
        fs::create_dir_all(&records).unwrap();
        fs::write(records.join(entry), "title archix\n").unwrap();
    }

    fn layout() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        deployment(root, "1");
        deployment(root, "2");
        deployment(root, ".staging-3");
        deployment(root, "5");
        fs::create_dir_all(root.join("archix/deploy/4/usr")).unwrap();
        std::os::unix::fs::symlink("deploy/1", root.join("archix/current")).unwrap();
        for entry in [
            "archix-1.conf",
            "archix-1-protected.conf",
            "archix-2.conf",
            "archix-3.conf",
            "archix-4.conf",
        ] {
            record(root, entry);
        }
        dir
    }

    fn chosen(generation: u32, selected: bool) -> Option<Chosen> {
        Some(Chosen { generation, selected })
    }

    #[test]
    fn the_selected_entry_boots_its_deployment_only_once_committed() {
        let dir = layout();
        let root = dir.path();
        assert_eq!(choose(root, Some("archix-2.conf")).unwrap(), chosen(2, true));
        assert_eq!(choose(root, Some("archix-2+1-2.conf")).unwrap(), chosen(2, true));
        assert_eq!(choose(root, Some("archix-1-protected.conf")).unwrap(), chosen(1, true));
        assert_eq!(
            choose(root, Some("archix-3.conf")).unwrap(),
            chosen(1, false),
            "a staging tree is never booted"
        );
        assert_eq!(
            choose(root, Some("archix-4.conf")).unwrap(),
            chosen(1, false),
            "deployment 4 lacks trees"
        );
        assert_eq!(
            choose(root, Some("archix-9.conf")).unwrap(),
            chosen(1, false),
            "no deployment 9"
        );
        assert_eq!(choose(root, Some("arch-linux.efi")).unwrap(), chosen(1, false));
        assert_eq!(choose(root, None).unwrap(), chosen(1, false));
    }

    #[test]
    fn the_fallback_uki_boots_its_deployment_while_it_exists_and_current_once_it_is_gone() {
        let dir = layout();
        let root = dir.path();
        assert_eq!(generation_of_entry("archix-fallback-2.efi"), Some(2));
        assert_eq!(generation_of_entry("archix-fallback-02.efi"), None);
        assert_eq!(
            choose(root, Some("archix-fallback-2.efi")).unwrap(),
            chosen(1, false),
            "no record of the fallback yet"
        );
        record(root, "archix-fallback-2.efi");
        assert_eq!(choose(root, Some("archix-fallback-2.efi")).unwrap(), chosen(2, true));
        fs::remove_dir_all(root.join("archix/deploy/2")).unwrap();
        assert_eq!(
            choose(root, Some("archix-fallback-2.efi")).unwrap(),
            chosen(1, false),
            "deployment 2 was collected, so current boots"
        );
    }

    #[test]
    fn an_entry_that_started_another_uki_than_archix_recorded_falls_back_to_current() {
        let dir = layout();
        let root = dir.path();
        let records = boot::entry_records(&Paths::under(root));
        fs::write(
            records.join("archix-2.conf"),
            "title archix\nversion 2\nefi /EFI/archix/7.2.9-arch1-1-aaaa.efi\n",
        )
        .unwrap();
        fs::write(
            records.join("archix-fallback-2.efi"),
            "uki /EFI/Linux/archix-fallback-2.efi\n",
        )
        .unwrap();
        let started = |entry: &str, stub: Option<&str>| choose_started(root, Some(entry), stub).unwrap();
        assert_eq!(
            started("archix-2.conf", Some("\\EFI\\archix\\7.2.9-arch1-1-aaaa.efi")),
            chosen(2, true)
        );
        assert_eq!(
            started("archix-2.conf", Some("\\EFI\\ARCHIX\\7.2.9-ARCH1-1-AAAA.EFI")),
            chosen(2, true),
            "FAT is case-blind"
        );
        assert_eq!(
            started("archix-2.conf", Some("\\EFI\\archix\\7.2.9-arch1-1-bbbb.efi")),
            chosen(1, false),
            "the entry was edited to start another UKI"
        );
        assert_eq!(
            started("archix-2.conf", None),
            chosen(2, true),
            "a stub that reports nothing"
        );
        assert_eq!(
            started("archix-fallback-2.efi", Some("\\EFI\\Linux\\archix-fallback-2.efi")),
            chosen(2, true)
        );
        assert_eq!(
            started("archix-fallback-2.efi", Some("\\EFI\\Linux\\archix-fallback-1.efi")),
            chosen(1, false)
        );
    }

    #[test]
    fn an_entry_archix_has_no_record_of_falls_back_to_current() {
        let dir = layout();
        let root = dir.path();
        assert_eq!(
            choose(root, Some("archix-5.conf")).unwrap(),
            chosen(1, false),
            "deployment 5 is committed, but archix never wrote archix-5.conf"
        );
        fs::remove_file(boot::entry_records(&Paths::under(root)).join("archix-2.conf")).unwrap();
        assert_eq!(choose(root, Some("archix-2.conf")).unwrap(), chosen(1, false));
        assert_eq!(
            choose(root, Some("archix-1-protected.conf")).unwrap(),
            chosen(1, true),
            "each variant has its own record"
        );
    }

    #[test]
    fn current_must_name_a_complete_committed_deployment() {
        let dir = layout();
        let root = dir.path();
        let point = |target: &str| {
            fs::remove_file(root.join("archix/current")).unwrap();
            std::os::unix::fs::symlink(target, root.join("archix/current")).unwrap();
        };
        point("deploy/.staging-3");
        assert!(choose(root, None).is_err());
        assert_eq!(
            choose(root, Some("archix-2.conf")).unwrap(),
            chosen(2, true),
            "a committed selected entry still boots"
        );
        point("deploy/4");
        assert!(choose(root, None).is_err());
        point("deploy/+2");
        assert!(choose(root, None).is_err());
        fs::remove_file(root.join("archix/current")).unwrap();
        assert!(choose(root, None).is_err(), "deployments without a current one");
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(choose(empty.path(), None).unwrap(), None, "not a deployment system");
    }

    #[test]
    fn mounting_fails_when_the_root_or_its_deployments_are_missing() {
        let dir = tempfile::tempdir().unwrap();
        let run = dir.path().join("run");
        let efivar = dir.path().join("no-efivar");
        let err = mount(&dir.path().join("not-mounted"), &efivar, &run).unwrap_err();
        assert!(format!("{err:#}").contains("isn't mounted"), "{err:#}");
        let root = dir.path().join("root");
        fs::create_dir_all(root.join("usr")).unwrap();
        assert!(mount(&root, &efivar, &run).is_err(), "a root without /archix");
        fs::create_dir_all(root.join("archix")).unwrap();
        let err = mount(&root, &efivar, &run).unwrap_err();
        assert!(format!("{err:#}").contains("no deployments"), "{err:#}");
        assert!(!run.exists());
    }

    #[test]
    fn separate_var_usr_opt_and_boot_mounts_are_named() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("fstab"),
            "UUID=a / ext4 rw 0 1\n# UUID=b /var ext4 rw 0 2\nUUID=c /var/ ext4 rw 0 2\nUUID=d /var/log ext4 rw 0 2\n\
             UUID=e /usr ext4 ro 0 2\nUUID=f /opt/ ext4 rw 0 2\nUUID=g /boot vfat rw 0 2\nUUID=h /efi vfat rw 0 2\nUUID=i /usr/local ext4 rw 0 2\n",
        )
        .unwrap();
        let at: Vec<String> = separate_mounts(dir.path()).into_iter().map(|(at, _)| at).collect();
        assert_eq!(at, ["/var", "/usr", "/opt", "/boot"]);
    }

    fn run_hook(archix: &str) -> std::process::Output {
        let hook = hook_files(Path::new("/"))
            .into_iter()
            .find(|(path, _)| path.ends_with("hooks/archix-deploy"))
            .unwrap()
            .1;
        assert!(hook.contains("/usr/bin/archix deploy-mount /new_root;"), "{hook}");
        let script = format!(
            "{}\nerr() {{ echo \"ERR $*\"; }}\nlaunch_interactive_shell() {{ echo SHELL; }}\nhalt() {{ echo \"HALT $*\"; exit 3; }}\nrun_latehook\necho BOOTING ON\n",
            hook.replace("/usr/bin/archix deploy-mount", archix)
        );
        std::process::Command::new("sh").arg("-c").arg(script).output().unwrap()
    }

    #[test]
    fn the_busybox_hook_halts_after_the_shell_when_mounting_fails() {
        let failed = run_hook("false");
        let out = String::from_utf8_lossy(&failed.stdout);
        assert_eq!(failed.status.code(), Some(3), "{out}");
        assert!(
            out.contains("SHELL\nERR archix: halting") && out.contains("HALT -f"),
            "{out}"
        );
        assert!(!out.contains("BOOTING ON"), "{out}");
        let mounted = run_hook("true");
        let out = String::from_utf8_lossy(&mounted.stdout);
        assert!(mounted.status.success() && out == "BOOTING ON\n", "{out}");
    }
}
