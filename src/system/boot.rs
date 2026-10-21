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
    /// The sha256 of systemd's PCR policy public key the deployment's UKIs were built with, so a new key makes a new deployment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pcr_key: Option<String>,
}

/// Makes a new PCR policy signing key, puts it in place of the old one in every deployment's /etc (the running one's included), then binds each LUKS device's TPM2 slot to it; the next switch builds UKIs whose policies it signs.
pub fn rotate_pcr_key(paths: &Paths, devices: &[PathBuf]) -> Result<()> {
    let mut etcs: Vec<PathBuf> = fs::read_dir(paths.root.join("archix/deploy"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.parse::<u32>().is_ok())
        })
        .map(|entry| entry.path().join("etc"))
        .filter(|etc| etc.is_dir())
        .collect();
    if etcs.is_empty() {
        etcs.push(paths.root.join("etc"));
    }
    let dir = std::env::temp_dir().join(format!("archix-pcr-key-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    std::os::unix::fs::DirBuilderExt::mode(&mut fs::DirBuilder::new(), 0o700)
        .create(&dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    let (private, public) = (dir.join("private.pem"), dir.join("public.pem"));
    let made = run(
        Command::new("openssl")
            .args([
                "genpkey",
                "-quiet",
                "-algorithm",
                "RSA",
                "-pkeyopt",
                "rsa_keygen_bits:2048",
                "-out",
            ])
            .arg(&private),
        "openssl genpkey",
    )
    .and_then(|()| {
        run(
            Command::new("openssl")
                .args(["pkey", "-pubout", "-in"])
                .arg(&private)
                .arg("-out")
                .arg(&public),
            "openssl pkey",
        )
    })
    .and_then(|()| Ok((fs::read(&private)?, fs::read(&public)?)));
    let _ = fs::remove_file(&private);
    let (private_pem, public_pem) = made?;
    for etc in &etcs {
        let at = |key: &str| etc.join(key.trim_start_matches("/etc/"));
        crate::durable::write_secret(&at(PCR_PRIVATE_KEY), &private_pem)?;
        crate::durable::write(&at(PCR_PUBLIC_KEY), &public_pem)?;
    }
    println!(
        "wrote a new PCR policy signing key to /etc/systemd in {}",
        if etcs.len() == 1 {
            "the running system".to_string()
        } else {
            format!("all {} deployments", etcs.len())
        }
    );
    let mut failed = Vec::new();
    for device in devices {
        let enrolled = Command::new("systemd-cryptenroll")
            .args(["--wipe-slot=tpm2", "--tpm2-device=auto", "--tpm2-public-key-pcrs=11"])
            .arg(format!("--tpm2-public-key={}", public.display()))
            .arg(device)
            .status()
            .is_ok_and(|status| status.success());
        match enrolled {
            true => println!("bound {}'s TPM2 slot to the new key", device.display()),
            false => failed.push(device.display().to_string()),
        }
    }
    let _ = fs::remove_dir_all(&dir);
    if !failed.is_empty() {
        bail!(
            "couldn't bind {} to the new key; the key is in place, so run `sudo systemd-cryptenroll --wipe-slot=tpm2 --tpm2-device=auto --tpm2-public-key={PCR_PUBLIC_KEY} --tpm2-public-key-pcrs=11 <device>` for each",
            failed.join(", ")
        );
    }
    println!(
        "run `archix switch` before the next restart: until a UKI built with the new key boots, TPM2-bound volumes unlock only with a passphrase or recovery key, and UKIs built with the old key no longer unlock them"
    );
    Ok(())
}

/// What `BootPlan::pcr_key` records for the /etc at `etc`: the public key's hash once the private key is there.
pub fn pcr_key_fingerprint(etc: &Path) -> Option<String> {
    let at = |key: &str| etc.join(key.trim_start_matches("/etc/"));
    at(PCR_PRIVATE_KEY)
        .is_file()
        .then(|| crate::fetch::sha256_bytes(&fs::read(at(PCR_PUBLIC_KEY)).unwrap_or_default()))
}

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

/// Every kernel package in pacman database `local_db`, by name, with the modules dir it owns.
pub fn kernels_in(local_db: &Path) -> Vec<(String, String)> {
    let mut kernels: Vec<(String, String)> = fs::read_dir(local_db)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let files = fs::read_to_string(entry.path().join("files")).ok()?;
            let version = files
                .lines()
                .find_map(|line| line.strip_prefix("usr/lib/modules/")?.strip_suffix("/vmlinuz"))?;
            let name = crate::pacman::name_from_entry(&entry.file_name().to_string_lossy()).to_string();
            Some((name, version.to_string()))
        })
        .collect();
    kernels.sort();
    kernels
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

/// A boot menu entry of a generation, which names the UKI it boots: archix-N.conf, or with a suffix for its protected twin (-protected) or another installed kernel (-linux-lts).
fn entry_name(generation: u32, suffix: &str) -> String {
    format!("archix-{generation}{suffix}.conf")
}

/// A generation's normal entry, with systemd-boot's boot counter when it gets `tries` attempts: archix-N+3.conf becomes archix-N.conf once a boot reaches boot-complete.target, and archix-N+0-3.conf is skipped for an older entry after three failed ones.
fn counted_entry_name(generation: u32, tries: u32) -> String {
    match tries {
        0 => entry_name(generation, ""),
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

/// An entry systemd-boot has given up on: no tries left.
pub fn is_bad(name: &str) -> bool {
    name.split_once('+')
        .and_then(|(_, counters)| counters.split(['-', '.']).next())
        .is_some_and(|left| left == "0")
}

/// The generation an entry file (archix-N.conf, archix-N-protected.conf, archix-N-linux-lts.conf, with or without boot counters) belongs to, and whether it's an extra entry rather than the normal one.
pub(crate) fn parse_entry(name: &str) -> Option<(u32, bool)> {
    let stem = name.strip_prefix("archix-")?.strip_suffix(".conf")?;
    let stem = stem.split('+').next().unwrap_or(stem);
    let (number, suffix) = stem.split_at(stem.bytes().take_while(u8::is_ascii_digit).count());
    if number.is_empty() || number.starts_with('0') {
        return None;
    }
    let extra = match suffix {
        "" => false,
        suffix if suffix.len() > 1 && suffix.starts_with('-') => true,
        _ => return None,
    };
    Some((number.parse().ok()?, extra))
}

/// The generations that have a boot entry on the ESP, with whether it's an extra entry (the protected twin or another kernel's).
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
    for (number, _, path) in entries(esp) {
        if number == generation {
            fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
            let id = entry_id(&path.file_name().unwrap_or_default().to_string_lossy());
            let _ = fs::remove_file(entry_records(paths).join(id));
        }
    }
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
    /// What the entry's file name adds after archix-N: nothing for the normal entry, "-protected", or "-" and another kernel's package name.
    suffix: String,
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

/// Builds a staged deployment's initramfs and UKIs inside its staging root, from its own kernels and /etc, signing them when Secure Boot signing is on; UKIs the ESP already has are reused. boot.kernel gets the normal entry, every other installed kernel an extra one.
pub fn build(paths: &Paths, plan: &BootPlan, number: u32, staging: &Staging, tx: &Transaction) -> Result<Built> {
    let version = owned_version(&staging.tree("pacman"), &plan.kernel).with_context(|| {
        format!(
            "the {} kernel package isn't installed in the new deployment; boot.kernel names it",
            plan.kernel
        )
    })?;
    let mut kernels = vec![(plan.kernel.clone(), version.clone())];
    kernels.extend(
        kernels_in(&staging.tree("pacman"))
            .into_iter()
            .filter(|(name, _)| *name != plan.kernel && !name.contains('+')),
    );
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
    let esp = Path::new(&plan.esp);
    let mut ukis = Vec::new();
    for (kernel, version) in &kernels {
        let main = *kernel == plan.kernel;
        let vmlinuz = Path::new(MODULES).join(version).join("vmlinuz");
        if !staging.read_path(&vmlinuz)?.is_file() {
            bail!("{} is missing from the new deployment", vmlinuz.display());
        }
        let initramfs = format!("{BUILT}/initramfs-{kernel}.img");
        in_view(
            &["mkinitcpio", "-k", version, "-g", &initramfs, "-S", "autodetect"],
            "mkinitcpio",
        )?;
        let initramfs_file = staging.read_path(Path::new(&initramfs))?;
        // (title, what the command line adds, what the entry title adds, the entry file suffix)
        let mut variants = vec![(
            "archix",
            String::new(),
            if main { String::new() } else { format!(", {kernel}") },
            if main { String::new() } else { format!("-{kernel}") },
        )];
        if main && plan.anticheat {
            variants.push((
                "archix protected",
                format!(" {}", super::secureboot::ANTICHEAT_CMDLINE),
                String::new(),
                "-protected".to_string(),
            ));
        }
        for (title, extra_cmdline, title_kernel, suffix) in variants {
            let mut text = release.clone();
            text.push(format!("PRETTY_NAME=\"Arch Linux ({title})\""));
            text.push("IMAGE_ID=archix".to_string());
            let release_text = text.join("\n") + "\n";
            let cmdline = format!("{}{extra_cmdline}", plan.cmdline.trim());
            let key = payload_key(
                &staging.read_path(&vmlinuz)?,
                &microcode_files,
                &initramfs_file,
                &cmdline,
                &release_text,
                secure_boot,
                pcr_key.as_deref(),
            )?;
            let protected = if suffix == "-protected" { "-protected" } else { "" };
            let name = format!("{version}{protected}-{key}.efi");
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
                title: format!("Arch Linux ({title}, generation {number}{title_kernel})"),
                suffix,
                name,
                built,
            });
        }
        in_view(&["rm", "-f", &initramfs], "removing the initramfs")?;
    }
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
            remove_unused_ukis_but(esp, &reused)?;
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
            let name = match uki.suffix.as_str() {
                "" => counted_entry_name(number, if counted { self.plan.tries } else { 0 }),
                suffix => entry_name(number, suffix),
            };
            // The record comes first: the initramfs boots an entry only once archix has recorded it.
            crate::durable::write(
                &entry_records(self.paths).join(entry_name(number, &uki.suffix)),
                text.as_bytes(),
            )?;
            crate::durable::write(&entries_dir(esp).join(name), text.as_bytes())?;
        }
        mark("deploy-esp-loader");
        set_default_entry(esp, number)
    }

    fn clean(&self) -> Result<()> {
        crate::deploy::commit::clean_esp(Path::new(&self.plan.esp))
    }

    fn abandon(&self, number: u32, previous: Option<u32>) -> Result<()> {
        let esp = Path::new(&self.plan.esp);
        crate::deploy::commit::clean_esp(esp)?;
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
    // In a chroot, such as an image build, or on a test root, the EFI variables are the host's.
    if super::secureboot::in_chroot() || crate::testhooks::root() != Path::new("/") {
        return Ok(());
    }
    // EFI variables set by `bootctl set-default` or `set-preferred` would override loader.conf.
    for verb in ["set-default", "set-preferred"] {
        let _ = Command::new("bootctl")
            .args([verb, ""])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
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
    for (_, _, path) in entries(Path::new(esp)) {
        fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    }
    match fs::remove_dir_all(uki_dir(Path::new(esp))) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e).context("removing archix's UKIs"),
        _ => {}
    }
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

