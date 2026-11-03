use std::fs;
use std::io::{self, BufRead, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};

use archix::paths::Paths;
use archix::system::{boot, password, rescue, rescue_auth};

/// Where the rescue boot mounts the root, and the ESP when the root can't say where it goes.
const ROOT_MOUNT: &str = "/run/archix-rescue/root";
/// Where `rollback` mounts the root with its layout and binds, as it boots.
const LAYOUT_MOUNT: &str = "/run/archix-rescue/layout";
const ESP_MOUNT: &str = "/run/archix-rescue/esp";
/// Where the login mounts the root read-only to read its /etc/shadow.
const LOGIN_MOUNT: &str = "/run/archix-rescue/login";
/// The name an encrypted root opens as when the kernel command line names none.
const LUKS_NAME: &str = "rescue-root";

#[derive(Args, Debug, Default, Clone)]
pub struct Options {
    /// The root filesystem's device, or UUID=, PARTUUID=, LABEL=; by default root= from the kernel command line
    #[arg(long, value_name = "DEVICE", global = true)]
    pub root: Option<String>,
    /// Mount options for the root, as rootflags= gives them (subvol=@ for a btrfs root in a subvolume)
    #[arg(long, value_name = "OPTIONS", global = true)]
    pub rootflags: Option<String>,
    /// The ESP's device, or PARTUUID=; by default the partition systemd-boot started from
    #[arg(long, value_name = "DEVICE", global = true)]
    pub esp: Option<String>,
    /// A root already mounted here, as from a live USB, instead of the one rescue mounts
    #[arg(long, value_name = "DIR", global = true)]
    pub root_dir: Option<PathBuf>,
    /// With --root-dir, where its ESP is mounted if not where boot.esp says under it
    #[arg(long, value_name = "DIR", global = true)]
    pub esp_dir: Option<PathBuf>,
}

#[derive(Subcommand, Debug, Clone)]
pub enum Action {
    /// Check and repair the root filesystem and the ESP with fsck; unmounts them first
    Check {
        /// On btrfs, repair with `btrfs check --repair`, which can make some damage worse; by default btrfs is only checked
        #[arg(long)]
        repair_btrfs: bool,
    },
    /// Set USER's password in the root's /etc, read twice from the console
    Password { user: String },
    /// Open a LUKS volume with its passphrase as /dev/mapper/NAME; never with a TPM, FIDO2 or PKCS#11 token
    Open {
        device: String,
        #[arg(default_value = "rescue-data")]
        name: String,
    },
    /// Open a shell; `exit` comes back to the prompt
    Shell,
    /// Roll back to snapshot N: `archix rollback N` on the root with its layout mounted; only with rollbacks on btrfs
    Rollback { number: u32 },
    /// Unmount everything and restart
    Reboot,
    /// Unmount everything and power off
    Poweroff,
}

/// One line typed at the rescue prompt.
#[derive(Parser)]
#[command(name = "rescue>", no_binary_name = true, disable_help_subcommand = true)]
struct Line {
    #[command(flatten)]
    options: Options,
    #[command(subcommand)]
    action: Option<Action>,
}

const HELP: &str = "\
  check [--repair-btrfs]       check and repair the root filesystem and the ESP
  password USER                set USER's password in the root's /etc
  open DEVICE [NAME]           open a LUKS volume with its passphrase as /dev/mapper/NAME (never with a TPM)
  shell                        a shell; the root is under /run/archix-rescue/root
  reboot, poweroff
  Before a command: --root DEVICE [--rootflags OPTIONS] for another root, such as /dev/mapper/NAME after `open`";

/// Whether this runs in an initramfs: mkinitcpio's has /init and its hooks, systemd's /etc/initrd-release.
fn in_initrd() -> bool {
    Path::new("/etc/initrd-release").exists() || (Path::new("/init").is_file() && Path::new("/hooks").is_dir())
}

