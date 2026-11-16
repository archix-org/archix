//! What applying a system change does to the running system besides writing files: checks before it, and its units, sysctl and udev after.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use super::apply::SystemChange;
use super::credentials;

/// Refuses, before anything changes, a sudoers drop-in visudo rejects or tmpfiles lines systemd-tmpfiles can't parse, checked on copies of what the switch would write.
pub fn check_files(system: &SystemChange, content: &dyn Fn(&str) -> Option<Vec<u8>>) -> Result<()> {
    let dir = Scratch::new()?;
    if system.to.files.contains_key(credentials::SUDOERS)
        && let Some(bytes) = content(credentials::SUDOERS)
    {
        let copy = dir.path().join("sudoers");
        fs::write(&copy, bytes).with_context(|| format!("writing {}", copy.display()))?;
        let output = Command::new("visudo")
            .args(["-c", "-f"])
            .arg(&copy)
            .output()
            .context("running visudo")?;
        if !output.status.success() {
            let said = String::from_utf8_lossy(&output.stderr)
                .lines()
                .next()
                .unwrap_or_default()
                .replace(&copy.display().to_string(), credentials::SUDOERS);
            bail!("visudo refuses the sudoers lines from sudo ({said}); nothing was changed");
        }
    }
    if system.to.files.contains_key(credentials::TMPFILES)
        && let Some(bytes) = content(credentials::TMPFILES)
    {
        let copy = dir.path().join("archix.conf");
        fs::write(&copy, bytes).with_context(|| format!("writing {}", copy.display()))?;
        let output = Command::new("systemd-tmpfiles")
            .args(["--dry-run", "--create"])
            .arg(&copy)
            .output()
            .context("running systemd-tmpfiles")?;
        let shown = copy.display().to_string();
        let errors = String::from_utf8_lossy(&output.stderr);
        // Users and groups the switch creates don't exist yet, so only theirs may fail to resolve.
        let created = |line: &str| {
            line.split('\'')
                .nth(1)
                .is_some_and(|name| system.to.users.contains_key(name) || system.to.groups.contains_key(name))
        };
        let unparsed: Vec<String> = errors
            .lines()
            .filter(|line| line.contains(&shown) && !(line.contains("Failed to resolve") && created(line)))
            .map(|line| line.replace(&shown, credentials::TMPFILES))
            .collect();
        if !unparsed.is_empty() {
            bail!(
                "systemd-tmpfiles refuses tmpfiles lines: {}; nothing was changed",
                unparsed.join("; ")
            );
        }
    }
    Ok(())
}

