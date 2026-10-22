use std::collections::BTreeSet;
use std::ffi::CString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::deploy::{EspStep, Staging, Transaction};
use crate::paths::Paths;
use crate::testhooks::mark;

/// How a generation's boot entry is built: kept with the generation, so a rollback that rebuilds its deployment builds the same entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BootPlan {
    pub esp: String,
    pub kernel: String,
    pub microcode: Vec<String>,
    pub cmdline: String,
    pub secure_boot: Option<bool>,
    /// Also write a protected entry with the command line Easy Anti-Cheat Linux v2 asks for.
    #[serde(default)]
    pub anticheat: bool,
    /// Boots systemd-boot tries the new entry before falling back to an older one; 0 turns boot counting off.
    #[serde(default)]
    pub tries: u32,
    /// Revoke the UKIs no entry uses after each switch or rollback that commits a deployment.
    #[serde(default)]
    pub revoke_unused: bool,
    /// What a new deployment's counted boot must reach before it counts as good.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health_check: Option<super::health::HealthCheck>,
}

/// A generation's entries and the Type #2 fallback, a copy of the current generation's normal UKI that systemd-boot finds without loader/entries.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Variant {
    Normal,
    Protected,
    Fallback,
}

/// The fallback's sort key: below every distribution's id, so systemd-boot starts it when loader.conf names no entry it has.
const FALLBACK_SORT_KEY: &str = "0archix-fallback";
/// The fallback's title in the boot menu, which tells it from the generations' own entries.
pub const FALLBACK_TITLE: &str = "archix (fallback)";

pub const MODULES: &str = "/usr/lib/modules";
const MARGIN: u64 = 16 * 1024 * 1024;
/// Where a deployment keeps the UKIs its switch built, which the ESP step copies.
const BUILT: &str = "/boot/archix";

/// The modules dir the kernel package `kernel` owns in pacman database `local_db`.
pub fn owned_version(local_db: &Path, kernel: &str) -> Option<String> {
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

/// Where systemd-boot finds the UKIs mkinitcpio's presets write, which `hand_over` uses.
fn linux_dir(esp: &str) -> PathBuf {
    Path::new(esp).join("EFI/Linux")
}

/// The shared UKIs, outside EFI/Linux so systemd-boot shows them only through the entries that name them.
fn uki_dir(esp: &Path) -> PathBuf {
    esp.join("EFI/archix")
}

fn entries_dir(esp: &Path) -> PathBuf {
    esp.join("loader/entries")
}

/// The boot menu entry of a generation, which names the UKI it boots; the fallback is a UKI systemd-boot lists by its file name.
fn entry_name(generation: u32, variant: Variant) -> String {
    match variant {
        Variant::Normal => format!("archix-{generation}.conf"),
        Variant::Protected => format!("archix-{generation}-protected.conf"),
        Variant::Fallback => format!("archix-fallback-{generation}.efi"),
    }
}

/// Where the fallback UKI of `generation` goes on the ESP.
fn fallback_path(esp: &Path, generation: u32) -> PathBuf {
    esp.join("EFI/Linux").join(entry_name(generation, Variant::Fallback))
}

/// The generation a fallback UKI's file name, which is its entry id, belongs to.
pub fn parse_fallback(name: &str) -> Option<u32> {
    let number = name.strip_prefix("archix-fallback-")?.strip_suffix(".efi")?;
    if number.is_empty() || number.starts_with('0') || !number.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    number.parse().ok()
}

/// The generations whose fallback UKI is on the ESP.
pub fn fallbacks(esp: &Path) -> Vec<u32> {
    let mut found: Vec<u32> = fs::read_dir(esp.join("EFI/Linux"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|file| parse_fallback(&file.file_name().to_string_lossy()))
        .collect();
    found.sort_unstable();
    found
}

/// Puts `uki` on the ESP as the fallback of `generation`, recorded first and written through a fsynced temporary file; the older fallbacks stay until `retire_fallbacks`.
pub fn install_fallback(paths: &Paths, esp: &Path, generation: u32, uki: &Path) -> Result<()> {
    let name = entry_name(generation, Variant::Fallback);
    crate::durable::write(
        &entry_records(paths).join(&name),
        format!("uki /EFI/Linux/{name}\n").as_bytes(),
    )?;
    let target = fallback_path(esp, generation);
    if let Some(dir) = target.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    crate::durable::copy(uki, &target)?;
    crate::durable::sync_filesystems_checked(&[esp])
}

/// Takes every fallback UKI but `generation`'s off the ESP, with its record, once `generation`'s is flushed.
pub fn retire_fallbacks(paths: &Paths, esp: &Path, generation: u32) -> Result<()> {
    if !fallbacks(esp).contains(&generation) {
        return Ok(());
    }
    let mut removed = false;
    for old in fallbacks(esp).into_iter().filter(|n| *n != generation) {
        let path = fallback_path(esp, old);
        fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        let _ = fs::remove_file(entry_records(paths).join(entry_name(old, Variant::Fallback)));
        removed = true;
    }
    if removed {
        crate::durable::sync_filesystems_checked(&[esp])?;
    }
    Ok(())
}

/// Makes the fallback UKI a copy of the UKI `generation`'s own entry starts, then takes the others off; false when it has no entry.
pub fn follow_with_fallback(paths: &Paths, esp: &Path, generation: u32) -> Result<bool> {
    let Some(uki) = entries(esp)
        .into_iter()
        .find(|(number, extra, _)| *number == generation && !extra)
        .and_then(|(_, _, entry)| uki_of(esp, &entry))
        .filter(|uki| uki.is_file())
    else {
        return Ok(false);
    };
    install_fallback(paths, esp, generation, &uki)?;
    retire_fallbacks(paths, esp, generation)?;
    Ok(true)
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
    entries(Path::new(esp.trim())).iter().any(|(number, extra, path)| {
        *number == generation && !extra && is_bad(&path.file_name().unwrap_or_default().to_string_lossy())
    })
}

/// statfs's f_type for FAT, which every ESP is.
const MSDOS_SUPER_MAGIC: u64 = 0x4d44;

/// Checked before a switch, rollback or gc changes anything: `esp` must be a mounted FAT filesystem, and the partition systemd-boot started from when it says which (LoaderDevicePartUUID), else entries would land in a bare directory or on a disk the firmware never reads.
pub fn require_esp(esp: &Path) -> Result<()> {
    if !crate::testhooks::check_esp() {
        return Ok(());
    }
    let c_path = CString::new(esp.as_os_str().as_encoded_bytes())?;
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c_path.as_ptr(), &mut stat) } != 0 {
        bail!(
            "boot.esp is {}, which can't be read ({}); mount the ESP there, then try again; nothing was changed",
            esp.display(),
            io::Error::last_os_error()
        );
    }
    if stat.f_type as u64 != MSDOS_SUPER_MAGIC {
        bail!(
            "boot.esp is {}, but no FAT filesystem is mounted there, so boot entries would go into a directory the firmware never reads; mount the ESP (`mount {}` with its fstab line), then try again; nothing was changed",
            esp.display(),
            esp.display()
        );
    }
    let Some(loader) = loader_var_entry("LoaderDevicePartUUID") else {
        return Ok(());
    };
    let Some(mounted) = partuuid_of(esp) else {
        return Ok(());
    };
    if !mounted.eq_ignore_ascii_case(loader.trim()) {
        bail!(
            "the FAT filesystem at {} is partition {mounted}, but systemd-boot started from partition {}, so its entries would go where the firmware doesn't look; mount that ESP at {}, then try again; nothing was changed",
            esp.display(),
            loader.trim(),
            esp.display()
        );
    }
    Ok(())
}

