//! Units a plan's package changes touch (SCOPE 3.1 services group): start, restart or reload lines, `! applies at reboot` for the never-restart-live list, and the [Install] symlinks of units not installed yet.
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use super::hooks::Op;
use super::model::{ServiceAction, ServiceItem};
use super::system::at_reboot;

pub const SYSTEM_UNIT_DIR: &str = "usr/lib/systemd/system/";
const KINDS: [&str; 4] = [".service", ".socket", ".timer", ".path"];
pub const AFTER_INSTALL: &str = "symlinks known after install";

/// The unit a package file path installs, for system units a switch could restart.
pub fn unit_name(path: &str) -> Option<&str> {
    let name = path.trim_start_matches('/').strip_prefix(SYSTEM_UNIT_DIR)?;
    (!name.is_empty() && !name.contains('/') && KINDS.iter().any(|k| name.ends_with(k))).then_some(name)
}

fn is_template(unit: &str) -> bool {
    unit.contains("@.")
}

fn section<'a>(text: &'a str, name: &'a str) -> impl Iterator<Item = (&'a str, &'a str)> + 'a {
    let mut current = String::new();
    text.lines().map(str::trim).filter_map(move |line| {
        if let Some(s) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            current = s.to_string();
            return None;
        }
        if current != name || line.starts_with('#') || line.starts_with(';') {
            return None;
        }
        let (key, value) = line.split_once('=')?;
        Some((key.trim(), value.trim()))
    })
}

/// Whether the unit declares a non-empty ExecReload=.
pub fn exec_reload(text: &str) -> bool {
    section(text, "Service").any(|(k, v)| k == "ExecReload" && !v.is_empty())
}

/// The symlinks `systemctl enable` makes from the unit's [Install] section, plus the units Also= enables with it.
pub fn install_symlinks(unit: &str, text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (key, value) in section(text, "Install") {
        for target in value.split_whitespace() {
            match key {
                "WantedBy" => out.push(format!("/etc/systemd/system/{target}.wants/{unit}")),
                "RequiredBy" => out.push(format!("/etc/systemd/system/{target}.requires/{unit}")),
                "UpheldBy" => out.push(format!("/etc/systemd/system/{target}.upholds/{unit}")),
                "Alias" => out.push(format!("/etc/systemd/system/{target}")),
                "Also" => out.push(format!("also enables {target}")),
                _ => {}
            }
        }
    }
    out
}

/// The downloaded package file for `name` at `version` in one of pacman's cache dirs, if it's there.
pub fn cached_package(cache_dirs: &[PathBuf], name: &str, version: &str) -> Option<PathBuf> {
    let prefix = format!("{name}-{version}-");
    cache_dirs.iter().find_map(|dir| {
        fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).find(|path| {
            let file = path.file_name().unwrap_or_default().to_string_lossy();
            file.strip_prefix(&prefix)
                .and_then(|rest| rest.split_once(".pkg.tar"))
                .is_some_and(|(arch, ext)| !arch.contains('-') && !ext.ends_with(".sig") && !ext.ends_with(".part"))
        })
    })
}

/// A package file's paths and the text of each system unit in it (gzip, zstd or plain tar).
pub fn read_package(path: &Path) -> Result<(Vec<String>, BTreeMap<String, String>)> {
    let raw = fs::read(path)?;
    let bytes = if raw.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        zstd::decode_all(&raw[..])?
    } else if raw.starts_with(&[0x1f, 0x8b]) {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(&raw[..]).read_to_end(&mut out)?;
        out
    } else if raw.get(257..262) == Some(b"ustar") {
        raw
    } else {
        bail!("{} isn't a zstd, gzip or plain tar package", path.display());
    };
    let mut files = Vec::new();
    let mut units = BTreeMap::new();
    let mut links = Vec::new();
    for entry in tar::Archive::new(&bytes[..]).entries()? {
        let mut entry = entry?;
        let name = entry.path()?.to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        if let Some(unit) = unit_name(&name).map(str::to_string) {
            if entry.header().entry_type().is_symlink() {
                let target = entry
                    .link_name()?
                    .map(|t| t.to_string_lossy().into_owned())
                    .unwrap_or_default();
                links.push((unit, target));
            } else {
                let mut text = String::new();
                entry.read_to_string(&mut text)?;
                units.insert(unit, text);
            }
        }
        files.push(name);
    }
    for (unit, target) in links {
        let base = target.rsplit('/').next().unwrap_or_default();
        if let Some(text) = units.get(base).cloned() {
            units.insert(unit, text);
        }
    }
    Ok((files, units))
}

