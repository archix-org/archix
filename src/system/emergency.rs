//! What emergency mode prints before sulogin on a system with required mounts, and the record a counted try that stopped there leaves for the fallback boot's note.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::fstab;
use crate::paths::Paths;

/// The drop-in on emergency.service on a system that declares a required mount.
pub const DROP_IN: &str = "/etc/systemd/system/emergency.service.d/archix-required-mounts.conf";

/// The drop-in's text: `archix emergency-hint` before sulogin, its failure ignored so sulogin always starts.
pub fn drop_in(rescue_entry: bool) -> String {
    let flag = if rescue_entry { "" } else { " --no-rescue-entry" };
    format!("[Service]\nExecStartPre=-/usr/bin/archix emergency-hint{flag}\n")
}

/// A required fstab line that isn't mounted, and whether its device is attached.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Unmounted {
    pub point: String,
    pub device: String,
    pub attached: bool,
}

impl Unmounted {
    fn describe(&self) -> String {
        format!("{} ({})", self.point, self.device)
    }
}

/// Whether the device an fstab line names is there now, a crypttab volume's when its LUKS device is, though it didn't open; a tmpfs, bind or network source counts as there.
fn attached(device: &str, crypttab: &str) -> bool {
    match device.strip_prefix("/dev/mapper/") {
        Some(_) if Path::new(device).exists() => true,
        Some(name) => crypttab
            .lines()
            .filter_map(fstab::fields)
            .find(|fields| fields.len() >= 2 && fields[0] == name)
            .is_some_and(|fields| attached(&fields[1], "")),
        None if fstab::device_backed(device) => fstab::resolve(device).is_some(),
        None => true,
    }
}

/// The lines of `fstab` that systemd waits for at boot (no nofail, noauto or automount, not the root or swap) with nothing mounted at their point under `root`.
pub fn unmounted(root: &Path, fstab: &str, crypttab: &str) -> Vec<Unmounted> {
    fstab
        .lines()
        .filter_map(fstab::fields)
        .filter(|fields| fields.len() >= 3 && fields[2] != "swap" && !fields[1].trim_end_matches('/').is_empty())
        .filter(|fields| {
            let options = fields.get(3).map_or("", String::as_str);
            !options
                .split(',')
                .any(|option| matches!(option, "nofail" | "noauto" | "x-systemd.automount"))
        })
        .filter(|fields| !fstab::mounted(root, &fields[1]))
        .map(|fields| Unmounted {
            attached: attached(&fields[0], crypttab),
            point: fields[1].clone(),
            device: fields[0].clone(),
        })
        .collect()
}

/// What emergency.service prints before sulogin: the required mounts that didn't mount and what to do; `counted` is the entry whose counted try this is.
pub fn hint(unmounted: &[Unmounted], counted: Option<&str>, rescue_entry: bool) -> Vec<String> {
    let mut lines = Vec::new();
    if unmounted.is_empty() {
        lines.push("archix: this boot stopped in emergency mode, though every required mount in /etc/fstab is mounted; `systemctl --failed` and `journalctl -xb` say what failed".to_string());
    } else {
        lines.push(
            "archix: this boot stopped in emergency mode because required mounts in /etc/fstab didn't mount:"
                .to_string(),
        );
        for mount in unmounted {
            lines.push(match mount.attached {
                false => format!("archix:   {}: its disk isn't attached", mount.describe()),
                true => format!(
                    "archix:   {}: its device is there, but it didn't mount; `journalctl -b -u {}.mount` says why",
                    mount.describe(),
                    fstab::escape_path(&mount.point)
                ),
            });
        }
    }
    let missing = unmounted.iter().any(|mount| !mount.attached);
    if missing {
        lines.push("archix: reattach the disk and reboot,".to_string());
    }
    if rescue_entry {
        lines.push(format!(
            "archix: {}start the archix rescue entry from the boot menu (hold Space at power-on)",
            if missing {
                "or "
            } else {
                "for a shell without root's password, "
            }
        ));
    }
    if let Some(entry) = counted {
        lines.push(format!(
            "archix: this is one of {entry}'s counted boot tries: each boot that stops here uses one, then systemd-boot falls back to how the system last booted fine; it restarts after 2 minutes with nobody logged in"
        ));
    }
    lines
}