/// The partition UUID of the device the filesystem at `path` is on, from /dev/disk/by-partuuid.
fn partuuid_of(path: &Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let dev = fs::metadata(path).ok()?.dev();
    fs::read_dir("/dev/disk/by-partuuid").ok()?.flatten().find_map(|link| {
        (fs::metadata(link.path()).ok()?.rdev() == dev).then(|| link.file_name().to_string_lossy().into_owned())
    })
}

/// fstab text with the ESP's line mounting it on access and unmounting it after a minute idle, so a forced reboot rarely finds the FAT mounted (systemd-bless-boot's renames and archix's writes mount it again); None when there's no line for `esp` or it already automounts or is noauto.
pub fn automount_esp(fstab: &str, esp: &str) -> Option<String> {
    let esp = esp.trim_end_matches('/');
    let mut changed = false;
    let lines: Vec<String> = fstab
        .lines()
        .map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let ours = !line.trim_start().starts_with('#')
                && fields.len() >= 4
                && fields[1].trim_end_matches('/') == esp
                && !fields[3]
                    .split(',')
                    .any(|option| option == "noauto" || option == "x-systemd.automount");
            if !ours {
                return line.to_string();
            }
            changed = true;
            let mut fields: Vec<String> = fields.iter().map(|f| f.to_string()).collect();
            fields[3].push_str(",x-systemd.automount,x-systemd.idle-timeout=60");
            fields.join(" ")
        })
        .collect();
    changed.then(|| lines.join("\n") + "\n")
}

/// Makes a staged deployment's fstab automount the ESP, unless the config manages /etc/fstab, which only gets a warning.
pub fn automount_esp_in(etc: &Path, esp: &str, managed: bool) -> Result<()> {
    let path = etc.join("fstab");
    let Ok(text) = fs::read_to_string(&path) else {
        return Ok(());
    };
    let Some(changed) = automount_esp(&text, esp) else {
        return Ok(());
    };
    if managed {
        eprintln!(
            "warning: the config's /etc/fstab keeps the ESP at {esp} mounted all the time, where a forced reboot can corrupt it; add x-systemd.automount,x-systemd.idle-timeout=60 to its options"
        );
        return Ok(());
    }
    crate::durable::write(&path, changed.as_bytes())?;
    println!(
        "fstab      the ESP at {esp} mounts on access and unmounts after a minute idle, so a forced reboot rarely catches it mounted"
    );
    Ok(())
}

/// After deploy-mount or activation failed on a boot systemd-boot is counting, reboots after a pause so the next try or the fallback happens with nobody at the machine; on other boots it returns, leaving the halt or emergency shell for whoever is there, since a reboot would only start the same entry again.
pub fn reboot_counted_try(what: &str, error: &anyhow::Error, root: &Path, initrd: bool) {
    if !crate::testhooks::counted_boot() {
        return;
    }
    println!(
        "archix: {what} failed ({error:#}) on one of a new deployment's counted boot tries; rebooting in 10 seconds so systemd-boot tries it again or falls back to the previous deployment"
    );
    if cfg!(debug_assertions) && std::env::var_os("ARCHIX_TEST_COUNTED_BOOT").is_some() {
        println!("archix: rebooting (skipped in a test)");
        return;
    }
    std::thread::sleep(std::time::Duration::from_secs(10));
    if !initrd {
        let _ = Command::new("systemctl")
            .args([
                "--no-block",
                "--job-mode=replace-irreversibly",
                "start",
                "reboot.target",
            ])
            .status();
        return;
    }
    if let Ok(dir) = fs::File::open(root) {
        unsafe { libc::syncfs(std::os::fd::AsRawFd::as_raw_fd(&dir)) };
    }
    unsafe { libc::reboot(libc::RB_AUTOBOOT) };
}

/// Whether systemd-boot still counts a generation's boot tries: its entry has counters and isn't bad yet.
pub fn counting(esp: &Path, generation: u32) -> bool {
    entries(esp).iter().any(|(number, extra, path)| {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        *number == generation && !extra && name.contains('+') && !is_bad(&name)
    })
}

/// Leaves a generation's counted entry no tries, as `systemd-bless-boot bad` does, so systemd-boot passes over it at the next boot; false when it has no counters.
pub fn mark_bad(esp: &Path, generation: u32) -> Result<bool> {
    for (number, extra, path) in entries(esp) {
        let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let Some((stem, counters)) = name.strip_suffix(".conf").and_then(|n| n.split_once('+')) else {
            continue;
        };
        if number != generation || extra {
            continue;
        }
        let parse = |n: &str| {
            n.parse::<u32>()
                .with_context(|| format!("{name} has a malformed boot counter"))
        };
        let (left, done) = match counters.split_once('-') {
            Some((left, done)) => (parse(left)?, parse(done)?),
            None => (parse(counters)?, 0),
        };
        let bad = path.with_file_name(format!("{stem}+0-{}.conf", left + done));
        if bad != path {
            fs::rename(&path, &bad).with_context(|| format!("renaming {}", path.display()))?;
            crate::durable::sync_filesystems_checked(&[esp])?;
        }
        return Ok(true);
    }
    Ok(false)
}

/// An entry systemd-boot has given up on: no tries left.
pub fn is_bad(name: &str) -> bool {
    name.split_once('+')
        .and_then(|(_, counters)| counters.split(['-', '.']).next())
        .is_some_and(|left| left == "0")
}

/// The generation an entry file (archix-N.conf, archix-N-protected.conf, with or without boot counters) belongs to, and whether it's the protected twin.
pub(crate) fn parse_entry(name: &str) -> Option<(u32, bool)> {
    let stem = name.strip_prefix("archix-")?.strip_suffix(".conf")?;
    let stem = stem.split('+').next().unwrap_or(stem);
    let (number, extra) = match stem.strip_suffix("-protected") {
        Some(number) => (number, true),
        None => (stem, false),
    };
    if number.is_empty() || number.starts_with('0') || !number.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((number.parse().ok()?, extra))
}

/// The generations that have a boot entry on the ESP, with whether it's the protected twin.
pub fn entries(esp: &Path) -> Vec<(u32, bool, PathBuf)> {
    let mut found = Vec::new();
    for entry in fs::read_dir(entries_dir(esp)).into_iter().flatten().flatten() {
        if let Some((number, extra)) = parse_entry(&entry.file_name().to_string_lossy()) {
            found.push((number, extra, entry.path()));
        }
    }
    found.sort();
    found
}

/// The UKI an entry boots.
fn uki_of(esp: &Path, entry: &Path) -> Option<PathBuf> {
    let text = fs::read_to_string(entry).ok()?;
    let path = text
        .lines()
        .find_map(|line| match line.split_once(char::is_whitespace) {
            Some(("efi" | "uki", path)) => Some(path.trim()),
            _ => None,
        })?;
    Some(esp.join(path.trim_start_matches('/')))
}

/// Takes a generation's entries and their records off the ESP, then the UKIs no entry uses any more; a deployment's entries go before the deployment does.
pub fn drop_entries(paths: &Paths, esp: &Path, generation: u32) -> Result<()> {
    for (number, extra, path) in entries(esp) {
        if number == generation {
            fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
            let variant = if extra { Variant::Protected } else { Variant::Normal };
            let _ = fs::remove_file(entry_records(paths).join(entry_name(number, variant)));
        }
    }
    // FAT reuses freed clusters, so the entries' removal reaches the disk before the UKIs they name are freed.
    crate::durable::sync_filesystems_checked(&[esp])?;
    mark("deploy-drop-ukis");
    remove_unused_ukis_but(esp, &[])?;
    crate::durable::sync_filesystems_checked(&[esp])
}

