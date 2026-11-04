use std::ffi::CString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::paths::Paths;

/// A root `build` makes boot images from: the running system, or a test root whose commands run chrooted.
pub trait BootRoot {
    /// Its pacman database.
    fn local_db(&self) -> PathBuf;
    /// Where `path`, absolute in the root, is from here.
    fn read_path(&self, path: &Path) -> Result<PathBuf>;
    fn output(&self, argv: &[&str]) -> Result<std::process::Output>;
    /// Like `output`, for the one step that may read the PCR policy key.
    fn signing_output(&self, argv: &[&str]) -> Result<std::process::Output>;
}

/// The boot loader archix writes entries for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Loader {
    #[default]
    SystemdBoot,
    Grub,
    Limine,
}

impl Loader {
    pub fn parse(name: &str) -> Option<Loader> {
        match name {
            "systemd-boot" => Some(Loader::SystemdBoot),
            "grub" => Some(Loader::Grub),
            "limine" => Some(Loader::Limine),
            _ => None,
        }
    }
}

fn unblessed_default() -> u32 {
    10
}

/// How a generation's boot images and entries are built, kept with the generation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BootPlan {
    #[serde(default)]
    pub loader: Loader,
    pub esp: String,
    pub kernel: String,
    pub microcode: Vec<String>,
    pub cmdline: String,
    pub secure_boot: Option<bool>,
    /// Also write a protected entry with the command line Easy Anti-Cheat Linux v2 asks for.
    #[serde(default)]
    pub anticheat: bool,
    /// Boots systemd-boot tries the current system's entry after a change before falling back; 0 turns boot counting off.
    #[serde(default)]
    pub tries: u32,
    /// Revoke the images no entry uses after each switch or rollback.
    #[serde(default)]
    pub revoke_unused: bool,
    /// The sha256 of systemd's PCR policy public key the images were built with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pcr_key: Option<String>,
    /// What a counted boot must reach before it counts as good.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health_check: Option<super::health::HealthCheck>,
    /// Keep a rescue entry on the ESP, refreshed after each switch.
    #[serde(default = "rescue_default")]
    pub rescue: bool,
    /// Minutes an unblessed boot may take, or sit in emergency or rescue mode, before it reboots itself; 0 is off.
    #[serde(default = "unblessed_default")]
    pub unblessed_timeout: u32,
}

fn rescue_default() -> bool {
    true
}

/// Makes a new PCR policy signing key, puts it in place of the old one in the root's /etc, then binds each LUKS device's TPM2 slot to it; the next switch builds images whose policies it signs.
pub fn rotate_pcr_key(paths: &Paths, devices: &[PathBuf]) -> Result<()> {
    let etcs = [paths.root.join("etc")];
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
    println!("wrote a new PCR policy signing key to /etc/systemd");
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
pub const MARGIN: u64 = 16 * 1024 * 1024;
/// Where `build` leaves the images it made in the root, for the menu to copy to the ESP.
pub const BUILT: &str = "/var/tmp/archix-boot";

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

/// The shared UKIs, outside EFI/Linux so systemd-boot shows them only through the entries that name them.
fn uki_dir(esp: &Path) -> PathBuf {
    esp.join("EFI/archix")
}

fn entries_dir(esp: &Path) -> PathBuf {
    esp.join("loader/entries")
}

/// statfs's f_type for FAT, which every ESP is.
const MSDOS_SUPER_MAGIC: u64 = 0x4d44;

/// Whether a FAT filesystem is mounted at `path`.
pub fn is_fat(path: &Path) -> bool {
    let Ok(c_path) = CString::new(path.as_os_str().as_encoded_bytes()) else {
        return false;
    };
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    unsafe { libc::statfs(c_path.as_ptr(), &mut stat) == 0 && stat.f_type as u64 == MSDOS_SUPER_MAGIC }
}

/// The name in /dev/disk/`kind` of the device the filesystem at `path` is on.
pub fn disk_link_of(path: &Path, kind: &str) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let dev = fs::metadata(path).ok()?.dev();
    fs::read_dir(Path::new("/dev/disk").join(kind))
        .ok()?
        .flatten()
        .find_map(|link| {
            (fs::metadata(link.path()).ok()?.rdev() == dev).then(|| link.file_name().to_string_lossy().into_owned())
        })
}

/// What `esp_fstab` changed on the ESP's fstab line.
#[derive(Debug, Default, PartialEq)]
pub struct EspFstab {
    pub added: bool,
    pub automount: bool,
    pub fsck: bool,
    pub private: bool,
}

/// The options of an ESP line archix adds: mounted on access and unmounted after a minute idle, and readable by root only, as systemd-gpt-auto-generator mounts it.
const ESP_OPTIONS: &str = "umask=0077,x-systemd.automount,x-systemd.idle-timeout=60";

