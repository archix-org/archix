//! The rescue boot entry: a UKI of its own, from the kernel of the newest deployment that booted fine, whose busybox initramfs mounts no deployment and opens `archix rescue`, so a machine whose every deployment is broken is repaired without a live USB.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::boot::{self, BootPlan};
use crate::deploy::{Staging, Store};
use crate::paths::Paths;

/// The rescue entry's file name, which no generation's entry parses as.
pub const ENTRY: &str = "archix-rescue.conf";
/// Where its UKIs live on the ESP, apart from EFI/archix, whose unused UKIs gc deletes.
pub const DIR: &str = "EFI/archix-rescue";
/// On the kernel command line: mount no deployment, open the rescue prompt.
pub const FLAG: &str = "archix.rescue";
/// Sorts after every deployment's entry (`archix`) and the fallback UKI's, so systemd-boot only starts it when picked, or when nothing else is left.
pub const SORT_KEY: &str = "archix-rescue";
/// Where the rescue build puts its initramfs and UKI inside the scratch staging root.
const BUILT: &str = "/boot/archix-rescue";

/// The rescue initramfs's mkinitcpio config: busybox's init, every storage, filesystem and keyboard module, no autodetect and no deployment hook.
pub const MKINITCPIO_CONF: &str =
    "MODULES=()\nBINARIES=()\nFILES=()\nHOOKS=(base udev modconf keyboard keymap block filesystems archix-rescue)\n";

/// The rescue hook's install script: the deployment's static archix and the tools the rescue commands run.
pub const INSTALL_HOOK: &str = r#"#!/bin/bash

build() {
    local tool
    add_binary /usr/lib/archix/archix-static /usr/bin/archix
    for tool in e2fsck fsck.fat btrfs cryptsetup lvm bsdtar; do
        if type -P "$tool" > /dev/null; then
            add_binary "$tool"
        fi
    done
    # Ahead of busybox's on the rescue's PATH: eject copies with GNU cp's --reflink and --no-target-directory.
    add_binary /usr/bin/cp /usr/lib/archix-rescue/cp
    map add_module 'vfat?' 'nls_cp437?' 'nls_iso8859_1?' 'nls_ascii?' 'efivarfs?' 'dm_mod?' 'dm_crypt?'
    add_runscript
}

help() {
    echo "archix rescue: with archix.rescue on the kernel command line, mounts no deployment and opens archix's rescue prompt"
}
"#;

/// The rescue hook's boot script, run after udev has found the disks.
pub const RUN_HOOK: &str = r#"#!/usr/bin/ash

run_hook() {
    case " $(cat /proc/cmdline) " in
        *" archix.rescue "*) ;;
        *) return 0 ;;
    esac
    export PATH="/usr/lib/archix-rescue:$PATH"
    if ! cttyhack /usr/bin/archix rescue; then
        err "archix rescue failed; this shell is what's left, and the machine restarts when it exits"
        launch_interactive_shell
    fi
    echo ":: archix rescue: restarting"
    reboot -f
}
"#;

/// Whether a kernel command line asks for the rescue prompt.
pub fn requested(cmdline: &str) -> bool {
    cmdline.split_whitespace().any(|word| word == FLAG)
}

pub fn entry_path(esp: &Path) -> PathBuf {
    esp.join("loader/entries").join(ENTRY)
}

/// The entry's text: no version and no boot counters, so systemd-boot never counts it or falls back to it.
pub fn entry_text(uki: &str, source: u32, kernel: &str) -> String {
    format!(
        "title archix rescue (no deployment; kernel {kernel} of deployment {source})\nsort-key {SORT_KEY}\nefi /{DIR}/{uki}\n"
    )
}

/// The UKI the rescue entry starts, if the entry is there.
pub fn uki(esp: &Path) -> Option<PathBuf> {
    let text = fs::read_to_string(entry_path(esp)).ok()?;
    text.lines()
        .find_map(|line| line.strip_prefix("efi "))
        .map(|path| esp.join(path.trim().trim_start_matches('/')))
}

