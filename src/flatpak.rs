//! System-wide Flatpak apps locked to exact commits, driven through the flatpak CLI.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Remote {
    pub url: String,
    pub gpg_verify: bool,
}

/// An app as the lock and generations record it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LockedApp {
    pub remote: String,
    #[serde(rename = "ref")]
    pub reference: String,
    pub commit: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct InstalledApp {
    pub origin: String,
    pub commit: String,
    pub reference: String,
}

pub fn available() -> bool {
    Command::new("flatpak")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn flatpak(args: &[&str]) -> Result<String> {
    let output = Command::new("flatpak")
        .args(args)
        .env("LC_ALL", "C")
        .output()
        .context("running flatpak")?;
    if !output.status.success() {
        bail!(
            "flatpak {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

pub fn ensure_remotes(remotes: &BTreeMap<String, Remote>) -> Result<()> {
    for (name, remote) in remotes {
        let mut args = vec!["remote-add", "--system", "--if-not-exists"];
        if !remote.gpg_verify {
            args.push("--no-gpg-verify");
        }
        args.extend([name.as_str(), remote.url.as_str()]);
        flatpak(&args)?;
    }
    Ok(())
}

/// The value after `key:` in flatpak's `Key: value` output.
fn field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines()
        .filter_map(|line| line.trim().split_once(':'))
        .find(|(k, _)| k.trim() == key)
        .map(|(_, value)| value.trim())
}

/// The system-wide remotes, with their repository URLs.
pub fn remotes() -> Result<BTreeMap<String, Remote>> {
    if !available() {
        return Ok(BTreeMap::new());
    }
    let text = flatpak(&["remotes", "--system", "--columns=name,url,options"])?;
    Ok(text
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split('\t').collect();
            let [name, url, ..] = fields[..] else { return None };
            let options = fields.get(2).copied().unwrap_or("");
            Some((
                name.to_string(),
                Remote {
                    url: url.to_string(),
                    gpg_verify: !options.contains("no-gpg-verify"),
                },
            ))
        })
        .collect())
}

/// The remote's current commit of an app.
pub fn latest(remote: &str, id: &str) -> Result<LockedApp> {
    let info = flatpak(&["remote-info", "--system", remote, id])?;
    let (Some(reference), Some(commit)) = (field(&info, "Ref"), field(&info, "Commit")) else {
        bail!("flatpak remote-info {remote} {id} printed no ref or commit");
    };
    Ok(LockedApp {
        remote: remote.to_string(),
        reference: reference.to_string(),
        commit: commit.to_string(),
    })
}

pub fn parse_list(text: &str) -> BTreeMap<String, InstalledApp> {
    text.lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split('\t').collect();
            let [id, origin, commit, ..] = fields[..] else {
                return None;
            };
            Some((
                id.to_string(),
                InstalledApp {
                    origin: origin.to_string(),
                    commit: commit.to_string(),
                    reference: String::new(),
                },
            ))
        })
        .collect()
}

/// System-wide apps with their full active commit; none when flatpak isn't installed.
pub fn installed() -> Result<BTreeMap<String, InstalledApp>> {
    if !available() {
        return Ok(BTreeMap::new());
    }
    let mut apps = parse_list(&flatpak(&[
        "list",
        "--system",
        "--app",
        "--columns=application,origin,active",
    ])?);
    for (id, app) in apps.iter_mut() {
        app.commit = flatpak(&["info", "--system", "--show-commit", id])?.trim().to_string();
        app.reference = flatpak(&["info", "--system", "--show-ref", id])?.trim().to_string();
    }
    Ok(apps)
}

/// One line per change syncing would make, plus apps archix doesn't manage.
pub fn describe(
    target: &BTreeMap<String, LockedApp>,
    previous: &BTreeMap<String, LockedApp>,
    installed: &BTreeMap<String, InstalledApp>,
) -> (Vec<String>, Vec<String>) {
    let mut lines = Vec::new();
    for (id, app) in target {
        match installed.get(id) {
            None => lines.push(format!("flatpak    install {id} ({})", short(&app.commit))),
            Some(now) if now.commit != app.commit => lines.push(format!(
                "flatpak    {id} {} -> {}",
                short(&now.commit),
                short(&app.commit)
            )),
            Some(_) => {}
        }
    }
    for id in previous.keys().filter(|id| !target.contains_key(*id)) {
        if installed.contains_key(id) {
            lines.push(format!("flatpak    remove {id}"));
        }
    }
    let unmanaged = installed
        .keys()
        .filter(|id| !target.contains_key(*id) && !previous.contains_key(*id))
        .cloned()
        .collect();
    (lines, unmanaged)
}

fn short(commit: &str) -> &str {
    &commit[..commit.len().min(12)]
}

