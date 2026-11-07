//! The checks a counted boot must pass before boot-complete.target, run by archix-health-check.service; a failure marks its entry bad, so the next boot falls back. Also what a good boot leaves behind, and the timeout that reboots a boot that never gets there.

use std::collections::BTreeSet;
use std::fs;
use std::net::{Ipv4Addr, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::User;
use crate::paths::Paths;

fn yes() -> bool {
    true
}

/// `boot.health_check`: what a counted boot must reach, beyond boot-complete.target, to count as good.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HealthCheck {
    #[serde(default = "yes")]
    pub default_target: bool,
    #[serde(default = "yes")]
    pub failed_units: bool,
    #[serde(default = "yes")]
    pub display_manager: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub units: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub listening: Vec<u16>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reachable: Vec<String>,
    #[serde(default)]
    pub login: bool,
    #[serde(default)]
    pub timeout: u32,
    #[serde(default)]
    pub reboot_on_failure: bool,
}

impl Default for HealthCheck {
    fn default() -> HealthCheck {
        HealthCheck {
            default_target: true,
            failed_units: true,
            display_manager: true,
            units: Vec::new(),
            listening: Vec::new(),
            reachable: Vec::new(),
            login: false,
            timeout: 120,
            reboot_on_failure: false,
        }
    }
}

impl HealthCheck {
    /// Whether it names targets of its own beyond the default ones.
    pub fn is_set(&self) -> bool {
        !self.units.is_empty() || !self.listening.is_empty() || !self.reachable.is_empty() || self.login
    }
}

/// One thing a boot must reach, named as the notes name it.
#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    DefaultTarget,
    /// No unit failed but these, which had failed at the last good boot.
    NoNewFailures(Vec<String>),
    DisplayManager,
    Shells,
    Unit(String),
    Listening(u16),
    Reachable(String),
    Login(String),
}

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Target::DefaultTarget => write!(f, "\"the default target reached\""),
            Target::NoNewFailures(_) => write!(f, "\"no unit failed since the last good boot\""),
            Target::DisplayManager => write!(f, "\"the display manager active\""),
            Target::Shells => write!(f, "\"declared users' login shells\""),
            Target::Unit(unit) => write!(f, "\"{unit} active\""),
            Target::Listening(port) => write!(f, "\"TCP port {port} listening\""),
            Target::Reachable(host) => write!(f, "\"{host} reachable\""),
            Target::Login(user) => write!(f, "\"{user} can log in\""),
        }
    }
}

/// The targets of a boot's check: the default target, no new failed units and the display manager unless turned off, the declared users' shells always, then what `boot.health_check` lists; `allowed` are the units that had failed at the last good boot.
pub fn targets(
    check: Option<&HealthCheck>,
    users: &std::collections::BTreeMap<String, User>,
    allowed: &[String],
) -> Vec<Target> {
    let defaults = HealthCheck::default();
    let check = check.unwrap_or(&defaults);
    let mut targets = Vec::new();
    if check.default_target {
        targets.push(Target::DefaultTarget);
    }
    if check.failed_units {
        targets.push(Target::NoNewFailures(allowed.to_vec()));
    }
    if check.display_manager {
        targets.push(Target::DisplayManager);
    }
    targets.push(Target::Shells);
    targets.extend(check.units.iter().cloned().map(Target::Unit));
    targets.extend(check.listening.iter().copied().map(Target::Listening));
    targets.extend(check.reachable.iter().cloned().map(Target::Reachable));
    if check.login {
        targets.extend(
            users
                .iter()
                .filter(|(_, user)| !no_login(&user.shell))
                .map(|(name, _)| Target::Login(name.clone())),
        );
    }
    targets
}

/// Shells that mean an account isn't for logging in.
fn no_login(shell: &str) -> bool {
    matches!(
        Path::new(shell).file_name().and_then(|name| name.to_str()),
        Some("nologin" | "false")
    )
}

/// Declared users whose login shell is missing under `root` or not listed in its /etc/shells, which PAM and chsh refuse.
pub fn shell_problems(root: &Path, users: &std::collections::BTreeMap<String, User>) -> Vec<String> {
    let listed: BTreeSet<String> = fs::read_to_string(root.join("etc/shells"))
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_string)
        .collect();
    let mut problems = Vec::new();
    for (name, user) in users {
        if no_login(&user.shell) {
            continue;
        }
        let path = root.join(user.shell.trim_start_matches('/'));
        if !path.is_file() {
            problems.push(format!("{name}'s shell {} doesn't exist", user.shell));
        } else if !listed.contains(&user.shell) {
            problems.push(format!("{name}'s shell {} isn't in /etc/shells", user.shell));
        }
    }
    problems
}