/// The size of the rescue UKI on the ESP, which the next refresh writes again beside it.
pub fn uki_size(esp: &Path) -> Option<u64> {
    fs::metadata(uki(esp)?).ok().map(|meta| meta.len())
}

/// How a deployment's own boot entry stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryState {
    Missing,
    /// Not counted, and the deployment never activated: it hasn't booted yet.
    NotBooted,
    /// Not counted any more, or never, and the deployment has booted.
    Good,
    /// systemd-boot is still counting its tries; the number left.
    Counting(u32),
    /// No tries left.
    Bad,
}

/// Whether deployment `number` has ever activated, which its first boot records.
pub fn activated(paths: &Paths, number: u32) -> bool {
    let prefix = format!("{number}-");
    fs::read_dir(paths.state.join("activated"))
        .into_iter()
        .flatten()
        .flatten()
        .any(|marker| marker.file_name().to_string_lossy().starts_with(&prefix))
}

pub fn entry_state(paths: &Paths, esp: &Path, number: u32) -> EntryState {
    let Some(name) = boot::entries(esp)
        .into_iter()
        .find(|(n, extra, _)| *n == number && !extra)
        .map(|(_, _, path)| path.file_name().unwrap_or_default().to_string_lossy().into_owned())
    else {
        return EntryState::Missing;
    };
    if boot::is_bad(&name) {
        return EntryState::Bad;
    }
    if let Some((_, counters)) = name.split_once('+') {
        let left = counters
            .split(['-', '.'])
            .next()
            .and_then(|n| n.parse().ok())
            .unwrap_or(0);
        return EntryState::Counting(left);
    }
    if activated(paths, number) {
        EntryState::Good
    } else {
        EntryState::NotBooted
    }
}

/// The deployment whose kernel the rescue entry boots: the newest whose entry booted fine, else the running one, else `current`.
pub fn source(paths: &Paths, esp: &Path, committed: &[u32], booted: Option<u32>, current: Option<u32>) -> Option<u32> {
    committed
        .iter()
        .rev()
        .copied()
        .find(|n| entry_state(paths, esp, *n) == EntryState::Good)
        .or(booted.filter(|n| committed.contains(n)))
        .or(current.filter(|n| committed.contains(n)))
}

/// What the rescue entry on the ESP was built from, kept on the root.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub source: u32,
    pub kernel: String,
    pub key: String,
    pub uki: String,
}

fn record_path(paths: &Paths) -> PathBuf {
    paths.state.join("boot").join("rescue.json")
}

pub fn read_record(paths: &Paths) -> Option<Record> {
    serde_json::from_str(&fs::read_to_string(record_path(paths)).ok()?).ok()
}

/// The rescue entry's state for `archix status`.
pub fn describe(paths: &Paths, esp: &Path) -> String {
    match (uki(esp).filter(|uki| uki.is_file()), read_record(paths)) {
        (Some(_), Some(record)) => format!(
            "the rescue entry ({ENTRY}) boots kernel {} of deployment {} into `archix rescue`",
            record.kernel, record.source
        ),
        (Some(_), None) => format!("the rescue entry ({ENTRY}) is on the ESP"),
        (None, _) => format!(
            "there's no rescue entry on the ESP; the next switch writes {ENTRY} unless boot.rescue_entry = false"
        ),
    }
}

/// Takes the rescue entry off the ESP, the entry before its UKIs, each removal flushed; false when there was none.
pub fn remove_from_esp(esp: &Path) -> Result<bool> {
    let entry = entry_path(esp);
    let dir = esp.join(DIR);
    let had = entry.exists() || dir.exists();
    match fs::remove_file(&entry) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            return Err(e).with_context(|| format!("removing {}", entry.display()));
        }
        _ => {}
    }
    if !had {
        return Ok(false);
    }
    crate::durable::sync_esp(esp)?;
    match fs::remove_dir_all(&dir) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            return Err(e).with_context(|| format!("removing {}", dir.display()));
        }
        _ => {}
    }
    crate::durable::sync_esp(esp)?;
    Ok(true)
}