/// The required mounts a counted try of an entry stopped in emergency mode without.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MountFailure {
    pub entry: String,
    pub mounts: Vec<Unmounted>,
    pub at: u64,
}

/// Beside the menu's state, so the fallback's try boot reads it.
fn record(paths: &Paths) -> PathBuf {
    super::menu::dir(paths).join("mount-failed.json")
}

pub fn read(paths: &Paths) -> Option<MountFailure> {
    serde_json::from_str(&fs::read_to_string(record(paths)).ok()?).ok()
}

/// Drops the record, once a boot of the current system reached boot-complete.target.
pub fn forget(paths: &Paths) {
    let _ = fs::remove_file(record(paths));
}

/// Run by emergency.service before sulogin: prints the hint, and on a counted try records what was missing; never fails.
pub fn run(paths: &Paths, rescue_entry: bool) {
    let read = |file: &str| fs::read_to_string(paths.root.join("etc").join(file)).unwrap_or_default();
    let found = unmounted(&paths.root, &read("fstab"), &read("crypttab"));
    // By name, from the EFI variable: reading the entry's text would wait on the ESP's automount, whose mount emergency mode may hold back, while sulogin waits for this.
    let counted = crate::testhooks::counted_boot()
        .then(super::boot::booted_by_name)
        .flatten()
        .map(|entry| entry.id());
    let text = format!("\n{}\n\n", hint(&found, counted.as_deref(), rescue_entry).join("\n"));
    {
        use std::io::Write as _;
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(text.as_bytes());
        let _ = out.flush();
        // sulogin flushes the console's queue as it takes it over, and a slow serial console still held the hint's tail.
        unsafe { libc::tcdrain(1) };
    }
    if let Some(entry) = counted
        && !found.is_empty()
    {
        let failure = MountFailure {
            entry,
            mounts: found,
            at: crate::generations::now(),
        };
        if let Err(e) = write(paths, &failure) {
            println!("archix: couldn't record the missing mounts for the next boot: {e:#}");
        }
    }
}

fn write(paths: &Paths, failure: &MountFailure) -> Result<()> {
    let path = record(paths);
    fs::create_dir_all(path.parent().context("the record has a directory")?)?;
    crate::durable::write(&path, serde_json::to_string_pretty(failure)?.as_bytes())
}

