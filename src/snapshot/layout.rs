use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::system::fstab::FstabLine;

/// The subvolume /var/lib moves to, beside `@` at the top level, so a rollback leaves services' data alone.
pub const VAR_LIB: &str = "@varlib";
/// The subvolume mounted at /.archix with the snapshots and their records, which a rollback mustn't take back.
pub const ARCHIX: &str = "@archix";
/// The parts of /var/lib that describe what's installed, so they roll back with `/`; they stay on the root here, bound over their usual paths.
pub const ROLLED: [&str; 3] = ["pacman", "dkms", "archix"];
/// Where on the root ROLLED live, outside the /var/lib mount that would hide them.
pub const ON_ROOT: &str = "var/rollback";
const PENDING: &str = ".split-pending";
/// The unit that moves /var/lib into its subvolume at the boot after setup, before anything uses it.
pub const SPLIT_UNIT: &str = "archix-split-var-lib.service";

/// The subvolume `path` is on, as btrfs names it from the top level, like `/@`; none if it isn't btrfs or isn't mounted.
pub fn subvolume_of(path: &Path) -> Option<String> {
    let output = Command::new("findmnt")
        .args(["-n", "-o", "FSROOT,FSTYPE", "-T"])
        .arg(path)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let mut fields = text.split_whitespace();
    let (root, fs_type) = (fields.next()?, fields.next()?);
    (output.status.success() && fs_type == "btrfs").then(|| root.to_string())
}

/// The fstab lines of the layout for the btrfs `uuid` whose root is mounted with `options`: /var/lib and /.archix as subvolumes of their own, then ROLLED bound from the root.
pub fn fstab_lines(uuid: &str, options: &str) -> Vec<FstabLine> {
    let kept: Vec<&str> = options
        .split(',')
        .filter(|option| !option.is_empty() && !option.starts_with("subvol=") && !option.starts_with("subvolid="))
        .collect();
    let with = |subvolume: &str| {
        let mut all = kept.clone();
        let named = format!("subvol=/{subvolume}");
        all.push(&named);
        all.join(",")
    };
    let mut lines = vec![
        FstabLine {
            device: format!("UUID={uuid}"),
            point: "/var/lib".into(),
            fs_type: "btrfs".into(),
            options: with(VAR_LIB),
            passno: 0,
            size: None,
        },
        FstabLine {
            device: format!("UUID={uuid}"),
            point: format!("/{}", super::MOUNT),
            fs_type: "btrfs".into(),
            options: with(ARCHIX),
            passno: 0,
            size: None,
        },
    ];
    // No nofail: pacman with an empty /var/lib/pacman would think nothing is installed, so a failed bind must stop the boot.
    lines.extend(ROLLED.iter().map(|name| FstabLine {
        device: format!("/{ON_ROOT}/{name}"),
        point: format!("/var/lib/{name}"),
        fs_type: "none".into(),
        options: "bind,x-systemd.requires-mounts-for=/var/lib".into(),
        passno: 0,
        size: None,
    }));
    lines
}

/// `fstab` with `lines` in place of any line mounting the same points, appended in order.
pub fn with_lines(fstab: &str, lines: &[FstabLine]) -> String {
    let points: Vec<&str> = lines.iter().map(|line| line.point.as_str()).collect();
    let mut text: String = fstab
        .lines()
        .filter(|line| {
            let mut fields = line.split_whitespace();
            line.trim_start().starts_with('#') || fields.nth(1).is_none_or(|point| !points.contains(&point))
        })
        .map(|line| format!("{line}\n"))
        .collect();
    for line in lines {
        text.push_str(&line.line());
        text.push('\n');
    }
    text
}

/// The unit that runs the split at boot: after the root is writable, before /var/lib is mounted or anything reads it.
pub fn split_unit() -> String {
    format!(
        "[Unit]\nDescription=archix: move /var/lib into its own subvolume\nDefaultDependencies=no\n\
         After=systemd-remount-fs.service\nBefore=var-lib.mount local-fs.target\nConditionPathExists=/{ON_ROOT}/{PENDING}\n\n\
         [Service]\nType=oneshot\nRemainAfterExit=yes\nExecStart=/usr/bin/archix split-var-lib\nStandardOutput=journal+console\n\n\
         [Install]\nRequiredBy=var-lib.mount\n"
    )
}