/// Takes the rescue entry off the ESP and forgets what it was built from.
pub fn remove(paths: &Paths, esp: &Path) -> Result<bool> {
    let removed = remove_from_esp(esp)?;
    let _ = fs::remove_file(record_path(paths));
    Ok(removed)
}

/// Deletes the rescue UKIs the entry doesn't name, once the entry naming the new one is flushed.
fn remove_unused(esp: &Path, keep: &str) -> Result<()> {
    let mut removed = false;
    for file in fs::read_dir(esp.join(DIR)).into_iter().flatten().flatten() {
        if file.file_name().to_string_lossy() != keep {
            fs::remove_file(file.path()).with_context(|| format!("removing {}", file.path().display()))?;
            removed = true;
        }
    }
    if removed {
        crate::durable::sync_esp(esp)?;
    }
    Ok(())
}

/// What everything the rescue UKI is built from hashes to, so an unchanged one isn't built again.
fn key(source_id: &str, kernel: &str, cmdline: &str, signed: bool) -> String {
    let parts = [
        source_id,
        kernel,
        cmdline,
        if signed { "signed" } else { "unsigned" },
        MKINITCPIO_CONF,
        INSTALL_HOOK,
        RUN_HOOK,
    ];
    crate::fetch::sha256_bytes(parts.join("\n").as_bytes())
}