/// What a fallback from an entry whose tries stopped in emergency mode says at login and in `archix status`.
pub fn fallback_note(failure: &MountFailure) -> String {
    let from = &failure.entry;
    let list: Vec<String> = failure.mounts.iter().map(Unmounted::describe).collect();
    let list = match list.split_last() {
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
        None => "a required mount".to_string(),
    };
    let one = failure.mounts.len() == 1;
    if failure.mounts.iter().all(|mount| !mount.attached) {
        return format!(
            "archix: {from} failed its boot tries because {list} {} missing, not because of the system, so systemd-boot fell back; reattach it and reboot",
            if one { "was" } else { "were" }
        );
    }
    format!(
        "archix: {from} failed its boot tries in emergency mode, as {list} didn't mount (last at {}), so systemd-boot fell back; switch again once {} mounts",
        super::health::utc(failure.at),
        if one { "it" } else { "they" }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mount(point: &str, device: &str, attached: bool) -> Unmounted {
        Unmounted {
            point: point.into(),
            device: device.into(),
            attached,
        }
    }

    #[test]
    fn only_required_lines_with_nothing_mounted_count_as_unmounted() {
        let fstab = "# fstab\nUUID=archix-root / ext4 rw 0 1\n\
                     proc /proc proc defaults 0 0\n\
                     UUID=archix-test-home /home ext4 defaults 0 0\n\
                     UUID=archix-test-data /mnt/data ext4 nofail,x-systemd.device-timeout=30s 0 2\n\
                     UUID=AB12 /efi vfat x-systemd.automount 0 2\n\
                     UUID=archix-test-later /mnt/later ext4 noauto 0 0\n\
                     /dev/mapper/archixtestvol /srv/vol ext4 defaults 0 0\n\
                     /dev/mapper/archixtestnull /srv/null ext4 defaults 0 0\n\
                     /var/archix-test-www /srv/www none bind 0 0\n\
                     /swapfile none swap defaults 0 0\n";
        assert_eq!(
            unmounted(Path::new("/"), fstab, "archixtestnull /dev/null none luks\n"),
            [
                mount("/home", "UUID=archix-test-home", false),
                mount("/srv/vol", "/dev/mapper/archixtestvol", false),
                mount("/srv/null", "/dev/mapper/archixtestnull", true),
                mount("/srv/www", "/var/archix-test-www", true),
            ],
            "/proc is mounted; a volume whose LUKS device is there is attached; nofail, noauto, automount and swap lines don't stop a boot"
        );
    }

    #[test]
    fn the_hint_names_each_mount_then_what_to_do() {
        let missing = [mount("/home", "UUID=h", false)];
        assert_eq!(
            hint(&missing, None, true),
            [
                "archix: this boot stopped in emergency mode because required mounts in /etc/fstab didn't mount:",
                "archix:   /home (UUID=h): its disk isn't attached",
                "archix: reattach the disk and reboot,",
                "archix: or start the archix rescue entry from the boot menu (hold Space at power-on)",
            ]
        );
        let failed = [mount("/srv/x", "UUID=s", true)];
        assert_eq!(
            hint(&failed, Some("archix-current.conf"), false),
            [
                "archix: this boot stopped in emergency mode because required mounts in /etc/fstab didn't mount:",
                "archix:   /srv/x (UUID=s): its device is there, but it didn't mount; `journalctl -b -u srv-x.mount` says why",
                "archix: this is one of archix-current.conf's counted boot tries: each boot that stops here uses one, then systemd-boot falls back to how the system last booted fine; it restarts after 2 minutes with nobody logged in",
            ]
        );
        assert!(hint(&[], None, true)[0].contains("every required mount in /etc/fstab is mounted"));
    }

    #[test]
    fn the_drop_in_runs_the_hint_before_sulogin_and_never_stops_it() {
        assert_eq!(
            drop_in(true),
            "[Service]\nExecStartPre=-/usr/bin/archix emergency-hint\n"
        );
        assert!(drop_in(false).ends_with("emergency-hint --no-rescue-entry\n"));
    }

    #[test]
    fn a_fallback_for_a_missing_disk_says_the_system_isnt_to_blame() {
        let failure = MountFailure {
            entry: "archix-current.conf".into(),
            mounts: vec![mount("/home", "UUID=h", false)],
            at: 1_792_576_985,
        };
        assert_eq!(
            fallback_note(&failure),
            "archix: archix-current.conf failed its boot tries because /home (UUID=h) was missing, not because of the system, so systemd-boot fell back; reattach it and reboot"
        );
        let both = MountFailure {
            mounts: vec![mount("/home", "UUID=h", false), mount("/srv", "UUID=s", false)],
            ..failure.clone()
        };
        assert!(fallback_note(&both).contains("because /home (UUID=h) and /srv (UUID=s) were missing, not because"));
        let attached = MountFailure {
            mounts: vec![mount("/home", "UUID=h", false), mount("/srv", "UUID=s", true)],
            ..failure
        };
        let note = fallback_note(&attached);
        assert!(
            note.contains(
                "in emergency mode, as /home (UUID=h) and /srv (UUID=s) didn't mount (last at 2026-10-21 10:03:05 UTC)"
            ) && !note.contains("not because of the system"),
            "{note}"
        );
    }
}
