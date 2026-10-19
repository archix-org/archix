use std::collections::BTreeSet;
use std::ffi::CString;
use std::fs;
use std::io;
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
    /// Boots systemd-boot tries the new entry before falling back to an older one; 0 turns boot counting off.
    #[serde(default)]
    pub tries: u32,
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

/// Refreshes the copy each time, so it carries the DKMS modules built since the kernel was first kept.
pub(crate) fn preserve_from(modules: &Path, paths: &Paths, version: &str) -> Result<()> {
    let dest = kernels(paths).join(version);
    if !modules.join(version).is_dir() && dest.exists() {
        return Ok(());
    }
    fs::create_dir_all(kernels(paths))?;
    let tmp = kernels(paths).join(format!(".{version}.{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    let status = Command::new("cp")
        .arg("-al")
        .arg(modules.join(version))
        .arg(&tmp)
        .status()?;
    if !status.success() {
        let _ = fs::remove_dir_all(&tmp);
        bail!("could not keep a copy of {version}'s modules");
    }
    if dest.exists() {
        exchange(&tmp, &dest).with_context(|| format!("refreshing the copy of {version}'s modules"))?;
        fs::remove_dir_all(&tmp)?;
    } else {
        fs::rename(&tmp, &dest)?;
    }
    Ok(())
}

/// Swaps two paths atomically, so a crash never leaves a kernel without its kept modules.
fn exchange(a: &Path, b: &Path) -> Result<()> {
    let (a, b) = (
        CString::new(a.as_os_str().as_encoded_bytes())?,
        CString::new(b.as_os_str().as_encoded_bytes())?,
    );
    let flags = libc::RENAME_EXCHANGE;
    if unsafe { libc::renameat2(libc::AT_FDCWD, a.as_ptr(), libc::AT_FDCWD, b.as_ptr(), flags) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
}

/// Puts a preserved kernel's modules back when the running kernel's are gone; true if it had to.
pub fn restore_modules(paths: &Paths, version: &str) -> Result<bool> {
    restore_into(Path::new(MODULES), paths, version)
}

/// A modules directory without `modules.dep` is what pacman leaves when unowned files, such as DKMS's directories, keep it from deleting it; nothing loads from it.
pub fn restore_into(modules: &Path, paths: &Paths, version: &str) -> Result<bool> {
    let target = modules.join(version);
    let source = kernels(paths).join(version);
    if target.join("modules.dep").exists() || !source.exists() {
        return Ok(false);
    }
    let tmp = modules.join(format!(".{version}.restoring"));
    if tmp.exists() {
        fs::remove_dir_all(&tmp)?;
    }
    link_tree(&source, &tmp).with_context(|| format!("restoring {version}'s modules"))?;
    if fs::symlink_metadata(&target).is_ok_and(|meta| meta.is_dir()) {
        move_missing(&target, &tmp)?;
        exchange(&tmp, &target).with_context(|| format!("restoring {version}'s modules"))?;
        fs::remove_dir_all(&tmp)?;
    } else {
        fs::rename(&tmp, &target).with_context(|| format!("restoring {version}'s modules"))?;
    }
    Ok(true)
}

/// Moves what `from` has and `to` lacks into `to`, so files left in an incomplete modules directory survive its restore.
fn move_missing(from: &Path, to: &Path) -> Result<()> {
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let (source, target) = (entry.path(), to.join(entry.file_name()));
        match fs::symlink_metadata(&target) {
            Err(_) => fs::rename(&source, &target)?,
            Ok(meta) if meta.is_dir() && entry.file_type()?.is_dir() => move_missing(&source, &target)?,
            Ok(_) => {}
        }
    }
    Ok(())
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

/// Where archix put one UKI per generation before UKIs were shared; systemd-boot lists every UKI here on its own.
fn linux_dir(esp: &str) -> PathBuf {
    Path::new(esp).join("EFI/Linux")
}

/// The shared UKIs, outside EFI/Linux so systemd-boot shows them only through the entries that name them.
fn uki_dir(esp: &str) -> PathBuf {
    Path::new(esp).join("EFI/archix")
}

fn entries_dir(esp: &str) -> PathBuf {
    Path::new(esp).join("loader/entries")
}

/// The boot menu entry of a generation, which names the UKI it boots.
fn entry_name(generation: u32, variant: Variant) -> String {
    match variant {
        Variant::Normal => format!("archix-{generation}.conf"),
        Variant::Rescue => format!("archix-{generation}-rescue.conf"),
        Variant::Protected => format!("archix-{generation}-protected.conf"),
    }
}

/// A generation's normal entry, with systemd-boot's boot counter when it gets `tries` attempts: archix-N+3.conf becomes archix-N.conf once a boot reaches boot-complete.target, and archix-N+0-3.conf is skipped for an older entry after three failed ones.
fn counted_entry_name(generation: u32, tries: u32) -> String {
    match tries {
        0 => entry_name(generation, Variant::Normal),
        tries => format!("archix-{generation}+{tries}.conf"),
    }
}

/// Whether systemd-boot has used up a generation's tries without a boot reaching boot-complete.target.
pub fn failed_to_boot(paths: &Paths, generation: u32) -> bool {
    let Ok(esp) = fs::read_to_string(paths.state.join("boot").join("esp")) else {
        return false;
    };
    entries(esp.trim()).iter().any(|(number, extra, path)| {
        *number == generation && !extra && is_bad(&path.file_name().unwrap_or_default().to_string_lossy())
    })
}

/// An entry systemd-boot has given up on: no tries left.
pub fn is_bad(name: &str) -> bool {
    name.split_once('+')
        .and_then(|(_, counters)| counters.split(['-', '.']).next())
        .is_some_and(|left| left == "0")
}

/// The generation an entry file (archix-N.conf, archix-N-rescue.conf, or a pre-sharing archix-N.efi) belongs to, and whether it's an extra one.
fn parse_entry(name: &str) -> Option<(u32, bool)> {
    let stem = name.strip_prefix("archix-")?;
    let stem = stem.strip_suffix(".conf").or_else(|| stem.strip_suffix(".efi"))?;
    let stem = stem.split('+').next().unwrap_or(stem);
    let (number, extra) = match stem.strip_suffix("-rescue").or_else(|| stem.strip_suffix("-protected")) {
        Some(number) => (number, true),
        None => (stem, false),
    };
    Some((number.parse().ok()?, extra))
}

/// The generations that have a boot entry on the ESP, with whether it's an extra (rescue or protected) entry.
pub fn entries(esp: &str) -> Vec<(u32, bool, PathBuf)> {
    let mut found = Vec::new();
    for dir in [entries_dir(esp), linux_dir(esp)] {
        for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
            if let Some((number, extra)) = parse_entry(&entry.file_name().to_string_lossy()) {
                found.push((number, extra, entry.path()));
            }
        }
    }
    found.sort();
    found
}

/// The UKI an entry boots: the file a .conf names, or a pre-sharing entry itself.
fn uki_of(esp: &str, entry: &Path) -> Option<PathBuf> {
    if entry.extension().is_some_and(|ext| ext == "efi") {
        return Some(entry.to_path_buf());
    }
    let text = fs::read_to_string(entry).ok()?;
    let path = text.lines().find_map(|line| line.strip_prefix("efi"))?.trim();
    Some(Path::new(esp).join(path.trim_start_matches('/')))
}

/// Deletes shared UKIs no entry names any more.
fn remove_unused_ukis(esp: &str) -> Result<()> {
    remove_unused_ukis_but(esp, &[])
}

/// Deletes boot entries for generations that are no longer kept, and the UKIs only they used.
pub fn prune_entries(esp: &str, keep: &BTreeSet<u32>) -> Result<usize> {
    let mut removed = 0;
    for (number, _, path) in entries(esp) {
        if !keep.contains(&number) {
            fs::remove_file(&path)?;
            removed += 1;
        }
    }
    remove_unused_ukis(esp)?;
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

/// Whether sbctl has signing keys, which is when archix signs by default.
pub fn sbctl_has_keys() -> bool {
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
    let vmlinuz = Path::new(MODULES).join(&version).join("vmlinuz");
    let microcode: Vec<PathBuf> = plan.microcode.iter().map(|m| Path::new(&plan.esp).join(m)).collect();
    let shared = uki_dir(&plan.esp);
    // The UKI each variant boots, and the ones this switch has to build because no earlier generation's matches.
    let mut ukis = Vec::new();
    let mut built = Vec::new();
    for variant in [Variant::Normal, Variant::Rescue, Variant::Protected] {
        if (variant == Variant::Rescue && !rescue_hook) || (variant == Variant::Protected && !plan.anticheat) {
            continue;
        }
        let (title, suffix) = match variant {
            Variant::Normal => ("archix", ""),
            Variant::Rescue => ("archix rescue", "-rescue"),
            Variant::Protected => ("archix protected", "-protected"),
        };
        let mut text = release.clone();
        text.push(format!("PRETTY_NAME=\"Arch Linux ({title})\""));
        text.push("IMAGE_ID=archix".to_string());
        let release_text = text.join("\n") + "\n";
        let mut cmdline = plan.cmdline.trim().to_string();
        match variant {
            Variant::Rescue => cmdline.push_str(" archix.rescue"),
            Variant::Protected => {
                cmdline.push(' ');
                cmdline.push_str(super::secureboot::ANTICHEAT_CMDLINE);
            }
            Variant::Normal => {}
        }
        let key = payload_key(&vmlinuz, &microcode, &initramfs, &cmdline, &release_text, secure_boot)?;
        let dest = shared.join(format!("{version}{suffix}-{key}.efi"));
        if !dest.exists() {
            let release_file = work.join(format!("os-release{suffix}"));
            fs::write(&release_file, &release_text)?;
            let output = work.join(dest.file_name().unwrap_or_default());
            let mut ukify = Command::new("ukify");
            ukify.arg("build").arg("--linux").arg(&vmlinuz);
            for file in &microcode {
                ukify.arg("--initrd").arg(file);
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
            built.push((dest.clone(), output));
        }
        ukis.push((
            variant,
            format!("Arch Linux ({title}, generation {})", plan.generation),
            dest,
        ));
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

    let to_check: Vec<PathBuf> = ukis
        .iter()
        .map(|(_, _, dest)| {
            built
                .iter()
                .find(|(target, _)| target == dest)
                .map_or_else(|| dest.clone(), |(_, output)| output.clone())
        })
        .collect();
    refuse_unsigned(plan, &to_check)?;
    for dir in [&shared, &entries_dir(&plan.esp)] {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
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
    let protected = protected_generations(paths);
    kept.extend(&protected);
    let rescue_kept: BTreeSet<u32> = kept.iter().rev().take(2).copied().collect();
    for (number, rescue, path) in entries(&plan.esp) {
        if !kept.contains(&number) || (rescue && !rescue_kept.contains(&number)) {
            fs::remove_file(path)?;
        }
    }
    let reused: Vec<PathBuf> = ukis.iter().map(|(_, _, dest)| dest.clone()).collect();
    remove_unused_ukis_but(&plan.esp, &reused)?;
    let needed: u64 = built
        .iter()
        .map(|(_, output)| fs::metadata(output).map_or(0, |m| m.len()))
        .sum::<u64>()
        + MARGIN;
    while free_bytes(&shared)? < needed {
        let oldest = entries(&plan.esp)
            .into_iter()
            .find(|(number, _, _)| *number != plan.generation && !protected.contains(number));
        match oldest {
            Some((_, _, path)) => {
                fs::remove_file(path)?;
                remove_unused_ukis_but(&plan.esp, &reused)?;
            }
            None => bail!("the ESP at {} has no room for a boot entry", plan.esp),
        }
    }
    for (dest, output) in &built {
        let tmp = shared.join(format!(
            ".{}.tmp",
            dest.file_name().unwrap_or_default().to_string_lossy()
        ));
        fs::copy(output, &tmp).with_context(|| format!("writing {}", tmp.display()))?;
        fs::File::open(&tmp)?.sync_all()?;
        fs::rename(&tmp, dest)?;
    }
    crate::durable::sync_dir(&shared.join("x"))?;
    for (variant, title, dest) in &ukis {
        let relative = dest.strip_prefix(&plan.esp).unwrap_or(dest);
        let text = format!(
            "title {title}\nversion {}\nsort-key archix\nefi /{}\n",
            plan.generation,
            relative.display()
        );
        let name = match variant {
            Variant::Normal => {
                // A rerun of this step, as after a power cut, starts the generation's tries over.
                for (number, extra, path) in entries(&plan.esp) {
                    if number == plan.generation && !extra && path.extension().is_some_and(|ext| ext == "conf") {
                        fs::remove_file(path)?;
                    }
                }
                counted_entry_name(plan.generation, plan.tries)
            }
            _ => entry_name(plan.generation, *variant),
        };
        crate::durable::write(&entries_dir(&plan.esp).join(name), text.as_bytes())?;
    }
    let _ = fs::remove_dir_all(&work);
    Ok(version)
}

/// Deletes shared UKIs no entry names, except `keep`, which entries are about to name.
fn remove_unused_ukis_but(esp: &str, keep: &[PathBuf]) -> Result<()> {
    let used: BTreeSet<PathBuf> = entries(esp)
        .iter()
        .filter_map(|(_, _, entry)| uki_of(esp, entry))
        .chain(keep.iter().cloned())
        .collect();
    for file in fs::read_dir(uki_dir(esp)).into_iter().flatten().flatten() {
        if !used.contains(&file.path()) {
            fs::remove_file(file.path())?;
        }
    }
    Ok(())
}

/// What a UKI is built from, hashed: generations whose kernel, microcode, initramfs (which mkinitcpio builds reproducibly), command line and os-release match share one UKI.
fn payload_key(
    vmlinuz: &Path,
    microcode: &[PathBuf],
    initramfs: &Path,
    cmdline: &str,
    os_release: &str,
    signed: bool,
) -> Result<String> {
    let mut parts = vec![crate::fetch::sha256(vmlinuz)?];
    for file in microcode {
        parts.push(crate::fetch::sha256(file)?);
    }
    parts.push(crate::fetch::sha256(initramfs)?);
    parts.push(crate::fetch::sha256_bytes(cmdline.as_bytes()));
    parts.push(crate::fetch::sha256_bytes(os_release.as_bytes()));
    parts.push(signed.to_string());
    Ok(crate::fetch::sha256_bytes(parts.join("\n").as_bytes())[..12].to_string())
}

/// Makes systemd-boot start `generation` by default, so a rollback survives the next reboot instead of booting the newest entry.
pub fn set_default_entry(paths: &Paths, generation: u32) -> Result<()> {
    let Ok(esp) = fs::read_to_string(paths.state.join("boot").join("esp")) else {
        return Ok(());
    };
    let esp = esp.trim();
    let Some(entry) = entries(esp)
        .into_iter()
        .filter(|(number, extra, _)| *number == generation && !extra)
        .map(|(_, _, path)| path.file_name().unwrap_or_default().to_string_lossy().into_owned())
        .min_by_key(|name| !name.ends_with(".conf"))
    else {
        return Ok(());
    };
    write_default(esp, &entry)
}

/// Names `entry` as systemd-boot's default in loader.conf.
fn write_default(esp: &str, entry: &str) -> Result<()> {
    let conf = Path::new(esp).join("loader/loader.conf");
    let mut lines: Vec<String> = fs::read_to_string(&conf)
        .unwrap_or_default()
        .lines()
        .filter(|line| line.split_whitespace().next() != Some("default"))
        .map(str::to_string)
        .collect();
    lines.push(format!("default {entry}"));
    crate::durable::write(&conf, (lines.join("\n") + "\n").as_bytes())?;
    // An EFI variable set by `bootctl set-default` would override loader.conf.
    let _ = Command::new("bootctl")
        .args(["set-default", ""])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    Ok(())
}

/// Installed kernel packages by pkgbase, each with the version of the modules directory it owns.
pub fn installed_kernels() -> Vec<(String, String)> {
    let local = Path::new("/var/lib/pacman/local");
    let mut kernels: Vec<(String, String)> = fs::read_dir(MODULES)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let base = fs::read_to_string(entry.path().join("pkgbase"))
                .ok()?
                .trim()
                .to_string();
            let version = entry.file_name().to_string_lossy().into_owned();
            (owned_version(local, &base).as_deref() == Some(version.as_str())).then_some((base, version))
        })
        .collect();
    kernels.sort();
    kernels
}

/// How systemd-boot starts each kernel without archix: plain entries for /boot's kernel and initramfs when the ESP is /boot, else UKIs mkinitcpio's preset writes to EFI/Linux, which kernel updates rebuild. Makes `default`'s the default, then takes archix's entries and UKIs off.
pub fn hand_over(esp: &str, kernels: &[(String, String)], default: &str, sign: bool) -> Result<Vec<String>> {
    let plain = fs::canonicalize(esp).ok() == fs::canonicalize("/boot").ok();
    let cmdline = fs::read_to_string("/etc/kernel/cmdline")
        .map(|text| text.trim().to_string())
        .ok()
        .filter(|text| !text.is_empty())
        .context("the kernel command line isn't in /etc/kernel/cmdline, where archix writes kernel_params; nothing was changed")?;
    let mut written = Vec::new();
    let mut default_id = None;
    for (base, version) in kernels {
        // mkinitcpio's pacman hook keeps this copy; a preset reads the kernel from it.
        let vmlinuz = PathBuf::from(format!("/boot/vmlinuz-{base}"));
        if !vmlinuz.is_file() {
            fs::copy(Path::new(MODULES).join(version).join("vmlinuz"), &vmlinuz)
                .with_context(|| format!("copying {}", vmlinuz.display()))?;
        }
        let id = if plain {
            run(Command::new("mkinitcpio").args(["-p", base]), "mkinitcpio")?;
            let microcode: String = ["amd-ucode.img", "intel-ucode.img"]
                .iter()
                .filter(|image| Path::new(esp).join(image).is_file())
                .map(|image| format!("initrd /{image}\n"))
                .collect();
            for (suffix, initramfs) in [
                ("", format!("initramfs-{base}.img")),
                ("-fallback", format!("initramfs-{base}-fallback.img")),
            ] {
                if !Path::new(esp).join(&initramfs).is_file() {
                    if suffix.is_empty() {
                        bail!("mkinitcpio -p {base} wrote no {esp}/{initramfs}");
                    }
                    continue;
                }
                let name = format!("arch-{base}{suffix}.conf");
                let path = entries_dir(esp).join(&name);
                if !path.exists() {
                    let text = format!(
                        "title Arch Linux ({base}{suffix})\nlinux /vmlinuz-{base}\n{microcode}initrd /{initramfs}\noptions {cmdline}\n"
                    );
                    crate::durable::write(&path, text.as_bytes())?;
                    written.push(path.display().to_string());
                }
            }
            if sign {
                run(
                    Command::new("sbctl")
                        .env("ESP_PATH", esp)
                        .args(["sign", "-s"])
                        .arg(&vmlinuz),
                    "sbctl sign",
                )?;
            }
            format!("arch-{base}.conf")
        } else {
            let preset = PathBuf::from(format!("/etc/mkinitcpio.d/{base}.preset"));
            let uki = linux_dir(esp).join(format!("arch-{base}.efi"));
            let line = format!("default_uki=\"{}\"", uki.display());
            let text = fs::read_to_string(&preset).with_context(|| format!("reading {}", preset.display()))?;
            if !text.lines().any(|existing| existing.trim() == line) {
                let text = format!(
                    "{}\n# archix eject: a UKI on the ESP, which systemd-boot lists by itself\n{line}\n",
                    text.trim_end()
                );
                crate::durable::write(&preset, text.as_bytes())?;
            }
            fs::create_dir_all(linux_dir(esp))?;
            run(Command::new("mkinitcpio").args(["-p", base]), "mkinitcpio")?;
            if !uki.is_file() {
                bail!("mkinitcpio -p {base} wrote no {}", uki.display());
            }
            if sign {
                run(
                    Command::new("sbctl")
                        .env("ESP_PATH", esp)
                        .args(["sign", "-s"])
                        .arg(&uki),
                    "sbctl sign",
                )?;
            }
            written.push(uki.display().to_string());
            format!("arch-{base}.efi")
        };
        if base == default || default_id.is_none() {
            default_id = Some(id);
        }
    }
    let Some(default_id) = default_id else {
        bail!("no installed kernel package owns a directory in {MODULES}; nothing was changed");
    };
    write_default(esp, &default_id)?;
    for (_, _, path) in entries(esp) {
        fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    }
    match fs::remove_dir_all(uki_dir(esp)) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e).context("removing archix's UKIs"),
        _ => {}
    }
    written.insert(0, format!("default {default_id}"));
    Ok(written)
}

/// Generations whose entries pruning never takes: the one this boot came from and the current one, which a failed switch falls back to.
pub fn protected_generations(paths: &Paths) -> BTreeSet<u32> {
    [crate::generations::Store::new(paths).current().ok().flatten(), booted()]
        .into_iter()
        .flatten()
        .collect()
}

/// Checked before a switch changes anything: the new entries must fit in what's free plus what removing every older archix entry would free.
pub fn check_space(esp: &str, kernel: &str, protected: bool) -> Result<()> {
    let existing: Vec<(u32, bool, PathBuf)> = entries(esp)
        .into_iter()
        .filter_map(|(number, extra, entry)| Some((number, extra, uki_of(esp, &entry)?)))
        .collect();
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
    let protected = protected_generations(&Paths::system());
    let pinned: BTreeSet<&PathBuf> = existing
        .iter()
        .filter(|(number, _, _)| protected.contains(number))
        .map(|(_, _, uki)| uki)
        .collect();
    let reclaimable: u64 = existing
        .iter()
        .map(|(_, _, uki)| uki)
        .filter(|uki| !pinned.contains(uki))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter_map(|path| fs::metadata(path).ok().map(|m| m.len()))
        .sum();
    let measured = dir.ancestors().find(|path| path.exists()).unwrap_or(Path::new("/"));
    let free = free_bytes(measured)?;
    if free + reclaimable < needed {
        let mib = |bytes: u64| bytes / (1024 * 1024);
        bail!(
            "the ESP at {esp} has {} MiB free ({} MiB more after removing the older archix entries it may), but this switch's boot entries need about {} MiB; nothing was changed. Free up space on it, or set boot.entries lower",
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
fn refuse_unsigned(plan: &BootPlan, ukis: &[PathBuf]) -> Result<()> {
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
    for path in ukis.iter().cloned().chain(loaders) {
        if !super::secureboot::is_signed(&path)? {
            bail!(
                "Secure Boot is on, but {} isn't signed, so the firmware would refuse it; nothing was moved. Run `sudo archix secure-boot setup` (or set boot.secure_boot = true if sbctl already has keys)",
                path.file_name().unwrap_or_default().to_string_lossy()
            );
        }
    }
    Ok(())
}

/// The value of `archix.generation=` on a kernel command line, which UKIs carried before generations shared them.
pub fn booted_generation(cmdline: &str) -> Option<u32> {
    cmdline
        .split_whitespace()
        .find_map(|arg| arg.strip_prefix("archix.generation=")?.parse().ok())
}

/// The generation this boot came from: the command line's, else the boot menu entry systemd-boot started (LoaderEntrySelected).
pub fn booted() -> Option<u32> {
    let cmdline = crate::testhooks::kernel_cmdline();
    booted_generation(&cmdline).or_else(|| match crate::testhooks::selected_entry() {
        Some(entry) => parse_entry(&entry).map(|(number, _)| number),
        None => None,
    })
}

/// systemd-boot's LoaderEntrySelected: 4 bytes of attributes, then the entry's id in UTF-16LE.
pub fn read_selected_entry() -> Option<String> {
    const VAR: &str = "/sys/firmware/efi/efivars/LoaderEntrySelected-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f";
    efivar_string(&fs::read(VAR).ok()?)
}

fn efivar_string(bytes: &[u8]) -> Option<String> {
    let units: Vec<u16> = bytes
        .get(4..)?
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .take_while(|unit| *unit != 0)
        .collect();
    String::from_utf16(&units).ok()
}

/// In an initramfs without systemd, efivarfs may not be mounted yet.
pub fn mount_efivars() {
    let dir = Path::new("/sys/firmware/efi/efivars");
    if !Path::new("/sys/firmware/efi").exists() || fs::read_dir(dir).is_ok_and(|mut d| d.next().is_some()) {
        return;
    }
    let c = |s: &str| CString::new(s).unwrap_or_default();
    let flags = libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC | libc::MS_RDONLY;
    unsafe {
        libc::mount(
            c("efivarfs").as_ptr(),
            c("/sys/firmware/efi/efivars").as_ptr(),
            c("efivarfs").as_ptr(),
            flags,
            std::ptr::null(),
        )
    };
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
    fn generations_share_a_uki_and_pruning_keeps_it_while_one_still_boots_it() {
        let dir = tempfile::tempdir().unwrap();
        let esp = dir.path().to_str().unwrap();
        fs::create_dir_all(uki_dir(esp)).unwrap();
        fs::create_dir_all(entries_dir(esp)).unwrap();
        fs::create_dir_all(linux_dir(esp)).unwrap();
        fs::write(uki_dir(esp).join("7.2.9-arch1-1-aaaa.efi"), "uki").unwrap();
        fs::write(uki_dir(esp).join("7.2.9-arch1-1-bbbb.efi"), "uki").unwrap();
        for (number, uki) in [(4, "aaaa"), (5, "aaaa"), (6, "bbbb")] {
            let text = format!("title t\nversion {number}\nsort-key archix\nefi /EFI/archix/7.2.9-arch1-1-{uki}.efi\n");
            fs::write(entries_dir(esp).join(entry_name(number, Variant::Normal)), text).unwrap();
        }
        fs::write(linux_dir(esp).join("archix-3.efi"), "old layout").unwrap();
        let numbers: Vec<u32> = entries(esp).iter().map(|(n, _, _)| *n).collect();
        assert_eq!(numbers, [3, 4, 5, 6]);
        prune_entries(esp, &BTreeSet::from([5])).unwrap();
        let left: Vec<String> = fs::read_dir(uki_dir(esp))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            left,
            ["7.2.9-arch1-1-aaaa.efi"],
            "generation 5 still boots the UKI it shares with 4"
        );
        assert_eq!(entries(esp).iter().map(|(n, _, _)| *n).collect::<Vec<_>>(), [5]);
    }

    #[test]
    fn the_booted_entry_names_its_generation() {
        let mut bytes = vec![6, 0, 0, 0];
        bytes.extend("archix-12-rescue.conf".encode_utf16().flat_map(u16::to_le_bytes));
        bytes.extend([0, 0]);
        let entry = efivar_string(&bytes).unwrap();
        assert_eq!(entry, "archix-12-rescue.conf");
        assert_eq!(parse_entry(&entry), Some((12, true)));
        assert_eq!(parse_entry("archix-7.efi"), Some((7, false)));
        assert_eq!(parse_entry("arch.conf"), None);
    }

    #[test]
    fn entries_with_boot_counters_keep_their_generation() {
        for name in [
            "archix-9+3.conf",
            "archix-9+2-1.conf",
            "archix-9+0-3.conf",
            "archix-9.conf",
        ] {
            assert_eq!(parse_entry(name), Some((9, false)), "{name}");
        }
        assert_eq!(parse_entry("archix-9-rescue.conf"), Some((9, true)));
        assert!(is_bad("archix-9+0-3.conf") && !is_bad("archix-9+1-2.conf") && !is_bad("archix-9.conf"));
        assert_eq!(counted_entry_name(9, 3), "archix-9+3.conf");
        assert_eq!(counted_entry_name(9, 0), "archix-9.conf");
    }

    #[test]
    fn the_kept_modules_follow_dkms_builds() {
        let dir = tempfile::tempdir().unwrap();
        let modules = dir.path().join("modules");
        let paths = Paths::under(&dir.path().join("root"));
        fs::create_dir_all(modules.join("7.0.10-arch1-1/kernel")).unwrap();
        fs::write(modules.join("7.0.10-arch1-1/kernel/dummy.ko"), "in-tree").unwrap();
        preserve_from(&modules, &paths, "7.0.10-arch1-1").unwrap();
        fs::create_dir_all(modules.join("7.0.10-arch1-1/updates/dkms")).unwrap();
        fs::write(modules.join("7.0.10-arch1-1/updates/dkms/v4l2loopback.ko"), "dkms").unwrap();
        preserve_from(&modules, &paths, "7.0.10-arch1-1").unwrap();
        fs::remove_dir_all(modules.join("7.0.10-arch1-1")).unwrap();
        preserve_from(&modules, &paths, "7.0.10-arch1-1").unwrap();
        assert!(restore_into(&modules, &paths, "7.0.10-arch1-1").unwrap());
        assert!(modules.join("7.0.10-arch1-1/kernel/dummy.ko").exists());
        assert!(modules.join("7.0.10-arch1-1/updates/dkms/v4l2loopback.ko").exists());
        assert_eq!(fs::read_dir(paths.state.join("kernels")).unwrap().count(), 1);
    }

    #[test]
    fn a_modules_directory_dkms_kept_from_deletion_is_restored_too() {
        let dir = tempfile::tempdir().unwrap();
        let modules = dir.path().join("modules");
        let paths = Paths::under(&dir.path().join("root"));
        let release = modules.join("7.0.10-arch1-1");
        fs::create_dir_all(release.join("kernel/fs")).unwrap();
        fs::write(release.join("kernel/fs/vfat.ko"), "in-tree").unwrap();
        fs::write(release.join("modules.dep"), "kernel/fs/vfat.ko:\n").unwrap();
        preserve_from(&modules, &paths, "7.0.10-arch1-1").unwrap();
        assert!(
            !restore_into(&modules, &paths, "7.0.10-arch1-1").unwrap(),
            "a complete directory stays"
        );
        fs::remove_dir_all(&release).unwrap();
        fs::create_dir_all(release.join("updates/dkms")).unwrap();
        fs::write(release.join("updates/dkms/extra.ko"), "left behind").unwrap();
        assert!(restore_into(&modules, &paths, "7.0.10-arch1-1").unwrap());
        assert!(release.join("modules.dep").exists() && release.join("kernel/fs/vfat.ko").exists());
        assert_eq!(
            fs::read_to_string(release.join("updates/dkms/extra.ko")).unwrap(),
            "left behind"
        );
        assert_eq!(
            fs::read_dir(&modules).unwrap().count(),
            1,
            "no temporary directory is left"
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
