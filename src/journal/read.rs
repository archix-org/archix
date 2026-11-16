//! Reading a switch journal (SCOPE 4) for `archix log`; the types mirror the switch's writer until the merge swaps them for the shared ones.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::plan::{FileAction, PkgAction, Plan};

pub const SWITCHES: &str = "/var/log/archix/switches";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlpmWindow {
    pub start: Option<String>,
    pub end: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Running,
    Ok,
    Failed,
    Interrupted,
}

impl Outcome {
    pub fn text(self) -> &'static str {
        match self {
            Outcome::Running => "running",
            Outcome::Ok => "ok",
            Outcome::Failed => "failed",
            Outcome::Interrupted => "interrupted",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Meta {
    pub n: u32,
    pub archix_version: String,
    pub started: String,
    pub snapper_pre: Option<u32>,
    pub snapper_post: Option<u32>,
    pub alpm_window: AlpmWindow,
    #[serde(default)]
    pub home: BTreeMap<String, String>,
    pub outcome: Outcome,
    pub failed_step: Option<String>,
    /// Verify's own addition: removed packages' file lists, as the switch read them before the removal.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub removed_files: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ActionKind {
    Install,
    Upgrade,
    Downgrade,
    Remove,
    FileWrite,
    FileDelete,
    User,
    Group,
    UnitEnable,
    UnitDisable,
    UnitStart,
    UnitRestart,
    UnitReload,
    FlatpakInstall,
    FlatpakRemove,
    HomeActivate,
    SyncDbReplace,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "kebab-case")]
pub enum Event {
    StepStart {
        t: String,
        step: String,
    },
    StepEnd {
        t: String,
        step: String,
        ok: bool,
        detail: String,
    },
    Action {
        t: String,
        step: String,
        kind: ActionKind,
        target: String,
    },
}

pub fn switches_dir(root: &Path) -> PathBuf {
    root.join(SWITCHES.trim_start_matches('/'))
}

/// The switch numbers in the journal, ascending.
pub fn list(root: &Path) -> Result<Vec<u32>> {
    let dir = switches_dir(root);
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    let mut numbers: Vec<u32> = entries
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
        .collect();
    numbers.sort_unstable();
    Ok(numbers)
}

pub fn switch_dir(root: &Path, n: u32) -> PathBuf {
    switches_dir(root).join(n.to_string())
}

pub fn read_meta(dir: &Path) -> Result<Meta> {
    let path = dir.join("meta.json");
    let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

pub fn read_plan(dir: &Path) -> Result<Plan> {
    let path = dir.join("plan.json");
    let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// The journal's events; a torn last line (a cut mid-append) is reported, not fatal.
pub fn read_events(dir: &Path) -> Result<(Vec<Event>, Option<String>)> {
    let path = dir.join("journal.jsonl");
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), None)),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let mut events = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        match serde_json::from_str(line) {
            Ok(event) => events.push(event),
            Err(_) if i + 1 == lines.len() => return Ok((events, Some((*line).to_string()))),
            Err(e) => return Err(e).with_context(|| format!("{} line {}", path.display(), i + 1)),
        }
    }
    Ok((events, None))
}

/// Unix seconds (floored) of an RFC 3339 time or pacman.log's `2026-11-16T10:00:00+0100`.
pub fn parse_time(text: &str) -> Option<i64> {
    let b = text.as_bytes();
    if b.len() < 19
        || b[4] != b'-'
        || b[7] != b'-'
        || !(b[10] == b'T' || b[10] == b' ')
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let num = |range: std::ops::Range<usize>| -> Option<i64> {
        let s = text.get(range)?;
        s.bytes().all(|c| c.is_ascii_digit()).then(|| s.parse().ok())?
    };
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hour, minute, second) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let mut rest = &text[19..];
    if let Some(frac) = rest.strip_prefix('.') {
        let digits = frac.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        rest = &frac[digits..];
    }
    let offset = match rest {
        "Z" | "z" => 0,
        _ => {
            let sign = match rest.as_bytes().first()? {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let digits: String = rest[1..].chars().filter(|c| *c != ':').collect();
            if digits.len() != 4 || !digits.bytes().all(|c| c.is_ascii_digit()) || rest[1..].len() > 5 {
                return None;
            }
            let (h, m): (i64, i64) = (digits[..2].parse().ok()?, digits[2..].parse().ok()?);
            sign * (h * 3600 + m * 60)
        }
    };
    let days = days_from_civil(year, month, day);
    Some(days * 86_400 + hour * 3600 + minute * 60 + second - offset)
}