/// Deletes shared UKIs no entry names, except `keep`, which entries are about to name.
fn remove_unused_ukis_but(esp: &Path, keep: &[PathBuf]) -> Result<()> {
    let used: BTreeSet<PathBuf> = entries(esp)
        .iter()
        .filter_map(|(_, _, entry)| uki_of(esp, entry))
        .chain(keep.iter().cloned())
        .collect();
    for file in fs::read_dir(uki_dir(esp)).into_iter().flatten().flatten() {
        let name = file.file_name();
        if !used.contains(&file.path()) && !name.to_string_lossy().ends_with(".tmp") {
            fs::remove_file(file.path()).with_context(|| format!("removing {}", file.path().display()))?;
        }
    }
    Ok(())
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

/// One UKI a deployment's entries boot: built by this switch into the deployment's /boot/archix, or reused from the ESP.
#[derive(Debug, Clone)]
struct Uki {
    variant: Variant,
    title: String,
    name: String,
    built: bool,
}

/// The UKIs a staged deployment's entries boot.
#[derive(Debug, Clone)]
pub struct Built {
    pub version: String,
    ukis: Vec<Uki>,
}

/// Builds a staged deployment's initramfs and UKIs inside its staging root, from its own kernel and /etc, signing them when Secure Boot signing is on; UKIs the ESP already has are reused.
pub fn build(paths: &Paths, plan: &BootPlan, number: u32, staging: &Staging, tx: &Transaction) -> Result<Built> {
    let version = owned_version(&staging.tree("pacman"), &plan.kernel).with_context(|| {
        format!(
            "the {} kernel package isn't installed in the new deployment; boot.kernel names it",
            plan.kernel
        )
    })?;
    let vmlinuz = Path::new(MODULES).join(&version).join("vmlinuz");
    if !staging.read_path(&vmlinuz)?.is_file() {
        bail!("{} is missing from the new deployment", vmlinuz.display());
    }
    let in_tx = |tx: &Transaction, argv: &[&str], what: &str| -> Result<String> {
        let output = tx.output(argv)?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let tail: Vec<&str> = stdout.lines().chain(stderr.lines()).rev().take(12).collect();
            bail!(
                "{what} failed in the new deployment:\n{}",
                tail.into_iter().rev().collect::<Vec<_>>().join("\n")
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    };
    let in_view = |argv: &[&str], what: &str| in_tx(tx, argv, what);
    in_view(&["rm", "-rf", BUILT], "clearing /boot/archix")?;
    in_view(&["mkdir", "-p", BUILT], "creating /boot/archix")?;
    let initramfs = format!("{BUILT}/initramfs.img");
    in_view(
        &["mkinitcpio", "-k", &version, "-g", &initramfs, "-S", "autodetect"],
        "mkinitcpio",
    )?;
    let os_release = in_view(&["cat", "/etc/os-release"], "reading /etc/os-release")?;
    let release: Vec<String> = os_release
        .lines()
        .filter(|line| {
            !line.starts_with("PRETTY_NAME=") && !line.starts_with("IMAGE_ID=") && !line.starts_with("IMAGE_VERSION=")
        })
        .map(str::to_string)
        .collect();
    let secure_boot = plan.secure_boot.unwrap_or_else(sbctl_has_keys);
    let pcr_key = staging
        .read_path(Path::new(PCR_PRIVATE_KEY))?
        .is_file()
        .then(|| fs::read(staging.read_path(Path::new(PCR_PUBLIC_KEY)).ok()?).ok())
        .map(Option::unwrap_or_default);
    let microcode: Vec<String> = plan.microcode.iter().map(|m| format!("/boot/{m}")).collect();
    let mut microcode_files = Vec::new();
    for file in &microcode {
        microcode_files.push(staging.read_path(Path::new(file))?);
    }
    let initramfs_file = staging.read_path(Path::new(&initramfs))?;
    let esp = Path::new(&plan.esp);
    let mut ukis = Vec::new();
    for variant in [Variant::Normal, Variant::Protected] {
        if variant == Variant::Protected && !plan.anticheat {
            continue;
        }
        let (title, suffix) = match variant {
            Variant::Protected => ("archix protected", "-protected"),
            _ => ("archix", ""),
        };
        let mut text = release.clone();
        if variant == Variant::Normal {
            // Entries set their own title and sort key; these are what the fallback copy of this UKI shows.
            text.push(format!("PRETTY_NAME=\"{FALLBACK_TITLE}\""));
            text.push(format!("IMAGE_ID={FALLBACK_SORT_KEY}"));
            text.push(format!("IMAGE_VERSION={version}"));
        } else {
            text.push(format!("PRETTY_NAME=\"Arch Linux ({title})\""));
            text.push("IMAGE_ID=archix".to_string());
        }
        let release_text = text.join("\n") + "\n";
        let mut cmdline = plan.cmdline.trim().to_string();
        if variant == Variant::Protected {
            cmdline.push(' ');
            cmdline.push_str(super::secureboot::ANTICHEAT_CMDLINE);
        }
        let key = payload_key(
            &staging.read_path(&vmlinuz)?,
            &microcode_files,
            &initramfs_file,
            &cmdline,
            &release_text,
            secure_boot,
            pcr_key.as_deref(),
        )?;
        let name = format!("{version}{suffix}-{key}.efi");
        let built = !uki_dir(esp).join(&name).exists();
        if built {
            let output = format!("{BUILT}/{name}");
            let mut ukify: Vec<String> = ["ukify", "build", "--linux"].map(str::to_string).to_vec();
            ukify.push(vmlinuz.display().to_string());
            for file in &microcode {
                ukify.extend(["--initrd".to_string(), file.clone()]);
            }
            ukify.extend(["--initrd".to_string(), initramfs.clone()]);
            ukify.extend([
                "--cmdline".to_string(),
                cmdline.clone(),
                "--uname".to_string(),
                version.clone(),
            ]);
            ukify.extend(["--os-release".to_string(), release_text.clone()]);
            ukify.extend(["--output".to_string(), output.clone()]);
            if super::revoke::any_revoked(paths) {
                ukify.push(format!("--section=.archix:{}", unique_id()));
            }
            if pcr_key.is_some() {
                ukify.push(format!("--pcr-private-key={PCR_PRIVATE_KEY}"));
                if staging.read_path(Path::new(PCR_PUBLIC_KEY))?.is_file() {
                    ukify.push(format!("--pcr-public-key={PCR_PUBLIC_KEY}"));
                }
            }
            let argv: Vec<&str> = ukify.iter().map(String::as_str).collect();
            match pcr_key {
                Some(_) => in_tx(&staging.signing_transaction()?, &argv, "ukify build")?,
                None => in_view(&argv, "ukify build")?,
            };
            if secure_boot {
                let file = staging.read_path(Path::new(&output))?;
                run(
                    Command::new("sbctl").env("ESP_PATH", esp).arg("sign").arg(&file),
                    "sbctl sign",
                )?;
                super::revoke::record_signed(paths, &file, &name)?;
            }
        }
        ukis.push(Uki {
            variant,
            title: format!("Arch Linux ({title}, generation {number})"),
            name,
            built,
        });
    }
    in_view(&["rm", "-f", &initramfs], "removing the initramfs")?;
    if secure_boot {
        for loader in ["EFI/systemd/systemd-bootx64.efi", "EFI/BOOT/BOOTX64.EFI"] {
            let path = esp.join(loader);
            if path.exists() {
                sign_replacing(esp, &path)?;
            }
        }
    }
    let mut to_check = Vec::new();
    for uki in &ukis {
        to_check.push(match uki.built {
            true => staging.read_path(&Path::new(BUILT).join(&uki.name))?,
            false => uki_dir(esp).join(&uki.name),
        });
    }
    refuse_unsigned(plan, &to_check)?;
    Ok(Built { version, ukis })
}

/// Signs a loader on the ESP as a copy that replaces it by rename, so a crash never leaves it half written.
fn sign_replacing(esp: &Path, path: &Path) -> Result<()> {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    let tmp = path.with_file_name(name);
    let signed = fs::copy(path, &tmp)
        .with_context(|| format!("copying {}", path.display()))
        .and_then(|_| {
            run(
                Command::new("sbctl").env("ESP_PATH", esp).arg("sign").arg(&tmp),
                "sbctl sign",
            )
        })
        .and_then(|()| Ok(fs::File::open(&tmp)?.sync_all()?))
        .and_then(|()| fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display())));
    if signed.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    signed?;
    crate::durable::sync_dir(path)
}