/// Installs and pins `target`'s apps and removes the ones only `previous` had; returns what failed without failing.
pub fn sync(target: &BTreeMap<String, LockedApp>, previous: &BTreeMap<String, LockedApp>) -> Vec<String> {
    let mut failures = Vec::new();
    let before = match installed() {
        Ok(apps) => apps,
        Err(e) => return vec![format!("{e:#}")],
    };
    let mut attempt = |args: &[&str]| {
        if let Err(e) = flatpak(args) {
            failures.push(format!("{e:#}"));
        }
    };
    for (id, app) in target {
        if !before.contains_key(id) {
            attempt(&[
                "install",
                "--system",
                "-y",
                "--noninteractive",
                &app.remote,
                &app.reference,
            ]);
        }
        let now = installed().ok().and_then(|apps| apps.get(id).map(|a| a.commit.clone()));
        if now.as_deref() != Some(app.commit.as_str()) {
            let commit = format!("--commit={}", app.commit);
            attempt(&["update", "--system", "-y", "--noninteractive", &commit, &app.reference]);
        }
    }
    let mut removed = false;
    for id in previous.keys().filter(|id| !target.contains_key(*id)) {
        if before.contains_key(id) {
            attempt(&["uninstall", "--system", "-y", "--noninteractive", id]);
            removed = true;
        }
    }
    if removed {
        attempt(&["uninstall", "--system", "-y", "--noninteractive", "--unused"]);
    }
    failures
}

/// Where switches leave Flatpak apps for a deployment that brings flatpak itself, which archix-flatpak.service installs at its first boot.
pub const PENDING_DIR: &str = "/var/lib/archix/pending-flatpak";

/// The Flatpak apps a deployment's first boot installs, as the switch that built it declared them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pending {
    /// The config directory and host whose archix.lock gets the commits of apps locked at the boot.
    pub config_dir: PathBuf,
    pub host: String,
    /// Every declared app as (remote, id).
    pub apps: Vec<(String, String)>,
    pub remotes: BTreeMap<String, Remote>,
    /// The apps locked already; the boot locks the rest at their remote's current commit.
    pub locked: BTreeMap<String, LockedApp>,
    /// What the generation before had, whose apps the new one dropped are uninstalled.
    pub previous: BTreeMap<String, LockedApp>,
}

pub fn pending_path(state: &Path, number: u32) -> PathBuf {
    state.join("pending-flatpak").join(format!("{number}.json"))
}

/// What deployment `number`'s first boot installs; an unreadable record is kept aside and installs nothing.
pub fn pending(state: &Path, number: u32) -> Option<Pending> {
    let path = pending_path(state, number);
    let text = fs::read_to_string(&path).ok()?;
    serde_json::from_str(&text)
        .inspect_err(|e| {
            eprintln!(
                "warning: {} is unreadable ({e}); it's kept as .unreadable",
                path.display()
            );
            let _ = fs::rename(&path, path.with_extension("unreadable"));
        })
        .ok()
}

pub fn defer(state: &Path, number: u32, pending: &Pending) -> Result<()> {
    let path = pending_path(state, number);
    crate::durable::write(&path, serde_json::to_string_pretty(pending)?.as_bytes())
        .with_context(|| format!("writing {}", path.display()))
}

/// Drops what waits for deployment `number`'s boot, or for every deployment's: a switch or rollback that syncs the apps itself supersedes it.
pub fn drop_pending(state: &Path, number: Option<u32>) {
    match number {
        Some(number) => {
            let _ = fs::remove_file(pending_path(state, number));
        }
        None => {
            let _ = fs::remove_dir_all(state.join("pending-flatpak"));
        }
    }
}

/// At the first boot: adds the remotes, locks the apps not locked yet and installs and pins them all; returns the apps as locked and what failed.
pub fn install_pending(pending: &Pending) -> (BTreeMap<String, LockedApp>, Vec<String>) {
    let mut failures = Vec::new();
    if let Err(e) = ensure_remotes(&pending.remotes) {
        failures.push(format!("{e:#}"));
    }
    let mut locked = pending.locked.clone();
    for (remote, id) in &pending.apps {
        if !locked.contains_key(id) {
            match latest(remote, id) {
                Ok(app) => {
                    locked.insert(id.clone(), app);
                }
                Err(e) => failures.push(format!("{id}: {e:#}")),
            }
        }
    }
    failures.extend(sync(&locked, &pending.previous));
    (locked, failures)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_remote_info_and_list_output() {
        let info = "            ID: org.archix.Hello\n           Ref: app/org.archix.Hello/x86_64/stable\n\
                    \n        Commit: 0bb3a9e4\n        Parent: 5f880619\n";
        assert_eq!(field(info, "Ref"), Some("app/org.archix.Hello/x86_64/stable"));
        assert_eq!(field(info, "Commit"), Some("0bb3a9e4"));
        let list = parse_list("org.archix.Hello\tlocal\t0bb3a9e4\n");
        assert_eq!(list["org.archix.Hello"].commit, "0bb3a9e4");
    }

    #[test]
    fn describes_installs_pins_and_removals_but_leaves_unmanaged_apps() {
        let app = |commit: &str| LockedApp {
            remote: "flathub".into(),
            reference: "app/x/x86_64/stable".into(),
            commit: commit.into(),
        };
        let installed = parse_list("a\tflathub\t111\nb\tflathub\t222\nc\tflathub\t333\nmine\tflathub\t444\n");
        let target = BTreeMap::from([
            ("a".to_string(), app("111")),
            ("b".to_string(), app("999")),
            ("d".to_string(), app("555")),
        ]);
        let previous = BTreeMap::from([("c".to_string(), app("333"))]);
        let (lines, unmanaged) = describe(&target, &previous, &installed);
        assert_eq!(
            lines,
            [
                "flatpak    b 222 -> 999",
                "flatpak    install d (555)",
                "flatpak    remove c"
            ]
        );
        assert_eq!(unmanaged, ["mine"]);
    }
}