/// Howard Hinnant's days-from-civil.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = year - i64::from(month <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `archix log`: one line per switch.
pub fn render_list(root: &Path) -> Result<String> {
    let numbers = list(root)?;
    if numbers.is_empty() {
        return Ok(format!("no switches in {}\n", switches_dir(root).display()));
    }
    let mut out = format!(
        "{:>4}  {:<25}  {:<11}  {:<13}  snapper\n",
        "n", "started", "outcome", "failed step"
    );
    for n in numbers {
        match read_meta(&switch_dir(root, n)) {
            Ok(meta) => {
                let _ = writeln!(
                    out,
                    "{:>4}  {:<25}  {:<11}  {:<13}  {}",
                    n,
                    meta.started,
                    meta.outcome.text(),
                    meta.failed_step.as_deref().unwrap_or("-"),
                    snapper_pair(&meta)
                );
            }
            Err(e) => {
                let _ = writeln!(out, "{n:>4}  unreadable: {e:#}");
            }
        }
    }
    Ok(out)
}

fn snapper_pair(meta: &Meta) -> String {
    let num = |n: Option<u32>| n.map_or("-".to_string(), |n| n.to_string());
    format!("{}..{}", num(meta.snapper_pre), num(meta.snapper_post))
}

/// `archix log N`: the switch's steps with their actions, and the plan's summary.
pub fn render_one(root: &Path, n: u32) -> Result<String> {
    let dir = switch_dir(root, n);
    let meta = read_meta(&dir)?;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "switch {n}: {}, started {}, archix {}",
        meta.outcome.text(),
        meta.started,
        meta.archix_version
    );
    if let Some(step) = &meta.failed_step {
        let _ = writeln!(out, "failed step: {step}");
    }
    let _ = writeln!(out, "snapper: {}", snapper_pair(&meta));
    let _ = writeln!(
        out,
        "pacman lock window: {} .. {}",
        meta.alpm_window.start.as_deref().unwrap_or("-"),
        meta.alpm_window.end.as_deref().unwrap_or("-")
    );
    for (user, path) in &meta.home {
        let _ = writeln!(out, "home {user}: {path}");
    }
    match read_plan(&dir) {
        Ok(plan) => out.push_str(&plan_summary(&plan)),
        Err(e) => {
            let _ = writeln!(out, "plan: unreadable: {e:#}");
        }
    }
    let (events, torn) = read_events(&dir)?;
    out.push_str("steps:\n");
    for event in &events {
        match event {
            Event::StepStart { t, step } => {
                let _ = writeln!(out, "  {t}  {step}: started");
            }
            Event::StepEnd { t, step, ok, detail } => {
                let verdict = if *ok { "ok" } else { "FAILED" };
                let detail = if detail.is_empty() {
                    String::new()
                } else {
                    format!(" ({detail})")
                };
                let _ = writeln!(out, "  {t}  {step}: {verdict}{detail}");
            }
            Event::Action { kind, target, .. } => {
                let kind = serde_json::to_value(kind).ok();
                let kind = kind.as_ref().and_then(|k| k.as_str()).unwrap_or("?");
                let _ = writeln!(out, "      {kind} {target}");
            }
        }
    }
    if let Some(line) = torn {
        let _ = writeln!(out, "  torn last journal line (cut mid-write): {line}");
    }
    Ok(out)
}