/// The ESP step of a commit: copies the UKIs a deployment built, writes its entries and their records, and makes it systemd-boot's default; entries of `droppable` deployments go first when the ESP is short of room.
pub struct Entries<'a> {
    pub paths: &'a Paths,
    pub plan: &'a BootPlan,
    pub built: &'a Built,
    pub droppable: Vec<u32>,
}

impl EspStep for Entries<'_> {
    fn install(&self, number: u32, deployment: &Path) -> Result<()> {
        let esp = Path::new(&self.plan.esp);
        for dir in [uki_dir(esp), entries_dir(esp)] {
            fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        crate::durable::write(&self.paths.state.join("boot").join("esp"), self.plan.esp.as_bytes())?;
        let made: Vec<&Uki> = self.built.ukis.iter().filter(|uki| uki.built).collect();
        let needed: u64 = made
            .iter()
            .map(|uki| fs::metadata(deployment.join("boot/archix").join(&uki.name)).map_or(0, |m| m.len()))
            .sum::<u64>()
            + MARGIN;
        let reused: Vec<PathBuf> = self.built.ukis.iter().map(|uki| uki_dir(esp).join(&uki.name)).collect();
        for gone in &self.droppable {
            if free_bytes(&uki_dir(esp))? >= needed {
                break;
            }
            for (n, _, path) in entries(esp) {
                if n == *gone {
                    fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
                }
            }
            // Each removal reaches the disk before the clusters it frees can be written again.
            crate::durable::sync_filesystems_checked(&[esp])?;
            mark("deploy-esp-drop-ukis");
            remove_unused_ukis_but(esp, &reused)?;
            crate::durable::sync_filesystems_checked(&[esp])?;
        }
        mark("deploy-esp-uki");
        for uki in made {
            crate::durable::copy(
                &deployment.join("boot/archix").join(&uki.name),
                &uki_dir(esp).join(&uki.name),
            )?;
        }
        mark("deploy-esp-entry");
        crate::testhooks::fail_at("deploy-esp-entry")?;
        let counted = self.plan.tries > 0 && loader_counts_boots(crate::testhooks::loader_features());
        if self.plan.tries > 0
            && let Some(why) = counting_off()
        {
            eprintln!("warning: {why}");
        }
        // systemd-boot counts tries only for entries that start their UKI with `uki`.
        let key = if counted { "uki" } else { "efi" };
        for (n, extra, path) in entries(esp) {
            if n == number && !extra {
                fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
            }
        }
        for uki in &self.built.ukis {
            let text = format!(
                "title {}\nversion {number}\nsort-key archix\n{key} /EFI/archix/{}\n",
                uki.title, uki.name
            );
            let name = match uki.variant {
                Variant::Normal => counted_entry_name(number, if counted { self.plan.tries } else { 0 }),
                variant => entry_name(number, variant),
            };
            // The record comes first: the initramfs boots an entry only once archix has recorded it.
            crate::durable::write(
                &entry_records(self.paths).join(entry_name(number, uki.variant)),
                text.as_bytes(),
            )?;
            crate::durable::write(&entries_dir(esp).join(name), text.as_bytes())?;
        }
        mark("deploy-esp-loader");
        set_default_entry(esp, number)?;
        mark("deploy-esp-fallback");
        match self.built.ukis.iter().find(|uki| uki.variant == Variant::Normal) {
            Some(uki) => install_fallback(self.paths, esp, number, &uki_dir(esp).join(&uki.name)),
            None => Ok(()),
        }
    }

    fn clean(&self) -> Result<()> {
        crate::deploy::commit::clean_esp(Path::new(&self.plan.esp))
    }

    fn abandon(&self, number: u32, previous: Option<u32>) -> Result<()> {
        let esp = Path::new(&self.plan.esp);
        crate::deploy::commit::clean_esp(esp)?;
        let _ = fs::remove_file(fallback_path(esp, number));
        let _ = fs::remove_file(entry_records(self.paths).join(entry_name(number, Variant::Fallback)));
        drop_entries(self.paths, esp, number)?;
        match previous {
            Some(previous) if entries(esp).iter().any(|(n, extra, _)| *n == previous && !extra) => {
                set_default_entry(esp, previous)
            }
            _ => Ok(()),
        }
    }
}

