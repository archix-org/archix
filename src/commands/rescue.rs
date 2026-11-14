use std::cell::OnceCell;
use std::collections::BTreeSet;
use std::fs;
use std::io::{self, BufRead, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};

use archix::paths::Paths;
use archix::snapshot::layout;
use archix::system::{boot, mounts, password, rescue_auth};

/// Where the rescue mounts the root, and the ESP when the root can't say where it goes.
const ROOT_MOUNT: &str = "/run/archix-rescue/root";
/// Where `rollback` mounts the root with its layout and binds, as it boots.
const LAYOUT_MOUNT: &str = "/run/archix-rescue/layout";
const ESP_MOUNT: &str = "/run/archix-rescue/esp";
/// Where the login mounts the root read-only to read its /etc/shadow.
const LOGIN_MOUNT: &str = "/run/archix-rescue/login";
/// Where a filesystem is mounted read-only for a look at what it holds.
const PROBE_MOUNT: &str = "/run/archix-rescue/probe";
/// The name an encrypted root opens as when the kernel command line names none.
const LUKS_NAME: &str = "rescue-root";

#[derive(Args, Debug, Default, Clone)]
pub struct Options {
    /// The root filesystem's device, or UUID=, PARTUUID=, LABEL=; by default root= from the kernel command line in the rescue boot, elsewhere the device mounted at --root-dir, else the only archix root on this machine's disks
    #[arg(long, value_name = "DEVICE", global = true)]
    pub root: Option<String>,
    /// Mount options for the root, as rootflags= gives them; by default rootflags= in the rescue boot, elsewhere subvol=@ on a btrfs that has that subvolume
    #[arg(long, value_name = "OPTIONS", global = true)]
    pub rootflags: Option<String>,
    /// The ESP's device, or PARTUUID=; by default the partition systemd-boot started the rescue boot from, elsewhere the one the root's fstab mounts
    #[arg(long, value_name = "DEVICE", global = true)]
    pub esp: Option<String>,
    /// A root already mounted here, as from a live USB, instead of the one rescue mounts; `rollback` mounts its layout itself from that mount's device
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
    /// Open the rescue environment's shell in the root's directory; `exit` comes back to the prompt
    Shell,
    /// Roll back to snapshot N: mounts the root's layout and keeps the snapshot with this archix alone, running none of the root's programs; only with rollbacks on btrfs
    Rollback {
        number: u32,
        /// Don't ask before keeping the snapshot
        #[arg(long)]
        noconfirm: bool,
    },
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
  shell                        this rescue environment's shell, in the root's directory (/run/archix-rescue/root)
  reboot, poweroff
  Before a command: --root DEVICE [--rootflags OPTIONS] for another root, such as /dev/mapper/NAME after `open`";

/// Whether this runs in an initramfs, as the rescue entry's does: mkinitcpio's has /init and its hooks, systemd's /etc/initrd-release. Only there does the kernel command line describe the root to rescue; a live system's describes the live system.
fn in_initrd() -> bool {
    Path::new("/etc/initrd-release").exists() || (Path::new("/init").is_file() && Path::new("/hooks").is_dir())
}

/// `key=`'s value on a kernel command line.
fn value_on(cmdline: &str, key: &str) -> Option<String> {
    cmdline
        .split_whitespace()
        .filter_map(|word| word.strip_prefix(key)?.strip_prefix('='))
        .next_back()
        .map(str::to_string)
}

/// The command line of the boot that is rescuing its own root, which only the rescue boot is.
fn boot_cmdline() -> Option<String> {
    in_initrd().then(archix::testhooks::kernel_cmdline)
}

fn is_mount_point(path: &Path) -> bool {
    mounts::at(path).is_some()
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

/// Whether two paths name one block device.
fn same_device(a: &Path, b: &Path) -> bool {
    match (fs::metadata(a), fs::metadata(b)) {
        (Ok(a), Ok(b)) => a.file_type().is_block_device() && b.file_type().is_block_device() && a.rdev() == b.rdev(),
        _ => false,
    }
}

/// The block device a spec names, once udev has made it, looking `tries` times 200 ms apart.
fn find(spec: &str, tries: u32) -> Option<PathBuf> {
    let path = layout::device_path(spec);
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
    let encrypted = boot_cmdline().is_some_and(|cmdline| {
        ["rd.luks", "cryptdevice=", "luks"]
            .iter()
            .any(|word| cmdline.contains(word))
    }) || !luks_devices().is_empty();
    let hint = match encrypted {
        true => {
            "; it may be on an encrypted disk, so open that first: `open /dev/<partition> root`, then `--root /dev/mapper/root`"
        }
        false => "; `shell` and `blkid` list the disks, then `--root DEVICE` uses one",
    };
    bail!(
        "there's no device {spec} ({}){hint}",
        layout::device_path(spec).display()
    )
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
        .context("running cryptsetup; is it in this rescue image, or installed on this live system?")?;
    if !status.success() {
        bail!("cryptsetup didn't open {} ({status})", device.display());
    }
    Ok(mapped)
}

/// The LUKS volumes on this machine, by their headers.
fn luks_devices() -> Vec<PathBuf> {
    mounts::block_devices()
        .into_iter()
        .filter(|device| mounts::probe(device).is_some_and(|probe| probe.fs_type == "crypto_LUKS"))
        .collect()
}

/// The filesystem on `device` by its superblock, or as blkid names one archix doesn't know.
fn fs_type(device: &Path) -> Result<String> {
    if let Some(probe) = mounts::probe(device) {
        return Ok(probe.fs_type.to_string());
    }
    let output = Command::new("blkid")
        .args(["-p", "-o", "value", "-s", "TYPE"])
        .arg(device)
        .output();
    let fs = output
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_default();
    if fs.is_empty() {
        bail!("{} holds no filesystem archix or blkid recognizes", device.display());
    }
    Ok(fs)
}

/// Whether the current generation of the root mounted at `root` has rollbacks on; a root no switch recorded counts as on.
fn rollbacks_on(root: &Path) -> bool {
    let paths = Paths::on_root(root);
    let store = archix::generations::Store::new(&paths);
    match store.current() {
        Ok(Some(number)) => store.get(number).is_ok_and(|g| g.settings.rollback.is_some()),
        _ => true,
    }
}

fn run_mount(device: &Path, dir: &Path, fs: &str, options: &str) -> Result<()> {
    mounts::mount(&device.to_string_lossy(), dir, fs, options)
}

fn unmount(dir: &Path) -> Result<()> {
    if !is_mount_point(dir) {
        return Ok(());
    }
    archix::durable::sync_filesystems(&[dir]);
    mounts::unmount_under(dir).with_context(|| format!("couldn't unmount {}", dir.display()))
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

/// What `look` sees on the filesystem on `device`, mounted read-only for it (a btrfs at its top level) without replaying a journal.
fn peek<T>(device: &Path, fs: &str, look: impl FnOnce(&Path) -> T) -> Option<T> {
    let options = match fs {
        "btrfs" => "ro,subvolid=5",
        "ext4" => "ro,noload",
        _ => "ro",
    };
    let dir = Path::new(PROBE_MOUNT);
    run_mount(device, dir, fs, options).ok()?;
    let seen = look(dir);
    let _ = mounts::unmount_under(dir);
    Some(seen)
}

/// Whether the filesystem whose top is at `top` holds a root archix manages: its state, or the rollback layout's subvolumes.
fn holds_archix(top: &Path) -> bool {
    let root_in = |root: PathBuf| {
        root.join("var/lib/archix/generations").is_dir() || root.join(layout::ON_ROOT).join("archix").is_dir()
    };
    top.join(layout::ARCHIX).join("snapshots").is_dir()
        || root_in(top.to_path_buf())
        || root_in(top.join(archix::snapshot::ROOT))
}

/// What this machine's disks hold that the rescue could work on.
#[derive(Debug, Default, PartialEq)]
struct Disks {
    /// The filesystems holding an archix root, each by one of its devices.
    roots: Vec<PathBuf>,
    /// LUKS volumes nobody opened, which could hold one.
    locked: Vec<PathBuf>,
    /// Whether the running system's own root is one.
    running: bool,
}

impl Disks {
    /// Looks at every btrfs and ext4 filesystem among the block devices, read-only.
    fn scan() -> Disks {
        let running = mounts::of(Path::new("/")).map(|mount| PathBuf::from(mount.source));
        let probed: Vec<(PathBuf, mounts::Probe)> = mounts::block_devices()
            .into_iter()
            .filter(|device| {
                let name = device.file_name().unwrap_or_default().to_string_lossy();
                !["fd", "sr", "ram", "zram"]
                    .iter()
                    .any(|skipped| name.starts_with(skipped))
            })
            .filter_map(|device| Some((device.clone(), mounts::probe(&device)?)))
            .collect();
        for (device, probe) in &probed {
            if probe.fs_type == "btrfs" {
                archix::system::btrfs::scan_device(device);
            }
        }
        let mut disks = Disks::default();
        let mut seen = BTreeSet::new();
        for (device, probe) in probed {
            let opened = fs::read_dir(
                Path::new("/sys/class/block")
                    .join(device.file_name().unwrap_or_default())
                    .join("holders"),
            )
            .is_ok_and(|mut holders| holders.next().is_some());
            match probe.fs_type {
                "crypto_LUKS" if !opened => disks.locked.push(device),
                "btrfs" | "ext4" => {
                    if probe.uuid.as_ref().is_some_and(|uuid| !seen.insert(uuid.clone())) {
                        continue;
                    }
                    if running.as_ref().is_some_and(|running| same_device(running, &device)) {
                        disks.running = holds_archix(Path::new("/"));
                        continue;
                    }
                    let holds = match (probe.fs_type, mounted_at(&device)) {
                        ("ext4", Some(at)) => holds_archix(&at),
                        _ => peek(&device, probe.fs_type, holds_archix).unwrap_or(false),
                    };
                    if holds {
                        disks.roots.push(device);
                    }
                }
                _ => {}
            }
        }
        disks
    }

    /// The only archix root there is, or why there's none to pick.
    fn only_root(&self) -> Result<PathBuf> {
        let list = |devices: &[PathBuf]| {
            let names: Vec<String> = devices.iter().map(|device| device.display().to_string()).collect();
            names.join(", ")
        };
        match self.roots.as_slice() {
            [one] => Ok(one.clone()),
            [] => {
                let mut why = "no archix root was found on this machine's disks".to_string();
                if self.running {
                    why.push_str(" besides the running system's, where `archix rollback` does what the rescue does");
                }
                if !self.locked.is_empty() {
                    why.push_str(&format!(
                        "; {} is encrypted and wasn't looked into: `archix rescue open DEVICE root` opens one with its passphrase, then pass --root /dev/mapper/root",
                        list(&self.locked)
                    ));
                }
                bail!("{why}; name the root filesystem with --root DEVICE")
            }
            many => bail!(
                "{} archix roots were found ({}); pick one with --root DEVICE",
                many.len(),
                list(many)
            ),
        }
    }
}

/// What the rescue works on: a root and ESP it finds and mounts itself, or ones mounted already (--root-dir).
pub struct Session {
    options: Options,
    /// The root's device, once it was found.
    device: OnceCell<PathBuf>,
}

impl Session {
    pub fn new(options: Options) -> Result<Session> {
        if options.root_dir.as_deref() == Some(Path::new("/")) {
            bail!("--root-dir / is the running system; `archix rollback` works on it");
        }
        Ok(Session {
            options,
            device: OnceCell::new(),
        })
    }

    fn merge(&mut self, options: Options) {
        if options.root.is_some() || options.root_dir.is_some() {
            self.device = OnceCell::new();
        }
        let mine = &mut self.options;
        mine.root = options.root.or(mine.root.take());
        mine.rootflags = options.rootflags.or(mine.rootflags.take());
        mine.esp = options.esp.or(mine.esp.take());
        mine.root_dir = options.root_dir.or(mine.root_dir.take());
        mine.esp_dir = options.esp_dir.or(mine.esp_dir.take());
    }

    /// The root filesystem's block device: --root's, else the one mounted at --root-dir, else in the rescue boot root= from its kernel command line, else the only archix root among the disks. A live system's command line names the live system's root, so it's never read there.
    fn root_device(&self) -> Result<PathBuf> {
        if let Some(device) = self.device.get() {
            return Ok(device.clone());
        }
        let device = match (&self.options.root, &self.options.root_dir, boot_cmdline()) {
            (Some(spec), _, _) => self.opened(spec)?,
            (None, Some(dir), _) => {
                let dir = fs::canonicalize(dir).with_context(|| format!("--root-dir {}", dir.display()))?;
                let mount = mounts::at(&dir).filter(|mount| Path::new(&mount.source).is_absolute());
                let Some(mount) = mount else {
                    bail!(
                        "no filesystem on a disk is mounted at {}, so it doesn't say which device the root is on; mount the root there, or name its device with --root DEVICE",
                        dir.display()
                    );
                };
                PathBuf::from(mount.source)
            }
            (None, None, cmdline) => match cmdline.and_then(|cmdline| value_on(&cmdline, "root")) {
                Some(spec) => self.opened(&spec)?,
                None => {
                    let device = Disks::scan().only_root()?;
                    println!("archix rescue: the archix root is on {}", device.display());
                    device
                }
            },
        };
        let running = mounts::of(Path::new("/")).map(|mount| PathBuf::from(mount.source));
        if running.is_some_and(|running| same_device(&running, &device)) {
            bail!(
                "{} holds the running system's root; `archix rollback` and `passwd` work on it",
                device.display()
            );
        }
        Ok(self.device.get_or_init(|| device).clone())
    }

    /// The block device `spec` names; on an encrypted root, the volume the rescue boot's command line names (or each LUKS volume there is) opened with its passphrase first.
    fn opened(&self, spec: &str) -> Result<PathBuf> {
        if find(spec, 25).is_none() {
            let named = boot_cmdline()
                .map(|cmdline| rescue_auth::luks_on_cmdline(&cmdline))
                .unwrap_or_default();
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
                if find(spec, 25).is_some() {
                    break;
                }
            }
        }
        let device = resolve(spec)?;
        if fs_type(&device).is_ok_and(|fs| fs == "crypto_LUKS") {
            return open_luks(&device, LUKS_NAME);
        }
        Ok(device)
    }

    /// The root's own mount options: --rootflags, else rootflags= in the rescue boot, else subvol=/@ on a btrfs whose top level has an `@` holding a root and none itself.
    fn root_flags(&self, device: &Path, fs: &str) -> Option<String> {
        let given = self
            .options
            .rootflags
            .clone()
            .or_else(|| value_on(&boot_cmdline()?, "rootflags"));
        if given.is_some() || fs != "btrfs" {
            return given;
        }
        let root = archix::snapshot::ROOT;
        peek(device, fs, |top| {
            !top.join("etc/passwd").is_file() && top.join(root).join("etc/passwd").is_file()
        })
        .filter(|in_subvolume| *in_subvolume)
        .map(|_| format!("subvol=/{root}"))
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
                    let flags = self.root_flags(&device, &fs);
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

    /// The archix root, mounted read-write: --root-dir, else the root's device mounted by the rescue itself.
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
                    let fs = fs_type(&device)?;
                    // A btrfs mounts in several places at once; any other filesystem mounted elsewhere is someone else's to write.
                    if fs != "btrfs"
                        && let Some(at) = mounted_at(&device)
                    {
                        bail!(
                            "{} is mounted at {} already; unmount it first, or pass --root-dir {}",
                            device.display(),
                            at.display(),
                            at.display()
                        );
                    }
                    let flags = self.root_flags(&device, &fs);
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
            let esp = fs::read_to_string(Paths::on_root(&root).state.join("boot/esp")).ok()?;
            let at = root.join(esp.trim().trim_start_matches('/'));
            at.is_dir().then_some(at)
        });
        under_root.unwrap_or_else(|| PathBuf::from(ESP_MOUNT))
    }