/// Moves what's in `<root>/var/lib` into the subvolume at `var_lib`, ROLLED into `<root>/var/rollback`, and leaves the mount points for their binds; safe to run again after a crash, as each entry leaves the root only once its copy is whole.
pub fn split_var_lib(root: &Path, var_lib: &Path) -> Result<()> {
    let old = root.join("var/lib");
    if crate::system::fstab::mounted(root, "/var/lib") {
        bail!(
            "{} is mounted already, so its split is done; nothing was changed",
            old.display()
        );
    }
    let rolled = root.join(ON_ROOT);
    fs::create_dir_all(&rolled).with_context(|| format!("creating {}", rolled.display()))?;
    for name in ROLLED {
        let from = old.join(name);
        let to = rolled.join(name);
        if from.symlink_metadata().is_ok() && !to.exists() {
            crate::durable::rename(&from, &to)
                .with_context(|| format!("moving {} to {}", from.display(), to.display()))?;
        }
        fs::create_dir_all(&to).with_context(|| format!("creating {}", to.display()))?;
    }
    crate::durable::sync_dir(&rolled)?;
    let mut entries: Vec<PathBuf> = fs::read_dir(&old)
        .with_context(|| format!("reading {}", old.display()))?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            !path
                .file_name()
                .is_some_and(|name| ROLLED.iter().any(|rolled| name == *rolled))
        })
        .collect();
    entries.sort();
    for from in entries {
        let to = var_lib.join(from.file_name().unwrap_or_default());
        if to.symlink_metadata().is_ok() {
            remove(&to)?;
        }
        let output = Command::new("cp")
            .args(["-a", "--reflink=auto"])
            .arg(&from)
            .arg(&to)
            .output()
            .context("running cp")?;
        if !output.status.success() {
            bail!(
                "couldn't copy {} to {}: {}",
                from.display(),
                to.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        crate::durable::sync_filesystems_checked(&[var_lib])?;
        crate::testhooks::mark("split-entry-copied");
        remove(&from)?;
        crate::testhooks::mark("split-entry-removed");
    }
    for name in ROLLED {
        for dir in [var_lib.join(name), old.join(name)] {
            under_bind(&dir, name)?;
        }
    }
    crate::durable::sync_filesystems_checked(&[var_lib, root])?;
    crate::testhooks::mark("split-binds-ready");
    match fs::remove_file(rolled.join(PENDING)) {
        Ok(()) => crate::durable::sync_dir(&rolled),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", rolled.join(PENDING).display())),
    }
}

/// The README for the directory a bind covers, for whoever finds it uncovered.
fn readme(name: &str) -> String {
    format!(
        "archix binds /{ON_ROOT}/{name} from the root over this directory at boot, so it rolls back with the root.\n\
         You're seeing this directory because the bind isn't mounted, as on a live USB that mounted only some subvolumes.\n\
         The db.lck here keeps pacman from running against an empty database; use `archix chroot <device>`, which mounts everything.\n"
    )
}

/// Makes `dir` the mount point a bind covers: a permanent db.lck, so pacman refuses there, and a README saying why.
fn under_bind(dir: &Path, name: &str) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    crate::durable::write(&dir.join("db.lck"), b"")?;
    crate::durable::write(&dir.join("README"), readme(name).as_bytes())
}

fn remove(path: &Path) -> Result<()> {
    let result = if path.symlink_metadata()?.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    result.with_context(|| format!("removing {}", path.display()))
}

/// What setup did on the system: whether it still needs a reboot for /var/lib to move.
#[derive(Debug, PartialEq)]
pub enum Setup {
    Done,
    AtNextBoot,
}