/// One package in the plan, as the units logic sees it.
#[derive(Debug, Clone)]
pub struct PackageUnits {
    pub package: String,
    pub op: Op,
    /// The package's files now (empty when it isn't installed).
    pub old_files: Vec<String>,
    /// Its files after the plan, from the .files dbs or the package file; None when neither is there.
    pub new_files: Option<Vec<String>>,
    /// Unit texts from the downloaded package file; None when it isn't in the cache.
    pub archive: Option<BTreeMap<String, String>>,
}

pub struct UnitContext<'a> {
    pub root: &'a Path,
    /// Declared system services, true for enabled.
    pub declared: &'a BTreeMap<String, bool>,
    pub enabled: &'a dyn Fn(&str, bool) -> Option<bool>,
    pub active: &'a dyn Fn(&str, bool) -> Option<bool>,
    pub ssh_session: bool,
    pub display_manager: Option<&'a str>,
    /// Installed packages whose config files the plan's files group changes, with their file lists.
    pub config_changed: &'a BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Default)]
pub struct UnitLines {
    pub services: Vec<ServiceItem>,
    /// What each declared-enabled unit's enable line makes, for units whose text the plan reads.
    pub symlinks: BTreeMap<String, String>,
    pub notes: Vec<String>,
}

fn units_in(files: &[String]) -> BTreeSet<String> {
    files.iter().filter_map(|f| unit_name(f)).map(str::to_string).collect()
}

fn rank(action: ServiceAction) -> u8 {
    match action {
        ServiceAction::Restart => 3,
        ServiceAction::Reload => 2,
        ServiceAction::Start => 1,
        _ => 0,
    }
}

impl UnitContext<'_> {
    fn disk_text(&self, unit: &str) -> Option<String> {
        ["etc/systemd/system/", SYSTEM_UNIT_DIR]
            .iter()
            .find_map(|dir| fs::read_to_string(self.root.join(dir).join(unit)).ok())
    }

    fn item(&self, action: ServiceAction, unit: &str, reason: String) -> ServiceItem {
        let reboot = at_reboot(unit, self.ssh_session, self.display_manager);
        ServiceItem {
            action,
            unit: unit.to_string(),
            user: false,
            at_reboot: reboot,
            reason,
            risk: if reboot {
                vec!["applies at reboot".into()]
            } else {
                Vec::new()
            },
        }
    }

    /// Whether a unit counts as live: active, or enabled when its active state can't be told.
    fn live(&self, unit: &str) -> Option<bool> {
        match (self.active)(unit, false) {
            Some(active) => Some(active),
            None => (self.enabled)(unit, false).filter(|e| *e).map(|_| true),
        }
    }
}