/// Random hex that makes a UKI's hash its own: once archix has revoked images, a rebuild must never come out identical to one of them.
fn unique_id() -> String {
    let mut bytes = [0u8; 16];
    if let Ok(mut random) = fs::File::open("/dev/urandom") {
        let _ = std::io::Read::read_exact(&mut random, &mut bytes);
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The key systemd-measure signs PCR 11 policies with, where kernel-install's ukify plugin and systemd-cryptenroll look for it.
const PCR_PRIVATE_KEY: &str = "/etc/systemd/tpm2-pcr-private-key.pem";
const PCR_PUBLIC_KEY: &str = "/etc/systemd/tpm2-pcr-public-key.pem";

/// What a UKI is built from, hashed: generations whose kernel, microcode, initramfs (which mkinitcpio builds reproducibly), command line, os-release and PCR policy key match share one UKI.
fn payload_key(
    vmlinuz: &Path,
    microcode: &[PathBuf],
    initramfs: &Path,
    cmdline: &str,
    os_release: &str,
    signed: bool,
    pcr_key: Option<&[u8]>,
) -> Result<String> {
    let mut parts = vec![crate::fetch::sha256(vmlinuz)?];
    for file in microcode {
        parts.push(crate::fetch::sha256(file)?);
    }
    parts.push(crate::fetch::sha256(initramfs)?);
    parts.push(crate::fetch::sha256_bytes(cmdline.as_bytes()));
    parts.push(crate::fetch::sha256_bytes(os_release.as_bytes()));
    parts.push(signed.to_string());
    if let Some(pcr_key) = pcr_key {
        parts.push(crate::fetch::sha256_bytes(pcr_key));
    }
    Ok(crate::fetch::sha256_bytes(parts.join("\n").as_bytes())[..12].to_string())
}

/// Makes systemd-boot start `generation` by default. An entry still counting its tries is only `preferred`, which systemd-boot passes over once it's bad for `default`: the newest default that has booted.
pub fn set_default_entry(esp: &Path, generation: u32) -> Result<()> {
    let entry_of = |generation: u32| {
        entries(esp)
            .into_iter()
            .filter(|(number, extra, _)| *number == generation && !extra)
            .map(|(_, _, path)| path.file_name().unwrap_or_default().to_string_lossy().into_owned())
            .next()
    };
    let Some(entry) = entry_of(generation) else {
        bail!("deployment {generation} has no boot entry on {}", esp.display());
    };
    if !entry.contains('+') {
        return write_loader(esp, Some(&entry_id(&entry)), None);
    }
    let conf = fs::read_to_string(esp.join("loader/loader.conf")).unwrap_or_default();
    let setting = |key: &str| {
        conf.lines()
            .find_map(|line| match line.split_once(char::is_whitespace) {
                Some((name, value)) if name == key => Some(value.trim().to_string()),
                _ => None,
            })
    };
    let booted_preferred = setting("preferred")
        .and_then(|id| parse_entry(&id))
        .and_then(|(number, _)| entry_of(number))
        .filter(|name| !name.contains('+'))
        .map(|name| entry_id(&name));
    let default = booted_preferred.or_else(|| setting("default"));
    write_loader(esp, default.as_deref(), Some(&entry_id(&entry)))
}

/// Whether loader.conf's `default` or `preferred` names one of `generation`'s entries.
pub fn loader_names(esp: &Path, generation: u32) -> bool {
    fs::read_to_string(esp.join("loader/loader.conf"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| match line.split_once(char::is_whitespace) {
            Some(("default" | "preferred", id)) => parse_entry(&entry_id(id.trim())),
            _ => None,
        })
        .any(|(number, _)| number == generation)
}

/// Names systemd-boot's `default` and `preferred` entries in loader.conf, dropping either when it's None.
fn write_loader(esp: &Path, default: Option<&str>, preferred: Option<&str>) -> Result<()> {
    let conf = esp.join("loader/loader.conf");
    let mut lines: Vec<String> = fs::read_to_string(&conf)
        .unwrap_or_default()
        .lines()
        .filter(|line| !matches!(line.split_whitespace().next(), Some("default" | "preferred")))
        .map(str::to_string)
        .collect();
    lines.extend(default.map(|id| format!("default {id}")));
    lines.extend(preferred.map(|id| format!("preferred {id}")));
    crate::durable::write(&conf, (lines.join("\n") + "\n").as_bytes())?;
    clear_overrides(preferred.or(default));
    Ok(())
}

const LOADER_GUID: &str = "4a67b082-0a4c-41cf-b6c7-440b29bb8c4f";

/// systemd-boot's EFI variable `name`, where the test hooks put it in a debug build.
fn loader_var(name: &str) -> Option<PathBuf> {
    crate::testhooks::efivars().map(|dir| dir.join(format!("{name}-{LOADER_GUID}")))
}

/// The entry an EFI variable such as LoaderEntryDefault names, if it's set.
fn loader_var_entry(name: &str) -> Option<String> {
    efivar_string(&fs::read(loader_var(name)?).ok()?).filter(|entry| !entry.is_empty())
}

/// The default picked in the boot menu or with `bootctl set-default`, which overrides loader.conf's.
pub fn default_override() -> Option<String> {
    loader_var_entry("LoaderEntryDefault")
}

/// Removes systemd-boot's LoaderEntryDefault and LoaderEntryPreferred, which override loader.conf (the boot menu's `d` key and `bootctl set-default` set them), so the entry archix just made the default is the one that boots; a one-shot entry is only reported.
fn clear_overrides(wanted: Option<&str>) {
    let wanted_name = wanted.unwrap_or("archix's entry");
    for name in ["LoaderEntryDefault", "LoaderEntryPreferred"] {
        let (Some(path), Some(entry)) = (loader_var(name), loader_var_entry(name)) else {
            continue;
        };
        // efivarfs makes variables immutable; chattr fails harmlessly elsewhere.
        let _ = Command::new("chattr")
            .arg("-i")
            .arg(&path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        match fs::remove_file(&path) {
            Ok(()) if Some(entry_id(&entry).as_str()) != wanted => println!(
                "archix: cleared {name} ({entry}), set in the boot menu or with bootctl, which overrode loader.conf and would have kept {wanted_name} from booting"
            ),
            Ok(()) => {}
            Err(e) => eprintln!(
                "warning: systemd-boot's {name} variable names {entry} and overrides loader.conf, and archix couldn't clear it ({e}), so {wanted_name} may not boot; run `sudo bootctl set-default ''`"
            ),
        }
    }
    if let Some(entry) = loader_var_entry("LoaderEntryOneShot")
        && Some(entry_id(&entry).as_str()) != wanted
    {
        println!("archix: the next boot starts {entry} once (bootctl set-oneshot); the one after starts {wanted_name}");
    }
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
                let path = entries_dir(Path::new(esp)).join(&name);
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
    write_loader(Path::new(esp), Some(&default_id), None)?;
    mark("eject-loader");
    for (_, _, path) in entries(Path::new(esp)) {
        fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    }
    for generation in fallbacks(Path::new(esp)) {
        let path = fallback_path(Path::new(esp), generation);
        fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    }
    crate::durable::sync_filesystems_checked(&[Path::new(esp)])?;
    match fs::remove_dir_all(uki_dir(Path::new(esp))) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e).context("removing archix's UKIs"),
        _ => {}
    }
    crate::durable::sync_filesystems_checked(&[Path::new(esp)])?;
    written.insert(0, format!("default {default_id}"));
    Ok(written)
}

/// After a boot systemd-boot has counted as good, makes its generation the default, if it's the current one, so `default` doesn't lag until the next switch.
pub fn promote_booted(paths: &Paths) -> Result<()> {
    let (Some(booted), Some(current)) = (booted(), crate::generations::Store::new(paths).current()?) else {
        return Ok(());
    };
    let Ok(esp) = fs::read_to_string(paths.state.join("boot").join("esp")) else {
        return Ok(());
    };
    let esp = Path::new(esp.trim());
    let still_counting = entries(esp).iter().any(|(number, extra, path)| {
        *number == booted
            && !extra
            && path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().contains('+'))
    });
    if booted != current || still_counting {
        return Ok(());
    }
    set_default_entry(esp, booted)
}

/// Why a new generation's entry won't be counted, when the systemd-boot that started this boot can't do it.
pub fn counting_off() -> Option<String> {
    let features = crate::testhooks::loader_features();
    (features != 0 && !loader_counts_boots(features)).then(|| {
        "boot counting is off: the systemd-boot that started this boot is older than 260, so a generation that doesn't boot isn't passed over; `sudo bootctl update` installs the one your systemd package has".to_string()
    })
}

/// Checked before a switch changes anything: the new entries must fit in what's free plus what dropping the entries of deployments outside `pinned` would free; `kernel` stands in for a UKI when the ESP has none yet.
pub fn check_space(esp: &Path, pinned: &BTreeSet<u32>, kernel: Option<&Path>, protected: bool) -> Result<()> {
    let existing: Vec<(u32, bool, PathBuf)> = entries(esp)
        .into_iter()
        .filter_map(|(number, extra, entry)| Some((number, extra, uki_of(esp, &entry)?)))
        .collect();
    let estimate = || Some(fs::metadata(kernel?).ok()?.len() + 64 * 1024 * 1024);
    let Some(newest) = existing
        .iter()
        .filter(|(_, extra, _)| !extra)
        .filter_map(|(_, _, path)| fs::metadata(path).ok().map(|m| m.len()))
        .max()
        .or_else(estimate)
    else {
        return Ok(());
    };
    let needed = newest * (1 + u64::from(protected)) + MARGIN;
    let pinned_ukis: BTreeSet<&PathBuf> = existing
        .iter()
        .filter(|(number, _, _)| pinned.contains(number))
        .map(|(_, _, uki)| uki)
        .collect();
    let reclaimable: u64 = existing
        .iter()
        .map(|(_, _, uki)| uki)
        .filter(|uki| !pinned_ukis.contains(uki))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter_map(|path| fs::metadata(path).ok().map(|m| m.len()))
        .sum();
    let measured = uki_dir(esp)
        .ancestors()
        .find(|path| path.exists())
        .unwrap_or(Path::new("/"))
        .to_path_buf();
    let free = free_bytes(&measured)?;
    if free + reclaimable < needed {
        let mib = |bytes: u64| bytes / (1024 * 1024);
        bail!(
            "the ESP at {} has {} MiB free ({} MiB more after dropping the deployments it may), but this switch's boot entries need about {} MiB; nothing was changed. Free up space on it, or set deployments.keep lower",
            esp.display(),
            mib(free),
            mib(reclaimable),
            mib(needed)
        );
    }
    Ok(())
}

const GLOBAL_GUID: &str = "8be4df61-93ca-11d2-aa0d-00e098032b8c";

/// The file paths in an EFI load option (Boot####), lowercased with forward slashes, and its description.
fn load_option(bytes: &[u8]) -> Option<(String, Vec<String>)> {
    let data = bytes.get(4..)?;
    let list_len = u16::from_le_bytes(data.get(4..6)?.try_into().ok()?) as usize;
    let units: Vec<u16> = data
        .get(6..)?
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .take_while(|unit| *unit != 0)
        .collect();
    let description = String::from_utf16_lossy(&units);
    let start = 6 + (units.len() + 1) * 2;
    let mut list = data.get(start..start + list_len)?;
    let mut paths = Vec::new();
    while list.len() >= 4 {
        let (kind, subtype) = (list[0], list[1]);
        let len = u16::from_le_bytes([list[2], list[3]]) as usize;
        if len < 4 || len > list.len() {
            break;
        }
        if kind == 4 && subtype == 4 {
            let path: Vec<u16> = list[4..len]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| u16::from_le_bytes(*pair))
                .take_while(|unit| *unit != 0)
                .collect();
            paths.push(String::from_utf16_lossy(&path).replace('\\', "/").to_lowercase());
        }
        list = &list[len..];
    }
    Some((description, paths))
}

