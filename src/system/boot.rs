use std::collections::BTreeSet;
use std::ffi::CString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::paths::Paths;

/// What the boot step of a transaction writes: one UKI for a generation, plus its rescue twin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BootPlan {
    pub generation: u32,
    pub esp: String,
    pub kernel: String,
    pub microcode: Vec<String>,
    pub cmdline: String,
    pub keep: u32,
    pub secure_boot: Option<bool>,
    /// Also write a protected entry with the command line Easy Anti-Cheat Linux v2 asks for.
    #[serde(default)]
    pub anticheat: bool,
}

/// The entries one generation gets: the normal one, a rescue twin and a protected twin.
#[derive(Clone, Copy, PartialEq)]
enum Variant {
    Normal,
    Rescue,
    Protected,
}

pub const MODULES: &str = "/usr/lib/modules";
const MARGIN: u64 = 16 * 1024 * 1024;

/// The installed version of a kernel package, from the pkgbase file pacman's kernels leave in their modules dir.
pub fn kernel_version(kernel: &str) -> Result<String> {
    if let Some(version) = owned_version(Path::new("/var/lib/pacman/local"), kernel)
        && Path::new(MODULES).join(&version).join("vmlinuz").is_file()
    {
        return Ok(version);
    }
    for entry in fs::read_dir(MODULES)
        .with_context(|| format!("reading {MODULES}"))?
        .flatten()
    {
        if fs::read_to_string(entry.path().join("pkgbase")).is_ok_and(|base| base.trim() == kernel)
            && entry.path().join("vmlinuz").is_file()
        {
            return Ok(entry.file_name().to_string_lossy().into_owned());
        }
    }
    bail!("the {kernel} kernel isn't installed (no {MODULES}/*/pkgbase names it)")
}

/// The modules dir the installed kernel package owns; the running kernel's modules that archix keeps until reboot carry the same pkgbase.
fn owned_version(local_db: &Path, kernel: &str) -> Option<String> {
    let entry = fs::read_dir(local_db)
        .ok()?
        .flatten()
        .find(|entry| crate::pacman::name_from_entry(&entry.file_name().to_string_lossy()) == kernel)?;
    let files = fs::read_to_string(entry.path().join("files")).ok()?;
    files
        .lines()
        .find_map(|line| line.strip_prefix("usr/lib/modules/")?.strip_suffix("/vmlinuz"))
        .map(str::to_string)
}

fn kernels(paths: &Paths) -> PathBuf {
    paths.state.join("kernels")
}

/// Keeps a hardlinked copy of a kernel's modules, so older boot entries still find them after pacman removes them.
pub fn preserve_modules(paths: &Paths, version: &str) -> Result<()> {
    preserve_from(Path::new(MODULES), paths, version)
}

pub(crate) fn preserve_from(modules: &Path, paths: &Paths, version: &str) -> Result<()> {
    let dest = kernels(paths).join(version);
    if dest.exists() {
        return Ok(());
    }
    fs::create_dir_all(kernels(paths))?;
    let tmp = kernels(paths).join(format!(".{version}.{}", std::process::id()));
    let status = Command::new("cp")
        .arg("-al")
        .arg(modules.join(version))
        .arg(&tmp)
        .status()?;
    if !status.success() {
        let _ = fs::remove_dir_all(&tmp);
        bail!("could not keep a copy of {version}'s modules");
    }
    fs::rename(&tmp, &dest)?;
    Ok(())
}

/// Puts a preserved kernel's modules back when the running kernel's are gone; true if it had to.
pub fn restore_modules(paths: &Paths, version: &str) -> Result<bool> {
    restore_into(Path::new(MODULES), paths, version)
}

