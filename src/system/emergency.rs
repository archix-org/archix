//! What emergency mode prints before sulogin in a deployment with required mounts, and the record a counted try that stopped there leaves for the fallback boot's note.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::fstab;
use crate::paths::Paths;

/// The drop-in on emergency.service in every deployment that declares a required mount.
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

/// What emergency.service prints before sulogin: the required mounts that didn't mount and what to do; `counted` is the deployment whose counted try this is.
pub fn hint(unmounted: &[Unmounted], counted: Option<u32>, rescue_entry: bool) -> Vec<String> {
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
    if let Some(generation) = counted {
        lines.push(format!(
            "archix: this is one of deployment {generation}'s counted boot tries: each boot that stops here uses one, then systemd-boot starts the previous deployment"
        ));
    }
    lines
}

/// The required mounts a counted try of a deployment stopped in emergency mode without.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MountFailure {
    pub generation: u32,
    pub mounts: Vec<Unmounted>,
    pub at: u64,
}

fn record(paths: &Paths, generation: u32) -> PathBuf {
    paths.state.join("boot/mount-failed").join(format!("{generation}.json"))
}

pub fn read(paths: &Paths, generation: u32) -> Option<MountFailure> {
    serde_json::from_str(&fs::read_to_string(record(paths, generation)).ok()?).ok()
}

/// Drops a deployment's record, once its fallback note says why or a boot of it reached boot-complete.target.
pub fn forget(paths: &Paths, generation: u32) {
    let _ = fs::remove_file(record(paths, generation));
}

/// Run by emergency.service before sulogin: prints the hint, and on a counted try records what was missing; never fails.
pub fn run(paths: &Paths, rescue_entry: bool) {
    let read = |file: &str| fs::read_to_string(paths.root.join("etc").join(file)).unwrap_or_default();
    let found = unmounted(&paths.root, &read("fstab"), &read("crypttab"));
    let counted = crate::testhooks::counted_boot()
        .then(|| super::boot::booted_deployment(&paths.root))
        .flatten();
    println!();
    for line in hint(&found, counted, rescue_entry) {
        println!("{line}");
    }
    println!();
    if let Some(generation) = counted
        && !found.is_empty()
    {
        let failure = MountFailure {
            generation,
            mounts: found,
            at: crate::generations::now(),
        };
        if let Err(e) = write(paths, &failure) {
            println!("archix: couldn't record the missing mounts for the next boot: {e:#}");
        }
    }
}

fn write(paths: &Paths, failure: &MountFailure) -> Result<()> {
    let path = record(paths, failure.generation);
    fs::create_dir_all(path.parent().context("the record has a directory")?)?;
    crate::durable::write(&path, serde_json::to_string_pretty(failure)?.as_bytes())
}

/// What a fallback from a deployment whose tries stopped in emergency mode says at login and in `archix status`.
pub fn fallback_note(from: u32, to: u32, failure: &MountFailure) -> String {
    let list: Vec<String> = failure.mounts.iter().map(Unmounted::describe).collect();
    let list = match list.split_last() {
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
        None => "a required mount".to_string(),
    };
    let one = failure.mounts.len() == 1;
    if failure.mounts.iter().all(|mount| !mount.attached) {
        return format!(
            "archix: deployment {from} failed its boot tries because {list} {} missing, not because of the deployment, so systemd-boot started {to}; switch again (or `archix rollback {from}`) to retry it",
            if one { "was" } else { "were" }
        );
    }
    format!(
        "archix: deployment {from} failed its boot tries in emergency mode, as {list} didn't mount (last at {}), so systemd-boot started {to}; switch again (or `archix rollback {from}`) once {} mounts",
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
            hint(&failed, Some(7), false),
            [
                "archix: this boot stopped in emergency mode because required mounts in /etc/fstab didn't mount:",
                "archix:   /srv/x (UUID=s): its device is there, but it didn't mount; `journalctl -b -u srv-x.mount` says why",
                "archix: this is one of deployment 7's counted boot tries: each boot that stops here uses one, then systemd-boot starts the previous deployment",
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
    fn a_fallback_for_a_missing_disk_says_the_deployment_isnt_to_blame() {
        let failure = MountFailure {
            generation: 5,
            mounts: vec![mount("/home", "UUID=h", false)],
            at: 1_792_576_985,
        };
        assert_eq!(
            fallback_note(5, 4, &failure),
            "archix: deployment 5 failed its boot tries because /home (UUID=h) was missing, not because of the deployment, so systemd-boot started 4; switch again (or `archix rollback 5`) to retry it"
        );
        let both = MountFailure {
            mounts: vec![mount("/home", "UUID=h", false), mount("/srv", "UUID=s", false)],
            ..failure.clone()
        };
        assert!(
            fallback_note(5, 4, &both).contains("because /home (UUID=h) and /srv (UUID=s) were missing, not because")
        );
        let attached = MountFailure {
            mounts: vec![mount("/home", "UUID=h", false), mount("/srv", "UUID=s", true)],
            ..failure
        };
        let note = fallback_note(5, 4, &attached);
        assert!(
            note.contains(
                "in emergency mode, as /home (UUID=h) and /srv (UUID=s) didn't mount (last at 2026-10-21 10:03:05 UTC)"
            ) && !note.contains("not because of the deployment"),
            "{note}"
        );
    }
}