/// What could keep the firmware from starting systemd-boot, from the EFI variables in `efivars` and the ESP: no boot entry for it, another one first in BootOrder, or no systemd-boot at EFI/BOOT/BOOTX64.EFI for firmware that forgets its entries.
pub fn firmware_warnings(efivars: &Path, esp: &Path) -> Vec<String> {
    let mut warnings = Vec::new();
    let read = |name: &str| fs::read(efivars.join(format!("{name}-{GLOBAL_GUID}"))).ok();
    if let Some(order) = read("BootOrder") {
        let order: Vec<u16> = order
            .get(4..)
            .unwrap_or_default()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes(*pair))
            .collect();
        let options: Vec<(u16, String, bool)> = order
            .iter()
            .filter_map(|n| {
                let (description, paths) = load_option(&read(&format!("Boot{n:04X}"))?)?;
                let systemd_boot = paths
                    .iter()
                    .any(|path| path.ends_with("efi/systemd/systemd-bootx64.efi"));
                Some((*n, description, systemd_boot))
            })
            .collect();
        match options.iter().position(|(_, _, systemd_boot)| *systemd_boot) {
            None => warnings.push(
                "the firmware's boot order has no entry that starts systemd-boot (EFI/systemd/systemd-bootx64.efi), so it may start something else; `sudo bootctl install` adds one".to_string(),
            ),
            Some(0) => {}
            Some(_) => warnings.push(format!(
                "the firmware starts \"{}\" before systemd-boot; put systemd-boot first with `sudo efibootmgr -o` unless that entry chains to it",
                options[0].1
            )),
        }
    }
    let fallback = esp.join("EFI/BOOT/BOOTX64.EFI");
    match fs::read(&fallback) {
        Err(_) => warnings.push(format!(
            "{} is missing, so firmware that loses its boot entries finds no loader; `sudo bootctl install` puts systemd-boot there",
            fallback.display()
        )),
        Ok(bytes) if !bytes.windows(29).any(|w| w == b"#### LoaderInfo: systemd-boot") => warnings.push(format!(
            "{} isn't systemd-boot, so firmware that loses its boot entries starts another loader",
            fallback.display()
        )),
        Ok(_) => {}
    }
    warnings
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

/// With Secure Boot enforcing, an unsigned entry or loader would leave the new default unbootable, so it fails the switch before anything is committed.
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
                "Secure Boot is on, but {} isn't signed, so the firmware would refuse it; nothing was committed. Run `sudo archix secure-boot setup` (or set boot.secure_boot = true if sbctl already has keys)",
                path.file_name().unwrap_or_default().to_string_lossy()
            );
        }
    }
    Ok(())
}

/// The value of `archix.generation=` on a kernel command line, which tests use to fake the booted generation.
pub fn booted_generation(cmdline: &str) -> Option<u32> {
    cmdline
        .split_whitespace()
        .find_map(|arg| arg.strip_prefix("archix.generation=")?.parse().ok())
}

/// The generation this boot came from: the command line's, else the boot menu entry systemd-boot started (LoaderEntrySelected).
pub fn booted() -> Option<u32> {
    let cmdline = crate::testhooks::kernel_cmdline();
    booted_generation(&cmdline).or_else(|| {
        let entry = crate::testhooks::selected_entry()?;
        let (number, _) = parse_entry(&entry_id(&entry))?;
        entry_unchanged(&Paths::system(), &entry).then_some(number)
    })
}

/// The deployment this boot runs on the root at `root`: what the initramfs recorded mounting, else the entry systemd-boot started.
pub fn booted_deployment(root: &Path) -> Option<u32> {
    fs::read_to_string(root.join(DEPLOYMENT_RECORD.trim_start_matches('/')))
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .or_else(booted)
}

/// Where the initramfs records the deployment it mounted, which lasts until shutdown.
pub const DEPLOYMENT_RECORD: &str = "/run/archix/deployment";

/// The entry systemd-boot started when it names an archix generation but isn't what archix wrote there.
pub fn tampered_selected_entry() -> Option<String> {
    if booted_generation(&crate::testhooks::kernel_cmdline()).is_some() {
        return None;
    }
    let entry = crate::testhooks::selected_entry()?;
    parse_entry(&entry_id(&entry))?;
    (!entry_unchanged(&Paths::system(), &entry)).then_some(entry)
}

/// Where archix keeps a copy of each boot entry it wrote, on the root filesystem, which unlike the ESP holds no unsigned input to trust.
pub(crate) fn entry_records(paths: &Paths) -> PathBuf {
    paths.state.join("boot").join("entries")
}

/// The id systemd-boot reports for an entry file: its name without the boot counters.
pub(crate) fn entry_id(name: &str) -> String {
    match name.split_once('+') {
        Some((stem, _)) => format!("{stem}.conf"),
        None => name.to_string(),
    }
}

/// Whether the entry systemd-boot reports by `id` is still what archix recorded writing.
fn entry_unchanged(paths: &Paths, id: &str) -> bool {
    let Ok(record) = fs::read(entry_records(paths).join(entry_id(id))) else {
        return false;
    };
    let Ok(esp) = fs::read_to_string(paths.state.join("boot").join("esp")) else {
        return false;
    };
    let dir = entries_dir(Path::new(esp.trim()));
    fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .find(|file| entry_id(&file.file_name().to_string_lossy()) == entry_id(id))
        .is_some_and(|file| fs::read(file.path()).is_ok_and(|text| text == record))
}

/// systemd-boot's LoaderEntrySelected: 4 bytes of attributes, then the entry's id in UTF-16LE.
pub fn read_selected_entry() -> Option<String> {
    efivar_string(&fs::read(SELECTED_ENTRY_VAR).ok()?)
}

pub const SELECTED_ENTRY_VAR: &str =
    "/sys/firmware/efi/efivars/LoaderEntrySelected-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f";

/// systemd-boot's LoaderFeatures, a bit set of what the loader that started this boot supports.
pub fn read_loader_features() -> u64 {
    const VAR: &str = "/sys/firmware/efi/efivars/LoaderFeatures-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f";
    fs::read(VAR).ok().and_then(|bytes| efivar_u64(&bytes)).unwrap_or(0)
}

/// Whether the loader counts boots of an entry archix can write (systemd-boot counts only `linux` and `uki` entries, 258 added `uki`) and falls back past a bad one (260's `preferred`).
fn loader_counts_boots(features: u64) -> bool {
    const BOOT_COUNTING: u64 = 1 << 4;
    const TYPE1_UKI: u64 = 1 << 16;
    const ENTRY_PREFERRED: u64 = 1 << 19;
    let needed = BOOT_COUNTING | TYPE1_UKI | ENTRY_PREFERRED;
    features & needed == needed
}

fn efivar_u64(bytes: &[u8]) -> Option<u64> {
    Some(u64::from_le_bytes(bytes.get(4..12)?.try_into().ok()?))
}