/// `key=`'s value on the kernel command line.
fn cmdline_value(key: &str) -> Option<String> {
    let cmdline = archix::testhooks::kernel_cmdline();
    cmdline
        .split_whitespace()
        .filter_map(|word| word.strip_prefix(key)?.strip_prefix('='))
        .next_back()
        .map(str::to_string)
}

fn mount_points() -> Vec<PathBuf> {
    archix::conflicts::mount_points()
}

fn is_mount_point(path: &Path) -> bool {
    mount_points().iter().any(|point| point == path)
}

/// The device mounted at `dir`, from the mount table.
fn mount_source(dir: &Path) -> Option<String> {
    let text = fs::read_to_string("/proc/self/mountinfo").ok()?;
    text.lines().find_map(|line| {
        let (before, after) = line.split_once(" - ")?;
        let point = before.split_whitespace().nth(4)?;
        (Path::new(&point.replace("\\040", " ")) == dir).then(|| after.split_whitespace().nth(1).map(str::to_string))?
    })
}

/// Where `device` is mounted, by device number or by the name the mount table gives it.
fn mounted_at(device: &Path) -> Option<PathBuf> {
    let rdev = fs::metadata(device).ok()?.rdev();
    let canonical = fs::canonicalize(device).ok()?;
    let text = fs::read_to_string("/proc/self/mountinfo").ok()?;
    text.lines().find_map(|line| {
        let (before, after) = line.split_once(" - ")?;
        let fields: Vec<&str> = before.split_whitespace().collect();
        let (major, minor) = fields.get(2)?.split_once(':')?;
        let same_number = libc::makedev(major.parse().ok()?, minor.parse().ok()?) == rdev;
        let same_name = after
            .split_whitespace()
            .nth(1)
            .and_then(|source| fs::canonicalize(source).ok())
            .is_some_and(|source| source == canonical);
        (same_number || same_name).then(|| PathBuf::from(fields.get(4).copied().unwrap_or("?")))
    })
}

/// The path a spec names: a path, or UUID=, PARTUUID=, LABEL= and PARTLABEL= through udev's /dev/disk links.
fn spec_path(spec: &str) -> PathBuf {
    match spec.split_once('=') {
        Some(("UUID", value)) => Path::new("/dev/disk/by-uuid").join(value),
        Some(("PARTUUID", value)) => Path::new("/dev/disk/by-partuuid").join(value.to_lowercase()),
        Some(("LABEL", value)) => Path::new("/dev/disk/by-label").join(value),
        Some(("PARTLABEL", value)) => Path::new("/dev/disk/by-partlabel").join(value),
        _ => PathBuf::from(spec),
    }
}