/// Local TCP ports in LISTEN state, from /proc/net/tcp or tcp6.
pub fn listening_ports(table: &str) -> BTreeSet<u16> {
    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let local = fields.nth(1)?;
            let state = fields.nth(1)?;
            (state == "0A")
                .then(|| u16::from_str_radix(local.rsplit_once(':')?.1, 16).ok())
                .flatten()
        })
        .collect()
}

/// The default IPv4 gateway in /proc/net/route.
pub fn default_gateway(routes: &str) -> Option<Ipv4Addr> {
    routes.lines().skip(1).find_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.get(1) != Some(&"00000000") || fields.get(7) != Some(&"00000000") {
            return None;
        }
        let raw = u32::from_str_radix(fields.get(2)?, 16).ok()?;
        (raw != 0).then(|| Ipv4Addr::from(raw.to_le_bytes()))
    })
}

/// Whether a TCP connection to `host:port` succeeds within a few seconds.
pub fn connects(address: &str) -> Result<()> {
    let addresses: Vec<_> = address
        .to_socket_addrs()
        .with_context(|| format!("resolving {address}"))?
        .collect();
    let mut last = None;
    for socket in addresses {
        match TcpStream::connect_timeout(&socket, Duration::from_secs(3)) {
            Ok(_) => return Ok(()),
            Err(e) => last = Some(e),
        }
    }
    match last {
        Some(e) => Err(e).with_context(|| format!("connecting to {address}")),
        None => anyhow::bail!("{address} has no address"),
    }
}

fn quiet(command: &mut Command) -> Result<()> {
    let output = command
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("running {command:?}"))?;
    if output.status.success() {
        return Ok(());
    }
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    anyhow::bail!("{}", text.trim())
}

/// Whether a reachable target is host:port ([v6]:port), not a bare host to ping.
fn has_port(target: &str) -> bool {
    target.rsplit_once(':').is_some_and(|(host, port)| {
        port.parse::<u16>().is_ok() && (!host.contains(':') || host.starts_with('[') && host.ends_with(']'))
    })
}

/// Checks one target on the running system under `root`.
pub fn check(root: &Path, target: &Target, users: &std::collections::BTreeMap<String, User>) -> Result<()> {
    match target {
        Target::DefaultTarget => quiet(Command::new("systemctl").args(["is-active", "--quiet", "default.target"]))
            .map_err(|_| anyhow::anyhow!("default.target isn't active")),
        Target::NoNewFailures(allowed) => {
            let new: Vec<String> = failed_units()
                .into_iter()
                .filter(|unit| !allowed.contains(unit) && !OWN_UNITS.contains(&unit.as_str()))
                .collect();
            match new.is_empty() {
                true => Ok(()),
                false => anyhow::bail!("{} failed", new.join(", ")),
            }
        }
        Target::DisplayManager => match display_manager(root) {
            None => Ok(()),
            Some(unit) => quiet(Command::new("systemctl").args(["is-active", "--quiet", "display-manager.service"]))
                .map_err(|_| anyhow::anyhow!("the display manager ({unit}) isn't active")),
        },
        Target::Shells => match shell_problems(root, users).as_slice() {
            [] => Ok(()),
            problems => anyhow::bail!("{}", problems.join("; ")),
        },
        Target::Unit(unit) => quiet(Command::new("systemctl").args(["is-active", "--quiet", unit]))
            .map_err(|_| anyhow::anyhow!("{unit} isn't active")),
        Target::Listening(port) => {
            let mut ports = BTreeSet::new();
            for table in ["proc/net/tcp", "proc/net/tcp6"] {
                ports.extend(listening_ports(
                    &fs::read_to_string(root.join(table)).unwrap_or_default(),
                ));
            }
            match ports.contains(port) {
                true => Ok(()),
                false => anyhow::bail!("nothing listens on TCP port {port}"),
            }
        }
        Target::Reachable(host) if host == "gateway" => {
            let routes = fs::read_to_string(root.join("proc/net/route")).unwrap_or_default();
            let gateway = default_gateway(&routes).context("there's no default gateway")?;
            quiet(Command::new("ping").args(["-c", "1", "-W", "2", &gateway.to_string()]))
        }
        Target::Reachable(host) if has_port(host) => connects(host),
        Target::Reachable(host) => quiet(Command::new("ping").args(["-c", "1", "-W", "2", host])),
        Target::Login(user) => {
            let shell = users.get(user).map_or("/bin/sh", |u| u.shell.as_str());
            quiet(Command::new("su").args(["-s", shell, user, "-c", "true"]))
        }
    }
}