/// fstab text with the ESP's line mounting it on access and unmounting it after a minute idle, so a forced reboot rarely finds the FAT mounted (systemd-bless-boot's renames and archix's writes mount it again), and with fs_passno 2, without which systemd runs no fsck on it and the kernel remounts a FAT a power cut left half-written read-only at its first broken chain, and readable by root only when it sets no umask, fmask or dmask, as bootctl refuses to trust a world-readable random seed; a missing line is added for the filesystem `uuid` names, as systemd-gpt-auto-generator would mount the ESP at /boot instead; None when the line already does all three, or is missing and there's no `uuid` (a noauto line is the user's to mount, so only gets the fsck).
pub fn esp_fstab(fstab: &str, esp: &str, uuid: Option<&str>) -> Option<(String, EspFstab)> {
    let esp = esp.trim_end_matches('/');
    let mut changed = EspFstab::default();
    let lines: Vec<String> = fstab
        .lines()
        .map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if line.trim_start().starts_with('#') || fields.len() < 4 || fields[1].trim_end_matches('/') != esp {
                return line.to_string();
            }
            let automount = !fields[3]
                .split(',')
                .any(|option| option == "noauto" || option == "x-systemd.automount");
            let fsck = fields
                .get(5)
                .is_none_or(|passno| passno.parse::<u32>().is_ok_and(|n| n == 0));
            let private = !fields[3]
                .split(',')
                .any(|option| ["umask=", "fmask=", "dmask="].iter().any(|key| option.starts_with(key)));
            if !automount && !fsck && !private {
                return line.to_string();
            }
            let mut fields: Vec<String> = fields.iter().map(|f| f.to_string()).collect();
            if automount {
                changed.automount = true;
                fields[3].push_str(",x-systemd.automount,x-systemd.idle-timeout=60");
            }
            if private {
                changed.private = true;
                fields[3].push_str(",fmask=0077,dmask=0077");
            }
            if fsck {
                changed.fsck = true;
                fields.resize(6, "0".to_string());
                fields[5] = "2".to_string();
            }
            fields.join(" ")
        })
        .collect();
    let mut text = lines.join("\n") + "\n";
    let listed = fstab.lines().any(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        !line.trim_start().starts_with('#') && fields.len() >= 2 && fields[1].trim_end_matches('/') == esp
    });
    if let (false, Some(uuid)) = (listed, uuid) {
        changed.added = true;
        if text.trim().is_empty() {
            text.clear();
        }
        text.push_str(&format!("UUID={uuid} {esp} vfat {ESP_OPTIONS} 0 2\n"));
    }
    (changed != EspFstab::default()).then_some((text, changed))
}

/// Says what `esp_fstab` changed on the ESP's line of the root's fstab.
pub fn report_esp_fstab(esp: &str, changed: &EspFstab) {
    if changed.added {
        println!(
            "fstab      the ESP at {esp} had no line, so systemd-gpt-auto-generator would have mounted it at /boot instead; it has its own line now, mounted on access and fscked at boot"
        );
    }
    if changed.automount {
        println!(
            "fstab      the ESP at {esp} mounts on access and unmounts after a minute idle, so a forced reboot rarely catches it mounted"
        );
    }
    if changed.fsck {
        println!(
            "fstab      the ESP at {esp} is checked by fsck at boot (fs_passno 2), so a FAT a power cut left damaged is repaired before it's mounted"
        );
    }
    if changed.private {
        println!(
            "fstab      the ESP at {esp} is readable by root only (fmask=0077,dmask=0077), as bootctl wants for its random seed"
        );
    }
}

/// An entry systemd-boot has given up on: no tries left.
pub fn is_bad(name: &str) -> bool {
    name.split_once('+')
        .and_then(|(_, counters)| counters.split(['-', '.']).next())
        .is_some_and(|left| left == "0")
}