/// The block device a spec names, once udev has made it, looking `tries` times 200 ms apart.
fn find(spec: &str, tries: u32) -> Option<PathBuf> {
    let path = spec_path(spec);
    for _ in 0..tries {
        if path.exists() {
            return Some(path);
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    None
}

/// The block device a spec names.
fn resolve(spec: &str) -> Result<PathBuf> {
    if let Some(path) = find(spec, 50) {
        return Ok(path);
    }
    let encrypted = ["rd.luks", "cryptdevice=", "luks"]
        .iter()
        .any(|word| archix::testhooks::kernel_cmdline().contains(word));
    let hint = match encrypted {
        true => {
            "; it's on an encrypted disk, so open it first: `open /dev/<partition> root`, then `--root /dev/mapper/root list`"
        }
        false => "; `shell` and `blkid` list the disks, then `--root DEVICE list` uses one",
    };
    bail!("there's no device {spec} ({}){hint}", spec_path(spec).display())
}

/// Opens the LUKS volume on `device` as /dev/mapper/`name` with a passphrase typed at the console, unless it's open already.
fn open_luks(device: &Path, name: &str) -> Result<PathBuf> {
    let mapped = Path::new("/dev/mapper").join(name);
    if mapped.exists() {
        return Ok(mapped);
    }
    println!(
        "{} is encrypted: its passphrase opens it here, never a TPM, FIDO2 or PKCS#11 token",
        device.display()
    );
    let argv = rescue_auth::open_argv(&device.to_string_lossy(), name);
    let status = Command::new("cryptsetup")
        .args(&argv)
        .status()
        .context("running cryptsetup; is it in this rescue image?")?;
    if !status.success() {
        bail!("cryptsetup didn't open {} ({status})", device.display());
    }
    Ok(mapped)
}

/// The LUKS volumes on this machine, from blkid.
fn luks_devices() -> Vec<PathBuf> {
    let output = Command::new("blkid")
        .args(["-t", "TYPE=crypto_LUKS", "-o", "device"])
        .output();
    output
        .map(|out| {
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(PathBuf::from)
                .collect()
        })
        .unwrap_or_default()
}

fn fs_type(device: &Path) -> Result<String> {
    let output = Command::new("blkid")
        .args(["-p", "-o", "value", "-s", "TYPE"])
        .arg(device)
        .output()
        .context("running blkid")?;
    let fs = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if fs.is_empty() {
        bail!("{} holds no filesystem blkid recognizes", device.display());
    }
    Ok(fs)
}

/// Whether the current generation of the root mounted at `root` has rollbacks on; a root no switch recorded counts as on.
fn rollbacks_on(root: &Path) -> bool {
    let paths = Paths {
        state: root.join(archix::snapshot::layout::ON_ROOT).join("archix"),
        ..Paths::under(root)
    };
    let store = archix::generations::Store::new(&paths);
    match store.current() {
        Ok(Some(number)) => store.get(number).is_ok_and(|g| g.settings.rollback.is_some()),
        _ => true,
    }
}

fn run_mount(device: &Path, dir: &Path, fs: &str, options: &str) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let output = Command::new("mount")
        .args(["-t", fs, "-o", options])
        .arg(device)
        .arg(dir)
        .output()
        .context("running mount")?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(())
}

fn unmount(dir: &Path) -> Result<()> {
    if !is_mount_point(dir) {
        return Ok(());
    }
    archix::durable::sync_filesystems(&[dir]);
    let output = Command::new("umount")
        .arg("-R")
        .arg(dir)
        .output()
        .context("running umount")?;
    if !output.status.success() {
        bail!(
            "couldn't unmount {}: {}",
            dir.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Remounts the filesystem at `dir` read-write when it's mounted read-only, as the busybox initramfs mounts the root.
fn writable(dir: &Path) -> Result<()> {
    let c_dir = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes())?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c_dir.as_ptr(), &mut stat) } != 0 || stat.f_flag & libc::ST_RDONLY == 0 {
        return Ok(());
    }
    if unsafe {
        libc::mount(
            std::ptr::null(),
            c_dir.as_ptr(),
            std::ptr::null(),
            libc::MS_REMOUNT,
            std::ptr::null(),
        )
    } != 0
    {
        return Err(io::Error::last_os_error()).with_context(|| format!("remounting {} read-write", dir.display()));
    }
    Ok(())
}

/// What the rescue works on: a root and ESP it mounts itself, or ones mounted already (--root-dir).
pub struct Session {
    options: Options,
}

impl Session {
    pub fn new(options: Options) -> Result<Session> {
        if options.root_dir.is_none() && options.root.is_none() && !in_initrd() {
            bail!(
                "`archix rescue` runs in the rescue boot: pick \"archix rescue\" in the boot menu, or `sudo bootctl set-oneshot {}` and restart. On a running system `archix rollback` does the same; from a live USB, mount the root and pass --root-dir",
                rescue::ENTRY
            );
        }
        if options.root_dir.as_deref() == Some(Path::new("/")) {
            bail!("--root-dir / is the running system; `archix rollback` works on it");
        }
        Ok(Session { options })
    }

    fn merge(&mut self, options: Options) {
        let mine = &mut self.options;
        mine.root = options.root.or(mine.root.take());
        mine.rootflags = options.rootflags.or(mine.rootflags.take());
        mine.esp = options.esp.or(mine.esp.take());
        mine.root_dir = options.root_dir.or(mine.root_dir.take());
        mine.esp_dir = options.esp_dir.or(mine.esp_dir.take());
    }

    fn root_spec(&self) -> Result<String> {
        self.options
            .root
            .clone()
            .or_else(|| cmdline_value("root"))
            .context("the kernel command line has no root=; give the root with --root DEVICE")
    }

    /// The root filesystem's block device; on an encrypted root, the volume the kernel command line names (or each LUKS volume blkid finds) opened with its passphrase first.
    fn root_device(&self) -> Result<PathBuf> {
        let spec = self.root_spec()?;
        if find(&spec, 25).is_none() {
            let named = rescue_auth::luks_on_cmdline(&archix::testhooks::kernel_cmdline());
            let volumes: Vec<(PathBuf, String)> = match named.is_empty() {
                false => named
                    .iter()
                    .filter_map(|(device, name)| Some((find(device, 50)?, name.clone())))
                    .collect(),
                true => {
                    let name = spec.strip_prefix("/dev/mapper/").unwrap_or(LUKS_NAME).to_string();
                    luks_devices()
                        .into_iter()
                        .map(|device| (device, name.clone()))
                        .collect()
                }
            };
            for (device, name) in volumes {
                open_luks(&device, &name)?;
                if find(&spec, 25).is_some() {
                    break;
                }
            }
        }
        let device = resolve(&spec)?;
        if fs_type(&device).is_ok_and(|fs| fs == "crypto_LUKS") {
            return open_luks(&device, LUKS_NAME);
        }
        Ok(device)
    }

    /// The login before the rescue opens: the root's /etc/shadow, read from a read-only mount, must hold a password for root or a wheel user, and one of them must be typed.
    pub fn authenticate(&self) -> Result<()> {
        let mounted = match &self.options.root_dir {
            Some(dir) => dir.clone(),
            None => {
                let device = self.root_device()?;
                let dir = PathBuf::from(LOGIN_MOUNT);
                if !is_mount_point(&dir) {
                    let fs = fs_type(&device)?;
                    let flags = self.options.rootflags.clone().or_else(|| cmdline_value("rootflags"));
                    let with = |options: &str| flags.as_ref().map_or(options.to_string(), |f| format!("{options},{f}"));
                    let fallback = match fs.as_str() {
                        "btrfs" => "ro,rescue=all",
                        "ext2" | "ext3" | "ext4" => "ro,noload",
                        _ => "ro",
                    };
                    run_mount(&device, &dir, &fs, &with("ro"))
                        .or_else(|_| run_mount(&device, &dir, &fs, &with(fallback)))
                        .with_context(|| {
                            format!(
                                "the root on {} doesn't mount even read-only, so there's no /etc/shadow to check a login against; recover from a live USB: {}",
                                device.display(),
                                rescue_auth::GUIDE
                            )
                        })?;
                }
                dir
            }
        };
        let found = rescue_auth::accounts(&mounted.join("etc"));
        if self.options.root_dir.is_none() {
            let _ = unmount(Path::new(LOGIN_MOUNT));
        }
        let accounts = found.map_err(|e| {
            e.context(format!(
                "the rescue stays closed; recover from a live USB: {}",
                rescue_auth::GUIDE
            ))
        })?;
        if accounts.is_empty() {
            bail!("{}", rescue_auth::refusal());
        }
        let user = rescue_auth::login(
            &accounts,
            &mut |prompt, secret| read_answer(prompt, secret),
            &mut std::thread::sleep,
            &mut |line| println!("{line}"),
        )?;
        println!("archix rescue: logged in as {user}");
        Ok(())
    }

    /// The archix root, mounted read-write.
    fn root(&self) -> Result<PathBuf> {
        let dir = match &self.options.root_dir {
            Some(dir) => {
                writable(dir)?;
                dir.clone()
            }
            None => {
                let dir = PathBuf::from(ROOT_MOUNT);
                if !is_mount_point(&dir) {
                    let device = self.root_device()?;
                    if let Some(at) = mounted_at(&device) {
                        bail!(
                            "{} is mounted at {} already; unmount it first",
                            device.display(),
                            at.display()
                        );
                    }
                    let fs = fs_type(&device)?;
                    let flags = self.options.rootflags.clone().or_else(|| cmdline_value("rootflags"));
                    let options = flags.map_or("rw".to_string(), |flags| format!("rw,{flags}"));
                    run_mount(&device, &dir, &fs, &options).with_context(|| {
                        format!(
                            "the root filesystem on {} ({fs}) doesn't mount; `check` checks and repairs it",
                            device.display()
                        )
                    })?;
                }
                dir
            }
        };
        if !dir.join("etc/passwd").is_file() {
            bail!(
                "{} has no /etc/passwd, so it isn't a root (on btrfs, is the subvolume in --rootflags, such as subvol=@?)",
                dir.display()
            );
        }
        Ok(dir)
    }

    /// Where the ESP goes: under the root, if that's mounted, where boot.esp says, else a directory of its own.
    fn esp_target(&self) -> PathBuf {
        let root = match &self.options.root_dir {
            Some(dir) => Some(dir.clone()),
            None => is_mount_point(Path::new(ROOT_MOUNT)).then(|| PathBuf::from(ROOT_MOUNT)),
        };
        let under_root = root.and_then(|root| {
            let esp = fs::read_to_string(Paths::under(&root).state.join("boot/esp")).ok()?;
            let at = root.join(esp.trim().trim_start_matches('/'));
            at.is_dir().then_some(at)
        });
        under_root.unwrap_or_else(|| PathBuf::from(ESP_MOUNT))
    }

    fn esp_device(&self) -> Result<PathBuf> {
        if let Some(spec) = &self.options.esp {
            return resolve(spec);
        }
        boot::mount_efivars();
        match boot::loader_partition() {
            Some(uuid) => resolve(&format!("PARTUUID={uuid}")),
            None => bail!("systemd-boot didn't say which partition it started from; give the ESP with --esp DEVICE"),
        }
    }

    /// Unmounts what the rescue mounted, the ESP first.
    fn unmount_all(&self) -> Result<()> {
        if self.options.root_dir.is_some() {
            return Ok(());
        }
        let target = self.esp_target();
        unmount(&target)?;
        unmount(Path::new(ESP_MOUNT))?;
        unmount(Path::new(ROOT_MOUNT))
    }

    pub fn run(&mut self, action: Action) -> Result<()> {
        match action {
            Action::Check { repair_btrfs } => self.check(repair_btrfs),
            Action::Password { user } => self.password(&user),
            Action::Open { device, name } => {
                let mapped = open_luks(&resolve(&device)?, &name)?;
                println!("opened {device} as {}", mapped.display());
                Ok(())
            }
            Action::Shell => self.shell(),
            Action::Rollback { number } => self.rollback(number),
            Action::Reboot => self.power(libc::RB_AUTOBOOT),
            Action::Poweroff => self.power(libc::RB_POWER_OFF),
        }
    }

    /// Whether the root has rollbacks: btrfs with @archix, whose snapshots `rollback` can go back to, and not turned off by its current generation's config.
    fn has_rollbacks(&self) -> bool {
        self.root_device()
            .ok()
            .filter(|device| fs_type(device).is_ok_and(|fs| fs == "btrfs"))
            .is_some_and(|_| {
                self.root()
                    .is_ok_and(|root| root.join(archix::snapshot::MOUNT).is_dir() && rollbacks_on(&root))
            })
    }

    fn rollback(&self, number: u32) -> Result<()> {
        if !self.has_rollbacks() {
            bail!(
                "this root has no rollbacks (it isn't btrfs, or rollbacks were never set up), so there's no snapshot to go back to"
            );
        }
        let device = self.root_device()?;
        self.unmount_all()?;
        let target = Path::new(LAYOUT_MOUNT);
        let ran = archix::snapshot::layout::mount_all(&device.to_string_lossy(), target).and_then(|()| {
            for (what, fs) in [("proc", "proc"), ("sys", "sysfs"), ("dev", "devtmpfs")] {
                run_mount(Path::new(what), &target.join(what), fs, "rw")?;
            }
            // The keep points the boot menu at the kept root's kernel, so it needs the ESP where the root mounts it.
            let esp = fs::read_to_string(target.join("var/lib/archix/boot/esp"));
            if let (Ok(esp), Ok(device)) = (esp, self.esp_device()) {
                run_mount(&device, &target.join(esp.trim().trim_start_matches('/')), "vfat", "rw")
                    .context("mounting the ESP for the boot menu")?;
            }
            let status = Command::new("chroot")
                .arg(target)
                .args(["/usr/bin/archix", "rollback", &number.to_string()])
                .status()
                .context("running archix rollback in the root")?;
            if !status.success() {
                bail!("archix rollback {number} failed in the root");
            }
            Ok(())
        });
        let _ = Command::new("umount").arg("-R").arg(target).status();
        ran
    }

    fn check(&self, repair_btrfs: bool) -> Result<()> {
        if self.options.root_dir.is_some() {
            bail!("check needs the root and the ESP unmounted; unmount them and pass --root DEVICE and --esp DEVICE");
        }
        let esp_mounted = mount_source(&self.esp_target()).or_else(|| mount_source(Path::new(ESP_MOUNT)));
        self.unmount_all()?;
        let mut failed = Vec::new();
        match self.root_device() {
            Ok(device) => {
                if let Err(e) = fsck(&device, repair_btrfs) {
                    failed.push(format!("the root: {e:#}"));
                }
            }
            Err(e) => failed.push(format!("the root: {e:#}")),
        }
        let esp = match (self.esp_device(), esp_mounted) {
            (Ok(device), _) => Ok(device),
            (Err(_), Some(source)) => Ok(PathBuf::from(source)),
            (Err(e), None) => Err(e),
        };
        match esp {
            Ok(device) => {
                if let Err(e) = fsck(&device, false) {
                    failed.push(format!("the ESP: {e:#}"));
                }
            }
            Err(e) => failed.push(format!("the ESP: {e:#}")),
        }
        if !failed.is_empty() {
            bail!("{}", failed.join("\n"));
        }
        println!("the root and the ESP are clean");
        Ok(())
    }

    fn password(&self, user: &str) -> Result<()> {
        let root = self.root()?;
        let first = read_password(&format!("new password for {user}: "))?;
        if first.is_empty() {
            bail!("the password is empty; nothing was changed");
        }
        if read_password("again: ")? != first {
            bail!("the passwords differ; nothing was changed");
        }
        let hash = password::sha512_crypt(first.as_bytes(), &password::salt()?);
        password::set(&root.join("etc"), user, &hash).with_context(|| format!("setting {user}'s password"))?;
        println!("set {user}'s password");
        let paths = Paths::under(&root);
        let records = archix::generations::Store::new(&paths);
        let generation = records.current()?.and_then(|current| records.get(current).ok());
        let declared = generation
            .as_ref()
            .and_then(|generation| generation.system.users.get(user)?.hashed_password.clone());
        let secret = generation.as_ref().and_then(|generation| {
            let name = generation.system.users.get(user)?.hashed_password_secret.clone()?;
            let entry = generation.system.secrets.as_ref()?.secrets.get(&name)?.clone();
            Some((name, entry))
        });
        if let Some((name, entry)) = &secret {
            archix::system::credentials::record_secret_override(
                &paths.state,
                user,
                &archix::system::credentials::secret_mark(entry.hmac.as_deref(), &entry.sha256),
            )?;
            let note = format!(
                "archix: {user}'s password was set in the rescue entry; boots keep it over the crypt_hash secret {name} until a switch changes that secret"
            );
            let mut notes = fs::read_to_string(paths.notes()).unwrap_or_default();
            notes.push_str(&note);
            notes.push('\n');
            archix::durable::write(&paths.notes(), notes.as_bytes())?;
            println!("{note}");
        } else if let Some(declared) = declared {
            archix::system::credentials::record_override(&paths.state, user, Some(&declared))?;
            let note = format!(
                "archix: {user}'s password was set in the rescue entry; switches keep it over users.{user}.hashed_password until that hash changes in the config"
            );
            let mut notes = fs::read_to_string(paths.notes()).unwrap_or_default();
            notes.push_str(&note);
            notes.push('\n');
            archix::durable::write(&paths.notes(), notes.as_bytes())?;
            println!("{note}");
        }
        Ok(())
    }

    fn shell(&self) -> Result<()> {
        let dir = match self.options.root_dir.clone() {
            Some(dir) => dir,
            None if is_mount_point(Path::new(ROOT_MOUNT)) => PathBuf::from(ROOT_MOUNT),
            None => PathBuf::from("/"),
        };
        println!("a shell in {}; `exit` comes back to the rescue prompt", dir.display());
        Command::new("sh")
            .arg("-i")
            .env("PS1", "[rescue \\w]# ")
            .current_dir(&dir)
            .status()
            .context("running sh")?;
        Ok(())
    }

    fn power(&self, how: libc::c_int) -> Result<()> {
        if !in_initrd() {
            bail!("reboot and poweroff are for the rescue boot");
        }
        self.unmount_all()?;
        unsafe { libc::reboot(how) };
        Err(io::Error::last_os_error()).context("restarting")
    }
}

/// Checks and repairs the filesystem on `device`, which must not be mounted.
fn fsck(device: &Path, repair_btrfs: bool) -> Result<()> {
    if fs::metadata(device).is_ok_and(|meta| !meta.file_type().is_block_device()) {
        bail!("{} isn't a block device", device.display());
    }
    if let Some(at) = mounted_at(device) {
        bail!(
            "{} is mounted at {}, where fsck would damage it",
            device.display(),
            at.display()
        );
    }
    let fs = fs_type(device)?;
    let (program, args, fine): (&str, Vec<&str>, fn(i32) -> bool) = match fs.as_str() {
        "ext2" | "ext3" | "ext4" => ("e2fsck", vec!["-f", "-y"], |code| code & !3 == 0),
        "vfat" => ("fsck.fat", vec!["-a", "-w"], |code| code <= 1),
        "btrfs" if repair_btrfs => ("btrfs", vec!["check", "--repair"], |code| code == 0),
        "btrfs" => ("btrfs", vec!["check", "--readonly"], |code| code == 0),
        other => bail!("archix doesn't check {other} filesystems; `shell` has the tools there are"),
    };
    println!(":: {program} {} {}", args.join(" "), device.display());
    let status = Command::new(program)
        .args(&args)
        .arg(device)
        .status()
        .with_context(|| format!("running {program}; is it in this rescue image?"))?;
    let code = status.code().unwrap_or(-1);
    if !fine(code) {
        let more = match fs.as_str() {
            "btrfs" if !repair_btrfs => {
                "; `check --repair-btrfs` tries to repair it, which can make some damage worse, so copy what matters off first (`shell`, `mount -o ro,rescue=all`)"
            }
            _ => "",
        };
        bail!(
            "{program} found errors it didn't fix on {} (exit {code}){more}",
            device.display()
        );
    }
    println!(
        "{} ({fs}) is clean{}",
        device.display(),
        if code == 0 { "" } else { " now" }
    );
    Ok(())
}

/// Reads a line from the console without echoing it when that's a terminal.
fn read_password(prompt: &str) -> Result<String> {
    Ok(read_answer(prompt, true)?.unwrap_or_default())
}

/// Reads a line from the console, without echoing it when `secret` and that's a terminal; None at the end of input.
fn read_answer(prompt: &str, secret: bool) -> Result<Option<String>> {
    print!("{prompt}");
    io::stdout().flush()?;
    let fd = libc::STDIN_FILENO;
    let mut saved: libc::termios = unsafe { std::mem::zeroed() };
    let tty = secret && unsafe { libc::tcgetattr(fd, &mut saved) } == 0;
    if tty {
        let mut quiet = saved;
        quiet.c_lflag &= !libc::ECHO;
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &quiet) };
    }
    let mut line = String::new();
    let read = io::stdin().lock().read_line(&mut line);
    if tty {
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &saved) };
        println!();
    }
    if read.context("reading from the console")? == 0 {
        return Ok(None);
    }
    Ok(Some(line.trim_end_matches(['\n', '\r']).to_string()))
}

