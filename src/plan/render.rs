use std::fmt::Write as _;

use super::model::{AccountAction, FileAction, PkgAction, Plan, SecretStatus, ServiceAction};
use crate::alpm::PinMode;

struct Line {
    risky: bool,
    text: String,
    risk: Vec<String>,
}

fn line(text: String, risk: &[String]) -> Line {
    Line {
        risky: !risk.is_empty(),
        text,
        risk: risk.to_vec(),
    }
}

fn pkg_verb(action: PkgAction) -> &'static str {
    match action {
        PkgAction::Install => "install",
        PkgAction::Upgrade => "upgrade",
        PkgAction::Downgrade => "downgrade",
        PkgAction::Remove => "remove",
        PkgAction::MarkExplicit => "mark explicit",
        PkgAction::MarkDependency => "mark dependency",
    }
}

fn file_verb(action: FileAction) -> &'static str {
    match action {
        FileAction::Create => "create",
        FileAction::Modify => "modify",
        FileAction::Delete => "delete",
        FileAction::Mode => "mode",
        FileAction::Owner => "owner",
        FileAction::Unmanage => "unmanage",
    }
}

fn service_verb(action: ServiceAction) -> &'static str {
    match action {
        ServiceAction::Enable => "enable",
        ServiceAction::Disable => "disable",
        ServiceAction::Mask => "mask",
        ServiceAction::Unmask => "unmask",
        ServiceAction::Start => "start",
        ServiceAction::Restart => "restart",
        ServiceAction::Reload => "reload",
    }
}

fn account_verb(action: AccountAction) -> &'static str {
    match action {
        AccountAction::AddUser => "add user",
        AccountAction::ModifyUser => "modify user",
        AccountAction::RemoveUser => "remove user",
        AccountAction::AddGroup => "add group",
        AccountAction::ModifyGroup => "modify group",
        AccountAction::RemoveGroup => "remove group",
    }
}

fn groups(plan: &Plan, with_diffs: bool) -> Vec<(&'static str, Vec<(Line, Option<String>)>)> {
    let mut out = Vec::new();
    let packages = plan
        .packages
        .iter()
        .map(|p| {
            let version = match (&p.from, &p.to, p.action) {
                (Some(from), Some(to), _) => format!(" {from} -> {to}"),
                (None, Some(to), _) => format!(" {to}"),
                (Some(from), None, PkgAction::Remove) => format!(" {from}"),
                _ => String::new(),
            };
            let repo = p.repo.as_ref().map(|r| format!(" [{r}]")).unwrap_or_default();
            let detail = p.detail.as_ref().map(|d| format!(", {d}")).unwrap_or_default();
            (
                line(
                    format!("{:<15} {}{version}{repo}  ({}{detail})", pkg_verb(p.action), p.name, p.reason.text()),
                    &p.risk,
                ),
                None,
            )
        })
        .collect();
    out.push(("packages", packages));
    let aur = plan
        .aur
        .iter()
        .map(|a| {
            let commit = a.commit.as_deref().unwrap_or("commit unknown");
            let helper = a.helper.as_ref().map(|h| format!(", helper {h}")).unwrap_or_default();
            (line(format!("build {} at {commit}{helper}  ({})", a.name, a.reason), &a.risk), None)
        })
        .collect();
    out.push(("AUR", aur));
    let hooks = plan
        .hooks
        .iter()
        .map(|h| {
            (
                line(format!("{} ({}): {}  (triggered by {})", h.name, h.when, h.description, h.reason), &h.risk),
                None,
            )
        })
        .collect();
    out.push(("hooks", hooks));
    let files = plan
        .files
        .iter()
        .map(|f| {
            let secret = match f.secret {
                Some(SecretStatus::Changed) => " [secret changed]",
                Some(SecretStatus::Unchanged) => " [secret unchanged]",
                Some(SecretStatus::NotChecked) => " [secret: not checked without root]",
                None => "",
            };
            let diff = if with_diffs && f.secret.is_none() { f.diff.clone() } else { None };
            (line(format!("{:<15} {}{secret}  ({})", file_verb(f.action), f.path, f.reason), &f.risk), diff)
        })
        .collect();
    out.push(("files", files));
    let services = plan
        .services
        .iter()
        .map(|s| {
            let user = if s.user { " (user)" } else { "" };
            let reboot = if s.at_reboot { ", applies at reboot" } else { "" };
            (
                line(format!("{:<15} {}{user}  ({}{reboot})", service_verb(s.action), s.unit, s.reason), &s.risk),
                None,
            )
        })
        .collect();
    out.push(("services", services));
    let users = plan
        .users
        .iter()
        .map(|u| {
            let changes = if u.changes.is_empty() { String::new() } else { format!(": {}", u.changes.join(", ")) };
            (line(format!("{:<15} {}{changes}  ({})", account_verb(u.action), u.name, u.reason), &u.risk), None)
        })
        .collect();
    out.push(("users and groups", users));
    for (title, items) in [("home", &plan.home), ("flatpak", &plan.flatpak)] {
        let lines = items
            .iter()
            .map(|i| (line(format!("{}  ({})", i.line, i.reason), &i.risk), None))
            .collect();
        out.push((title, lines));
    }
    if let Some(s) = &plan.snapshot {
        out.push(("snapshot", vec![(line(format!("{}  ({})", s.line, s.reason), &s.risk), None)]));
    }
    out
}