/// How many UKIs a deployment of `plan` built from pacman database `local_db` gets: one per kernel, and the protected twin.
pub fn uki_count(plan: &BootPlan, local_db: &Path) -> u64 {
    let others = kernels_in(local_db)
        .iter()
        .filter(|(name, _)| *name != plan.kernel)
        .count();
    1 + u64::from(plan.anticheat) + others as u64
}

/// Checked before a switch changes anything: the `ukis` new UKIs must fit in what's free plus what dropping the entries of deployments outside `pinned` would free; `kernel` stands in for a UKI when the ESP has none yet.
pub fn check_space(esp: &Path, pinned: &BTreeSet<u32>, kernel: Option<&Path>, ukis: u64) -> Result<()> {
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
    let needed = newest * ukis.max(1) + MARGIN;
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
        fs::write(uki_dir(esp.path()).join("7.2.9-arch1-1-cccc.efi"), "uki").unwrap();
        for (name, uki) in [
            ("archix-4.conf", "aaaa"),
            ("archix-5+2-1.conf", "aaaa"),
            ("archix-5-protected.conf", "bbbb"),
            ("archix-5-linux-lts.conf", "cccc"),
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
        assert_eq!(parse_entry("archix-9-linux-lts.conf"), Some((9, true)));
        assert_eq!(parse_entry("archix-9-.conf"), None);
        assert_eq!(parse_entry("archix-9x.conf"), None);
        assert_eq!(entry_name(9, "-linux-lts"), "archix-9-linux-lts.conf");
        assert!(is_bad("archix-9+0-3.conf") && !is_bad("archix-9+1-2.conf") && !is_bad("archix-9.conf"));
        assert_eq!(counted_entry_name(9, 3), "archix-9+3.conf");
        assert_eq!(counted_entry_name(9, 0), "archix-9.conf");
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
        fs::write(
            db.path().join("linux-lts-6.12.1-1/files"),
            "%FILES%\nusr/lib/modules/6.12.1-1-lts/vmlinuz\n\n",
        )
        .unwrap();
        assert_eq!(
            kernels_in(db.path()),
            [
                ("linux".to_string(), "7.2.9-arch1-1".to_string()),
                ("linux-lts".to_string(), "6.12.1-1-lts".to_string())
            ]
        );
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