fn in_view(tx: &crate::deploy::Transaction, argv: &[&str], what: &str) -> Result<String> {
    let output = tx.output(argv)?;
    if !output.status.success() {
        let text = String::from_utf8_lossy(&output.stdout).into_owned() + &String::from_utf8_lossy(&output.stderr);
        let tail: Vec<&str> = text.lines().rev().take(12).collect();
        bail!(
            "{what} failed for the rescue entry:\n{}",
            tail.into_iter().rev().collect::<Vec<_>>().join("\n")
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Builds the rescue UKI in a scratch staging root of deployment `source` and returns where it is in there, signed when `signed`.
fn build(
    paths: &Paths,
    esp: &Path,
    staging: &Staging,
    kernel: &str,
    cmdline: &str,
    name: &str,
    signed: bool,
) -> Result<PathBuf> {
    let etc = staging.tree("etc");
    for (path, text) in [
        ("initcpio/install/archix-rescue", INSTALL_HOOK),
        ("initcpio/hooks/archix-rescue", RUN_HOOK),
        ("archix-rescue/mkinitcpio.conf", MKINITCPIO_CONF),
    ] {
        let target = etc.join(path);
        fs::create_dir_all(target.parent().unwrap_or(&etc))?;
        fs::write(&target, text).with_context(|| format!("writing {}", target.display()))?;
    }
    let tx = staging.transaction()?;
    in_view(&tx, &["rm", "-rf", BUILT], "clearing /boot/archix-rescue")?;
    in_view(&tx, &["mkdir", "-p", BUILT], "creating /boot/archix-rescue")?;
    let initramfs = format!("{BUILT}/initramfs.img");
    in_view(
        &tx,
        &[
            "mkinitcpio",
            "-k",
            kernel,
            "-c",
            "/etc/archix-rescue/mkinitcpio.conf",
            "-g",
            &initramfs,
        ],
        "mkinitcpio",
    )?;
    let os_release = in_view(&tx, &["cat", "/etc/os-release"], "reading /etc/os-release")?;
    let mut release: Vec<String> = os_release
        .lines()
        .filter(|line| {
            !["PRETTY_NAME=", "IMAGE_ID=", "IMAGE_VERSION="]
                .iter()
                .any(|key| line.starts_with(key))
        })
        .map(str::to_string)
        .collect();
    release.push("PRETTY_NAME=\"archix rescue\"".to_string());
    release.push(format!("IMAGE_ID={SORT_KEY}"));
    let release = release.join("\n") + "\n";
    let output = format!("{BUILT}/{name}");
    let vmlinuz = format!("{}/{kernel}/vmlinuz", boot::MODULES);
    let mut ukify = vec![
        "ukify",
        "build",
        "--linux",
        &vmlinuz,
        "--initrd",
        &initramfs,
        "--cmdline",
        cmdline,
        "--uname",
        kernel,
        "--os-release",
        &release,
        "--output",
        &output,
    ];
    // A signed PCR policy would let the rescue's root shell unseal TPM-bound volumes, so it never gets one.
    let unique = format!("--section=.archix:{}", super::password::salt()?);
    if super::revoke::any_revoked(paths) {
        ukify.push(&unique);
    }
    in_view(&tx, &ukify, "ukify build")?;
    drop(tx);
    let file = staging.read_path(Path::new(&output))?;
    if signed {
        let status = std::process::Command::new("sbctl")
            .env("ESP_PATH", esp)
            .arg("sign")
            .arg(&file)
            .output()
            .context("running sbctl; is it installed?")?;
        if !status.status.success() {
            bail!(
                "sbctl sign failed for the rescue UKI: {}",
                String::from_utf8_lossy(&status.stderr).trim()
            );
        }
        super::revoke::record_signed(paths, &file, name)?;
    }
    Ok(file)
}

/// The tools a rescue boot needs to check this root that deployment `deployment` lacks, by package.
fn missing_tools(root: &Path, deployment: &Path) -> Vec<&'static str> {
    let has = |tool: &str| deployment.join("usr/bin").join(tool).is_file();
    let mut missing = Vec::new();
    match crate::deploy::Backend::detect(&root.join("archix")) {
        Ok(crate::deploy::Backend::Snapshot) if !has("btrfs") => missing.push("btrfs-progs"),
        Ok(crate::deploy::Backend::Overlay) if is_ext(root) && !has("e2fsck") => missing.push("e2fsprogs"),
        _ => {}
    }
    if !has("fsck.fat") {
        missing.push("dosfstools");
    }
    missing
}

fn is_ext(path: &Path) -> bool {
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) else {
        return false;
    };
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    unsafe { libc::statfs(c_path.as_ptr(), &mut stat) == 0 && stat.f_type as u64 == 0xef53 }
}

/// Keeps the rescue entry in step after a switch: built again in a scratch staging root of the newest deployment that booted fine when that or anything it's built from changed, put on the ESP beside the old one, which goes once the entry names the new one; with `boot.rescue_entry = false`, taken off. Returns what it did.
pub fn refresh(paths: &Paths, plan: &BootPlan, store: &Store) -> Result<String> {
    let esp = Path::new(&plan.esp);
    if !plan.rescue {
        return Ok(match remove(paths, esp)? {
            true => "rescue     took the rescue entry off the ESP, as boot.rescue_entry = false".to_string(),
            false => String::new(),
        });
    }
    let committed = store.deployments()?;
    let Some(source) = source(
        paths,
        esp,
        &committed,
        boot::booted_deployment(store.root()),
        store.current()?,
    ) else {
        return Ok(String::new());
    };
    let deployment = store.path(source);
    let kernel = boot::owned_version(&deployment.join("pacman"), &plan.kernel)
        .or_else(|| {
            boot::kernels_in(&deployment.join("pacman"))
                .into_iter()
                .next()
                .map(|(_, version)| version)
        })
        .with_context(|| format!("deployment {source} has no kernel for the rescue entry"))?;
    let signed = plan.secure_boot.unwrap_or_else(boot::sbctl_has_keys);
    let enforcing = super::secureboot::Machine::new()
        .state()
        .is_some_and(|state| state.enabled && !state.setup_mode);
    if enforcing && !signed && !super::secureboot::in_chroot() {
        bail!(
            "Secure Boot is on, but the rescue entry wouldn't be signed, so the firmware would refuse it; the rescue entry stays as it was"
        );
    }
    let cmdline = format!("{} {FLAG}", plan.cmdline.trim());
    let key = key(
        &super::activate::deployment_id(&deployment, source)?,
        &kernel,
        &cmdline,
        signed,
    );
    let name = format!("rescue-{}.efi", &key[..12]);
    let warn = match missing_tools(store.root(), &deployment).as_slice() {
        [] => String::new(),
        missing => format!(
            "; deployment {source} lacks {}, so the rescue can't check this root and the ESP with it; declare it",
            missing.join(" and ")
        ),
    };
    if let Some(record) = read_record(paths)
        && record.key == key
        && uki(esp).is_some_and(|uki| uki == esp.join(DIR).join(&record.uki) && uki.is_file())
    {
        return Ok(String::new());
    }
    eprintln!(":: building the rescue entry from deployment {source}'s kernel {kernel}");
    let staging = Staging::scratch(store, source)?;
    let written = build(paths, esp, &staging, &kernel, &cmdline, &name, signed).and_then(|built| {
        let size = fs::metadata(&built)?.len();
        let free = boot::free_bytes(&esp.join("loader"))?;
        if free < size + boot::MARGIN {
            let mib = |bytes: u64| bytes / (1024 * 1024);
            bail!(
                "the ESP has {} MiB free, but the rescue UKI needs {} MiB; the rescue entry stays as it was",
                mib(free),
                mib(size + boot::MARGIN)
            );
        }
        crate::durable::copy(&built, &esp.join(DIR).join(&name))?;
        crate::durable::sync_esp(esp)
    });
    let discarded = staging.discard();
    written?;
    discarded?;
    crate::testhooks::mark("rescue-uki");
    crate::durable::write(&entry_path(esp), entry_text(&name, source, &kernel).as_bytes())?;
    crate::durable::sync_esp(esp)?;
    crate::testhooks::mark("rescue-entry");
    remove_unused(esp, &name)?;
    let record = Record {
        source,
        kernel: kernel.clone(),
        key,
        uki: name,
    };
    crate::durable::write(&record_path(paths), serde_json::to_string_pretty(&record)?.as_bytes())?;
    Ok(format!(
        "rescue     the rescue entry boots deployment {source}'s kernel {kernel} into `archix rescue`{warn}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn esp_with(entries: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("loader/entries")).unwrap();
        for name in entries {
            fs::write(dir.path().join("loader/entries").join(name), "title t\n").unwrap();
        }
        dir
    }

    fn activate(paths: &Paths, number: u32) {
        let dir = paths.state.join("activated");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(format!("{number}-77-0-abc")), "").unwrap();
    }

    #[test]
    fn only_the_rescue_flag_itself_asks_for_the_rescue() {
        assert!(requested("root=UUID=x rw archix.rescue quiet"));
        assert!(requested("archix.rescue"));
        assert!(!requested("root=UUID=x archix.rescued"));
        assert!(!requested("root=UUID=x archix.rescue=0x"));
        assert!(!requested(""));
    }

    #[test]
    fn the_rescue_entry_is_no_generations_entry_and_sorts_after_them() {
        assert_eq!(boot::parse_entry(ENTRY), None);
        assert_eq!(crate::deploy::keep::entry_number(ENTRY), None);
        assert_eq!(crate::system::deploy_boot::generation_of_entry(ENTRY), None);
        assert!(boot::FALLBACK_SORT_KEY < "archix" && "archix" < SORT_KEY);
        let text = entry_text("rescue-abc.efi", 4, "7.1.2-arch1-1");
        assert!(
            text.contains(&format!("sort-key {SORT_KEY}\n"))
                && text.contains("efi /EFI/archix-rescue/rescue-abc.efi\n")
        );
        assert!(
            !text.contains("version") && !ENTRY.contains('+'),
            "no version and no counters: {text}"
        );
    }

    #[test]
    fn the_newest_deployment_that_booted_fine_gives_the_kernel() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::under(root.path());
        let esp = esp_with(&[
            "archix-1.conf",
            "archix-2.conf",
            "archix-3+0-3.conf",
            "archix-4+3.conf",
            "archix-5.conf",
        ]);
        let committed = [1, 2, 3, 4, 5];
        assert_eq!(
            source(&paths, esp.path(), &committed, Some(4), Some(5)),
            Some(4),
            "none booted fine yet: the running one"
        );
        assert_eq!(
            source(&paths, esp.path(), &committed, None, Some(5)),
            Some(5),
            "nothing running, as at the first layout: current"
        );
        assert_eq!(source(&paths, esp.path(), &committed, None, None), None);
        activate(&paths, 1);
        activate(&paths, 2);
        activate(&paths, 3);
        activate(&paths, 4);
        assert_eq!(
            source(&paths, esp.path(), &committed, Some(4), Some(5)),
            Some(2),
            "3 failed its tries, 4 is still counting, 5 never booted"
        );
        assert_eq!(entry_state(&paths, esp.path(), 3), EntryState::Bad);
        assert_eq!(entry_state(&paths, esp.path(), 4), EntryState::Counting(3));
        assert_eq!(entry_state(&paths, esp.path(), 5), EntryState::NotBooted);
        assert_eq!(entry_state(&paths, esp.path(), 6), EntryState::Missing);
        fs::rename(
            esp.path().join("loader/entries/archix-4+3.conf"),
            esp.path().join("loader/entries/archix-4.conf"),
        )
        .unwrap();
        assert_eq!(
            source(&paths, esp.path(), &committed, Some(4), Some(5)),
            Some(4),
            "4 was blessed"
        );
        assert_eq!(
            source(&paths, esp.path(), &[1, 2], Some(4), Some(5)),
            Some(2),
            "only committed deployments count"
        );
    }

    #[test]
    fn taking_the_rescue_entry_off_removes_the_entry_then_its_ukis() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::under(root.path());
        let esp = esp_with(&["archix-2.conf"]);
        assert!(!remove(&paths, esp.path()).unwrap(), "nothing to take off");
        fs::create_dir_all(esp.path().join(DIR)).unwrap();
        fs::write(esp.path().join(DIR).join("rescue-a.efi"), "uki").unwrap();
        fs::write(entry_path(esp.path()), entry_text("rescue-a.efi", 2, "7.1")).unwrap();
        assert_eq!(uki(esp.path()), Some(esp.path().join(DIR).join("rescue-a.efi")));
        assert_eq!(uki_size(esp.path()), Some(3));
        assert!(remove(&paths, esp.path()).unwrap());
        assert!(!entry_path(esp.path()).exists() && !esp.path().join(DIR).exists());
        assert!(esp.path().join("loader/entries/archix-2.conf").exists());
    }

    #[test]
    fn only_the_rescue_uki_the_entry_names_is_kept() {
        let esp = esp_with(&[]);
        fs::create_dir_all(esp.path().join(DIR)).unwrap();
        for name in ["rescue-old.efi", "rescue-new.efi"] {
            fs::write(esp.path().join(DIR).join(name), "uki").unwrap();
        }
        remove_unused(esp.path(), "rescue-new.efi").unwrap();
        let left: Vec<String> = fs::read_dir(esp.path().join(DIR))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, ["rescue-new.efi"]);
    }

    #[test]
    fn the_key_changes_with_whatever_the_uki_is_built_from() {
        let base = key("4-1-2-abc", "7.1", "root=x archix.rescue", false);
        assert_eq!(base, key("4-1-2-abc", "7.1", "root=x archix.rescue", false));
        for other in [
            key("5-1-2-abc", "7.1", "root=x archix.rescue", false),
            key("4-1-2-abc", "7.2", "root=x archix.rescue", false),
            key("4-1-2-abc", "7.1", "root=y archix.rescue", false),
            key("4-1-2-abc", "7.1", "root=x archix.rescue", true),
        ] {
            assert_ne!(base, other);
        }
    }
}
