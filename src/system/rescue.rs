//! The rescue boot entry: an image of its own, from the current kernel, whose busybox initramfs mounts nothing of the layout (no binds, no @varlib) and opens `archix rescue`, so a machine whose every entry is broken is repaired without a live USB.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::boot::{self, BootPlan};
use crate::paths::Paths;

/// The rescue entry's file name, which no menu entry parses as.
pub const ENTRY: &str = "archix-rescue.conf";
/// Where its images live on the ESP, apart from EFI/archix, whose unused images the menu deletes.
pub const DIR: &str = "EFI/archix-rescue";
/// On the kernel command line: mount nothing, open the rescue prompt.
pub const FLAG: &str = "archix.rescue";
/// Sorts after every other archix entry, so systemd-boot only starts it when picked, or when every counted entry is bad.
pub const SORT_KEY: &str = "archix-rescue";

/// The rescue initramfs's mkinitcpio config: busybox's init, every storage, filesystem and keyboard module, no autodetect and no snapshot hook.
pub const MKINITCPIO_CONF: &str =
    "MODULES=()\nBINARIES=()\nFILES=()\nHOOKS=(base udev modconf keyboard keymap block filesystems archix-rescue)\n";

/// The rescue hook's install script: the root's static archix and the tools the rescue commands run.
pub const INSTALL_HOOK: &str = r#"#!/bin/bash

build() {
    local tool
    add_binary /usr/lib/archix/archix-static /usr/bin/archix
    for tool in e2fsck fsck.fat btrfs cryptsetup lvm bsdtar; do
        if type -P "$tool" > /dev/null; then
            add_binary "$tool"
        fi
    done
    map add_module 'vfat?' 'nls_cp437?' 'nls_iso8859_1?' 'nls_ascii?' 'efivarfs?' 'dm_mod?' 'dm_crypt?'
    # What cryptsetup needs here, as mkinitcpio's encrypt hook adds it: without the dm udev rules it waits forever for udev after the passphrase.
    if type -P cryptsetup > /dev/null; then
        add_all_modules '/crypto/'
        add_all_modules '/lib/crypto/'
        map add_udev_rule '10-dm.rules' '13-dm-disk.rules' '95-dm-notify.rules'
        add_binary /usr/lib/libgcc_s.so.1
    fi
    add_runscript
}

help() {
    echo "archix rescue: with archix.rescue on the kernel command line, mounts nothing and opens archix's rescue prompt"
}
"#;

/// The rescue hook's boot script, run after udev has found the disks: whatever the command line says it opens `archix rescue`, whose login guards its prompt and shell, and restarts after, never falling through to mkinitcpio's shells.
pub const RUN_HOOK: &str = r#"#!/usr/bin/ash

run_hook() {
    if cttyhack /usr/bin/archix rescue; then
        echo ":: archix rescue: restarting"
    else
        err "archix rescue stopped or stayed closed; the machine restarts in 15 s"
        sleep 15
    fi
    reboot -f
    poweroff -f
}
"#;

/// Kernel command line words that would start a shell instead of, or after, the rescue's init, which its command line never carries over from boot.cmdline.
const SHELL_WORDS: &[&str] = &[
    "init=",
    "rdinit=",
    "break",
    "rd.break",
    "rd.shell",
    "rd.emergency",
    "emergency",
    "single",
    "systemd.unit=",
    "systemd.debug_shell",
    "systemd.debug-shell",
];

/// The rescue UKI's command line: `cmdline` without SHELL_WORDS, then FLAG.
pub fn rescue_cmdline(cmdline: &str) -> String {
    let mut words: Vec<&str> = cmdline
        .split_whitespace()
        .filter(|word| {
            !SHELL_WORDS.iter().any(|shell| match shell.strip_suffix('=') {
                Some(key) => word.split('=').next() == Some(key),
                None => word.split('=').next() == Some(shell),
            })
        })
        .collect();
    words.push(FLAG);
    words.join(" ")
}

/// Whether a kernel command line asks for the rescue prompt.
pub fn requested(cmdline: &str) -> bool {
    cmdline.split_whitespace().any(|word| word == FLAG)
}

pub fn entry_path(esp: &Path) -> PathBuf {
    esp.join("loader/entries").join(ENTRY)
}

/// The entry's text: no boot counters, so systemd-boot never counts it, and the last sort key, so it starts only when picked or when every counted entry is bad.
pub fn entry_text(uki: &str, kernel: &str) -> String {
    format!("title archix rescue (kernel {kernel})\nsort-key {SORT_KEY}\nefi /{DIR}/{uki}\n")
}

/// The UKI the rescue entry starts, if the entry is there.
pub fn uki(esp: &Path) -> Option<PathBuf> {
    let text = fs::read_to_string(entry_path(esp)).ok()?;
    text.lines()
        .find_map(|line| line.strip_prefix("efi "))
        .map(|path| esp.join(path.trim().trim_start_matches('/')))
}

/// What the rescue entry on the ESP was built from, kept on the root.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
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
            "the rescue entry ({ENTRY}) boots kernel {} into `archix rescue`",
            record.kernel
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