/// Retries the targets every two seconds until all pass or `timeout` runs out; the first that still fails, and why.
pub fn run(
    root: &Path,
    targets: &[Target],
    users: &std::collections::BTreeMap<String, User>,
    timeout: u32,
) -> Option<(Target, String)> {
    let deadline = Instant::now() + Duration::from_secs(timeout.into());
    let mut left: Vec<&Target> = targets.iter().collect();
    loop {
        let mut failed = None;
        left.retain(|target| match check(root, target, users) {
            Ok(()) => false,
            Err(e) => {
                failed.get_or_insert_with(|| ((*target).clone(), format!("{e:#}")));
                true
            }
        });
        let failed = failed?;
        let now = Instant::now();
        if now >= deadline {
            return Some(failed);
        }
        std::thread::sleep((deadline - now).min(Duration::from_secs(2)));
    }
}

/// archix's own units, which the failed-unit check leaves to their own reports.
const OWN_UNITS: &[&str] = &[super::super::integration::HEALTH_UNIT, "archix-unblessed.service"];

/// The units systemd lists as failed now.
pub fn failed_units() -> Vec<String> {
    let Ok(output) = Command::new("systemctl")
        .args([
            "list-units",
            "--state=failed",
            "--plain",
            "--no-legend",
            "--no-pager",
            "--all",
        ])
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .map(str::to_string)
        .collect()
}

/// The display manager the root under `root` enables, by the unit display-manager.service links to.
pub fn display_manager(root: &Path) -> Option<String> {
    let link = fs::read_link(root.join("etc/systemd/system/display-manager.service")).ok()?;
    Some(link.file_name()?.to_string_lossy().into_owned())
}

/// What the check would conclude on this machine now, one line per target, for init.
pub fn conclude_now(root: &Path, check: Option<&HealthCheck>) -> Vec<String> {
    let users = std::collections::BTreeMap::new();
    targets(check, &users, &[])
        .iter()
        .map(|target| match self::check(root, target, &users) {
            Ok(()) => format!("passes  {target}"),
            Err(e) => format!("FAILS   {target}: {e:#}"),
        })
        .collect()
}

/// A health check that failed, kept until the fallback boot says why.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Failure {
    /// The entry whose boot failed it.
    pub entry: String,
    pub target: String,
    pub why: String,
    pub at: u64,
}

/// Beside the menu's state, so a try boot of a snapshot, whose own /var/lib/archix is the snapshot's, reads it.
pub fn failure_path(paths: &Paths) -> PathBuf {
    super::menu::dir(paths).join("health-failed.json")
}

pub fn read_failure(paths: &Paths) -> Option<Failure> {
    serde_json::from_str(&fs::read_to_string(failure_path(paths)).ok()?).ok()
}

/// A unix time as UTC, to the second.
pub fn utc(secs: u64) -> String {
    let date = crate::date::Date::from_unix(secs);
    let day = secs % 86_400;
    format!("{date} {:02}:{:02}:{:02} UTC", day / 3600, day / 60 % 60, day % 60)
}

/// What a fallback caused by a failed check says at login and in `archix status`.
pub fn fallback_note(failure: &Failure) -> String {
    format!(
        "archix: the boot of {} failed the health check {} at {} ({}); switch again once it passes",
        failure.entry,
        failure.target,
        utc(failure.at),
        failure.why
    )
}

/// The ESP the menu was last written to.
fn esp(paths: &Paths) -> Option<PathBuf> {
    let text = fs::read_to_string(paths.state.join("boot").join("esp")).ok()?;
    Some(PathBuf::from(text.trim()))
}