pub(crate) fn free_bytes(dir: &Path) -> Result<u64> {
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

/// Builds the root's boot images into BUILT, from its own kernels and /etc, with a reproducible initramfs, signing them when Secure Boot signing is on: one per distinct kernel, initramfs, command line and key, named by their hash, so an image the ESP already has is reused rather than built and signed again. boot.kernel's is the main image, every other installed kernel's an extra one, as is the protected twin.
pub fn build(paths: &Paths, plan: &BootPlan, root: &dyn BootRoot) -> Result<super::menu::Images> {
    let version = owned_version(&root.local_db(), &plan.kernel).with_context(|| {
        format!(
            "the {} kernel package isn't installed; boot.kernel names it",
            plan.kernel
        )
    })?;
    let mut kernels = vec![(plan.kernel.clone(), version.clone())];
    kernels.extend(
        kernels_in(&root.local_db())
            .into_iter()
            .filter(|(name, _)| *name != plan.kernel && !name.contains('+')),
    );
    let in_tx = |signing: bool, argv: &[&str], what: &str| -> Result<String> {
        let output = if signing {
            root.signing_output(argv)?
        } else {
            root.output(argv)?
        };
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let tail: Vec<&str> = stdout.lines().chain(stderr.lines()).rev().take(12).collect();
            bail!(
                "{what} failed:\n{}",
                tail.into_iter().rev().collect::<Vec<_>>().join("\n")
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    };
    let in_view = |argv: &[&str], what: &str| in_tx(false, argv, what);
    in_view(&["rm", "-rf", BUILT], "clearing the image build directory")?;
    in_view(&["mkdir", "-p", BUILT], "creating the image build directory")?;
    let os_release = in_view(&["cat", "/etc/os-release"], "reading /etc/os-release")?;
    let release: Vec<String> = os_release
        .lines()
        .filter(|line| {
            !line.starts_with("PRETTY_NAME=") && !line.starts_with("IMAGE_ID=") && !line.starts_with("IMAGE_VERSION=")
        })
        .map(str::to_string)
        .collect();
    let secure_boot = plan.secure_boot.unwrap_or_else(sbctl_has_keys);
    let pcr_key = root
        .read_path(Path::new(PCR_PRIVATE_KEY))?
        .is_file()
        .then(|| fs::read(root.read_path(Path::new(PCR_PUBLIC_KEY)).ok()?).ok())
        .map(Option::unwrap_or_default);
    let microcode: Vec<String> = plan.microcode.iter().map(|m| format!("/boot/{m}")).collect();
    let mut microcode_files = Vec::new();
    for file in &microcode {
        microcode_files.push(root.read_path(Path::new(file))?);
    }
    let esp = Path::new(&plan.esp);
    let mut images = super::menu::Images::default();
    let mut made = Vec::new();
    for (kernel, version) in &kernels {
        let main = *kernel == plan.kernel;
        let vmlinuz = Path::new(MODULES).join(version).join("vmlinuz");
        if !root.read_path(&vmlinuz)?.is_file() {
            bail!("{} is missing", vmlinuz.display());
        }
        let initramfs = format!("{BUILT}/initramfs-{kernel}.img");
        // A fixed epoch makes the same inputs give the same initramfs, so its image is shared rather than built again.
        in_view(
            &[
                "env",
                "SOURCE_DATE_EPOCH=0",
                "mkinitcpio",
                "-k",
                version,
                "-g",
                &initramfs,
                "-S",
                "autodetect",
            ],
            "mkinitcpio",
        )?;
        let initramfs_file = root.read_path(Path::new(&initramfs))?;
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
            let cmdline = with_panic_reboot(&format!("{}{extra_cmdline}", plan.cmdline.trim()), plan.tries);
            let key = payload_key(
                &root.read_path(&vmlinuz)?,
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
            crate::durable::write(
                &super::menu::dir(paths).join("cmdlines").join(&name),
                cmdline.as_bytes(),
            )?;
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
                    if root.read_path(Path::new(PCR_PUBLIC_KEY))?.is_file() {
                        ukify.push(format!("--pcr-public-key={PCR_PUBLIC_KEY}"));
                    }
                }
                let argv: Vec<&str> = ukify.iter().map(String::as_str).collect();
                match pcr_key {
                    Some(_) => in_tx(true, &argv, "ukify build")?,
                    None => in_view(&argv, "ukify build")?,
                };
                crate::testhooks::mark("boot-image-built");
                if secure_boot {
                    crate::testhooks::mark("boot-image-sign");
                    let file = root.read_path(Path::new(&output))?;
                    run(
                        Command::new("sbctl").env("ESP_PATH", esp).arg("sign").arg(&file),
                        "sbctl sign",
                    )?;
                    super::revoke::record_signed(paths, &file, &name)?;
                }
            }
            made.push(match built {
                true => root.read_path(&Path::new(BUILT).join(&name))?,
                false => uki_dir(esp).join(&name),
            });
            let image = super::menu::Image {
                name,
                version: version.clone(),
            };
            match suffix.is_empty() {
                true => images.main = Some(image),
                false => images.extras.push((
                    suffix.trim_start_matches('-').to_string(),
                    format!("Arch Linux ({title}{title_kernel})"),
                    image,
                )),
            }
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
    refuse_unsigned(plan, &made)?;
    Ok(images)
}

/// Seconds a kernel panic waits before it reboots, which archix's images set unless the command line already does: a boot whose init can't start (a broken glibc or systemd) panics, and without a reboot it would never use up its tries and fall back.
pub const PANIC_REBOOT: u32 = 10;

/// `cmdline` with panic=PANIC_REBOOT when boot counting is on and it sets no panic= of its own.
pub fn with_panic_reboot(cmdline: &str, tries: u32) -> String {
    if tries == 0 || cmdline.split_whitespace().any(|word| word.starts_with("panic=")) {
        return cmdline.to_string();
    }
    format!("{cmdline} panic={PANIC_REBOOT}").trim_start().to_string()
}

/// Signs a loader on the ESP as a copy that replaces it by rename, so a crash never leaves it half written.
pub(crate) fn sign_replacing(esp: &Path, path: &Path) -> Result<()> {
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
        .and_then(|()| {
            fs::File::open(&tmp)
                .and_then(|file| file.sync_all())
                .with_context(|| format!("writing {}", tmp.display()))
        })
        .and_then(|()| fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display())));
    if signed.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    signed?;
    crate::durable::sync_renamed(path)
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

/// Turns systemd-boot's entry editor off in loader.conf, as under Secure Boot it would let anyone at the keyboard change what an entry passes; unsigned, loader.conf's `editor` stays as the user has it.
fn lock_editor(esp: &Path) -> Result<()> {
    let conf = esp.join("loader/loader.conf");
    let text = fs::read_to_string(&conf).unwrap_or_default();
    let is_editor = |line: &&str| line.split_whitespace().next() == Some("editor");
    let editor: Vec<Vec<&str>> = text
        .lines()
        .filter(is_editor)
        .map(|line| line.split_whitespace().collect())
        .collect();
    if editor == [["editor", "no"]] {
        return Ok(());
    }
    let mut lines: Vec<&str> = text.lines().filter(|line| !is_editor(line)).collect();
    lines.push("editor no");
    crate::durable::write(&conf, (lines.join("\n") + "\n").as_bytes())
}

/// Points loader.conf at archix's menu: `preferred` the glob systemd-boot walks in sort-key order past entries without tries, `default` the entry it starts when every one is bad; the editor goes off when the images are signed. Clears the EFI variables that would override it.
pub fn point_loader(esp: &Path, preferred: &str, default: &str, signed: bool) -> Result<()> {
    let conf = esp.join("loader/loader.conf");
    let old = fs::read_to_string(&conf).unwrap_or_default();
    let mut lines: Vec<String> = old
        .lines()
        .filter(|line| !matches!(line.split_whitespace().next(), Some("default" | "preferred")))
        .map(str::to_string)
        .collect();
    lines.push(format!("default {default}"));
    lines.push(format!("preferred {preferred}"));
    let text = lines.join("\n") + "\n";
    if text != old {
        crate::durable::write(&conf, text.as_bytes())?;
    }
    if signed {
        lock_editor(esp)?;
    }
    clear_overrides(Some(default));
    Ok(())
}

pub(crate) const LOADER_GUID: &str = "4a67b082-0a4c-41cf-b6c7-440b29bb8c4f";

/// systemd-boot's EFI variable `name`, where the test hooks put it in a debug build.
fn loader_var(name: &str) -> Option<PathBuf> {
    crate::testhooks::efivars().map(|dir| dir.join(format!("{name}-{LOADER_GUID}")))
}

/// The entry an EFI variable such as LoaderEntryDefault names, if it's set.
fn loader_var_entry(name: &str) -> Option<String> {
    efivar_string(&fs::read(loader_var(name)?).ok()?).filter(|entry| !entry.is_empty())
}

/// The partition systemd-boot started from (LoaderDevicePartUUID), lowercase, which is the ESP.
pub fn loader_partition() -> Option<String> {
    loader_var_entry("LoaderDevicePartUUID").map(|uuid| uuid.trim().to_lowercase())
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

/// Run by systemd-bless-boot's drop-in just before it renames the booted entry to its name without boot counters: does that rename itself and flushes the ESP, whose fsync of the entry under its new name writes the name's directory entry. vfat's rename leaves the new name's directory entry empty until the file's inode is written, and systemd-bless-boot only syncs the directory and the filesystem, which on FAT doesn't flush the disk, so a power cut after its rename can leave the blessed entry empty; systemd-bless-boot then finds the rename done.
pub fn bless_booted(paths: &Paths) -> Result<()> {
    let Some(selected) = crate::testhooks::selected_entry() else {
        return Ok(());
    };
    let Ok(esp) = fs::read_to_string(paths.state.join("boot").join("esp")) else {
        return Ok(());
    };
    let id = entry_id(&selected);
    if super::menu::Entry::parse(&id).is_none() {
        return Ok(());
    }
    let dir = entries_dir(Path::new(esp.trim()));
    let target = dir.join(&id);
    if target.exists() {
        return Ok(());
    }
    let Some(counted) = fs::read_dir(&dir).into_iter().flatten().flatten().find_map(|file| {
        let name = file.file_name().to_string_lossy().into_owned();
        (name.contains('+') && entry_id(&name) == id).then_some(name)
    }) else {
        return Ok(());
    };
    crate::testhooks::mark("bless-rename");
    fs::rename(dir.join(&counted), &target).with_context(|| format!("renaming {counted} to {id}"))?;
    crate::testhooks::mark("bless-renamed");
    crate::durable::sync_esp(&target).with_context(|| format!("syncing {id}"))?;
    crate::testhooks::mark("bless-synced");
    println!("archix: the boot counted as good: renamed {counted} to {id}, synced under its new name");
    Ok(())
}

/// Where a boot that couldn't write the ESP leaves its note, until a boot that can.
fn esp_unwritable_record(paths: &Paths) -> PathBuf {
    paths.state.join("boot").join("esp-unwritable")
}

/// The note a boot that couldn't write the ESP left, for `archix status`.
pub fn esp_unwritable_note(paths: &Paths) -> Option<String> {
    fs::read_to_string(esp_unwritable_record(paths))
        .ok()
        .filter(|note| !note.is_empty())
}

/// Says, in the login notes and `archix status`, that `what` didn't happen as the ESP couldn't be written.
pub fn esp_unwritable(paths: &Paths, esp: &Path, what: &str, why: &str) -> Result<()> {
    let note = format!(
        "archix: the ESP at {} is read-only or can't be written ({why}), so {what}; current stays as it is, and archix tries again at every boot. The kernel turns a FAT read-only at a broken cluster chain, which a power cut can leave: `journalctl -k | grep -i fat` shows it, and a reboot repairs it with fsck when the ESP's fstab line has fs_passno 2 (or `umount {}; fsck.fat -a` on its device)",
        esp.display(),
        esp.display()
    );
    println!("{note}");
    let old = esp_unwritable_note(paths);
    crate::durable::write(&esp_unwritable_record(paths), note.as_bytes())?;
    let mut notes = fs::read_to_string(paths.notes()).unwrap_or_default();
    if let Some(old) = old {
        notes = notes.replace(&format!("{old}\n"), "");
    }
    notes.push_str(&note);
    notes.push('\n');
    crate::durable::write(&paths.notes(), notes.as_bytes())
}

/// Why a new generation's entry won't be counted, when the systemd-boot that started this boot can't do it.
pub fn counting_off() -> Option<String> {
    let features = crate::testhooks::loader_features();
    (features != 0 && !loader_counts_boots(features)).then(|| {
        "boot counting is off: the systemd-boot that started this boot is older than 260, so a generation that doesn't boot isn't passed over; `sudo bootctl update` installs the one your systemd package has".to_string()
    })
}

pub(crate) const GLOBAL_GUID: &str = "8be4df61-93ca-11d2-aa0d-00e098032b8c";

/// The file paths in an EFI load option (Boot####), lowercased with forward slashes, and its description.
pub(crate) fn load_option(bytes: &[u8]) -> Option<(String, Vec<String>)> {
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

/// On Limine, which boots the kernel and initramfs it copied to the ESP rather than the root's, a keep of the snapshot at `snapshot` would boot it with the current root's kernel: refused when the kernels (by modules directory and vmlinuz) or the mkinitcpio config differ.
pub fn limine_keep_refusal(current: &Path, snapshot: &Path) -> Option<String> {
    let kernels = |root: &Path| -> Vec<(String, Option<String>)> {
        let mut found: Vec<(String, Option<String>)> = fs::read_dir(root.join("usr/lib/modules"))
            .into_iter()
            .flatten()
            .flatten()
            .filter(|dir| dir.path().join("vmlinuz").is_file())
            .map(|dir| {
                let hash = crate::fetch::sha256(&dir.path().join("vmlinuz")).ok();
                (dir.file_name().to_string_lossy().into_owned(), hash)
            })
            .collect();
        found.sort();
        found
    };
    let initramfs = |root: &Path| -> Vec<(String, String)> {
        let mut found: Vec<(String, String)> = std::iter::once(root.join("etc/mkinitcpio.conf"))
            .chain(
                fs::read_dir(root.join("etc/mkinitcpio.conf.d"))
                    .into_iter()
                    .flatten()
                    .flatten()
                    .map(|file| file.path()),
            )
            .filter_map(|path| {
                Some((
                    path.file_name()?.to_string_lossy().into_owned(),
                    fs::read_to_string(&path).ok()?,
                ))
            })
            .collect();
        found.sort();
        found
    };
    let what = match (
        kernels(current) == kernels(snapshot),
        initramfs(current) == initramfs(snapshot),
    ) {
        (true, true) => return None,
        (false, _) => "kernel",
        (true, false) => "initramfs",
    };
    Some(format!(
        "Limine boots the kernel and initramfs on the ESP, and the snapshot's {what} differs from the current one's, so it would boot with the wrong one: install the snapshot's kernel first, or switch to systemd-boot"
    ))
}

/// Whether this machine's firmware enforces Secure Boot; never for a test root, which this firmware doesn't boot, unless a debug build fakes one with ARCHIX_TEST_SECUREBOOT_ROOT.
pub fn firmware_enforces() -> bool {
    let faked = cfg!(debug_assertions) && std::env::var_os("ARCHIX_TEST_SECUREBOOT_ROOT").is_some();
    (crate::testhooks::root() == Path::new("/") || faked)
        && super::secureboot::Machine::new()
            .state()
            .is_some_and(|state| state.enabled && !state.setup_mode)
}

/// Checked before a switch or revert changes anything: with Secure Boot enforcing, images archix won't sign could never boot.
pub fn check_signing(plan: &BootPlan) -> Result<()> {
    if plan.loader == Loader::Limine {
        return Ok(());
    }
    let enforcing = firmware_enforces();
    let signed = plan.secure_boot.unwrap_or_else(sbctl_has_keys);
    if enforcing && !signed && super::secureboot::in_chroot() {
        eprintln!(
            "warning: this host's Secure Boot is on and the boot images won't be signed; ignored, since this runs in a chroot"
        );
    } else if enforcing && !signed {
        bail!(
            "Secure Boot is on, but this switch's boot images wouldn't be signed ({}), so the firmware would refuse them; nothing was changed. Run `sudo archix secure-boot setup`, or set boot.secure_boot = true if sbctl already has keys",
            match plan.secure_boot {
                Some(false) => "boot.secure_boot = false",
                _ => "sbctl has no keys",
            }
        );
    }
    Ok(())
}

/// With Secure Boot enforcing, an unsigned image or loader would leave the menu unbootable, so it fails before the menu points at it.
fn refuse_unsigned(plan: &BootPlan, ukis: &[PathBuf]) -> Result<()> {
    if !firmware_enforces() {
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
                "Secure Boot is on, but {} isn't signed, so the firmware would refuse it; the menu still starts the old images. Run `sudo archix secure-boot setup` (or set boot.secure_boot = true if sbctl already has keys)",
                path.file_name().unwrap_or_default().to_string_lossy()
            );
        }
    }
    Ok(())
}

/// The archix entry systemd-boot started this boot from (LoaderEntrySelected), if archix wrote it as it is.
pub fn booted() -> Option<super::menu::Entry> {
    let entry = crate::testhooks::selected_entry()?;
    let parsed = super::menu::Entry::parse(&entry_id(&entry))?;
    entry_unchanged(&Paths::system(), &entry).then_some(parsed)
}

/// The entry systemd-boot started when it has an archix entry's name but isn't what archix wrote there.
pub fn tampered_selected_entry() -> Option<String> {
    let entry = crate::testhooks::selected_entry()?;
    super::menu::Entry::parse(&entry_id(&entry))?;
    (!entry_unchanged(&Paths::system(), &entry)).then_some(entry)
}

/// The entry file with id `id` on the ESP, whatever its counters.
pub fn entry_file(esp: &Path, id: &str) -> Option<PathBuf> {
    fs::read_dir(entries_dir(esp))
        .into_iter()
        .flatten()
        .flatten()
        .find(|file| {
            let name = file.file_name().to_string_lossy().into_owned();
            name.ends_with(".conf") && entry_id(&name) == entry_id(id)
        })
        .map(|file| file.path())
}

/// Leaves the entry `id` no tries, as `systemd-bless-boot bad` does, so systemd-boot passes over it at the next boot; false when it has no counters.
pub fn mark_bad(esp: &Path, id: &str) -> Result<bool> {
    let Some(path) = entry_file(esp, id) else {
        return Ok(false);
    };
    let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
    let Some((stem, counters)) = name.strip_suffix(".conf").and_then(|n| n.split_once('+')) else {
        return Ok(false);
    };
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
        crate::durable::sync_esp(esp)?;
    }
    Ok(true)
}