fn header(plan: &Plan) -> String {
    let h = &plan.header;
    let mut out = String::new();
    match (h.mode, h.date) {
        (PinMode::Archive, Some(date)) => {
            write!(out, "pinning: archive {date}").unwrap();
            if let Some(from) = h.moved_from {
                write!(out, " (moved from the lock's {from}: never older than installed)").unwrap();
            }
        }
        (PinMode::Archive, None) => out.push_str("pinning: archive, no lock date"),
        (PinMode::Live, _) => {
            out.push_str("pinning: live");
            if plan.has_package_transaction() {
                out.push_str(" (the package transaction is a full upgrade)");
            }
        }
    }
    writeln!(out).unwrap();
    writeln!(out, "fingerprint: {}", h.fingerprint).unwrap();
    if h.cached_dbs {
        writeln!(out, "sync dbs: from the cache, not fetched now").unwrap();
    }
    out
}

/// The grouped human output: a header, each group with a reason per line and `!` markers, then the summary repeating every `!` line.
pub fn human(plan: &Plan) -> String {
    let mut out = header(plan);
    let mut risky = Vec::new();
    for (title, lines) in groups(plan, true) {
        if lines.is_empty() {
            continue;
        }
        writeln!(out, "\n{title}").unwrap();
        for (l, diff) in lines {
            let mark = if l.risky { "!" } else { " " };
            writeln!(out, "{mark} {}", l.text).unwrap();
            if l.risky {
                writeln!(out, "    ! {}", l.risk.join("; ")).unwrap();
                risky.push(format!("{title}: {}  [{}]", l.text, l.risk.join("; ")));
            }
            if let Some(diff) = diff {
                for d in diff.lines() {
                    writeln!(out, "      {d}").unwrap();
                }
            }
        }
    }
    if !plan.notes.is_empty() {
        writeln!(out, "\nnotes").unwrap();
        for note in &plan.notes {
            writeln!(out, "  {note}").unwrap();
        }
    }
    if !plan.unchecked.is_empty() {
        writeln!(out, "\nnot checked (incomplete plan)").unwrap();
        for item in &plan.unchecked {
            writeln!(out, "  {item}").unwrap();
        }
    }
    if !plan.refusals.is_empty() {
        writeln!(out, "\nswitch would refuse").unwrap();
        for refusal in &plan.refusals {
            writeln!(out, "  {refusal}").unwrap();
        }
    }
    writeln!(out, "\nsummary").unwrap();
    if !plan.has_changes() {
        writeln!(out, "  nothing to do").unwrap();
    } else {
        let count: usize = groups(plan, false)
            .iter()
            .filter(|(t, _)| !matches!(*t, "hooks" | "snapshot"))
            .map(|(_, l)| l.len())
            .sum();
        writeln!(out, "  {count} changes, {} marked !", risky.len()).unwrap();
        for r in &risky {
            writeln!(out, "  ! {r}").unwrap();
        }
    }
    if plan.has_package_transaction() {
        writeln!(out, "  file conflicts and disk space are checked at commit").unwrap();
    }
    out
}