/// The archix entry systemd-boot started and is counting when the ESP holding it isn't mounted, as after a power cut that took the running kernel's vfat module away.
fn unreachable_counted_entry(paths: &Paths) -> Option<String> {
    let selected = crate::testhooks::selected_entry()?;
    super::menu::Entry::parse(&super::boot::entry_id(&selected))?;
    let reachable = esp(paths).is_some_and(|esp| super::boot::entry_file(&esp, &selected).is_some());
    (crate::testhooks::counted_boot() && !reachable).then_some(selected)
}

/// The settings a counted boot is checked by: the current generation's, or with none yet, those of the config whose switch laid the root out and stopped for the reboot.
fn chosen(from_generation: Option<Option<HealthCheck>>, laid_out: Option<HealthCheck>) -> Option<HealthCheck> {
    match from_generation {
        Some(check) => check,
        None => laid_out,
    }
}

/// archix-health-check.service: checks a boot systemd-boot is still counting, and on failure marks its entry bad, records why, and reboots if asked to.
pub fn run_boot(paths: &Paths) -> Result<bool> {
    // By name, not by text: an entry a cut left torn is still archix's, and its counted boot is still checked.
    let Some(entry) = super::boot::booted_by_name() else {
        if let Some(selected) = unreachable_counted_entry(paths) {
            println!(
                "archix: {selected} is a counted boot try, but its entry isn't on a mounted ESP, so this boot can't count as good; it fails its check, and boot.unblessed_timeout reboots it so the boot counter can fall back"
            );
            return Ok(false);
        }
        println!("archix: this boot isn't from an archix entry; nothing to check");
        return Ok(true);
    };
    if !crate::testhooks::counted_boot() {
        println!("archix: {} already counts as good; nothing to check", entry.id());
        return Ok(true);
    }
    let store = crate::generations::Store::new(paths);
    let generation = store.current()?.map(|number| store.get(number)).transpose()?;
    let menu = super::menu::load(&super::menu::dir(paths)).unwrap_or_default();
    let check = chosen(
        generation
            .as_ref()
            .map(|g| g.settings.boot.as_ref().and_then(|boot| boot.health_check.clone())),
        menu.laid_out_check.clone(),
    );
    let users = generation.as_ref().map(|g| g.system.users.clone()).unwrap_or_default();
    let allowed = menu.blessed_failed_units;
    // An edited entry isn't a failure by itself: a cut one has the mark, which goes only after the boot step is flushed.
    let unfinished = crate::pacman::hooks::unfinished(&paths.state);
    let torn = unfinished.is_some();
    let failed = match unfinished {
        Some(unfinished) => Some((unfinished.target().to_string(), unfinished.why())),
        None => {
            let targets = targets(check.as_ref(), &users, &allowed);
            let timeout = check.as_ref().map_or(120, |check| check.timeout);
            run(&paths.root, &targets, &users, timeout).map(|(target, why)| (target.to_string(), why))
        }
    };
    let Some((target, why)) = failed else {
        println!("archix: {} passed its health check", entry.id());
        return Ok(true);
    };
    let failure = Failure {
        entry: entry.id(),
        target,
        why,
        at: crate::generations::now(),
    };
    crate::durable::write(&failure_path(paths), serde_json::to_string_pretty(&failure)?.as_bytes())?;
    let marked = match esp(paths) {
        Some(esp) => super::boot::mark_bad(&esp, &entry.id())?,
        None => false,
    };
    // A root the mark says is torn reboots at once, as nothing on it is worth waiting for; each reboot uses a try even unmarked.
    let reboot = torn || check.as_ref().is_some_and(|check| check.reboot_on_failure);
    let note = format!(
        "archix: {} failed its health check {} ({}); {}",
        failure.entry,
        failure.target,
        failure.why,
        match (marked, reboot) {
            (true, true) => "its entry is marked bad, and the machine reboots into the fallback now",
            (true, false) => "its entry is marked bad, so the next boot falls back",
            (false, true) => "its entry couldn't be marked bad, so the machine reboots now, using up its tries",
            (false, false) => "but its entry couldn't be marked bad, so the next boot starts it again",
        }
    );
    super::boot::add_login_note(paths, &note);
    if reboot && (marked || torn) {
        if cfg!(debug_assertions)
            && let Some(record) = std::env::var_os("ARCHIX_TEST_REBOOT")
        {
            let _ = fs::write(record, "reboot\n");
        } else {
            let _ = Command::new("systemctl").args(["--no-block", "reboot"]).status();
        }
    }
    Ok(false)
}