pub fn restore_into(modules: &Path, paths: &Paths, version: &str) -> Result<bool> {
    let target = modules.join(version);
    let source = kernels(paths).join(version);
    if target.exists() || !source.exists() {
        return Ok(false);
    }
    let tmp = modules.join(format!(".{version}.restoring"));
    if tmp.exists() {
        fs::remove_dir_all(&tmp)?;
    }
    link_tree(&source, &tmp).with_context(|| format!("restoring {version}'s modules"))?;
    fs::rename(&tmp, &target).with_context(|| format!("restoring {version}'s modules"))?;
    Ok(true)
}

/// Recreates `from` at `to` with hard links (the preserved modules share the root filesystem), copying where linking fails; the initramfs has no cp.
fn link_tree(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to)?;
    fs::set_permissions(to, fs::metadata(from)?.permissions())?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let (source, target) = (entry.path(), to.join(entry.file_name()));
        let kind = entry.file_type()?;
        if kind.is_dir() {
            link_tree(&source, &target)?;
        } else if kind.is_symlink() {
            std::os::unix::fs::symlink(fs::read_link(&source)?, &target)?;
        } else if fs::hard_link(&source, &target).is_err() {
            fs::copy(&source, &target)?;
        }
    }
    Ok(())
}

/// The kernel version each generation's boot entry was built with.
pub fn kernel_of(paths: &Paths, generation: u32) -> Option<String> {
    fs::read_to_string(paths.state.join("boot").join(format!("{generation}.kernel"))).ok()
}

/// Deletes preserved kernels no remaining boot entry uses.
pub fn prune_kernels(paths: &Paths, keep: &BTreeSet<String>) -> Result<usize> {
    let mut removed = 0;
    for entry in fs::read_dir(kernels(paths)).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !keep.contains(&name) {
            fs::remove_dir_all(entry.path())?;
            removed += 1;
        }
    }
    Ok(removed)
}

fn linux_dir(esp: &str) -> PathBuf {
    Path::new(esp).join("EFI/Linux")
}

fn entry_name(generation: u32, variant: Variant) -> String {
    match variant {
        Variant::Normal => format!("archix-{generation}.efi"),
        Variant::Rescue => format!("archix-{generation}-rescue.efi"),
        Variant::Protected => format!("archix-{generation}-protected.efi"),
    }
}

/// The generations that have a boot entry on the ESP, with whether it's an extra (rescue or protected) entry.
pub fn entries(esp: &str) -> Vec<(u32, bool, PathBuf)> {
    let mut found = Vec::new();
    for entry in fs::read_dir(linux_dir(esp)).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(stem) = name.strip_prefix("archix-").and_then(|n| n.strip_suffix(".efi")) else {
            continue;
        };
        let (number, rescue) = match stem.strip_suffix("-rescue").or_else(|| stem.strip_suffix("-protected")) {
            Some(number) => (number, true),
            None => (stem, false),
        };
        if let Ok(number) = number.parse() {
            found.push((number, rescue, entry.path()));
        }
    }
    found.sort();
    found
}

/// Deletes boot entries (and the kernels only they used) for generations that are no longer kept.
pub fn prune_entries(esp: &str, keep: &BTreeSet<u32>) -> Result<usize> {
    let mut removed = 0;
    for (number, _, path) in entries(esp) {
        if !keep.contains(&number) {
            fs::remove_file(&path)?;
            removed += 1;
        }
    }
    Ok(removed)
}

fn free_bytes(dir: &Path) -> Result<u64> {
    let path = CString::new(dir.as_os_str().as_encoded_bytes())?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        bail!("checking free space on {}", dir.display());
    }
    Ok(stat.f_bavail * stat.f_frsize)
}

fn sbctl_has_keys() -> bool {
    Command::new("sbctl")
        .args(["status", "--json"])
        .output()
        .is_ok_and(|output| {
            String::from_utf8_lossy(&output.stdout).contains("\"installed\": true")
                || Path::new("/var/lib/sbctl/keys/db/db.key").exists()
                || Path::new("/usr/share/secureboot/keys/db/db.key").exists()
        })
}