/// A directory of root's own, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Result<Scratch> {
        let dir = std::env::temp_dir().join(format!("archix-check-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir(&dir).with_context(|| format!("creating {}", dir.display()))?;
        Ok(Scratch(dir))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn active(unit: &str) -> bool {
    Command::new("systemctl")
        .args(["is-active", "--quiet", unit])
        .status()
        .is_ok_and(|status| status.success())
}

/// On the running system: reloads systemd, stops the units the config turned off or dropped, starts the ones it newly enables and restarts those whose unit files changed; failures are returned.
pub fn services(change: &SystemChange) -> Vec<String> {
    let mut failures = Vec::new();
    systemctl(&["daemon-reload"], &mut failures);
    for (verb, unit) in unit_actions(change, &active) {
        systemctl(&[verb, &unit], &mut failures);
    }
    if udev_changed(change) {
        for argv in udev_commands() {
            let done = Command::new(argv[0]).args(&argv[1..]).status();
            if !done.is_ok_and(|status| status.success()) {
                failures.push(format!("{} failed", argv.join(" ")));
            }
        }
    }
    failures
}

fn systemctl(args: &[&str], failures: &mut Vec<String>) {
    let output = Command::new("systemctl").args(args).output();
    let what = format!("systemctl {}", args.join(" "));
    match output {
        Ok(output) if output.status.success() => {}
        Ok(output) => failures.push(format!("{what}: {}", String::from_utf8_lossy(&output.stderr).trim())),
        Err(e) => failures.push(format!("{what}: {e}")),
    }
}

/// Whether the switch added, changed or dropped a udev rule, archix's or one the config declares, which udev reads only when told to.
fn udev_changed(change: &SystemChange) -> bool {
    change
        .changed_paths()
        .iter()
        .any(|path| UDEV_DIRS.iter().any(|dir| path.starts_with(dir)) && path.ends_with(".rules"))
}

/// Reloads udev's rules and replays a change event for every device, as which subsystems a rule matches can't be told from its text reliably.
fn udev_commands() -> [Vec<&'static str>; 2] {
    [
        vec!["udevadm", "control", "--reload"],
        vec!["udevadm", "trigger", "--action=change"],
    ]
}

/// What a switch does to units on the running system, given which are `active`: stops the disabled or dropped, starts the newly enabled, try-restarts those whose unit file it rewrote, reloads those whose RELOADS file changed and restarts systemd-sysctl for new sysctl settings.
pub fn unit_actions(change: &SystemChange, active: &dyn Fn(&str) -> bool) -> Vec<(&'static str, String)> {
    let mut actions = Vec::new();
    let off = change
        .to
        .services
        .iter()
        .filter(|(_, enabled)| !**enabled)
        .map(|(unit, _)| unit)
        .chain(
            change
                .from
                .services
                .iter()
                .filter(|(unit, enabled)| **enabled && !change.to.services.contains_key(*unit))
                .map(|(unit, _)| unit),
        );
    for unit in off {
        if active(unit) {
            actions.push(("stop", unit.clone()));
        }
    }
    let changed = change.changed_paths();
    for (unit, enabled) in &change.to.services {
        if !*enabled {
            continue;
        }
        let rewritten = changed.iter().any(|path| {
            *path == format!("/etc/systemd/system/{unit}")
                || path.starts_with(&format!("/etc/systemd/system/{unit}.d/"))
        });
        let newly = change.from.services.get(unit) != Some(&true);
        let never = change.to.restart_if_changed.get(unit) == Some(&false);
        let reread = RELOADS
            .iter()
            .any(|(reloads, path)| *reloads == unit && changed.contains(path));
        if newly && !active(unit) {
            actions.push(("start", unit.clone()));
        } else if rewritten && !never {
            actions.push(("try-restart", unit.clone()));
        } else if reread {
            actions.push(("try-reload-or-restart", unit.clone()));
        }
    }
    // Any sysctl file the switch wrote, archix's own or one the config declares; a dropped one keeps its values until the next boot, as sysctl can't unset one.
    let sysctl = changed.iter().any(|path| {
        SYSCTL_DIRS.iter().any(|dir| path.starts_with(dir))
            && path.ends_with(".conf")
            && change.to.files.contains_key(*path)
    });
    if sysctl {
        actions.push(("restart", "systemd-sysctl.service".to_string()));
    }
    actions
}

/// The file `sysctl` renders, which systemd-sysctl applies at boot; restarting it applies a change at once.
pub const SYSCTL: &str = "/etc/sysctl.d/90-archix.conf";
/// Where systemd-sysctl reads the files a switch may write.
const SYSCTL_DIRS: [&str; 2] = ["/etc/sysctl.d/", "/usr/lib/sysctl.d/"];
/// Where udev reads the rules a switch may write.
const UDEV_DIRS: [&str; 2] = ["/etc/udev/rules.d/", "/usr/lib/udev/rules.d/"];

/// Units archix writes whose running service reads a file the switch may change on its own: reloaded when only that file changed.
const RELOADS: &[(&str, &str)] = &[(super::firewall::UNIT, super::firewall::RULES)];

#[cfg(test)]
mod unit_tests {
    use super::*;
    use crate::system::{ManagedFile, SystemState, firewall};

    fn file(sha256: &str) -> ManagedFile {
        ManagedFile {
            sha256: sha256.to_string(),
            link: None,
            mode: 0o644,
            owner: "root".to_string(),
            group: "root".to_string(),
            merge: None,
        }
    }

    fn with_rules(sha256: &str) -> SystemState {
        let mut state = SystemState::default();
        state.files.insert(firewall::RULES.to_string(), file(sha256));
        state
            .files
            .insert(format!("/etc/systemd/system/{}", firewall::UNIT), file("unit"));
        state.services.insert(firewall::UNIT.to_string(), true);
        state
    }

    #[test]
    fn the_firewall_reloads_when_only_its_rules_change_and_not_when_nothing_does() {
        let change = SystemChange {
            from: with_rules("a"),
            to: with_rules("b"),
        };
        assert_eq!(
            unit_actions(&change, &|_| true),
            vec![("try-reload-or-restart", firewall::UNIT.to_string())]
        );
        let same = SystemChange {
            from: with_rules("a"),
            to: with_rules("a"),
        };
        assert!(unit_actions(&same, &|_| true).is_empty());
    }

    #[test]
    fn changed_or_dropped_udev_rules_reload_udev_and_retrigger_devices() {
        let with = |name: &str, sha: &str| {
            let mut state = SystemState::default();
            state.files.insert(format!("/etc/udev/rules.d/{name}.rules"), file(sha));
            state.files.insert("/etc/hostname".to_string(), file("h"));
            state
        };
        let change = |from, to| SystemChange { from, to };
        assert!(
            udev_changed(&change(with("70-mouse", "a"), with("70-mouse", "b"))),
            "a changed rule"
        );
        assert!(
            udev_changed(&change(SystemState::default(), with("70-mouse", "a"))),
            "a new rule"
        );
        assert!(
            udev_changed(&change(with("70-mouse", "a"), SystemState::default())),
            "a dropped rule"
        );
        assert!(
            !udev_changed(&change(with("70-mouse", "a"), with("70-mouse", "a"))),
            "the same rules"
        );
        assert_eq!(
            udev_commands(),
            [
                vec!["udevadm", "control", "--reload"],
                vec!["udevadm", "trigger", "--action=change"]
            ]
        );
    }

    #[test]
    fn any_sysctl_file_or_udev_rule_the_switch_writes_applies_at_once() {
        for path in ["/etc/sysctl.d/80-swappiness.conf", "/usr/lib/sysctl.d/50-tuning.conf"] {
            let mut to = SystemState::default();
            to.files.insert(path.to_string(), file("b"));
            let change = SystemChange {
                from: SystemState::default(),
                to,
            };
            assert_eq!(
                unit_actions(&change, &|_| true),
                vec![("restart", "systemd-sysctl.service".to_string())],
                "{path}"
            );
        }
        let mut to = SystemState::default();
        to.files
            .insert("/usr/lib/udev/rules.d/70-mine.rules".to_string(), file("b"));
        assert!(udev_changed(&SystemChange {
            from: SystemState::default(),
            to
        }));
    }

    #[test]
    fn new_sysctl_settings_apply_at_once_through_systemd_sysctl() {
        let with = |sha: &str| {
            let mut state = SystemState::default();
            state.files.insert(SYSCTL.to_string(), file(sha));
            state
        };
        let change = SystemChange {
            from: with("a"),
            to: with("b"),
        };
        assert_eq!(
            unit_actions(&change, &|_| true),
            vec![("restart", "systemd-sysctl.service".to_string())]
        );
        let gone = SystemChange {
            from: with("a"),
            to: SystemState::default(),
        };
        assert!(
            unit_actions(&gone, &|_| true).is_empty(),
            "a dropped setting keeps its value until the next boot, as sysctl can't unset one"
        );
    }
}
