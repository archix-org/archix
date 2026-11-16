//! SCOPE 5's snapshots: snapper pre/post pairs marked important=yes, and the wait for limine-snapper-sync's boot entry before the first package or ESP write.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use super::Refused;

/// How long the switch waits for limine-snapper-sync's entry before it refuses.
pub const ENTRY_TIMEOUT: Duration = Duration::from_secs(120);

pub fn snapper_configured(root: &Path) -> bool {
    root.join("etc/snapper/configs/root").exists()
}

pub fn description(n: u32) -> String {
    format!("archix switch {n}")
}

fn parse_number(stdout: &str) -> Result<u32> {
    stdout
        .trim()
        .parse()
        .with_context(|| format!("snapper printed {:?}, not a snapshot number", stdout.trim()))
}

fn snapper(args: &[String]) -> Result<u32> {
    let output = Command::new("snapper").args(args).output().context("running snapper")?;
    if !output.status.success() {
        bail!(
            "snapper {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    parse_number(&String::from_utf8_lossy(&output.stdout))
}

pub fn pre_args(n: u32) -> Vec<String> {
    [
        "-c",
        "root",
        "create",
        "--type",
        "pre",
        "--print-number",
        "--cleanup-algorithm",
        "number",
        "--userdata",
        "important=yes",
        "--description",
    ]
    .map(String::from)
    .into_iter()
    .chain([description(n)])
    .collect()
}

pub fn post_args(n: u32, pre: u32) -> Vec<String> {
    [
        "-c",
        "root",
        "create",
        "--type",
        "post",
        "--pre-number",
        &pre.to_string(),
        "--print-number",
        "--cleanup-algorithm",
        "number",
        "--userdata",
        "important=yes",
        "--description",
    ]
    .map(String::from)
    .into_iter()
    .chain([description(n)])
    .collect()
}

pub fn create_pre(n: u32) -> Result<u32> {
    snapper(&pre_args(n))
}

pub fn create_post(n: u32, pre: u32) -> Result<u32> {
    snapper(&post_args(n, pre))
}

fn shell_value(text: &str, key: &str) -> Option<String> {
    text.lines().rev().find_map(|line| {
        let (k, v) = line.trim().split_once('=')?;
        (k.trim() == key).then(|| v.trim().trim_matches(['"', '\'']).to_string())
    })
}

/// Every limine.conf limine-snapper-sync may write, the configured ESP's first.
pub fn limine_confs(root: &Path) -> Vec<PathBuf> {
    let mut esps: Vec<String> = ["etc/limine-snapper-sync.conf", "etc/default/limine"]
        .iter()
        .filter_map(|f| fs::read_to_string(root.join(f)).ok())
        .filter_map(|text| shell_value(&text, "ESP_PATH"))
        .collect();
    esps.extend(["/boot", "/efi", "/boot/efi"].map(String::from));
    let mut out = Vec::new();
    for esp in esps {
        let esp = root.join(esp.trim_start_matches('/'));
        for rel in [
            "limine.conf",
            "limine/limine.conf",
            "EFI/limine/limine.conf",
            "boot/limine/limine.conf",
        ] {
            let path = esp.join(rel);
            if !out.contains(&path) {
                out.push(path);
            }
        }
    }
    out
}

/// Whether limine.conf's text has a boot entry for snapshot `n`.
pub fn limine_has(text: &str, n: u32) -> bool {
    let needle = format!("/.snapshots/{n}/snapshot");
    text.lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .any(|line| line.contains(&needle))
}

/// Waits until a limine.conf under `root` boots snapshot `n`, refusing after `timeout`.
pub fn wait_for_entry(root: &Path, n: u32, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let found = limine_confs(root)
            .iter()
            .filter_map(|p| fs::read_to_string(p).ok())
            .any(|text| limine_has(&text, n));
        if found {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(Refused(format!(
                "limine-snapper-sync didn't add a boot entry for snapshot {n} to limine.conf within {}s, so there would be nothing to boot back into; check `systemctl status limine-snapper-sync`, then switch again; no package or file was changed",
                timeout.as_secs()
            ))
            .into());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapper_pairs_are_important_and_described_by_the_switch_number() {
        let pre = pre_args(12).join(" ");
        assert!(pre.contains("--type pre --print-number"), "{pre}");
        assert!(pre.contains("--userdata important=yes"), "{pre}");
        assert!(pre.ends_with("--description archix switch 12"), "{pre}");
        let post = post_args(12, 41).join(" ");
        assert!(post.contains("--type post --pre-number 41"), "{post}");
        assert!(post.contains("--userdata important=yes"), "{post}");
        assert_eq!(parse_number("41\n").unwrap(), 41);
        assert!(parse_number("").is_err());
    }

    #[test]
    fn an_entry_is_found_by_its_snapshot_path_only() {
        let conf = "/+Arch Linux\n  //Snapshots\n    ///12 │ 2026-11-16\n      cmdline: rootflags=subvol=/@/.snapshots/12/snapshot rw\n";
        assert!(limine_has(conf, 12));
        assert!(!limine_has(conf, 1), "12 isn't 1");
        assert!(!limine_has("# rootflags=subvol=/@/.snapshots/3/snapshot\n", 3));
    }

    #[test]
    fn the_configured_esp_is_looked_at_first_and_the_wait_refuses_on_timeout() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("etc")).unwrap();
        fs::write(
            root.path().join("etc/limine-snapper-sync.conf"),
            "ESP_PATH=\"/mnt/esp\"\n",
        )
        .unwrap();
        let confs = limine_confs(root.path());
        assert_eq!(confs[0], root.path().join("mnt/esp/limine.conf"));
        let err = wait_for_entry(root.path(), 5, Duration::ZERO).unwrap_err();
        assert!(err.downcast_ref::<Refused>().is_some(), "{err:#}");
        fs::create_dir_all(root.path().join("boot/limine")).unwrap();
        fs::write(
            root.path().join("boot/limine/limine.conf"),
            "cmdline: rootflags=subvol=@/.snapshots/5/snapshot\n",
        )
        .unwrap();
        wait_for_entry(root.path(), 5, Duration::ZERO).unwrap();
    }
}