extern "C" fn ignore(_: libc::c_int) {}

/// `archix rescue`: one command, or with none the rescue prompt.
pub fn run(options: Options, action: Option<Action>) -> Result<()> {
    let mut session = Session::new(options)?;
    if in_initrd() || archix::testhooks::rescue_login() {
        // Handlers, not SIG_IGN, which children would keep: Ctrl-C, Ctrl-\ and Ctrl-Z neither end nor stop the login.
        for signal in [libc::SIGINT, libc::SIGQUIT, libc::SIGTSTP] {
            unsafe { libc::signal(signal, ignore as *const () as libc::sighandler_t) };
        }
        session.authenticate()?;
    }
    match action {
        Some(action) => session.run(action),
        None => prompt(&mut session),
    }
}

fn prompt(session: &mut Session) -> Result<()> {
    // A handler, not SIG_IGN, which children would keep: Ctrl-C stops what runs, not the prompt.
    unsafe { libc::signal(libc::SIGINT, ignore as *const () as libc::sighandler_t) };
    println!("\narchix rescue: this boot doesn't mount the root. Commands:\n{HELP}\n");
    if session.has_rollbacks() {
        println!(
            "  rollback N                   roll back to snapshot N from the next boot on (`archix rollback N`)\n"
        );
    }
    let stdin = io::stdin();
    loop {
        print!("rescue> ");
        io::stdout().flush()?;
        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 {
            break;
        }
        let words: Vec<&str> = line.split_whitespace().collect();
        match words.as_slice() {
            [] => continue,
            ["help" | "?"] => {
                println!("{HELP}");
                continue;
            }
            ["exit" | "quit"] => break,
            _ => {}
        }
        match Line::try_parse_from(&words) {
            Err(e) => println!("{e}"),
            Ok(line) => {
                session.merge(line.options);
                match line.action {
                    Some(action) => {
                        if let Err(e) = session.run(action) {
                            println!("error: {e:#}");
                        }
                    }
                    None => println!("options kept for the next commands"),
                }
            }
        }
    }
    session.unmount_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_lines_parse_as_rescue_commands() {
        let line = Line::try_parse_from(["--root", "/dev/mapper/root", "check"]).unwrap();
        assert_eq!(line.options.root.as_deref(), Some("/dev/mapper/root"));
        assert!(matches!(line.action, Some(Action::Check { repair_btrfs: false })));
        let line = Line::try_parse_from(["password", "alice"]).unwrap();
        assert!(matches!(line.action, Some(Action::Password { ref user }) if user == "alice"));
        for gone in ["list", "use", "verify", "restore-etc", "eject"] {
            assert!(Line::try_parse_from([gone]).is_err(), "{gone}");
        }
        assert!(Line::try_parse_from(["frobnicate"]).is_err());
    }

    #[test]
    fn options_typed_at_the_prompt_stay_for_later_commands() {
        let mut session = Session {
            options: Options {
                root: Some("UUID=a".into()),
                ..Options::default()
            },
        };
        session.merge(Options {
            rootflags: Some("subvol=@".into()),
            ..Options::default()
        });
        assert_eq!(session.options.root.as_deref(), Some("UUID=a"));
        assert_eq!(session.options.rootflags.as_deref(), Some("subvol=@"));
        session.merge(Options {
            root: Some("/dev/mapper/root".into()),
            ..Options::default()
        });
        assert_eq!(session.options.root.as_deref(), Some("/dev/mapper/root"));
    }
}