/// Adds `note` to the login notes once, which the next switch clears, and prints it.
pub fn add_login_note(paths: &Paths, note: &str) {
    println!("{note}");
    let mut notes = fs::read_to_string(paths.notes()).unwrap_or_default();
    if notes.lines().any(|line| line == note) {
        return;
    }
    notes.push_str(note);
    notes.push('\n');
    if let Err(e) = crate::durable::write(&paths.notes(), notes.as_bytes()) {
        println!("archix: couldn't add that to the login notes: {e:#}");
    }
}

/// archix's copy of the boot entry systemd-boot reports as `id`.
pub fn entry_record(paths: &Paths, id: &str) -> PathBuf {
    super::menu::record_path(&super::menu::dir(paths), id)
}

/// The UKI path an entry's text, or archix's record of it, names in its `efi` or `uki` line.
pub fn recorded_uki(text: &str) -> Option<&str> {
    text.lines()
        .find_map(|line| match line.split_once(char::is_whitespace) {
            Some(("efi" | "uki", path)) => Some(path.trim()),
            _ => None,
        })
}

/// The id systemd-boot reports for an entry file: its name without the boot counters.
pub fn entry_id(name: &str) -> String {
    match name.split_once('+') {
        Some((stem, _)) => format!("{stem}.conf"),
        None => name.to_string(),
    }
}