pub(crate) fn efivar_string(bytes: &[u8]) -> Option<String> {
    let units: Vec<u16> = bytes
        .get(4..)?
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
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

    fn esp_with(entries: &[&str], loader: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(entries_dir(dir.path())).unwrap();
        for name in entries {
            fs::write(entries_dir(dir.path()).join(name), "title t\n").unwrap();
        }
        fs::write(dir.path().join("loader/loader.conf"), loader).unwrap();
        dir
    }

    #[test]
    fn a_rollback_becomes_the_default_entry() {
        let esp = esp_with(
            &["archix-2.conf", "archix-3.conf"],
            "timeout 2\ndefault archix-3.conf\n",
        );
        set_default_entry(esp.path(), 2).unwrap();
        assert_eq!(
            fs::read_to_string(esp.path().join("loader/loader.conf")).unwrap(),
            "timeout 2\ndefault archix-2.conf\n"
        );
        assert!(
            set_default_entry(esp.path(), 7).is_err(),
            "a deployment without an entry can't become the default"
        );
        assert!(
            fs::read_to_string(esp.path().join("loader/loader.conf"))
                .unwrap()
                .contains("archix-2.conf")
        );
    }

    #[test]
    fn a_counted_entry_is_preferred_and_the_last_proven_one_stays_default() {
        let esp = esp_with(
            &["archix-3.conf", "archix-5+3.conf"],
            "timeout 2\ndefault archix-3.conf\n",
        );
        let entries = entries_dir(esp.path());
        let loader = || fs::read_to_string(esp.path().join("loader/loader.conf")).unwrap();
        set_default_entry(esp.path(), 5).unwrap();
        assert_eq!(loader(), "timeout 2\ndefault archix-3.conf\npreferred archix-5.conf\n");
        fs::rename(entries.join("archix-5+3.conf"), entries.join("archix-5.conf")).unwrap();
        fs::write(entries.join("archix-6+3.conf"), "title t\n").unwrap();
        set_default_entry(esp.path(), 6).unwrap();
        assert_eq!(
            loader(),
            "timeout 2\ndefault archix-5.conf\npreferred archix-6.conf\n",
            "a preferred entry that booted becomes the fallback"
        );
        set_default_entry(esp.path(), 3).unwrap();
        assert_eq!(loader(), "timeout 2\ndefault archix-3.conf\n");
    }

    #[test]
    fn only_a_loader_with_uki_entries_and_preferred_counts_boots() {
        let systemd_262 = 0b1111_1111_1111_1111_1111;
        let systemd_257 = (1 << 4) | (1 << 0);
        assert!(loader_counts_boots(systemd_262));
        assert!(!loader_counts_boots(systemd_257));
        assert!(!loader_counts_boots(0));
        let mut bytes = vec![6, 0, 0, 0];
        bytes.extend(0x1234_u64.to_le_bytes());
        assert_eq!(efivar_u64(&bytes), Some(0x1234));
    }

    #[test]
    fn an_entry_names_its_uki_with_an_efi_or_uki_line() {
        let dir = tempfile::tempdir().unwrap();
        for key in ["efi", "uki"] {
            let entry = dir.path().join(format!("{key}.conf"));
            fs::write(&entry, format!("title t\n{key} /EFI/archix/a.efi\n")).unwrap();
            assert_eq!(
                uki_of(dir.path(), &entry),
                Some(dir.path().join("EFI/archix/a.efi")),
                "{key}"
            );
        }
    }

    #[test]
    fn dropping_a_deployments_entries_keeps_a_uki_another_entry_still_boots() {
        let esp = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::under(root.path());
        fs::create_dir_all(uki_dir(esp.path())).unwrap();
        fs::create_dir_all(entries_dir(esp.path())).unwrap();
        fs::create_dir_all(entry_records(&paths)).unwrap();
        fs::write(uki_dir(esp.path()).join("7.2.9-arch1-1-aaaa.efi"), "uki").unwrap();
        fs::write(uki_dir(esp.path()).join("7.2.9-arch1-1-bbbb.efi"), "uki").unwrap();
        for (name, uki) in [
            ("archix-4.conf", "aaaa"),
            ("archix-5+2-1.conf", "aaaa"),
            ("archix-5-protected.conf", "bbbb"),
            ("archix-6.conf", "bbbb"),
        ] {
            let text = format!("title t\nsort-key archix\nefi /EFI/archix/7.2.9-arch1-1-{uki}.efi\n");
            fs::write(entries_dir(esp.path()).join(name), &text).unwrap();
            fs::write(entry_records(&paths).join(entry_id(name)), &text).unwrap();
        }
        drop_entries(&paths, esp.path(), 5).unwrap();
        drop_entries(&paths, esp.path(), 4).unwrap();
        let numbers: Vec<u32> = entries(esp.path()).iter().map(|(n, _, _)| *n).collect();
        assert_eq!(numbers, [6]);
        let left: Vec<String> = fs::read_dir(uki_dir(esp.path()))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, ["7.2.9-arch1-1-bbbb.efi"]);
        let records: Vec<String> = fs::read_dir(entry_records(&paths))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(records, ["archix-6.conf"]);
    }

    #[test]
    fn the_booted_entry_names_its_generation() {
        let mut bytes = vec![6, 0, 0, 0];
        bytes.extend("archix-12-protected.conf".encode_utf16().flat_map(u16::to_le_bytes));
        bytes.extend([0, 0]);
        let entry = efivar_string(&bytes).unwrap();
        assert_eq!(entry, "archix-12-protected.conf");
        assert_eq!(parse_entry(&entry), Some((12, true)));
        assert_eq!(parse_entry("archix-7.conf"), Some((7, false)));
        assert_eq!(parse_entry("archix-07.conf"), None);
        assert_eq!(parse_entry("archix-7.efi"), None);
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
        assert_eq!(parse_entry("archix-9-protected.conf"), Some((9, true)));
        assert!(is_bad("archix-9+0-3.conf") && !is_bad("archix-9+1-2.conf") && !is_bad("archix-9.conf"));
        assert_eq!(counted_entry_name(9, 3), "archix-9+3.conf");
        assert_eq!(counted_entry_name(9, 0), "archix-9.conf");
    }

    #[test]
    fn a_fallback_uki_is_named_and_titled_apart_from_its_generations_entry() {
        assert_eq!(entry_name(12, Variant::Fallback), "archix-fallback-12.efi");
        assert_eq!(parse_fallback("archix-fallback-12.efi"), Some(12));
        for other in [
            "archix-fallback-012.efi",
            "archix-fallback-.efi",
            "archix-fallback-12+3.efi",
            "archix-12.conf",
            "arch-linux.efi",
        ] {
            assert_eq!(parse_fallback(other), None, "{other}");
        }
        assert_eq!(parse_entry("archix-fallback-12.efi"), None, "it's no Type #1 entry");
        assert_eq!(FALLBACK_TITLE, "archix (fallback)");
        assert!(
            FALLBACK_SORT_KEY < "arch" && FALLBACK_SORT_KEY < "archix",
            "sorts before other distributions' UKIs and archix's entries"
        );
        let esp = tempfile::tempdir().unwrap();
        fs::create_dir_all(esp.path().join("EFI/Linux")).unwrap();
        for name in [
            "archix-fallback-4.efi",
            "archix-fallback-9.efi",
            "arch-linux.efi",
            "archix-fallback-5.efi.tmp",
        ] {
            fs::write(esp.path().join("EFI/Linux").join(name), "uki").unwrap();
        }
        assert_eq!(fallbacks(esp.path()), [4, 9]);
    }

    #[test]
    fn installing_a_fallback_records_it_and_retiring_leaves_only_the_newest() {
        let esp = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::under(root.path());
        let uki = root.path().join("built.efi");
        for generation in [3, 4] {
            fs::write(&uki, format!("uki {generation}")).unwrap();
            install_fallback(&paths, esp.path(), generation, &uki).unwrap();
        }
        assert_eq!(fallbacks(esp.path()), [3, 4], "the older one stays until retired");
        assert_eq!(
            fs::read_to_string(entry_records(&paths).join("archix-fallback-4.efi")).unwrap(),
            "uki /EFI/Linux/archix-fallback-4.efi\n"
        );
        retire_fallbacks(&paths, esp.path(), 9).unwrap();
        assert_eq!(
            fallbacks(esp.path()),
            [3, 4],
            "nothing goes before generation 9's is there"
        );
        retire_fallbacks(&paths, esp.path(), 4).unwrap();
        assert_eq!(fallbacks(esp.path()), [4]);
        assert!(!entry_records(&paths).join("archix-fallback-3.efi").exists());
        assert_eq!(fs::read_to_string(fallback_path(esp.path(), 4)).unwrap(), "uki 4");
    }

    fn boot_option(description: &str, file: &str) -> Vec<u8> {
        let utf16 = |text: &str| -> Vec<u8> { text.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect() };
        let mut list = vec![4, 1, 8, 0, 1, 2, 3, 4];
        let path = utf16(file);
        list.extend([4, 4]);
        list.extend(((path.len() + 4) as u16).to_le_bytes());
        list.extend(path);
        list.extend([0x7f, 0xff, 4, 0]);
        let mut bytes = vec![7, 0, 0, 0, 1, 0, 0, 0];
        bytes.extend((list.len() as u16).to_le_bytes());
        bytes.extend(utf16(description));
        bytes.extend(list);
        bytes
    }

    #[test]
    fn the_firmware_must_have_a_boot_entry_for_systemd_boot_and_bootx64_must_be_systemd_boot() {
        let vars = tempfile::tempdir().unwrap();
        let esp = tempfile::tempdir().unwrap();
        let var =
            |name: &str, bytes: &[u8]| fs::write(vars.path().join(format!("{name}-{GLOBAL_GUID}")), bytes).unwrap();
        var(
            "Boot0001",
            &boot_option("Windows Boot Manager", "\\EFI\\Microsoft\\Boot\\bootmgfw.efi"),
        );
        var(
            "Boot0002",
            &boot_option("Linux Boot Manager", "\\EFI\\systemd\\systemd-bootx64.efi"),
        );
        let order = |numbers: &[u16]| {
            let mut bytes = vec![7, 0, 0, 0];
            bytes.extend(numbers.iter().flat_map(|n| n.to_le_bytes()));
            var("BootOrder", &bytes);
        };
        let loader = esp.path().join("EFI/BOOT/BOOTX64.EFI");
        fs::create_dir_all(loader.parent().unwrap()).unwrap();
        fs::write(&loader, b"MZ...#### LoaderInfo: systemd-boot 260.1-1-arch ####...").unwrap();
        order(&[2, 1]);
        assert_eq!(firmware_warnings(vars.path(), esp.path()), Vec::<String>::new());
        order(&[1, 2]);
        let warnings = firmware_warnings(vars.path(), esp.path());
        assert!(
            warnings.len() == 1 && warnings[0].contains("\"Windows Boot Manager\" before systemd-boot"),
            "{warnings:?}"
        );
        order(&[1]);
        let warnings = firmware_warnings(vars.path(), esp.path());
        assert!(
            warnings.len() == 1 && warnings[0].contains("no entry that starts systemd-boot"),
            "{warnings:?}"
        );
        order(&[2]);
        fs::write(&loader, b"MZ shim").unwrap();
        let warnings = firmware_warnings(vars.path(), esp.path());
        assert!(
            warnings.len() == 1 && warnings[0].contains("isn't systemd-boot"),
            "{warnings:?}"
        );
        fs::remove_file(&loader).unwrap();
        let warnings = firmware_warnings(vars.path(), esp.path());
        assert!(
            warnings.len() == 1 && warnings[0].contains("is missing"),
            "{warnings:?}"
        );
        let none = tempfile::tempdir().unwrap();
        assert_eq!(
            firmware_warnings(none.path(), esp.path()).len(),
            1,
            "without EFI variables only the ESP is checked"
        );
    }

    #[test]
    fn loader_conf_names_a_generation_by_its_default_or_preferred_entry() {
        let esp = esp_with(&[], "timeout 2\ndefault archix-3.conf\npreferred archix-5.conf\n");
        assert!(loader_names(esp.path(), 3) && loader_names(esp.path(), 5));
        assert!(!loader_names(esp.path(), 4));
        let esp = esp_with(&[], "timeout 2\n");
        assert!(!loader_names(esp.path(), 3), "a lost default names nothing");
    }

    #[test]
    fn the_kernel_version_is_the_one_pacman_owns() {
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
        assert_eq!(booted_generation("root=/dev/vda2"), None);
    }

    #[test]
    fn a_failed_health_check_leaves_the_counted_entry_no_tries() {
        let esp = esp_with(
            &[
                "archix-4.conf",
                "archix-5+2-1.conf",
                "archix-5-protected.conf",
                "archix-6+3.conf",
            ],
            "",
        );
        assert!(counting(esp.path(), 5) && counting(esp.path(), 6) && !counting(esp.path(), 4));
        assert!(mark_bad(esp.path(), 5).unwrap());
        assert!(mark_bad(esp.path(), 6).unwrap());
        assert!(
            !mark_bad(esp.path(), 4).unwrap(),
            "an entry without counters can't be marked"
        );
        let mut names: Vec<String> = fs::read_dir(entries_dir(esp.path()))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                "archix-4.conf",
                "archix-5+0-3.conf",
                "archix-5-protected.conf",
                "archix-6+0-3.conf"
            ]
        );
        assert!(!counting(esp.path(), 5), "a bad entry isn't counting any more");
    }

    #[test]
    fn the_esp_mounts_on_access_and_unmounts_when_idle() {
        let fstab = "# /dev/vda2\nUUID=a / ext4 rw,relatime 0 1\n\n# esp\nUUID=b\t/efi/\tvfat\tdefaults\t0 2\n";
        assert_eq!(
            automount_esp(fstab, "/efi").unwrap(),
            "# /dev/vda2\nUUID=a / ext4 rw,relatime 0 1\n\n# esp\nUUID=b /efi/ vfat defaults,x-systemd.automount,x-systemd.idle-timeout=60 0 2\n"
        );
        let once = automount_esp(fstab, "/efi").unwrap();
        assert_eq!(automount_esp(&once, "/efi"), None, "already automounted");
        assert_eq!(
            automount_esp("UUID=b /efi vfat noauto 0 2\n", "/efi"),
            None,
            "noauto is the user's call"
        );
        assert_eq!(automount_esp(fstab, "/boot/efi"), None, "no line for that ESP");
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("fstab"), fstab).unwrap();
        automount_esp_in(dir.path(), "/efi", true).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("fstab")).unwrap(),
            fstab,
            "a managed fstab is left alone"
        );
        automount_esp_in(dir.path(), "/efi", false).unwrap();
        assert_eq!(fs::read_to_string(dir.path().join("fstab")).unwrap(), once);
    }

    #[test]
    fn an_entry_is_trusted_only_while_it_matches_its_record() {
        let esp = esp_with(&["archix-3+1-2.conf"], "default archix-3.conf\n");
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::under(root.path());
        fs::create_dir_all(entry_records(&paths)).unwrap();
        fs::write(paths.state.join("boot/esp"), esp.path().to_str().unwrap()).unwrap();
        assert!(!entry_unchanged(&paths, "archix-3.conf"), "no record");
        fs::write(entry_records(&paths).join("archix-3.conf"), "title t\n").unwrap();
        assert!(entry_unchanged(&paths, "archix-3.conf"));
        fs::write(entries_dir(esp.path()).join("archix-3+1-2.conf"), "title evil\n").unwrap();
        assert!(!entry_unchanged(&paths, "archix-3.conf"), "changed on the ESP");
    }
}