pub fn plan_summary(plan: &Plan) -> String {
    let mut out = String::new();
    let header = &plan.header;
    let mode = serde_json::to_value(header.mode).ok();
    let mode = mode.as_ref().and_then(|m| m.as_str()).unwrap_or("?").to_string();
    let date = header.date.map_or_else(|| "live".to_string(), |d| d.to_string());
    let _ = writeln!(out, "plan: {mode}, {date}, fingerprint {}", header.fingerprint);
    let mut packages: BTreeMap<&str, usize> = BTreeMap::new();
    for item in &plan.packages {
        let verb = match item.action {
            PkgAction::Install => "install",
            PkgAction::Upgrade => "upgrade",
            PkgAction::Downgrade => "downgrade",
            PkgAction::Remove => "remove",
            PkgAction::MarkExplicit | PkgAction::MarkDependency => "reason change",
        };
        *packages.entry(verb).or_default() += 1;
    }
    let counts = |map: &BTreeMap<&str, usize>| {
        map.iter()
            .map(|(k, v)| format!("{v} {k}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    if !packages.is_empty() {
        let _ = writeln!(out, "  packages: {}", counts(&packages));
    }
    if !plan.aur.is_empty() {
        let names: Vec<&str> = plan.aur.iter().map(|a| a.name.as_str()).collect();
        let _ = writeln!(out, "  AUR: {}", names.join(", "));
    }
    let mut files: BTreeMap<&str, usize> = BTreeMap::new();
    for item in &plan.files {
        let verb = match item.action {
            FileAction::Create => "create",
            FileAction::Modify => "modify",
            FileAction::Delete => "delete",
            FileAction::Mode => "mode",
            FileAction::Owner => "owner",
            FileAction::Unmanage => "unmanage",
        };
        *files.entry(verb).or_default() += 1;
    }
    if !files.is_empty() {
        let _ = writeln!(out, "  files: {}", counts(&files));
    }
    for (label, len) in [
        ("hooks", plan.hooks.len()),
        ("services", plan.services.len()),
        ("users and groups", plan.users.len()),
        ("home", plan.home.len()),
        ("flatpak", plan.flatpak.len()),
    ] {
        if len > 0 {
            let _ = writeln!(out, "  {label}: {len}");
        }
    }
    let marked: Vec<String> = plan
        .packages
        .iter()
        .filter(|p| !p.risk.is_empty())
        .map(|p| format!("{} ({})", p.name, p.risk.join("; ")))
        .chain(plan.files.iter().filter(|f| !f.risk.is_empty()).map(|f| f.path.clone()))
        .collect();
    for line in marked {
        let _ = writeln!(out, "  ! {line}");
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn meta(n: u32) -> Meta {
        Meta {
            n,
            archix_version: "0.3.0".into(),
            started: "2026-11-16T10:00:00+01:00".into(),
            snapper_pre: Some(41),
            snapper_post: Some(42),
            alpm_window: AlpmWindow {
                start: Some("2026-11-16T09:00:10.250Z".into()),
                end: Some("2026-11-16T09:02:00.900Z".into()),
            },
            home: BTreeMap::new(),
            outcome: Outcome::Ok,
            failed_step: None,
            removed_files: BTreeMap::new(),
        }
    }

    #[test]
    fn meta_and_events_read_the_interface_format() {
        let meta: Meta = serde_json::from_str(
            r#"{"n": 3, "archix_version": "0.3.0", "started": "2026-11-16T10:00:00+01:00", "snapper_pre": 41, "snapper_post": null,
                "alpm_window": {"start": "2026-11-16T09:00:10Z", "end": null}, "home": {"alice": "/nix/store/abc-home-manager-generation"},
                "outcome": "failed", "failed_step": "install"}"#,
        )
        .unwrap();
        assert_eq!(meta.outcome, Outcome::Failed);
        assert_eq!(meta.snapper_post, None);
        assert_eq!(meta.home["alice"], "/nix/store/abc-home-manager-generation");
        let events: Vec<Event> = [
            r#"{"t": "2026-11-16T09:00:00Z", "event": "step-start", "step": "plan"}"#,
            r#"{"t": "2026-11-16T09:00:01Z", "event": "step-end", "step": "plan", "ok": true, "detail": ""}"#,
            r#"{"t": "2026-11-16T09:00:02Z", "event": "action", "step": "install", "kind": "sync-db-replace", "target": "core"}"#,
        ]
        .iter()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
        assert!(matches!(
            &events[2],
            Event::Action {
                kind: ActionKind::SyncDbReplace,
                ..
            }
        ));
    }

    #[test]
    fn times_parse_with_every_offset_form() {
        let utc = parse_time("2026-11-16T09:00:00Z").unwrap();
        assert_eq!(parse_time("2026-11-16T10:00:00+0100"), Some(utc));
        assert_eq!(parse_time("2026-11-16T10:00:00+01:00"), Some(utc));
        assert_eq!(parse_time("2026-11-16T04:30:00-0430"), Some(utc));
        assert_eq!(parse_time("2026-11-16T09:00:00.999999Z"), Some(utc));
        assert_eq!(parse_time("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_time("2026-11-16 09:00"), None);
        assert_eq!(parse_time("2026-11-16T09:00:00"), None);
    }

    #[test]
    fn list_and_show_read_the_journal_tree() {
        let root = tempfile::tempdir().unwrap();
        let dir = switch_dir(root.path(), 2);
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(switch_dir(root.path(), 10)).unwrap();
        fs::write(dir.join("meta.json"), serde_json::to_string(&meta(2)).unwrap()).unwrap();
        fs::write(
            dir.join("journal.jsonl"),
            concat!(
                r#"{"t":"2026-11-16T09:00:10Z","event":"step-start","step":"install"}"#,
                "\n",
                r#"{"t":"2026-11-16T09:00:11Z","event":"action","step":"install","kind":"upgrade","target":"glibc"}"#,
                "\n",
                r#"{"t":"2026-11-16T09:00:12Z","event":"step-end","step":"install","ok":false,"detail":"conflict"}"#,
                "\n",
                r#"{"t":"2026-11-16T09:00:1"#,
            ),
        )
        .unwrap();
        assert_eq!(list(root.path()).unwrap(), vec![2, 10]);
        let listing = render_list(root.path()).unwrap();
        assert!(listing.contains("41..42"), "{listing}");
        assert!(listing.contains("  10  unreadable"), "{listing}");
        let one = render_one(root.path(), 2).unwrap();
        assert!(one.contains("install: FAILED (conflict)"), "{one}");
        assert!(one.contains("upgrade glibc"), "{one}");
        assert!(one.contains("torn last journal line"), "{one}");
        assert!(one.contains("plan: unreadable"), "{one}");
    }
}