/// Whether the entry systemd-boot reports by `id` is still what archix recorded writing.
fn entry_unchanged(paths: &Paths, id: &str) -> bool {
    let Ok(record) = fs::read(entry_record(paths, id)) else {
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
        .find(|file| {
            let name = file.file_name().to_string_lossy().into_owned();
            name.ends_with(".conf") && super::menu::Entry::parse(&name).is_some() && entry_id(&name) == entry_id(id)
        })
        .is_some_and(|file| fs::read(file.path()).is_ok_and(|text| text == record))
}

/// systemd-boot's LoaderEntrySelected: 4 bytes of attributes, then the entry's id in UTF-16LE.
pub fn read_selected_entry() -> Option<String> {
    efivar_string(&fs::read(SELECTED_ENTRY_VAR).ok()?)
}

/// The entry file systemd-boot is counting this boot's try on, from LoaderBootCountPath; None on an uncounted boot.
pub fn boot_count_file() -> Option<String> {
    let path = efivar_string(&fs::read(BOOT_COUNT_PATH_VAR).ok()?)?;
    path.rsplit(['\\', '/']).next().map(str::to_string)
}

const BOOT_COUNT_PATH_VAR: &str = "/sys/firmware/efi/efivars/LoaderBootCountPath-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f";

pub const SELECTED_ENTRY_VAR: &str =
    "/sys/firmware/efi/efivars/LoaderEntrySelected-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f";

/// systemd-boot's LoaderFeatures, a bit set of what the loader that started this boot supports.
pub fn read_loader_features() -> u64 {
    const VAR: &str = "/sys/firmware/efi/efivars/LoaderFeatures-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f";
    fs::read(VAR).ok().and_then(|bytes| efivar_u64(&bytes)).unwrap_or(0)
}

/// Whether the loader counts boots of an entry archix can write (systemd-boot counts only `linux` and `uki` entries, 258 added `uki`) and falls back past a bad one (260's `preferred`).
pub fn loader_counts_boots(features: u64) -> bool {
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

    #[test]
    fn counted_images_reboot_after_a_panic_unless_the_command_line_says_otherwise() {
        assert_eq!(with_panic_reboot("root=UUID=x rw", 2), "root=UUID=x rw panic=10");
        assert_eq!(with_panic_reboot("root=UUID=x panic=0 rw", 2), "root=UUID=x panic=0 rw");
        assert_eq!(with_panic_reboot("root=UUID=x rw", 0), "root=UUID=x rw");
        assert_eq!(with_panic_reboot("", 2), "panic=10");
    }
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
    fn on_limine_a_keep_is_refused_when_the_snapshots_kernel_or_initramfs_differs() {
        let (current, snapshot) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        for root in [current.path(), snapshot.path()] {
            fs::create_dir_all(root.join("usr/lib/modules/7.1.0-arch1-1")).unwrap();
            fs::write(root.join("usr/lib/modules/7.1.0-arch1-1/vmlinuz"), "kernel").unwrap();
            fs::create_dir_all(root.join("etc")).unwrap();
            fs::write(root.join("etc/mkinitcpio.conf"), "HOOKS=(base)\n").unwrap();
        }
        assert_eq!(limine_keep_refusal(current.path(), snapshot.path()), None);
        fs::create_dir_all(snapshot.path().join("etc/mkinitcpio.conf.d")).unwrap();
        fs::write(snapshot.path().join("etc/mkinitcpio.conf.d/x.conf"), "FILES=(/x)\n").unwrap();
        let why = limine_keep_refusal(current.path(), snapshot.path()).unwrap();
        assert!(
            why.contains("initramfs differs") && why.contains("switch to systemd-boot"),
            "{why}"
        );
        fs::write(snapshot.path().join("usr/lib/modules/7.1.0-arch1-1/vmlinuz"), "older").unwrap();
        let why = limine_keep_refusal(current.path(), snapshot.path()).unwrap();
        assert!(
            why.contains("kernel differs") && why.contains("install the snapshot's kernel first"),
            "{why}"
        );
    }

    #[test]
    fn signing_turns_the_menu_editor_off_and_keeps_the_rest_of_loader_conf() {
        let esp = esp_with(
            &["archix-2.conf"],
            "timeout 2\neditor yes\ndefault archix-2.conf\neditor  1\n",
        );
        let loader = || fs::read_to_string(esp.path().join("loader/loader.conf")).unwrap();
        lock_editor(esp.path()).unwrap();
        assert_eq!(loader(), "timeout 2\ndefault archix-2.conf\neditor no\n");
        point_loader(esp.path(), "archix-*", "archix-current.conf", false).unwrap();
        assert_eq!(
            loader(),
            "timeout 2\neditor no\ndefault archix-current.conf\npreferred archix-*\n",
            "the line outlives a new default"
        );
        point_loader(esp.path(), "archix-*", "archix-current.conf", true).unwrap();
        assert_eq!(
            loader(),
            "timeout 2\neditor no\ndefault archix-current.conf\npreferred archix-*\n",
            "an editor already off stays where it is"
        );
        let bare = tempfile::tempdir().unwrap();
        lock_editor(bare.path()).unwrap();
        assert_eq!(
            fs::read_to_string(bare.path().join("loader/loader.conf")).unwrap(),
            "editor no\n"
        );
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
                recorded_uki(&fs::read_to_string(&entry).unwrap()),
                Some("/EFI/archix/a.efi"),
                "{key}"
            );
        }
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
    fn a_failed_health_check_leaves_the_counted_entry_no_tries() {
        let esp = esp_with(
            &[
                "archix-snapshot-4.conf",
                "archix-current+2-1.conf",
                "archix-current-protected.conf",
                "archix-snapshot-6+1.conf",
            ],
            "",
        );
        assert!(mark_bad(esp.path(), "archix-current.conf").unwrap());
        assert!(mark_bad(esp.path(), "archix-snapshot-6.conf").unwrap());
        assert!(
            !mark_bad(esp.path(), "archix-snapshot-4.conf").unwrap(),
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
                "archix-current+0-3.conf",
                "archix-current-protected.conf",
                "archix-snapshot-4.conf",
                "archix-snapshot-6+0-1.conf"
            ]
        );
        assert!(is_bad("archix-current+0-3.conf") && !is_bad("archix-current+1-1.conf"));
    }

    #[test]
    fn the_esp_mounts_on_access_and_unmounts_when_idle() {
        let fstab = "# /dev/vda2\nUUID=a / ext4 rw,relatime 0 1\n\n# esp\nUUID=b\t/efi/\tvfat\tdefaults\t0 2\n";
        let (once, changed) = esp_fstab(fstab, "/efi", Some("c")).unwrap();
        assert_eq!(
            once,
            "# /dev/vda2\nUUID=a / ext4 rw,relatime 0 1\n\n# esp\nUUID=b /efi/ vfat defaults,x-systemd.automount,x-systemd.idle-timeout=60,fmask=0077,dmask=0077 0 2\n"
        );
        assert_eq!(
            changed,
            EspFstab {
                added: false,
                automount: true,
                fsck: false,
                private: true
            }
        );
        assert_eq!(esp_fstab(&once, "/efi", None), None, "already automounted and fscked");
        assert_eq!(
            esp_fstab("UUID=b /efi vfat noauto,umask=0077 0 2\n", "/efi", None),
            None,
            "noauto is the user's call"
        );
        assert_eq!(
            esp_fstab(fstab, "/boot/efi", None),
            None,
            "no line for that ESP, nor its UUID"
        );
    }

    #[test]
    fn the_esp_is_fscked_at_boot_whatever_its_fstab_line_said() {
        let automounted = "umask=0077,x-systemd.automount,x-systemd.idle-timeout=60";
        for (line, want) in [
            (
                "UUID=b /efi vfat umask=0077 0 0",
                format!("UUID=b /efi vfat {automounted} 0 2"),
            ),
            (
                "UUID=b /efi vfat umask=0077",
                format!("UUID=b /efi vfat {automounted} 0 2"),
            ),
            (
                "UUID=b /efi vfat umask=0077 1",
                format!("UUID=b /efi vfat {automounted} 1 2"),
            ),
            (
                &format!("UUID=b /efi vfat {automounted} 0 0"),
                format!("UUID=b /efi vfat {automounted} 0 2"),
            ),
            (
                "UUID=b /efi vfat noauto,umask=0077 0 0",
                "UUID=b /efi vfat noauto,umask=0077 0 2".to_string(),
            ),
        ] {
            let (text, changed) = esp_fstab(&format!("UUID=a / ext4 rw 0 0\n{line}\n"), "/efi", None).unwrap();
            assert_eq!(text, format!("UUID=a / ext4 rw 0 0\n{want}\n"), "{line}");
            assert!(changed.fsck, "{line}");
            assert_eq!(
                changed.automount,
                !line.contains("automount") && !line.contains("noauto")
            );
        }
        assert_eq!(
            esp_fstab("UUID=b /efi vfat noauto,umask=0077 0 1\n", "/efi", None),
            None,
            "1 is fscked too"
        );
        assert_eq!(
            esp_fstab("UUID=a / ext4 rw 0 1\n", "/efi", Some("AB12-CD34")),
            Some((
                "UUID=a / ext4 rw 0 1\nUUID=AB12-CD34 /efi vfat umask=0077,x-systemd.automount,x-systemd.idle-timeout=60 0 2\n".to_string(),
                EspFstab {
                    added: true,
                    automount: false,
                    fsck: false,
                    private: false
                }
            )),
            "with no line, systemd-gpt-auto-generator would mount the ESP at /boot"
        );
        assert_eq!(
            esp_fstab("# empty\n", "/efi", Some("AB12-CD34")).unwrap().0,
            "# empty\nUUID=AB12-CD34 /efi vfat umask=0077,x-systemd.automount,x-systemd.idle-timeout=60 0 2\n"
        );
        assert_eq!(
            esp_fstab("", "/efi", Some("AB12-CD34")).unwrap().0,
            "UUID=AB12-CD34 /efi vfat umask=0077,x-systemd.automount,x-systemd.idle-timeout=60 0 2\n"
        );
    }

    #[test]
    fn the_esp_is_readable_by_root_only_unless_its_line_sets_a_mask() {
        let (text, changed) =
            esp_fstab("UUID=b /efi vfat rw,relatime,x-systemd.automount 0 2\n", "/efi", None).unwrap();
        assert_eq!(
            text,
            "UUID=b /efi vfat rw,relatime,x-systemd.automount,fmask=0077,dmask=0077 0 2\n"
        );
        assert!(changed.private && !changed.automount && !changed.fsck);
        for options in ["umask=0022", "rw,fmask=0022,dmask=0022,codepage=437", "dmask=0077"] {
            let line = format!("UUID=b /efi vfat {options},x-systemd.automount 0 2\n");
            assert_eq!(esp_fstab(&line, "/efi", None), None, "{options} is the user's choice");
        }
    }

    #[test]
    fn an_entry_is_trusted_only_while_it_matches_its_record() {
        let esp = esp_with(&["archix-snapshot-3+1-2.conf"], "");
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::under(root.path());
        let id = "archix-snapshot-3.conf";
        fs::create_dir_all(entry_record(&paths, id).parent().unwrap()).unwrap();
        fs::write(paths.state.join("boot/esp"), esp.path().to_str().unwrap()).unwrap();
        assert!(!entry_unchanged(&paths, id), "no record");
        fs::write(entry_record(&paths, id), "title t\n").unwrap();
        assert!(entry_unchanged(&paths, id));
        fs::write(
            entries_dir(esp.path()).join("archix-snapshot-3+1-2.conf"),
            "title evil\n",
        )
        .unwrap();
        assert!(!entry_unchanged(&paths, id), "changed on the ESP");
        fs::remove_file(entries_dir(esp.path()).join("archix-snapshot-3+1-2.conf")).unwrap();
        fs::write(
            entries_dir(esp.path()).join("archix-snapshot-3+1-2.conf.tmp"),
            "title t\n",
        )
        .unwrap();
        assert!(
            !entry_unchanged(&paths, id),
            "a temporary file a power cut left isn't the entry"
        );
    }
}
