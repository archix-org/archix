use std::fs;
use std::io::{self, BufRead, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};

use archix::deploy;
use archix::paths::Paths;
use archix::system::{boot, password, rescue};

use super::confirm;

/// Where the rescue boot mounts the root, and the ESP when the root can't say where it goes.
const ROOT_MOUNT: &str = "/run/archix-rescue/root";
const ESP_MOUNT: &str = "/run/archix-rescue/esp";

/// Account files that hold the machine's users, which restoring the packaged copy would wipe.
const ACCOUNTS: &[&str] = &["passwd", "shadow", "group", "gshadow"];

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
    /// List the deployments: which one is current and the boot default, and how each one's boot entry stands
    List,
    /// Make deployment N current and the boot default, restoring files that differ from its packages first, as `archix rollback N` does
    Use { deployment: u32 },
    /// Check and repair the root filesystem and the ESP with fsck; unmounts them first
    Check {
        /// On btrfs, repair with `btrfs check --repair`, which can make some damage worse; by default btrfs is only checked
        #[arg(long)]
        repair_btrfs: bool,
    },
    /// Check deployments' /usr, /opt and /boot against their packages, as `archix verify` does
    Verify {
        /// Restore what differs from the package cache
        #[arg(long)]
        repair: bool,
        /// Only these deployments
        deployments: Vec<u32>,
    },
    /// Set USER's password in a deployment's /etc (current's by default), read twice from the console
    Password {
        user: String,
        #[arg(long, conflicts_with = "all")]
        deployment: Option<u32>,
        /// In every deployment
        #[arg(long)]
        all: bool,
    },
    /// Put a packaged /etc file, such as a PAM or sudo config, back as its package ships it
    RestoreEtc {
        /// Such as /etc/pam.d/system-auth or /etc/sudoers
        path: String,
        #[arg(long, conflicts_with = "all")]
        deployment: Option<u32>,
        /// In every deployment
        #[arg(long)]
        all: bool,
    },
    /// Make a deployment (current by default) the root's own trees, plain Arch without archix, as `archix eject` does
    Eject {
        #[arg(long)]
        deployment: Option<u32>,
        /// Don't ask for confirmation
        #[arg(long)]
        noconfirm: bool,
    },
    /// Open a shell; `exit` comes back to the prompt
    Shell,
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
  list                         the deployments, which one is current, how each boot entry stands
  use N                        make deployment N current and the boot default, after checking its files
  check [--repair-btrfs]       check and repair the root filesystem and the ESP
  verify [--repair] [N...]     check the deployments' files against their packages; --repair restores them
  password USER [--all]        set USER's password in the current deployment's /etc (--deployment N, or all)
  restore-etc PATH [--all]     put a packaged /etc file (PAM, sudoers) back as its package ships it
  eject [--deployment N]       make the current deployment plain Arch, without archix
  shell                        a shell; each deployment's /etc is under /run/archix-rescue/root/archix/deploy/N/etc
  reboot, poweroff
  Before a command: --root DEVICE [--rootflags OPTIONS] for another root, as after `cryptsetup open` in the shell";

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
    deploy::view::mount_points(&fs::read("/proc/self/mountinfo").unwrap_or_default())
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