/// The units failed right now, but archix's own, when this boot is the current system's and already counts as good, so the counted boot a switch or a transaction is about to give tries doesn't fail its check for a unit that was failing before; None otherwise.
pub fn failed_while_good() -> Option<Vec<String>> {
    if crate::testhooks::counted_boot()
        || super::initrd::trying().is_some()
        || super::boot::booted() != Some(super::menu::Entry::Current)
    {
        return None;
    }
    Some(
        failed_units()
            .into_iter()
            .filter(|unit| !OWN_UNITS.contains(&unit.as_str()))
            .collect(),
    )
}

/// archix-boot-complete.service, after systemd-bless-boot: a good boot of the current system arms the slot, so the next snapshot takes it, and its failed units become the ones the next check allows; a good try boot changes nothing.
pub fn boot_complete(paths: &Paths) -> Result<()> {
    if super::initrd::trying().is_some() {
        return Ok(());
    }
    match super::boot::booted() {
        Some(super::menu::Entry::Current) => {}
        _ => return Ok(()),
    }
    let dir = super::menu::dir(paths);
    let mut state = super::menu::load(&dir)?;
    state.armed = true;
    state.blessed_failed_units = failed_units()
        .into_iter()
        .filter(|unit| !OWN_UNITS.contains(&unit.as_str()))
        .collect();
    super::menu::save(&dir, &state)?;
    let _ = fs::remove_file(failure_path(paths));
    Ok(())
}

/// Whether anyone is logged in: a logind session, or a shell an emergency or rescue unit opened past sulogin.
fn someone_logged_in() -> bool {
    let sessions = Command::new("loginctl")
        .args(["list-sessions", "--no-legend"])
        .output()
        .map(|output| !String::from_utf8_lossy(&output.stdout).trim().is_empty())
        .unwrap_or(false);
    sessions
        || ["emergency.service", "rescue.service"].iter().any(|unit| {
            fs::read_to_string(format!("/sys/fs/cgroup/system.slice/{unit}/cgroup.procs"))
                .unwrap_or_default()
                .lines()
                .filter_map(|pid| fs::read_to_string(format!("/proc/{}/comm", pid.trim())).ok())
                .any(|comm| !comm.trim().starts_with("sulogin") && !comm.trim().starts_with("systemd-sulogin"))
        })
}

fn active(unit: &str) -> bool {
    Command::new("systemctl")
        .args(["is-active", "--quiet", unit])
        .status()
        .is_ok_and(|status| status.success())
}

/// Seconds since boot.
fn uptime() -> u64 {
    fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|text| text.split('.').next()?.parse().ok())
        .unwrap_or(0)
}

/// Why an unblessed boot should reboot itself now, if it should: counted, past `timeout_secs` without boot-complete.target or in emergency or rescue mode, and nobody logged in.
/// How long a counted boot stuck in emergency or rescue mode waits for someone to log in before it reboots.
pub const STUCK_SECS: u64 = 120;

pub fn should_reboot(
    counted: bool,
    uptime: u64,
    timeout_secs: u64,
    complete: bool,
    stuck: bool,
    logged_in: bool,
) -> Option<String> {
    // A boot stuck in emergency or rescue mode won't count as good by itself, so it needn't wait the whole timeout.
    let timeout_secs = match stuck {
        true => timeout_secs.min(STUCK_SECS),
        false => timeout_secs,
    };
    if !counted || timeout_secs == 0 || uptime < timeout_secs || logged_in {
        return None;
    }
    match (stuck, complete) {
        (true, _) => Some(format!(
            "this boot has sat in emergency or rescue mode for {} min with nobody logged in",
            timeout_secs / 60
        )),
        (false, false) => Some(format!(
            "this boot hasn't counted as good within {} min, with nobody logged in",
            timeout_secs / 60
        )),
        (false, true) => None,
    }
}

/// Where the last switch puts boot.unblessed_timeout for archix-unblessed.service: in /etc, as emergency mode may have nothing else mounted.
pub const TIMEOUT_DROP_IN: &str = "/etc/systemd/system/archix-unblessed.service.d/timeout.conf";
const TIMEOUT_VARIABLE: &str = "ARCHIX_UNBLESSED_MINUTES";