/// The start/restart/reload lines and enable-line symlinks the plan's package and config changes call for.
pub fn plan_units(packages: &[PackageUnits], ctx: &UnitContext) -> UnitLines {
    let mut out = UnitLines::default();
    let mut lines: BTreeMap<String, ServiceItem> = BTreeMap::new();
    let mut push = |item: ServiceItem| match lines.get(&item.unit) {
        Some(have) if rank(have.action) >= rank(item.action) => {}
        _ => {
            lines.insert(item.unit.clone(), item);
        }
    };
    let installing: BTreeSet<String> = packages
        .iter()
        .filter(|p| p.op != Op::Remove)
        .flat_map(|p| {
            let mut units = units_in(p.new_files.as_deref().unwrap_or_default());
            units.extend(p.archive.iter().flat_map(|a| a.keys().cloned()));
            units
        })
        .collect();
    let mut unknown_installs = false;
    for p in packages {
        let mut new_units = units_in(p.new_files.as_deref().unwrap_or_default());
        new_units.extend(p.archive.iter().flat_map(|a| a.keys().cloned()));
        let old_units = units_in(&p.old_files);
        match p.op {
            Op::Install => {
                if p.new_files.is_none() && p.archive.is_none() {
                    unknown_installs = true;
                }
                for unit in &new_units {
                    if is_template(unit) {
                        continue;
                    }
                    if ctx.declared.get(unit) == Some(&true) {
                        let text = p.archive.as_ref().and_then(|a| a.get(unit));
                        let symlinks = match text {
                            Some(text) => install_symlinks(unit, text).join(", "),
                            None => AFTER_INSTALL.to_string(),
                        };
                        out.symlinks.insert(unit.clone(), symlinks);
                        push(ctx.item(
                            ServiceAction::Start,
                            unit,
                            format!("declared, from {} installed now", p.package),
                        ));
                    } else if ctx.live(unit) == Some(true) {
                        push(ctx.item(ServiceAction::Restart, unit, format!("{} now provides it", p.package)));
                    }
                }
            }
            Op::Upgrade => {
                for unit in new_units.union(&old_units) {
                    if is_template(unit) {
                        if at_reboot(unit, ctx.ssh_session, ctx.display_manager) {
                            push(ctx.item(ServiceAction::Restart, unit, format!("{} upgraded", p.package)));
                        }
                        continue;
                    }
                    if ctx.live(unit) != Some(true) {
                        continue;
                    }
                    if p.new_files.is_some() && !new_units.contains(unit) && !installing.contains(unit) {
                        out.notes.push(format!(
                            "{unit} is active and {} drops its unit file; it keeps running until stopped or reboot",
                            p.package
                        ));
                        continue;
                    }
                    push(ctx.item(ServiceAction::Restart, unit, format!("{} upgraded", p.package)));
                }
            }
            Op::Remove => {
                for unit in old_units.iter().filter(|u| !is_template(u) && !installing.contains(*u)) {
                    if ctx.live(unit) == Some(true) {
                        out.notes.push(format!(
                            "{unit} is active and {} is removed; it keeps running until stopped or reboot",
                            p.package
                        ));
                    }
                }
            }
        }
    }
    for (package, files) in ctx.config_changed {
        for unit in units_in(files).iter().filter(|u| !is_template(u)) {
            if ctx.live(unit) != Some(true) {
                continue;
            }
            let reload = ctx.disk_text(unit).is_some_and(|t| exec_reload(&t));
            let action = if reload {
                ServiceAction::Reload
            } else {
                ServiceAction::Restart
            };
            push(ctx.item(action, unit, format!("config of {package} changed")));
        }
    }
    for (unit, enable) in ctx.declared {
        if !*enable || out.symlinks.contains_key(unit) {
            continue;
        }
        match ctx.disk_text(unit) {
            Some(text) => {
                let links = install_symlinks(unit, &text);
                if !links.is_empty() {
                    out.symlinks.insert(unit.clone(), links.join(", "));
                }
            }
            None if unknown_installs => {
                out.symlinks.insert(unit.clone(), AFTER_INSTALL.to_string());
            }
            None => {}
        }
    }
    out.services = lines.into_values().collect();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const NGINX: &str = "[Unit]\nDescription=nginx\n[Service]\nExecStart=/usr/bin/nginx\nExecReload=/usr/bin/nginx -s reload\n[Install]\nWantedBy=multi-user.target\n";
    const SSHD: &str =
        "[Service]\nExecStart=/usr/bin/sshd -D\n[Install]\nWantedBy=multi-user.target\nAlias=ssh.service\n";

    fn files(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|p| p.to_string()).collect()
    }

    struct Fx {
        root: tempfile::TempDir,
        declared: BTreeMap<String, bool>,
        active: BTreeSet<&'static str>,
        enabled: BTreeSet<&'static str>,
        config: BTreeMap<String, Vec<String>>,
        ssh: bool,
    }

    impl Fx {
        fn new() -> Fx {
            Fx {
                root: tempfile::tempdir().unwrap(),
                declared: BTreeMap::new(),
                active: BTreeSet::new(),
                enabled: BTreeSet::new(),
                config: BTreeMap::new(),
                ssh: false,
            }
        }

        fn unit_on_disk(&self, unit: &str, text: &str) {
            let dir = self.root.path().join(SYSTEM_UNIT_DIR);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join(unit), text).unwrap();
        }

        fn run(&self, packages: &[PackageUnits]) -> UnitLines {
            let active = |u: &str, _: bool| Some(self.active.contains(u));
            let enabled = |u: &str, _: bool| Some(self.enabled.contains(u));
            plan_units(
                packages,
                &UnitContext {
                    root: self.root.path(),
                    declared: &self.declared,
                    enabled: &enabled,
                    active: &active,
                    ssh_session: self.ssh,
                    display_manager: Some("sddm.service"),
                    config_changed: &self.config,
                },
            )
        }
    }

    fn upgrade(name: &str, list: &[&str]) -> PackageUnits {
        PackageUnits {
            package: name.into(),
            op: Op::Upgrade,
            old_files: files(list),
            new_files: Some(files(list)),
            archive: None,
        }
    }

    fn lines(l: &UnitLines) -> Vec<(&str, ServiceAction, bool)> {
        l.services
            .iter()
            .map(|s| (s.unit.as_str(), s.action, s.at_reboot))
            .collect()
    }

    #[test]
    fn unit_names_are_system_units_a_switch_can_restart() {
        assert_eq!(unit_name("usr/lib/systemd/system/nginx.service"), Some("nginx.service"));
        assert_eq!(unit_name("usr/lib/systemd/system/fstrim.timer"), Some("fstrim.timer"));
        assert_eq!(unit_name("usr/lib/systemd/system/"), None);
        assert_eq!(
            unit_name("usr/lib/systemd/system/multi-user.target.wants/x.service"),
            None
        );
        assert_eq!(unit_name("usr/lib/systemd/system/local-fs.target"), None);
        assert_eq!(unit_name("usr/lib/systemd/user/pipewire.service"), None);
    }

    #[test]
    fn install_sections_become_symlinks_and_exec_reload_is_read_from_service() {
        assert_eq!(
            install_symlinks("sshd.service", SSHD),
            [
                "/etc/systemd/system/multi-user.target.wants/sshd.service",
                "/etc/systemd/system/ssh.service"
            ]
        );
        assert!(exec_reload(NGINX));
        assert!(!exec_reload(SSHD));
        assert!(!exec_reload("[Unit]\nExecReload=/bin/true\n[Service]\nExecReload=\n"));
    }

    #[test]
    fn upgrades_restart_active_units_and_leave_inactive_ones() {
        let mut fx = Fx::new();
        fx.active.insert("cups.service");
        let l = fx.run(&[upgrade(
            "cups",
            &[
                "usr/bin/cupsd",
                "usr/lib/systemd/system/cups.service",
                "usr/lib/systemd/system/cups.socket",
            ],
        )]);
        assert_eq!(lines(&l), [("cups.service", ServiceAction::Restart, false)]);
        assert_eq!(l.services[0].reason, "cups upgraded");
    }

    #[test]
    fn the_never_restart_live_list_applies_at_reboot_with_a_mark() {
        let mut fx = Fx::new();
        fx.ssh = true;
        fx.active.extend([
            "systemd-journald.service",
            "systemd-logind.service",
            "sshd.service",
            "sddm.service",
            "dbus.service",
        ]);
        let l = fx.run(&[
            upgrade(
                "systemd",
                &[
                    "usr/lib/systemd/system/systemd-journald.service",
                    "usr/lib/systemd/system/systemd-logind.service",
                    "usr/lib/systemd/system/user@.service",
                    "usr/lib/systemd/system/getty@.service",
                ],
            ),
            upgrade("openssh", &["usr/lib/systemd/system/sshd.service"]),
            upgrade("sddm", &["usr/lib/systemd/system/sddm.service"]),
            upgrade("dbus", &["usr/lib/systemd/system/dbus.service"]),
        ]);
        assert_eq!(
            lines(&l),
            [
                ("dbus.service", ServiceAction::Restart, true),
                ("sddm.service", ServiceAction::Restart, true),
                ("sshd.service", ServiceAction::Restart, true),
                ("systemd-journald.service", ServiceAction::Restart, true),
                ("systemd-logind.service", ServiceAction::Restart, true),
                ("user@.service", ServiceAction::Restart, true),
            ]
        );
        assert!(l.services.iter().all(|s| s.risk == ["applies at reboot"]));
        fx.ssh = false;
        let l = fx.run(&[upgrade("openssh", &["usr/lib/systemd/system/sshd.service"])]);
        assert_eq!(
            lines(&l),
            [("sshd.service", ServiceAction::Restart, false)],
            "sshd restarts live off ssh"
        );
    }

    #[test]
    fn a_config_only_change_reloads_where_the_unit_declares_exec_reload() {
        let mut fx = Fx::new();
        fx.active.extend(["nginx.service", "sshd.service"]);
        fx.unit_on_disk("nginx.service", NGINX);
        fx.unit_on_disk("sshd.service", SSHD);
        fx.config.insert(
            "nginx".into(),
            files(&["etc/nginx/nginx.conf", "usr/lib/systemd/system/nginx.service"]),
        );
        fx.config.insert(
            "openssh".into(),
            files(&["etc/ssh/sshd_config", "usr/lib/systemd/system/sshd.service"]),
        );
        let l = fx.run(&[]);
        assert_eq!(
            lines(&l),
            [
                ("nginx.service", ServiceAction::Reload, false),
                ("sshd.service", ServiceAction::Restart, false)
            ]
        );
        let l = fx.run(&[upgrade("nginx", &["usr/lib/systemd/system/nginx.service"])]);
        assert_eq!(
            l.services[0].action,
            ServiceAction::Restart,
            "a package upgrade beats a config reload"
        );
    }

    #[test]
    fn new_packages_units_read_install_from_the_package_file_or_say_after_install() {
        let mut fx = Fx::new();
        fx.declared.insert("nginx.service".into(), true);
        fx.declared.insert("sshd.service".into(), true);
        let nginx = PackageUnits {
            package: "nginx".into(),
            op: Op::Install,
            old_files: Vec::new(),
            new_files: Some(files(&["usr/lib/systemd/system/nginx.service"])),
            archive: Some(BTreeMap::from([("nginx.service".to_string(), NGINX.to_string())])),
        };
        let openssh = PackageUnits {
            package: "openssh".into(),
            op: Op::Install,
            old_files: Vec::new(),
            new_files: Some(files(&["usr/lib/systemd/system/sshd.service"])),
            archive: None,
        };
        let l = fx.run(&[nginx, openssh]);
        assert_eq!(
            l.symlinks["nginx.service"],
            "/etc/systemd/system/multi-user.target.wants/nginx.service"
        );
        assert_eq!(l.symlinks["sshd.service"], AFTER_INSTALL);
        assert_eq!(
            lines(&l),
            [
                ("nginx.service", ServiceAction::Start, false),
                ("sshd.service", ServiceAction::Start, false)
            ]
        );
    }

    #[test]
    fn an_install_with_no_file_list_leaves_declared_units_after_install() {
        let mut fx = Fx::new();
        fx.declared.insert("tor.service".into(), true);
        let tor = PackageUnits {
            package: "tor".into(),
            op: Op::Install,
            old_files: Vec::new(),
            new_files: None,
            archive: None,
        };
        let l = fx.run(&[tor]);
        assert_eq!(l.symlinks["tor.service"], AFTER_INSTALL);
        assert!(l.services.is_empty());
    }

    #[test]
    fn a_swap_restarts_the_unit_the_new_package_takes_over_and_removals_only_note() {
        let mut fx = Fx::new();
        fx.active.extend(["ntpd.service", "chronyd.service"]);
        let old = PackageUnits {
            package: "ntp".into(),
            op: Op::Remove,
            old_files: files(&["usr/lib/systemd/system/ntpd.service"]),
            new_files: None,
            archive: None,
        };
        let new = PackageUnits {
            package: "ntpsec".into(),
            op: Op::Install,
            old_files: Vec::new(),
            new_files: Some(files(&["usr/lib/systemd/system/ntpd.service"])),
            archive: None,
        };
        let chrony = PackageUnits {
            package: "chrony".into(),
            op: Op::Remove,
            old_files: files(&["usr/lib/systemd/system/chronyd.service"]),
            new_files: None,
            archive: None,
        };
        let l = fx.run(&[old, new, chrony]);
        assert_eq!(lines(&l), [("ntpd.service", ServiceAction::Restart, false)]);
        assert_eq!(l.notes.len(), 1);
        assert!(
            l.notes[0].starts_with("chronyd.service is active and chrony is removed"),
            "{:?}",
            l.notes
        );
    }

    #[test]
    fn unknown_active_state_falls_back_to_enabled() {
        let fx = Fx::new();
        let active = |_: &str, _: bool| None;
        let enabled = |u: &str, _: bool| Some(u == "cronie.service");
        let l = plan_units(
            &[upgrade("cronie", &["usr/lib/systemd/system/cronie.service"])],
            &UnitContext {
                root: fx.root.path(),
                declared: &fx.declared,
                enabled: &enabled,
                active: &active,
                ssh_session: false,
                display_manager: None,
                config_changed: &fx.config,
            },
        );
        assert_eq!(lines(&l), [("cronie.service", ServiceAction::Restart, false)]);
    }

    #[test]
    fn units_and_install_sections_are_read_from_a_cached_package_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut tar = tar::Builder::new(Vec::new());
        let mut add = |path: &str, body: &[u8]| {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(&mut header, path, body).unwrap();
        };
        add(".PKGINFO", b"pkgname = nginx\n");
        add("usr/bin/nginx", b"");
        add("usr/lib/systemd/system/nginx.service", NGINX.as_bytes());
        let mut link = tar::Header::new_gnu();
        link.set_entry_type(tar::EntryType::Symlink);
        link.set_size(0);
        tar.append_link(&mut link, "usr/lib/systemd/system/nginx-alias.service", "nginx.service")
            .unwrap();
        let zst = zstd::encode_all(&tar.into_inner().unwrap()[..], 3).unwrap();
        fs::write(dir.path().join("nginx-1.27.0-1-x86_64.pkg.tar.zst"), &zst).unwrap();
        fs::write(dir.path().join("nginx-1.27.0-1-x86_64.pkg.tar.zst.sig"), b"").unwrap();
        fs::write(dir.path().join("nginx-mod-1.27.0-1-x86_64.pkg.tar.zst"), b"").unwrap();
        let dirs = [PathBuf::from("/nonexistent"), dir.path().to_path_buf()];
        let found = cached_package(&dirs, "nginx", "1.27.0-1").unwrap();
        assert!(found.ends_with("nginx-1.27.0-1-x86_64.pkg.tar.zst"));
        assert!(cached_package(&dirs, "nginx", "1.26.0-1").is_none());
        let (paths, units) = read_package(&found).unwrap();
        assert!(paths.contains(&"usr/bin/nginx".to_string()) && !paths.contains(&".PKGINFO".to_string()));
        assert!(exec_reload(&units["nginx.service"]));
        assert_eq!(units["nginx-alias.service"], NGINX);
    }
}