/// `--stat`: the header and names only.
pub fn stat(plan: &Plan) -> String {
    let mut out = header(plan);
    for (title, names) in [
        ("packages", plan.packages.iter().map(|p| format!("{} {}", pkg_verb(p.action), p.name)).collect::<Vec<_>>()),
        ("AUR", plan.aur.iter().map(|a| a.name.clone()).collect()),
        ("hooks", plan.hooks.iter().map(|h| h.name.clone()).collect()),
        ("files", plan.files.iter().map(|f| format!("{} {}", file_verb(f.action), f.path)).collect()),
        ("services", plan.services.iter().map(|s| format!("{} {}", service_verb(s.action), s.unit)).collect()),
        ("users and groups", plan.users.iter().map(|u| format!("{} {}", account_verb(u.action), u.name)).collect()),
    ] {
        if !names.is_empty() {
            writeln!(out, "{title}: {}", names.join(", ")).unwrap();
        }
    }
    for refusal in &plan.refusals {
        writeln!(out, "refuses: {refusal}").unwrap();
    }
    for item in &plan.unchecked {
        writeln!(out, "unchecked: {item}").unwrap();
    }
    out
}

pub fn json(plan: &Plan) -> String {
    let mut text = serde_json::to_string_pretty(plan).expect("a plan serializes");
    text.push('\n');
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::model::*;

    fn plan() -> Plan {
        let mut plan = Plan::new(Header {
            mode: PinMode::Archive,
            date: Some("2026-11-03".parse().unwrap()),
            moved_from: Some("2026-11-01".parse().unwrap()),
            fingerprint: "abc".into(),
            cached_dbs: false,
        });
        plan.packages.push(PackageItem {
            action: PkgAction::Upgrade,
            name: "glibc".into(),
            from: Some("2.40-1".into()),
            to: Some("2.41-1".into()),
            repo: Some("core".into()),
            reason: PkgReason::LockMoved,
            detail: None,
            risk: vec!["in the shared-library closure of pacman, sudo, systemd, login or the boot tools".into()],
        });
        plan.packages.push(PackageItem {
            action: PkgAction::Install,
            name: "ripgrep".into(),
            from: None,
            to: Some("14-1".into()),
            repo: Some("extra".into()),
            reason: PkgReason::Declared,
            detail: None,
            risk: Vec::new(),
        });
        plan.files.push(FileItem {
            action: FileAction::Modify,
            path: "/etc/wifi.psk".into(),
            reason: "secret changed".into(),
            diff: Some("SHOULD NOT PRINT".into()),
            secret: Some(SecretStatus::Changed),
            hand_edited: false,
            risk: Vec::new(),
        });
        plan.services.push(ServiceItem {
            action: ServiceAction::Enable,
            unit: "NetworkManager.service".into(),
            user: false,
            at_reboot: true,
            reason: "declared".into(),
            risk: vec!["applies at reboot".into()],
        });
        plan
    }

    #[test]
    fn human_output_groups_reasons_marks_and_summary() {
        let text = human(&plan());
        assert!(text.starts_with("pinning: archive 2026-11-03 (moved from the lock's 2026-11-01"), "{text}");
        assert!(text.contains("\npackages\n! upgrade         glibc 2.40-1 -> 2.41-1 [core]  (lock moved)\n"), "{text}");
        assert!(text.contains("  install         ripgrep 14-1 [extra]  (declared)\n"), "{text}");
        assert!(text.contains("applies at reboot"), "{text}");
        assert!(!text.contains("SHOULD NOT PRINT"));
        let summary = &text[text.find("\nsummary").unwrap()..];
        assert!(summary.contains("4 changes, 2 marked !"), "{summary}");
        assert!(summary.contains("! packages: upgrade         glibc"), "{summary}");
        assert!(summary.contains("! services: enable"), "{summary}");
        assert!(summary.contains("file conflicts and disk space are checked at commit"));
    }

    #[test]
    fn stat_lists_names_only() {
        let text = stat(&plan());
        assert!(text.contains("packages: upgrade glibc, install ripgrep\n"), "{text}");
        assert!(!text.contains("lock moved"));
    }

    #[test]
    fn empty_plan_says_nothing_to_do() {
        let p = Plan::new(Header {
            mode: PinMode::Live,
            date: None,
            moved_from: None,
            fingerprint: "f".into(),
            cached_dbs: true,
        });
        let text = human(&p);
        assert!(text.contains("pinning: live\n"));
        assert!(text.contains("sync dbs: from the cache"));
        assert!(text.contains("nothing to do"));
        assert_eq!(p.exit_code(), 0);
    }
}