/// Lays out the root at `root` for rollbacks, with the top level of its btrfs mounted at `top`: creates @archix and @varlib, writes their fstab lines into `<root>/etc/fstab` and the split unit, and marks the split pending for the next boot; nothing moves on the running system, whose services hold /var/lib open.
pub fn setup(root: &Path, top: &Path, uuid: &str, root_options: &str) -> Result<Setup> {
    if let Some(why) = super::unsupported(root) {
        bail!("{why}");
    }
    let pending = root.join(ON_ROOT).join(PENDING);
    let split = top.join(VAR_LIB).exists() && !pending.exists();
    // Before @varlib, whose existence without this marker means the split is done.
    if !split {
        fs::create_dir_all(root.join(ON_ROOT))?;
        crate::durable::write(&pending, b"")?;
    }
    for subvolume in [ARCHIX, VAR_LIB] {
        let path = top.join(subvolume);
        if !path.exists() {
            let output = Command::new("btrfs")
                .args(["-q", "subvolume", "create"])
                .arg(&path)
                .output()
                .context("running btrfs")?;
            if !output.status.success() {
                bail!(
                    "btrfs couldn't create {}: {}",
                    path.display(),
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
        }
    }
    crate::testhooks::mark("layout-subvolumes");
    fs::create_dir_all(root.join(super::MOUNT))?;
    let archix = top.join(ARCHIX);
    super::record_root(&archix, top)?;
    if !split {
        let unit = root.join("etc/systemd/system").join(SPLIT_UNIT);
        fs::create_dir_all(unit.parent().unwrap_or(root))?;
        crate::durable::write(&unit, split_unit().as_bytes())?;
        let wants = root.join("etc/systemd/system/var-lib.mount.requires");
        fs::create_dir_all(&wants)?;
        let link = wants.join(SPLIT_UNIT);
        if link.symlink_metadata().is_err() {
            std::os::unix::fs::symlink(format!("../{SPLIT_UNIT}"), &link)?;
        }
    }
    // The fstab comes last and after a flush: its lines without the split bind an empty database over pacman's.
    crate::durable::sync_filesystems_checked(&[root, top])?;
    crate::testhooks::mark("layout-split-armed");
    let fstab = root.join("etc/fstab");
    let old = fs::read_to_string(&fstab).unwrap_or_default();
    let new = with_lines(&old, &fstab_lines(uuid, root_options));
    if new != old {
        crate::durable::write(&fstab, new.as_bytes())?;
    }
    crate::testhooks::mark("layout-fstab-written");
    Ok(if split { Setup::Done } else { Setup::AtNextBoot })
}

/// Whether `fstab` mounts /var/lib, without which there's nothing for the split to move it under.
pub fn split_is_due(fstab: &str) -> bool {
    fstab
        .lines()
        .filter_map(crate::system::fstab::fields)
        .any(|fields| fields.get(1).is_some_and(|point| point == "/var/lib"))
}

/// Mounts the root on `device` at `target` as it boots: `@`, then @archix and @varlib, then the binds from /var/rollback; for repair from a live USB.
pub fn mount_all(device: &str, target: &Path) -> Result<()> {
    let mount = |args: &[&str], at: &Path| -> Result<()> {
        let output = Command::new("mount")
            .args(args)
            .arg(at)
            .output()
            .context("running mount")?;
        if !output.status.success() {
            bail!(
                "couldn't mount {}: {}",
                at.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    };
    fs::create_dir_all(target).with_context(|| format!("creating {}", target.display()))?;
    mount(&["-o", &format!("subvol=/{}", super::ROOT), device], target)?;
    for (subvolume, point) in [(ARCHIX, super::MOUNT), (VAR_LIB, "var/lib")] {
        let at = target.join(point);
        if !at.is_dir() {
            bail!(
                "{} has no /{point}, so it isn't laid out for rollbacks; mount it by hand",
                device
            );
        }
        mount(&["-o", &format!("subvol=/{subvolume}"), device], &at)?;
    }
    for name in ROLLED {
        let source = target.join(ON_ROOT).join(name);
        mount(
            &["--bind", &source.to_string_lossy()],
            &target.join("var/lib").join(name),
        )?;
    }
    Ok(())
}

/// The partition type GUID of an EFI system partition.
const ESP_TYPE: &str = "c12a7328-f81f-11d2-ba4b-00a0c93ec93b";

/// The device spec and mount point of the ESP in an fstab's text: the line mounting `point` (boot.esp, as archix recorded it), else the first vfat line.
fn esp_in_fstab(fstab: &str, point: Option<&str>) -> Option<(String, String)> {
    let lines: Vec<Vec<String>> = fstab
        .lines()
        .filter_map(crate::system::fstab::fields)
        .filter(|f| f.len() >= 3)
        .collect();
    let found = match point {
        Some(point) => lines
            .iter()
            .find(|f| f[1].trim_end_matches('/') == point.trim_end_matches('/')),
        None => lines.iter().find(|f| f[2] == "vfat"),
    }?;
    Some((found[0].clone(), found[1].clone()))
}

/// The partitions `lsblk -p -P -o NAME,PARTTYPE` lists with the ESP's type.
fn esp_partitions(lsblk: &str) -> Vec<String> {
    let value = |line: &str, key: &str| {
        let start = line.find(&format!("{key}=\""))? + key.len() + 2;
        Some(line[start..].split('"').next()?.to_string())
    };
    lsblk
        .lines()
        .filter(|line| value(line, "PARTTYPE").is_some_and(|t| t.eq_ignore_ascii_case(ESP_TYPE)))
        .filter_map(|line| value(line, "NAME"))
        .collect()
}

/// The one ESP among `found`, or a refusal naming them.
fn pick_esp(found: &[String]) -> Result<String> {
    match found {
        [one] => Ok(one.clone()),
        [] => bail!(
            "the root's fstab has no ESP line and no EFI system partition was found; mount it yourself, or name it with --esp DEVICE"
        ),
        many => bail!(
            "the root's fstab has no ESP line and there are {} EFI system partitions ({}); name the root's with --esp DEVICE",
            many.len(),
            many.join(", ")
        ),
    }
}

/// Mounts the ESP of the root laid out at `target` where it boots: `esp` if given, else the device the root's fstab names for boot.esp (as archix's state records it, /efi by default), else the only EFI system partition there is. Returns where it went.
pub fn mount_esp(target: &Path, esp: Option<&str>) -> Result<PathBuf> {
    let recorded = fs::read_to_string(target.join(ON_ROOT).join("archix/boot/esp")).ok();
    let recorded = recorded.as_deref().map(str::trim).filter(|p| p.starts_with('/'));
    let fstab = fs::read_to_string(target.join("etc/fstab")).unwrap_or_default();
    let (device, point) = match (esp, esp_in_fstab(&fstab, recorded)) {
        (Some(device), found) => (
            device.to_string(),
            found
                .map(|(_, point)| point)
                .or(recorded.map(str::to_string))
                .unwrap_or_else(|| "/efi".to_string()),
        ),
        (None, Some(found)) => found,
        (None, None) => {
            let output = Command::new("lsblk")
                .args(["-p", "-P", "-o", "NAME,PARTTYPE"])
                .output()
                .context("running lsblk")?;
            let device = pick_esp(&esp_partitions(&String::from_utf8_lossy(&output.stdout)))?;
            (device, recorded.unwrap_or("/efi").to_string())
        }
    };
    let at = target.join(point.trim_start_matches('/'));
    fs::create_dir_all(&at).with_context(|| format!("creating {}", at.display()))?;
    let output = Command::new("mount")
        .args(["-o", "umask=0077"])
        .arg(&device)
        .arg(&at)
        .output()
        .context("running mount")?;
    if !output.status.success() {
        bail!(
            "couldn't mount the ESP {device} at {}: {}",
            at.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(at)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_split_is_due_only_once_the_fstab_mounts_var_lib() {
        assert!(!split_is_due(
            "UUID=a / btrfs rw,subvol=/@ 0 0\n# UUID=a /var/lib btrfs rw 0 0\n"
        ));
        assert!(split_is_due("UUID=a /var/lib btrfs rw,subvol=/@varlib 0 0\n"));
    }

    #[test]
    fn the_esp_is_where_the_roots_fstab_mounts_boot_esp_else_its_vfat_line() {
        let fstab = "# comment\nUUID=abcd / btrfs rw,subvol=/@ 0 0\nUUID=ESP1 /efi vfat umask=0077,noauto,x-systemd.automount 0 2\nUUID=DATA /boot/x vfat defaults 0 2\n";
        assert_eq!(
            esp_in_fstab(fstab, Some("/efi")),
            Some(("UUID=ESP1".to_string(), "/efi".to_string()))
        );
        assert_eq!(
            esp_in_fstab(fstab, Some("/boot/x")),
            Some(("UUID=DATA".to_string(), "/boot/x".to_string())),
            "boot.esp names it"
        );
        assert_eq!(
            esp_in_fstab(fstab, None),
            Some(("UUID=ESP1".to_string(), "/efi".to_string())),
            "without archix's record, the first vfat line"
        );
        assert_eq!(esp_in_fstab("UUID=abcd / ext4 rw 0 1\n", None), None);
    }

    #[test]
    fn one_esp_partition_is_taken_and_several_are_refused_naming_them() {
        let lsblk = "NAME=\"/dev/sda1\" PARTTYPE=\"c12a7328-f81f-11d2-ba4b-00a0c93ec93b\"\nNAME=\"/dev/sda2\" PARTTYPE=\"0fc63daf-8483-4772-8e79-3d69d8477de4\"\nNAME=\"/dev/nvme0n1p1\" PARTTYPE=\"C12A7328-F81F-11D2-BA4B-00A0C93EC93B\"\n";
        let found = esp_partitions(lsblk);
        assert_eq!(found, ["/dev/sda1", "/dev/nvme0n1p1"]);
        assert_eq!(pick_esp(&found[1..]).unwrap(), "/dev/nvme0n1p1");
        let error = format!("{:#}", pick_esp(&found).unwrap_err());
        assert!(
            error.contains("/dev/sda1") && error.contains("/dev/nvme0n1p1") && error.contains("--esp"),
            "{error}"
        );
        assert!(format!("{:#}", pick_esp(&[]).unwrap_err()).contains("no EFI system partition"));
    }

    #[test]
    fn the_layout_mounts_its_subvolumes_with_the_roots_options_and_binds_what_rolls_back() {
        let lines: Vec<String> = fstab_lines("abcd", "rw,noatime,compress=zstd:3,subvol=/@,subvolid=256")
            .iter()
            .map(FstabLine::line)
            .collect();
        assert_eq!(
            lines,
            [
                "UUID=abcd /var/lib btrfs rw,noatime,compress=zstd:3,subvol=/@varlib 0 0",
                "UUID=abcd /.archix btrfs rw,noatime,compress=zstd:3,subvol=/@archix 0 0",
                "/var/rollback/pacman /var/lib/pacman none bind,x-systemd.requires-mounts-for=/var/lib 0 0",
                "/var/rollback/dkms /var/lib/dkms none bind,x-systemd.requires-mounts-for=/var/lib 0 0",
                "/var/rollback/archix /var/lib/archix none bind,x-systemd.requires-mounts-for=/var/lib 0 0",
            ]
        );
    }

    #[test]
    fn layout_lines_replace_ones_for_the_same_points_and_leave_the_rest() {
        let old = "# comment\nUUID=abcd / btrfs subvol=/@ 0 0\nUUID=abcd /var/lib btrfs subvol=/old 0 0\nUUID=ef /boot vfat umask=0077 0 2\n";
        let new = with_lines(old, &fstab_lines("abcd", "subvol=/@"));
        assert!(new.starts_with("# comment\nUUID=abcd / btrfs subvol=/@ 0 0\nUUID=ef /boot vfat umask=0077 0 2\n"));
        assert!(!new.contains("subvol=/old"));
        assert_eq!(
            with_lines(&new, &fstab_lines("abcd", "subvol=/@")),
            new,
            "applying it twice changes nothing"
        );
    }
}