fn run(cmd: &mut Command, what: &str) -> Result<()> {
    let output = cmd
        .output()
        .with_context(|| format!("running {what}; is it installed?"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail: Vec<&str> = stderr.lines().rev().take(8).collect();
        bail!(
            "{what} failed:\n{}",
            tail.into_iter().rev().collect::<Vec<_>>().join("\n")
        );
    }
    Ok(())
}

/// Builds the generation's UKI (and its rescue twin when the rescue hook is set up), signs it, and installs it on the ESP.
pub fn write_entry(paths: &Paths, plan: &BootPlan) -> Result<String> {
    let version = kernel_version(&plan.kernel)?;
    preserve_modules(paths, &version)?;
    let work = paths.state.join("boot-work");
    let _ = fs::remove_dir_all(&work);
    fs::create_dir_all(&work)?;
    let records = paths.state.join("boot");
    fs::create_dir_all(&records)?;
    fs::write(records.join(format!("{}.kernel", plan.generation)), &version)?;
    crate::durable::write(&records.join("esp"), plan.esp.as_bytes())?;

    let initramfs = work.join("initramfs.img");
    run(
        Command::new("mkinitcpio")
            .args(["-k", &version, "-g"])
            .arg(&initramfs)
            .args(["-S", "autodetect"]),
        "mkinitcpio",
    )?;
    let os_release = fs::read_to_string("/etc/os-release").unwrap_or_default();
    let release: Vec<String> = os_release
        .lines()
        .filter(|line| {
            !line.starts_with("PRETTY_NAME=") && !line.starts_with("IMAGE_ID=") && !line.starts_with("IMAGE_VERSION=")
        })
        .map(str::to_string)
        .collect();
    let secure_boot = plan.secure_boot.unwrap_or_else(sbctl_has_keys);
    let rescue_hook = Path::new("/etc/initcpio/install/archix-rescue").exists();
    let mut built = Vec::new();
    for variant in [Variant::Normal, Variant::Rescue, Variant::Protected] {
        if (variant == Variant::Rescue && !rescue_hook) || (variant == Variant::Protected && !plan.anticheat) {
            continue;
        }
        let title = match variant {
            Variant::Normal => "archix",
            Variant::Rescue => "archix rescue",
            Variant::Protected => "archix protected",
        };
        let mut text = release.clone();
        text.push(format!(
            "PRETTY_NAME=\"Arch Linux ({title}, generation {})\"",
            plan.generation
        ));
        text.push("IMAGE_ID=archix".to_string());
        text.push(format!("IMAGE_VERSION={}", plan.generation));
        let release_file = work.join(format!("os-release-{title}").replace(' ', "-"));
        fs::write(&release_file, text.join("\n") + "\n")?;
        let mut cmdline = format!("{} archix.generation={}", plan.cmdline.trim(), plan.generation);
        match variant {
            Variant::Rescue => cmdline.push_str(" archix.rescue"),
            Variant::Protected => {
                cmdline.push(' ');
                cmdline.push_str(super::secureboot::ANTICHEAT_CMDLINE);
            }
            Variant::Normal => {}
        }
        let output = work.join(entry_name(plan.generation, variant));
        let mut ukify = Command::new("ukify");
        ukify
            .arg("build")
            .arg("--linux")
            .arg(Path::new(MODULES).join(&version).join("vmlinuz"));
        for microcode in &plan.microcode {
            ukify.arg("--initrd").arg(Path::new(&plan.esp).join(microcode));
        }
        ukify.arg("--initrd").arg(&initramfs);
        ukify.args(["--cmdline", &cmdline, "--uname", &version]);
        ukify.arg("--os-release").arg(format!("@{}", release_file.display()));
        ukify.arg("--output").arg(&output);
        run(&mut ukify, "ukify build")?;
        if secure_boot {
            run(
                Command::new("sbctl")
                    .env("ESP_PATH", &plan.esp)
                    .arg("sign")
                    .arg(&output),
                "sbctl sign",
            )?;
        }
        built.push((variant, output));
    }
    if secure_boot {
        for loader in ["EFI/systemd/systemd-bootx64.efi", "EFI/BOOT/BOOTX64.EFI"] {
            let path = Path::new(&plan.esp).join(loader);
            if path.exists() {
                run(
                    Command::new("sbctl")
                        .env("ESP_PATH", &plan.esp)
                        .args(["sign", "-s"])
                        .arg(&path),
                    "sbctl sign",
                )?;
            }
        }
    }

    refuse_unsigned(plan, &built)?;
    let dir = linux_dir(&plan.esp);
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut keep: Vec<u32> = entries(&plan.esp)
        .into_iter()
        .filter(|(_, rescue, _)| !rescue)
        .map(|(n, _, _)| n)
        .collect();
    keep.retain(|n| *n != plan.generation);
    keep.sort_unstable();
    let slots = plan.keep.saturating_sub(1) as usize;
    let mut kept: BTreeSet<u32> = keep.iter().rev().take(slots).copied().collect();
    kept.insert(plan.generation);
    // Pruning must never take the entry this boot came from, nor the one a failed switch falls back to.
    let protected: BTreeSet<u32> = [
        crate::generations::Store::new(paths).current().ok().flatten(),
        fs::read_to_string("/proc/cmdline")
            .ok()
            .and_then(|cmdline| booted_generation(&cmdline)),
    ]
    .into_iter()
    .flatten()
    .collect();
    kept.extend(&protected);
    let rescue_kept: BTreeSet<u32> = kept.iter().rev().take(2).copied().collect();
    for (number, rescue, path) in entries(&plan.esp) {
        if !kept.contains(&number) || (rescue && !rescue_kept.contains(&number)) {
            fs::remove_file(path)?;
        }
    }
    let needed: u64 = built
        .iter()
        .map(|(_, path)| fs::metadata(path).map_or(0, |m| m.len()))
        .sum::<u64>()
        + MARGIN;
    while free_bytes(&dir)? < needed {
        let oldest = entries(&plan.esp)
            .into_iter()
            .find(|(number, _, _)| *number != plan.generation && !protected.contains(number));
        match oldest {
            Some((_, _, path)) => fs::remove_file(path)?,
            None => bail!("the ESP at {} has no room for a boot entry", plan.esp),
        }
    }
    for (variant, path) in built {
        let dest = dir.join(entry_name(plan.generation, variant));
        let tmp = dir.join(format!(".{}.tmp", entry_name(plan.generation, variant)));
        fs::copy(&path, &tmp).with_context(|| format!("writing {}", tmp.display()))?;
        fs::File::open(&tmp)?.sync_all()?;
        fs::rename(&tmp, &dest)?;
    }
    crate::durable::sync_dir(&dir.join("x"))?;
    let _ = fs::remove_dir_all(&work);
    Ok(version)
}

/// Makes systemd-boot start `generation` by default, so a rollback survives the next reboot instead of booting the newest entry.
pub fn set_default_entry(paths: &Paths, generation: u32) -> Result<()> {
    let Ok(esp) = fs::read_to_string(paths.state.join("boot").join("esp")) else {
        return Ok(());
    };
    let esp = esp.trim();
    if !entries(esp)
        .iter()
        .any(|(number, extra, _)| *number == generation && !extra)
    {
        return Ok(());
    }
    let conf = Path::new(esp).join("loader/loader.conf");
    let mut lines: Vec<String> = fs::read_to_string(&conf)
        .unwrap_or_default()
        .lines()
        .filter(|line| line.split_whitespace().next() != Some("default"))
        .map(str::to_string)
        .collect();
    lines.push(format!("default {}", entry_name(generation, Variant::Normal)));
    crate::durable::write(&conf, (lines.join("\n") + "\n").as_bytes())?;
    // An EFI variable set by `bootctl set-default` would override loader.conf.
    let _ = Command::new("bootctl")
        .args(["set-default", ""])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    Ok(())
}

/// Checked before a switch changes anything: the new entries must fit in what's free plus what removing every older archix entry would free.
pub fn check_space(esp: &str, kernel: &str, protected: bool) -> Result<()> {
    let existing = entries(esp);
    // Without an entry to go by, the kernel image plus room for an initramfs stands in for one.
    let estimate = || {
        let version = kernel_version(kernel).ok()?;
        let image = fs::metadata(Path::new(MODULES).join(version).join("vmlinuz")).ok()?;
        Some(image.len() + 64 * 1024 * 1024)
    };
    let Some(newest) = existing
        .iter()
        .filter(|(_, extra, _)| !extra)
        .filter_map(|(_, _, path)| fs::metadata(path).ok().map(|m| m.len()))
        .max()
        .or_else(estimate)
    else {
        return Ok(());
    };
    let rescue = Path::new("/etc/initcpio/install/archix-rescue").exists();
    let copies = 1 + u64::from(rescue) + u64::from(protected);
    let needed = newest * copies + MARGIN;
    let dir = linux_dir(esp);
    let reclaimable: u64 = existing
        .iter()
        .filter_map(|(_, _, path)| fs::metadata(path).ok().map(|m| m.len()))
        .sum();
    let measured = dir.ancestors().find(|path| path.exists()).unwrap_or(Path::new("/"));
    let free = free_bytes(measured)?;
    if free + reclaimable < needed {
        let mib = |bytes: u64| bytes / (1024 * 1024);
        bail!(
            "the ESP at {esp} has {} MiB free ({} MiB more after removing every older archix entry), but this switch's boot entries need about {} MiB; nothing was changed. Free up space on it, or set boot.entries lower",
            mib(free),
            mib(reclaimable),
            mib(needed)
        );
    }
    Ok(())
}

/// Checked before a switch changes anything: with Secure Boot enforcing, entries archix won't sign could never boot.
pub fn check_signing(secure_boot: Option<bool>) -> Result<()> {
    let enforcing = super::secureboot::Machine::new()
        .state()
        .is_some_and(|state| state.enabled && !state.setup_mode);
    if enforcing && !secure_boot.unwrap_or_else(sbctl_has_keys) && super::secureboot::in_chroot() {
        eprintln!(
            "warning: this host's Secure Boot is on and the boot entry won't be signed; ignored, since this runs in a chroot"
        );
    } else if enforcing && !secure_boot.unwrap_or_else(sbctl_has_keys) {
        bail!(
            "Secure Boot is on, but this switch's boot entry wouldn't be signed ({}), so the firmware would refuse it; nothing was changed. Run `sudo archix secure-boot setup`, or set boot.secure_boot = true if sbctl already has keys",
            if secure_boot == Some(false) {
                "boot.secure_boot = false"
            } else {
                "sbctl has no keys"
            }
        );
    }
    Ok(())
}

/// With Secure Boot enforcing, an unsigned entry or loader would leave the new default unbootable, so it fails the switch before anything moves.
fn refuse_unsigned(plan: &BootPlan, built: &[(Variant, PathBuf)]) -> Result<()> {
    let Some(state) = super::secureboot::Machine::new().state() else {
        return Ok(());
    };
    if !state.enabled || state.setup_mode {
        return Ok(());
    }
    if super::secureboot::in_chroot() {
        eprintln!(
            "warning: not checking the new boot entry's signature against this host's Secure Boot, since this runs in a chroot"
        );
        return Ok(());
    }
    let loaders = ["EFI/systemd/systemd-bootx64.efi", "EFI/BOOT/BOOTX64.EFI"]
        .iter()
        .map(|loader| Path::new(&plan.esp).join(loader))
        .filter(|path| path.exists());
    for path in built.iter().map(|(_, path)| path.clone()).chain(loaders) {
        if !super::secureboot::is_signed(&path)? {
            bail!(
                "Secure Boot is on, but {} isn't signed, so the firmware would refuse it; nothing was moved. Run `sudo archix secure-boot setup` (or set boot.secure_boot = true if sbctl already has keys)",
                path.file_name().unwrap_or_default().to_string_lossy()
            );
        }
    }
    Ok(())
}

/// The value of `archix.generation=` on a kernel command line.
pub fn booted_generation(cmdline: &str) -> Option<u32> {
    cmdline
        .split_whitespace()
        .find_map(|arg| arg.strip_prefix("archix.generation=")?.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rollback_becomes_the_default_entry() {
        let dir = tempfile::tempdir().unwrap();
        let esp = dir.path().join("esp");
        fs::create_dir_all(esp.join("EFI/Linux")).unwrap();
        fs::create_dir_all(esp.join("loader")).unwrap();
        for name in ["archix-2.efi", "archix-3.efi"] {
            fs::write(esp.join("EFI/Linux").join(name), "uki").unwrap();
        }
        fs::write(esp.join("loader/loader.conf"), "timeout 2\ndefault archix-3.efi\n").unwrap();
        let paths = Paths::under(&dir.path().join("root"));
        fs::create_dir_all(paths.state.join("boot")).unwrap();
        fs::write(paths.state.join("boot/esp"), esp.to_str().unwrap()).unwrap();
        set_default_entry(&paths, 2).unwrap();
        assert_eq!(
            fs::read_to_string(esp.join("loader/loader.conf")).unwrap(),
            "timeout 2\ndefault archix-2.efi\n"
        );
        set_default_entry(&paths, 7).unwrap();
        assert!(
            fs::read_to_string(esp.join("loader/loader.conf"))
                .unwrap()
                .contains("archix-2.efi"),
            "a generation without an entry leaves the default alone"
        );
    }

    #[test]
    fn the_kernel_version_is_the_one_pacman_owns_not_a_kept_one() {
        let db = tempfile::tempdir().unwrap();
        let entry = db.path().join("linux-7.2.9.arch1-1");
        fs::create_dir_all(&entry).unwrap();
        fs::write(
            entry.join("files"),
            "%FILES%\nusr/\nusr/lib/modules/7.2.9-arch1-1/\nusr/lib/modules/7.2.9-arch1-1/pkgbase\nusr/lib/modules/7.2.9-arch1-1/vmlinuz\n\n",
        )
        .unwrap();
        fs::create_dir_all(db.path().join("linux-lts-6.12.1-1")).unwrap();
        assert_eq!(owned_version(db.path(), "linux").as_deref(), Some("7.2.9-arch1-1"));
        assert_eq!(owned_version(db.path(), "linux-zen"), None);
    }

    #[test]
    fn reads_the_generation_from_the_command_line() {
        assert_eq!(
            booted_generation("root=/dev/vda2 rw archix.generation=12 quiet"),
            Some(12)
        );
        assert_eq!(booted_generation("root=/dev/vda2 archix.rescue"), None);
    }

    #[test]
    fn lists_entries_with_their_generation() {
        let dir = tempfile::tempdir().unwrap();
        let esp = dir.path().to_str().unwrap();
        fs::create_dir_all(linux_dir(esp)).unwrap();
        for name in ["archix-3.efi", "archix-10.efi", "archix-10-rescue.efi", "other.efi"] {
            fs::write(linux_dir(esp).join(name), "").unwrap();
        }
        let found: Vec<(u32, bool)> = entries(esp).into_iter().map(|(n, r, _)| (n, r)).collect();
        assert_eq!(found, [(3, false), (10, false), (10, true)]);
        prune_entries(esp, &BTreeSet::from([10])).unwrap();
        assert_eq!(entries(esp).len(), 2);
    }
}