    /// The ESP's device: --esp's, else in the rescue boot the partition systemd-boot started it from, else `named`, the one the root's fstab mounts, else the only EFI system partition there is. A live system's loader started from the live system's own ESP, so its variables are never read there.
    fn esp_device(&self, named: Option<String>) -> Result<PathBuf> {
        if let Some(spec) = &self.options.esp {
            return resolve(spec);
        }
        if in_initrd() {
            boot::mount_efivars();
            if let Some(uuid) = boot::loader_partition() {
                return resolve(&format!("PARTUUID={uuid}"));
            }
        }
        match named {
            Some(spec) => resolve(&spec),
            None => layout::only_esp()
                .map(PathBuf::from)
                .context("the ESP wasn't found; give it with --esp DEVICE"),
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
            Action::Rollback { number, noconfirm } => self.rollback(number, noconfirm),
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

    /// Mounts the ESP where the root laid out at `target` mounts it, which the keep's boot menu is written to; a root whose boot archix doesn't manage needs none.
    fn mount_layout_esp(&self, target: &Path) -> Result<()> {
        let recorded = fs::read_to_string(target.join("var/lib/archix/boot/esp")).unwrap_or_default();
        let Some(point) = Some(recorded.trim()).filter(|point| point.starts_with('/')) else {
            return Ok(());
        };
        let at = target.join(point.trim_start_matches('/'));
        let theirs = match (&self.options.esp_dir, &self.options.root_dir) {
            (Some(dir), _) => Some(dir.clone()),
            (None, Some(root)) => fs::canonicalize(root.join(point.trim_start_matches('/')))
                .ok()
                .filter(|dir| is_mount_point(dir)),
            (None, None) => None,
        };
        let mounted = match theirs {
            Some(dir) => mounts::bind(&dir, &at, false),
            None => {
                let named = layout::esp_of(target).map(|(spec, _)| spec);
                run_mount(&self.esp_device(named)?, &at, "vfat", "umask=0077")
            }
        };
        mounted.context("mounting the ESP for the boot menu")
    }

    fn rollback(&self, number: u32, noconfirm: bool) -> Result<()> {
        let device = self.root_device()?;
        if !fs_type(&device).is_ok_and(|fs| fs == "btrfs") {
            bail!(
                "the root on {} isn't btrfs, so it has no snapshots to go back to",
                device.display()
            );
        }
        self.unmount_all()?;
        let target = Path::new(LAYOUT_MOUNT);
        mounts::unmount_under(target)?;
        let ran = layout::mount_all(&device.to_string_lossy(), target)
            .with_context(|| {
                format!(
                    "the root on {} isn't laid out for rollbacks (the subvolumes @, @archix and @varlib), so there's no snapshot to go back to",
                    device.display()
                )
            })
            .and_then(|()| {
                if !rollbacks_on(target) {
                    bail!("this root's config has rollbacks turned off, so there's no snapshot to go back to");
                }
                for (what, fs) in [("proc", "proc"), ("sys", "sysfs"), ("dev", "devtmpfs")] {
                    mounts::mount(what, &target.join(what), fs, "")?;
                }
                self.mount_layout_esp(target)?;
                // This archix again, inside the root: a static binary needs nothing there, so the root's own programs and libraries can all be broken.
                let root = target.to_path_buf();
                let mut keep = Command::new("/proc/self/exe");
                keep.args(["rollback", &number.to_string()]);
                if noconfirm {
                    keep.arg("--noconfirm");
                }
                unsafe {
                    keep.pre_exec(move || {
                        std::os::unix::fs::chroot(&root)?;
                        std::env::set_current_dir("/")
                    });
                }
                let status = keep.status().context(
                    "starting archix inside the root; only the static build (the root's /usr/lib/archix/archix-static) starts there whatever the root has lost",
                )?;
                if !status.success() {
                    bail!("the rollback to snapshot {number} failed ({status})");
                }
                Ok(())
            });
        archix::durable::sync_filesystems(&[target]);
        let unmounted = mounts::unmount_under(target);
        ran?;
        unmounted?;
        if let Some(dir) = &self.options.root_dir {
            println!(
                "{} still shows the root that was replaced; unmount it and restart to boot snapshot {number}",
                dir.display()
            );
        }
        Ok(())
    }

    fn check(&self, repair_btrfs: bool) -> Result<()> {
        if let Some(dir) = &self.options.root_dir {
            let device = self
                .root_device()
                .map_or("DEVICE".to_string(), |device| device.display().to_string());
            bail!(
                "check needs the root and the ESP unmounted, and {} is mounted: unmount it, then `archix rescue --root {device} check` (with --esp DEVICE if the root's fstab doesn't name its ESP)",
                dir.display()
            );
        }
        // Outside the rescue boot the ESP is found through the root, so without one there's nothing to check.
        if !in_initrd() && self.options.esp.is_none() {
            self.root_device()?;
        }
        let esp_mounted = mounts::at(&self.esp_target())
            .or_else(|| mounts::at(Path::new(ESP_MOUNT)))
            .map(|mount| mount.source);
        self.unmount_all()?;
        // Outside the rescue boot the root's fstab is what names the ESP, read from a read-only mount that replays no journal.
        let named = match self.options.esp.is_some() || in_initrd() {
            true => None,
            false => self.root_device().ok().and_then(|device| {
                let fs = fs_type(&device).ok()?;
                let found = peek(&device, &fs, |top| {
                    [top.join(archix::snapshot::ROOT), top.to_path_buf()]
                        .iter()
                        .find(|root| root.join("etc/fstab").is_file())
                        .and_then(|root| layout::esp_of(root))
                });
                found.flatten().map(|(spec, _)| spec)
            }),
        };
        let esp = self.esp_device(named);
        let mut failed = Vec::new();
        match self.root_device() {
            Ok(device) => {
                if let Err(e) = fsck(&device, repair_btrfs) {
                    failed.push(format!("the root: {e:#}"));
                }
            }
            Err(e) => failed.push(format!("the root: {e:#}")),
        }
        let esp = match (esp, esp_mounted) {
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
        let paths = Paths::on_root(&root);
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

    /// The rescue environment's own shell, never the root's, whose programs may not start; in the root's directory when it mounts.
    fn shell(&self) -> Result<()> {
        let dir = match self.root() {
            Ok(dir) => dir,
            Err(e) => {
                println!("the root isn't mounted ({e:#}), so the shell starts in /");
                PathBuf::from("/")
            }
        };
        println!(
            "this rescue environment's shell in {}; `exit` comes back to the rescue prompt",
            dir.display()
        );
        Command::new("sh")
            .arg("-i")
            .env("PS1", "[rescue \\w]# ")
            .current_dir(&dir)
            .status()
            .context("this environment has no `sh` on its PATH to start; the root's own shell is never run, as the root may be what's broken")?;
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
        Some(action) => {
            let ran = session.run(action);
            // One command from a live system leaves nothing mounted behind it.
            let unmounted = session.unmount_all();
            ran.and(unmounted)
        }
        None => prompt(&mut session),
    }
}

fn prompt(session: &mut Session) -> Result<()> {
    // A handler, not SIG_IGN, which children would keep: Ctrl-C stops what runs, not the prompt.
    unsafe { libc::signal(libc::SIGINT, ignore as *const () as libc::sighandler_t) };
    println!("\narchix rescue: nothing of the root is mounted until a command needs it. Commands:\n{HELP}\n");
    if session.has_rollbacks() {
        println!(
            "  rollback N                   roll back to snapshot N from the next boot on, with this archix alone\n"
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
            device: OnceCell::new(),
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