/// Deletes the rescue images the entry doesn't name, once the entry naming the new one is flushed.
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

/// What everything the rescue image is built from hashes to, so an unchanged one isn't built again.
fn key(vmlinuz: &str, kernel: &str, cmdline: &str, signed: bool) -> String {
    let parts = [
        vmlinuz,
        kernel,
        cmdline,
        if signed { "signed" } else { "unsigned" },
        MKINITCPIO_CONF,
        INSTALL_HOOK,
        RUN_HOOK,
    ];
    crate::fetch::sha256_bytes(parts.join("\n").as_bytes())
}

/// Where the rescue image is built in the root.
const BUILT: &str = "/var/tmp/archix-rescue";

fn in_root(root: &dyn boot::BootRoot, argv: &[&str], what: &str) -> Result<String> {
    let output = root.output(argv)?;
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

/// Builds the rescue image in the root from `kernel`, signed when `signed`, and returns where it is.
fn build(
    paths: &Paths,
    esp: &Path,
    root: &dyn boot::BootRoot,
    kernel: &str,
    cmdline: &str,
    name: &str,
    signed: bool,
) -> Result<PathBuf> {
    for (path, text) in [
        ("/etc/initcpio/install/archix-rescue", INSTALL_HOOK),
        ("/etc/initcpio/hooks/archix-rescue", RUN_HOOK),
        ("/etc/archix-rescue/mkinitcpio.conf", MKINITCPIO_CONF),
    ] {
        let target = root.read_path(Path::new(path))?;
        if let Some(dir) = target.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(&target, text).with_context(|| format!("writing {}", target.display()))?;
    }
    in_root(root, &["rm", "-rf", BUILT], "clearing the rescue build directory")?;
    in_root(root, &["mkdir", "-p", BUILT], "creating the rescue build directory")?;
    let initramfs = format!("{BUILT}/initramfs.img");
    in_root(
        root,
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
    let os_release = in_root(root, &["cat", "/etc/os-release"], "reading /etc/os-release")?;
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
    in_root(root, &ukify, "ukify build")?;
    let file = root.read_path(Path::new(&output))?;
    if signed {
        let status = std::process::Command::new("sbctl")
            .env("ESP_PATH", esp)
            .arg("sign")
            .arg(&file)
            .output()
            .context("running sbctl; is it installed?")?;
        if !status.status.success() {
            bail!(
                "sbctl sign failed for the rescue image: {}",
                String::from_utf8_lossy(&status.stderr).trim()
            );
        }
        super::revoke::record_signed(paths, &file, name)?;
    }
    Ok(file)
}

/// Keeps the rescue entry in step after a switch: built again from the current kernel when that or anything it's built from changed, put on the ESP beside the old one, which goes once the entry names the new one; with `boot.rescue_entry = false`, or on Limine, taken off. Returns what it did.
pub fn refresh(paths: &Paths, plan: &BootPlan, root: &dyn boot::BootRoot) -> Result<String> {
    let esp = Path::new(&plan.esp);
    if !plan.rescue || plan.loader == boot::Loader::Limine {
        return Ok(match remove(paths, esp)? {
            true => "rescue     took the rescue entry off the ESP, as boot.rescue_entry = false".to_string(),
            false => String::new(),
        });
    }
    let local_db = root.local_db();
    let kernel = boot::owned_version(&local_db, &plan.kernel)
        .or_else(|| {
            boot::kernels_in(&local_db)
                .into_iter()
                .next()
                .map(|(_, version)| version)
        })
        .context("no kernel is installed for the rescue entry")?;
    let signed = plan.secure_boot.unwrap_or_else(boot::sbctl_has_keys);
    if boot::firmware_enforces() && !signed && !super::secureboot::in_chroot() {
        bail!(
            "Secure Boot is on, but the rescue entry wouldn't be signed, so the firmware would refuse it; the rescue entry stays as it was"
        );
    }
    let cmdline = rescue_cmdline(&plan.cmdline);
    let vmlinuz = root.read_path(&Path::new(boot::MODULES).join(&kernel).join("vmlinuz"))?;
    let key = key(&crate::fetch::sha256(&vmlinuz)?, &kernel, &cmdline, signed);
    let name = format!("rescue-{}.efi", &key[..12]);
    if let Some(record) = read_record(paths)
        && record.key == key
        && uki(esp).is_some_and(|uki| uki == esp.join(DIR).join(&record.uki) && uki.is_file())
    {
        return Ok(String::new());
    }
    eprintln!(":: building the rescue entry from kernel {kernel}");
    let built = build(paths, esp, root, &kernel, &cmdline, &name, signed)?;
    let size = fs::metadata(&built)?.len();
    fs::create_dir_all(esp.join(DIR))?;
    let free = boot::free_bytes(&esp.join(DIR))?;
    if free < size + boot::MARGIN {
        bail!(
            "the ESP has {} MiB free, but the rescue image needs {} MiB; the rescue entry stays as it was",
            free >> 20,
            (size + boot::MARGIN) >> 20
        );
    }
    crate::durable::copy(&built, &esp.join(DIR).join(&name))?;
    crate::durable::sync_esp(esp)?;
    let _ = fs::remove_dir_all(root.read_path(Path::new(BUILT))?);
    crate::testhooks::mark("rescue-uki");
    crate::durable::write(&entry_path(esp), entry_text(&name, &kernel).as_bytes())?;
    crate::durable::sync_esp(esp)?;
    crate::testhooks::mark("rescue-entry");
    remove_unused(esp, &name)?;
    let record = Record {
        kernel: kernel.clone(),
        key,
        uki: name,
    };
    crate::durable::write(&record_path(paths), serde_json::to_string_pretty(&record)?.as_bytes())?;
    Ok(format!(
        "rescue     the rescue entry boots kernel {kernel} into `archix rescue`"
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

    #[test]
    fn only_the_rescue_flag_itself_asks_for_the_rescue() {
        assert!(requested("root=UUID=x rw archix.rescue quiet"));
        assert!(requested("archix.rescue"));
        assert!(!requested("root=UUID=x archix.rescued"));
        assert!(!requested("root=UUID=x archix.rescue=0x"));
        assert!(!requested(""));
    }

    #[test]
    fn the_rescue_entry_is_no_menu_entry_and_sorts_after_them() {
        assert_eq!(super::super::menu::Entry::parse(ENTRY), None);
        assert!(super::super::menu::SORT_EXTRA < SORT_KEY);
        let text = entry_text("rescue-abc.efi", "7.1.2-arch1-1");
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
    fn taking_the_rescue_entry_off_removes_the_entry_then_its_ukis() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::under(root.path());
        let esp = esp_with(&["archix-2.conf"]);
        assert!(!remove(&paths, esp.path()).unwrap(), "nothing to take off");
        fs::create_dir_all(esp.path().join(DIR)).unwrap();
        fs::write(esp.path().join(DIR).join("rescue-a.efi"), "uki").unwrap();
        fs::write(entry_path(esp.path()), entry_text("rescue-a.efi", "7.1")).unwrap();
        assert_eq!(uki(esp.path()), Some(esp.path().join(DIR).join("rescue-a.efi")));
        assert_eq!(fs::metadata(uki(esp.path()).unwrap()).unwrap().len(), 3);
        assert!(remove(&paths, esp.path()).unwrap());
        assert!(!entry_path(esp.path()).exists() && !esp.path().join(DIR).exists());
        assert!(esp.path().join("loader/entries/archix-2.conf").exists());
    }

    /// RUN_HOOK under sh with `archix` standing in for the rescue, mkinitcpio's shell and the reboot stubbed.
    fn run_hook_with(archix: &str, cmdline: &str) -> std::process::Output {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("cmdline");
        fs::write(&file, cmdline).unwrap();
        let script = format!(
            "{}\nerr() {{ echo \"ERR $*\"; }}\nlaunch_interactive_shell() {{ echo SHELL; }}\ncttyhack() {{ \"$@\"; }}\nsleep() {{ echo \"SLEEP $*\"; }}\nreboot() {{ echo \"REBOOT $*\"; exit 4; }}\npoweroff() {{ echo \"POWEROFF $*\"; exit 5; }}\nrun_hook\necho FELL THROUGH\n",
            RUN_HOOK
                .replace("/usr/bin/archix rescue", archix)
                .replace("/proc/cmdline", &file.display().to_string())
        );
        std::process::Command::new("sh").arg("-c").arg(script).output().unwrap()
    }

    #[test]
    fn the_rescue_hook_restarts_after_the_rescue_and_never_opens_a_shell() {
        for (archix, cmdline) in [
            ("false", "root=UUID=x archix.rescue"),
            ("true", "root=UUID=x archix.rescue"),
            ("true", "root=UUID=x break=premount"),
            ("false", "root=UUID=x"),
        ] {
            let out = run_hook_with(archix, cmdline);
            let text = String::from_utf8_lossy(&out.stdout);
            assert_eq!(out.status.code(), Some(4), "{archix} with {cmdline}: {text}");
            assert!(
                text.contains("REBOOT -f") && !text.contains("SHELL") && !text.contains("FELL THROUGH"),
                "{archix} with {cmdline}: {text}"
            );
        }
        let failed = String::from_utf8_lossy(&run_hook_with("false", "archix.rescue").stdout).into_owned();
        assert!(failed.contains("SLEEP 15"), "{failed}");
    }

    #[test]
    fn the_rescue_command_line_drops_words_that_would_start_a_shell() {
        assert_eq!(
            rescue_cmdline(
                " root=UUID=x rw init=/bin/sh rdinit=/bin/sh break=premount rd.break rd.shell=1 systemd.unit=emergency.target emergency single quiet initrd=x "
            ),
            "root=UUID=x rw quiet initrd=x archix.rescue"
        );
        assert_eq!(rescue_cmdline("root=/dev/vda2 rw"), "root=/dev/vda2 rw archix.rescue");
    }
}