pub fn timeout_drop_in(minutes: u32) -> String {
    format!("[Service]\nEnvironment={TIMEOUT_VARIABLE}={minutes}\n")
}

/// archix-unblessed.service, every minute of a boot from its start, in emergency and rescue mode too: reboots a counted boot that's stuck, so the boot counter runs down and systemd-boot falls back with nobody at the machine.
pub fn unblessed() -> Result<()> {
    let minutes: u64 = std::env::var(TIMEOUT_VARIABLE)
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .unwrap_or(10);
    let stuck = active("emergency.target") || active("rescue.target");
    let why = should_reboot(
        crate::testhooks::counted_boot(),
        uptime(),
        minutes * 60,
        active("boot-complete.target"),
        stuck,
        someone_logged_in(),
    );
    if let Some(why) = why {
        println!("archix: {why}; rebooting so the boot counter can fall back");
        let status = Command::new("systemctl").args(["reboot"]).status();
        if !status.is_ok_and(|status| status.success()) {
            let _ = Command::new("systemctl").args(["reboot", "--force"]).status();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn the_layouts_boot_with_no_generation_yet_is_checked_as_the_config_that_laid_it_out_says() {
        let off = HealthCheck {
            failed_units: false,
            ..HealthCheck::default()
        };
        assert_eq!(
            chosen(None, Some(off.clone())),
            Some(off.clone()),
            "no generation: the layout's settings"
        );
        assert_eq!(
            chosen(Some(None), Some(off.clone())),
            None,
            "a generation's own settings win, its defaults too"
        );
        assert_eq!(chosen(Some(Some(off.clone())), None), Some(off));
        assert_eq!(chosen(None, None), None);
    }

    #[test]
    fn a_counted_boot_stuck_in_emergency_mode_reboots_after_two_minutes_not_the_unblessed_timeout() {
        let ten = 10 * 60;
        assert!(
            should_reboot(true, 150, ten, false, true, false).is_some(),
            "stuck past two minutes"
        );
        assert!(
            should_reboot(true, 100, ten, false, true, false).is_none(),
            "not two minutes yet"
        );
        assert!(
            should_reboot(true, 150, ten, false, true, true).is_none(),
            "someone logged in at the prompt"
        );
        assert!(
            should_reboot(true, 150, ten, false, false, false).is_none(),
            "a boot still coming up keeps the unblessed timeout"
        );
        assert!(
            should_reboot(true, 150, 60, false, true, false).is_some(),
            "a shorter unblessed timeout still holds"
        );
    }

    fn user(shell: &str) -> User {
        User {
            uid: None,
            group: None,
            groups: Vec::new(),
            shell: shell.into(),
            home: None,
            description: String::new(),
            system: false,
            hashed_password: None,
            initial_hashed_password: None,
            hashed_password_secret: None,
        }
    }

    #[test]
    fn a_declared_shell_must_exist_and_be_listed_in_etc_shells() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("usr/bin")).unwrap();
        fs::create_dir_all(root.join("etc")).unwrap();
        fs::write(root.join("usr/bin/bash"), "").unwrap();
        fs::write(root.join("usr/bin/zsh"), "").unwrap();
        fs::write(root.join("etc/shells"), "# shells\n/usr/bin/bash\n").unwrap();
        let users: BTreeMap<String, User> = [
            ("ann".to_string(), user("/usr/bin/bash")),
            ("bob".to_string(), user("/usr/bin/zsh")),
            ("cat".to_string(), user("/usr/bin/fish")),
            ("svc".to_string(), user("/usr/bin/nologin")),
        ]
        .into();
        assert_eq!(
            shell_problems(root, &users),
            [
                "bob's shell /usr/bin/zsh isn't in /etc/shells",
                "cat's shell /usr/bin/fish doesn't exist"
            ]
        );
        let check = HealthCheck {
            login: true,
            ..Default::default()
        };
        let named: Vec<String> = targets(Some(&check), &users, &[])
            .iter()
            .map(Target::to_string)
            .collect();
        assert_eq!(
            named,
            [
                "\"the default target reached\"",
                "\"no unit failed since the last good boot\"",
                "\"the display manager active\"",
                "\"declared users' login shells\"",
                "\"ann can log in\"",
                "\"bob can log in\"",
                "\"cat can log in\""
            ]
        );
        let off = HealthCheck {
            default_target: false,
            failed_units: false,
            display_manager: false,
            ..HealthCheck::default()
        };
        assert_eq!(
            targets(Some(&off), &users, &[]),
            [Target::Shells],
            "the shells are checked whatever is turned off"
        );
        assert_eq!(
            targets(None, &users, &["a.service".to_string()])[1],
            Target::NoNewFailures(vec!["a.service".to_string()])
        );
    }

    #[test]
    fn listening_ports_and_the_default_gateway_come_from_proc() {
        let tcp = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   \
                   0: 00000000:0016 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 1 1\n   \
                   1: 0100007F:1F90 0100007F:D431 01 00000000:00000000 00:00000000 00000000     0        0 2 1\n";
        assert_eq!(listening_ports(tcp), BTreeSet::from([22]));
        let routes = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
                      enp0s2\t00000000\t0202000A\t0003\t0\t0\t1024\t00000000\t0\t0\t0\n\
                      enp0s2\t0002000A\t00000000\t0001\t0\t0\t1024\t00FFFFFF\t0\t0\t0\n";
        assert_eq!(default_gateway(routes), Some(Ipv4Addr::new(10, 0, 2, 2)));
        assert_eq!(default_gateway("Iface\tDestination\n"), None);
        assert!(has_port("example.org:443") && has_port("10.0.2.2:8079") && has_port("[fd00::1]:22"));
        assert!(!has_port("example.org") && !has_port("fd00::1") && !has_port("gateway"));
    }

    #[test]
    fn a_target_is_retried_until_the_timeout_and_the_first_failure_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let open = format!("127.0.0.1:{}", listener.local_addr().unwrap().port());
        let closed = {
            let gone = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            format!("127.0.0.1:{}", gone.local_addr().unwrap().port())
        };
        let users = BTreeMap::new();
        assert_eq!(run(dir.path(), &[Target::Reachable(open.clone())], &users, 0), None);
        let started = Instant::now();
        let (target, why) = run(
            dir.path(),
            &[Target::Reachable(open), Target::Reachable(closed.clone())],
            &users,
            3,
        )
        .expect("a closed port can't pass");
        assert_eq!(target, Target::Reachable(closed));
        assert!(why.contains("connecting"), "{why}");
        assert!(
            started.elapsed() >= Duration::from_secs(3),
            "it retried until the timeout"
        );
    }

    #[test]
    fn a_fallback_note_names_the_target_and_the_time() {
        let failure = Failure {
            entry: "archix-current.conf".into(),
            target: Target::Listening(22).to_string(),
            why: "nothing listens on TCP port 22".into(),
            at: 1_792_576_985,
        };
        assert_eq!(
            fallback_note(&failure),
            "archix: the boot of archix-current.conf failed the health check \"TCP port 22 listening\" at 2026-10-21 10:03:05 UTC (nothing listens on TCP port 22); switch again once it passes"
        );
    }

    #[test]
    fn a_stuck_counted_boot_reboots_once_its_time_is_up_and_nobody_is_logged_in() {
        let ten = 600;
        assert!(
            should_reboot(true, 601, ten, false, false, false).is_some(),
            "never got to boot-complete"
        );
        assert!(
            should_reboot(true, 601, ten, true, true, false).is_some(),
            "in emergency mode"
        );
        assert_eq!(
            should_reboot(true, 601, ten, true, false, false),
            None,
            "it counted as good"
        );
        assert_eq!(should_reboot(true, 599, ten, false, false, false), None, "not yet");
        assert_eq!(
            should_reboot(true, 601, ten, false, true, true),
            None,
            "someone is logged in"
        );
        assert_eq!(should_reboot(false, 601, ten, false, true, false), None, "not counted");
        assert_eq!(should_reboot(true, 6001, 0, false, true, false), None, "turned off");
    }

    #[test]
    fn the_display_manager_is_whatever_display_manager_service_links_to() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(display_manager(dir.path()), None);
        let units = dir.path().join("etc/systemd/system");
        fs::create_dir_all(&units).unwrap();
        std::os::unix::fs::symlink(
            "/usr/lib/systemd/system/gdm.service",
            units.join("display-manager.service"),
        )
        .unwrap();
        assert_eq!(display_manager(dir.path()).as_deref(), Some("gdm.service"));
    }
}