/// The block device a spec names: a path, or UUID=, PARTUUID=, LABEL= and PARTLABEL= through udev's /dev/disk links.
fn resolve(spec: &str) -> Result<PathBuf> {
    let path = match spec.split_once('=') {
        Some(("UUID", value)) => Path::new("/dev/disk/by-uuid").join(value),
        Some(("PARTUUID", value)) => Path::new("/dev/disk/by-partuuid").join(value.to_lowercase()),
        Some(("LABEL", value)) => Path::new("/dev/disk/by-label").join(value),
        Some(("PARTLABEL", value)) => Path::new("/dev/disk/by-partlabel").join(value),
        _ => PathBuf::from(spec),
    };
    for _ in 0..50 {
        if path.exists() {
            return Ok(path);
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    let encrypted = ["rd.luks", "cryptdevice=", "luks"]
        .iter()
        .any(|word| archix::testhooks::kernel_cmdline().contains(word));
    let hint = match encrypted {
        true => {
            "; it's on an encrypted disk, so open it first: `shell`, `cryptsetup open /dev/<partition> root`, `exit`, then `--root /dev/mapper/root list`"
        }
        false => "; `shell` and `blkid` list the disks, then `--root DEVICE list` uses one",
    };
    bail!("there's no device {spec} ({}){hint}", path.display())
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
    if !archix::system::deploy_boot::read_only(dir) {
        return Ok(());
    }
    let c_dir = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes())?;
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
                "`archix rescue` runs in the rescue boot: pick \"archix rescue\" in the boot menu, or `sudo bootctl set-oneshot {}` and restart. On a running system `archix rollback`, `verify` and `eject` do the same; from a live USB, mount the root and pass --root-dir",
                rescue::ENTRY
            );
        }
        if options.root_dir.as_deref() == Some(Path::new("/")) {
            bail!("--root-dir / is the running system; `archix rollback`, `verify` and `eject` work on it");
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
                    let spec = self.root_spec()?;
                    let device = resolve(&spec)?;
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
        if !dir.join("archix").is_dir() {
            bail!(
                "{} has no /archix, so it isn't an archix root (on btrfs, is the subvolume in --rootflags, such as subvol=@?)",
                dir.display()
            );
        }
        archix::system::deploy_boot::writable_store(&dir)?;
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

    /// The ESP, mounted.
    fn esp(&self) -> Result<PathBuf> {
        if let Some(dir) = &self.options.esp_dir {
            return Ok(dir.clone());
        }
        let target = self.esp_target();
        if self.options.root_dir.is_some() && self.options.esp.is_none() {
            if target.join("loader").is_dir() {
                return Ok(target);
            }
            bail!("mount the ESP at {}, or pass --esp-dir", target.display());
        }
        if is_mount_point(&target) {
            return Ok(target);
        }
        let device = self.esp_device()?;
        run_mount(&device, &target, "vfat", "umask=0077").with_context(|| {
            format!(
                "the ESP on {} doesn't mount; `check` checks and repairs it",
                device.display()
            )
        })?;
        Ok(target)
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
            Action::List => self.list(),
            Action::Use { deployment } => self.use_deployment(deployment),
            Action::Check { repair_btrfs } => self.check(repair_btrfs),
            Action::Verify { repair, deployments } => {
                let root = self.root()?;
                super::deployments::verify_in(
                    &Paths::under(&root),
                    repair,
                    &deployments,
                    "archix rescue verify --repair",
                )
            }
            Action::Password { user, deployment, all } => self.password(&user, deployment, all),
            Action::RestoreEtc { path, deployment, all } => self.restore_etc(&path, deployment, all),
            Action::Eject { deployment, noconfirm } => self.eject(deployment, noconfirm),
            Action::Shell => self.shell(),
            Action::Reboot => self.power(libc::RB_AUTOBOOT),
            Action::Poweroff => self.power(libc::RB_POWER_OFF),
        }
    }

    /// The deployments `deployment` and `all` pick, current's by default.
    fn targets(&self, store: &deploy::Store, deployment: Option<u32>, all: bool) -> Result<Vec<u32>> {
        let kept = store.deployments()?;
        let picked = match (deployment, all) {
            (_, true) => kept.clone(),
            (Some(number), false) => vec![number],
            (None, false) => vec![
                store
                    .current()?
                    .context("there's no current deployment; name one with --deployment")?,
            ],
        };
        if let Some(missing) = picked.iter().find(|n| !kept.contains(n)) {
            bail!("there's no complete deployment {missing}; `list` shows them");
        }
        Ok(picked)
    }

    fn list(&self) -> Result<()> {
        let root = self.root()?;
        let paths = Paths::under(&root);
        let store = deploy::Store::open(&root)?;
        let esp = match self.esp() {
            Ok(esp) => Some(esp),
            Err(e) => {
                println!("the ESP: {e:#}");
                None
            }
        };
        let conf = esp
            .as_ref()
            .and_then(|esp| fs::read_to_string(esp.join("loader/loader.conf")).ok())
            .unwrap_or_default();
        let default = deploy::keep::boot_default(&conf);
        let current = store.current().unwrap_or_else(|e| {
            println!("{e:#}");
            None
        });
        let records = archix::generations::Store::new(&paths);
        let committed = store.deployments()?;
        println!(
            "the root: {} ({})",
            root.display(),
            mount_source(&root).unwrap_or_else(|| "mounted".to_string())
        );
        if committed.is_empty() {
            println!("no complete deployment is left; `shell` to look around");
        }
        for number in committed.iter().rev() {
            let mut marks = Vec::new();
            if current == Some(*number) {
                marks.push("current");
            }
            if default == Some(*number) {
                marks.push("boot default");
            }
            let entry = match esp.as_deref().map(|esp| rescue::entry_state(&paths, esp, *number)) {
                None => "entry unknown".to_string(),
                Some(rescue::EntryState::Missing) => "no boot entry".to_string(),
                Some(rescue::EntryState::NotBooted) => "never booted".to_string(),
                Some(rescue::EntryState::Good) => "booted fine".to_string(),
                Some(rescue::EntryState::Counting(left)) => format!("new, {left} boot tries left"),
                Some(rescue::EntryState::Bad) => "failed its boot tries".to_string(),
            };
            let generation = records.get(*number).ok();
            let kernel = generation
                .as_ref()
                .and_then(|g| g.settings.boot.as_ref())
                .and_then(|plan| boot::owned_version(&store.path(*number).join("pacman"), &plan.kernel))
                .or_else(|| {
                    boot::kernels_in(&store.path(*number).join("pacman"))
                        .into_iter()
                        .next()
                        .map(|(_, version)| version)
                })
                .unwrap_or_else(|| "none".to_string());
            let made = generation.map_or(String::new(), |g| {
                format!(", made {}", archix::date::Date::from_unix(g.created))
            });
            println!(
                "{} deployment {number:<4} {:<22} {entry}, kernel {kernel}{made}",
                if current == Some(*number) { "*" } else { " " },
                marks.join(", ")
            );
        }
        if let Some(esp) = &esp {
            println!("{}", rescue::describe(&paths, esp));
        }
        Ok(())
    }

    fn use_deployment(&self, number: u32) -> Result<()> {
        let root = self.root()?;
        let esp = self.esp()?;
        let paths = Paths::under(&root);
        let store = deploy::Store::open(&root)?;
        let _lock = store.lock()?;
        if !store.deployments()?.contains(&number) {
            bail!("there's no complete deployment {number}; `list` shows them");
        }
        if rescue::entry_state(&paths, &esp, number) == rescue::EntryState::Missing {
            bail!(
                "deployment {number} has no boot entry, which the rescue can't build; `use` one that has, then switch from it"
            );
        }
        deploy::verify::ensure_intact(&paths, &store, number)
            .with_context(|| format!("deployment {number} stays as it was; `verify {number}` shows what differs"))?;
        if boot::retry_entry(&esp, number)? {
            println!("deployment {number}'s entry had used up its boot tries; it gets them again");
        }
        boot::repoint(&paths, &esp, &store, number, &|| {
            bail!("deployment {number}'s entry names no UKI on the ESP")
        })?;
        println!("deployment {number} is current and the boot default; `reboot` starts it");
        Ok(())
    }

    fn check(&self, repair_btrfs: bool) -> Result<()> {
        if self.options.root_dir.is_some() {
            bail!("check needs the root and the ESP unmounted; unmount them and pass --root DEVICE and --esp DEVICE");
        }
        let esp_mounted = mount_source(&self.esp_target()).or_else(|| mount_source(Path::new(ESP_MOUNT)));
        self.unmount_all()?;
        let mut failed = Vec::new();
        match self.root_spec().and_then(|spec| resolve(&spec)) {
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

    fn password(&self, user: &str, deployment: Option<u32>, all: bool) -> Result<()> {
        let root = self.root()?;
        let store = deploy::Store::open(&root)?;
        let targets = self.targets(&store, deployment, all)?;
        let first = read_password(&format!("new password for {user}: "))?;
        if first.is_empty() {
            bail!("the password is empty; nothing was changed");
        }
        if read_password("again: ")? != first {
            bail!("the passwords differ; nothing was changed");
        }
        let hash = password::sha512_crypt(first.as_bytes(), &password::salt()?);
        for number in targets {
            password::set(&store.path(number).join("etc"), user, &hash)
                .with_context(|| format!("setting {user}'s password in deployment {number}"))?;
            println!("set {user}'s password in deployment {number}");
        }
        Ok(())
    }

    fn restore_etc(&self, path: &str, deployment: Option<u32>, all: bool) -> Result<()> {
        let rel = path.trim_start_matches('/').trim_start_matches("etc/");
        if ACCOUNTS.contains(&rel) {
            bail!(
                "/etc/{rel} holds this machine's users and groups, which the packaged copy would wipe; `password` resets a password"
            );
        }
        let root = self.root()?;
        let paths = Paths::under(&root);
        let store = deploy::Store::open(&root)?;
        for number in self.targets(&store, deployment, all)? {
            let package = deploy::verify::restore_etc(&paths, &store.path(number), rel)
                .with_context(|| format!("deployment {number}"))?;
            println!("restored /etc/{rel} in deployment {number} as {package} ships it");
        }
        Ok(())
    }

    fn eject(&self, deployment: Option<u32>, noconfirm: bool) -> Result<()> {
        let root = self.root()?;
        let esp = self.esp()?;
        let paths = Paths::under(&root);
        let store = deploy::Store::open(&root)?;
        let number = match deployment {
            Some(number) => number,
            None => store
                .current()?
                .context("there's no current deployment; name one with --deployment")?,
        };
        println!(
            "eject: deployment {number} becomes the root's own /usr, /etc, /opt, /boot and pacman database, the ESP boots it through a plain UKI, and /archix with every deployment goes; packages, users, /var and homes stay"
        );
        if !noconfirm && !confirm("eject?")? {
            bail!("aborted; nothing was changed");
        }
        let written = deploy::eject::offline(&paths, Some(number), &esp).map_err(|e| {
            e.context("the eject stopped partway; the deployments still boot, and `eject` again finishes it")
        })?;
        for line in written {
            println!("boot: {line}");
        }
        println!("the machine is plain Arch now; `reboot` starts it");
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
    print!("{prompt}");
    io::stdout().flush()?;
    let fd = libc::STDIN_FILENO;
    let mut saved: libc::termios = unsafe { std::mem::zeroed() };
    let tty = unsafe { libc::tcgetattr(fd, &mut saved) } == 0;
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
    read.context("reading the password")?;
    Ok(line.trim_end_matches(['\n', '\r']).to_string())
}

extern "C" fn ignore(_: libc::c_int) {}

/// `archix rescue`: one command, or with none the rescue prompt.
pub fn run(options: Options, action: Option<Action>) -> Result<()> {
    let mut session = Session::new(options)?;
    match action {
        Some(action) => session.run(action),
        None => prompt(&mut session),
    }
}

fn prompt(session: &mut Session) -> Result<()> {
    // A handler, not SIG_IGN, which children would keep: Ctrl-C stops what runs, not the prompt.
    unsafe { libc::signal(libc::SIGINT, ignore as *const () as libc::sighandler_t) };
    println!("\narchix rescue: this boot mounts no deployment. Commands:\n{HELP}\n");
    if let Err(e) = session.list() {
        println!("error: {e:#}");
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
        let line = Line::try_parse_from(["use", "3"]).unwrap();
        assert!(matches!(line.action, Some(Action::Use { deployment: 3 })));
        let line = Line::try_parse_from(["--root", "/dev/mapper/root", "list"]).unwrap();
        assert_eq!(line.options.root.as_deref(), Some("/dev/mapper/root"));
        assert!(matches!(line.action, Some(Action::List)));
        let line = Line::try_parse_from(["password", "alice", "--all"]).unwrap();
        assert!(
            matches!(line.action, Some(Action::Password { ref user, all: true, deployment: None }) if user == "alice")
        );
        assert!(Line::try_parse_from(["password", "alice", "--all", "--deployment", "2"]).is_err());
        let line = Line::try_parse_from(["verify", "--repair", "1", "2"]).unwrap();
        assert!(
            matches!(line.action, Some(Action::Verify { repair: true, ref deployments }) if deployments == &[1, 2])
        );
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
