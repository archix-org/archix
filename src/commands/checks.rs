//! What `archix switch` refuses before it asks anything, in one place, so `archix diff` reports the same refusals.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::Result;

use archix::archive::Archive;
use archix::downgrade::unsafe_downgrades;
use archix::lock::HostLock;
use archix::packages::{Packages, check_packages};
use archix::pacman::LocalPkg;
use archix::paths::Paths;
use archix::plan::Change;
use archix::system::apply::SystemChange;
use archix::system::boot::BootPlan;
use archix::system::{self, Rendered};

use super::snapshot::Rollbacks;
use super::{Declared, Target, count};

/// A switch as planned: what it would change, and what it changes it on.
pub(super) struct Planned<'a> {
    pub target: &'a Target<'a>,
    pub paths: &'a Paths,
    pub declared: &'a Declared,
    pub boot: Option<&'a BootPlan>,
    pub rollbacks: &'a Rollbacks,
    pub lock: &'a HostLock,
    pub installed: &'a BTreeMap<String, LocalPkg>,
    pub archive: &'a Archive,
    pub rendered: &'a Rendered,
    pub change: &'a SystemChange,
    pub changes: &'a [Change],
    /// Whether the switch applies anything; one that finds nothing to do builds no boot images and changes no packages.
    pub applies: bool,
    pub allow_limine_boot: bool,
    /// Also has pacman verify the package files at hand, which a switch does anyway once it has them all.
    pub signatures: bool,
}

/// What the checks found.
#[derive(Default)]
pub(super) struct Found {
    /// Why the switch is refused, each as the switch fails with it.
    pub refusals: Vec<anyhow::Error>,
    pub warnings: Vec<String>,
    /// What couldn't be checked here: it needs root, a download or a build.
    pub unchecked: Vec<String>,
    /// Downgrades the switch asks about, and refuses with --noconfirm unless --allow-unsafe-downgrade.
    pub unsafe_downgrades: Vec<String>,
    /// The switch only has the firmware try systemd-boot, as Limine started this boot.
    pub limine_boot: bool,
}

impl Found {
    /// Fails with the refusals, printing all but the last, which the caller's error is.
    pub(super) fn fail(&mut self) -> Result<()> {
        let Some(last) = self.refusals.pop() else {
            return Ok(());
        };
        for refusal in self.refusals.drain(..) {
            eprintln!("error: {refusal:#}");
        }
        Err(last)
    }

    fn take(&mut self, what: &str, result: Result<()>) {
        let Err(e) = result else { return };
        let denied = e.chain().any(|cause| {
            cause
                .downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::PermissionDenied)
        });
        match denied && !is_root() {
            true => self.unchecked.push(format!("{what} (needs root)")),
            false => self.refusals.push(e),
        }
    }
}

fn is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

/// Every check a switch makes before its prompt: nothing here changes the system, downloads a package or builds one.
pub(super) fn before_prompt(planned: &Planned) -> Found {
    let Planned {
        target,
        paths,
        lock,
        rendered,
        change,
        changes,
        ..
    } = *planned;
    let mut found = Found::default();
    let here = super::switch::current_root(paths);
    let db = super::switch::installed_db(paths);
    let place = |path: &str| Some(here.join(path.trim_start_matches('/')));

    found.take("the root", super::switch::refuse_unusual_root(paths, "switch"));
    found.take("the lock's CPU level", fits_cpu(lock, target.host));
    found.take("managed files", system::apply::check(paths, change, &place));
    found.take(
        "sudoers and tmpfiles lines",
        system::live::check_files(change, &|path: &str| {
            let file = rendered.state.files.get(path)?;
            rendered.blobs.get(&file.sha256).cloned()
        }),
    );
    found.take(
        "AppImages' files in /usr",
        system::live::refuse_packaged_usr_files(change, &db).map_err(|e| anyhow::anyhow!("{e:#}; nothing was changed")),
    );

    let new_text = |path: &str| match rendered.state.files.get(path) {
        Some(file) => rendered
            .blobs
            .get(&file.sha256)
            .map(|blob| String::from_utf8_lossy(blob).into_owned()),
        None => fs::read_to_string(here.join(path.trim_start_matches('/'))).ok(),
    };
    let crypttab = format!(
        "{}{}",
        new_text("/etc/crypttab").unwrap_or_default(),
        new_text("/etc/crypttab.initramfs").unwrap_or_default()
    );
    let cmdline = new_text("/etc/kernel/cmdline").unwrap_or_default();
    match system::fstab::check(&paths.root, &rendered.state.fstab, &crypttab, &cmdline) {
        Ok(warnings) => found.warnings.extend(warnings),
        Err(e) => found.take("fstab", Err(e)),
    }
    let new_crypttab = super::switch::next_crypttab(paths, &here, change, &rendered.blobs);
    match system::fstab::check_drops(
        &paths.root,
        &here.join("etc"),
        system::fstab::esp_of(target.config),
        &rendered.state.fstab,
        &new_crypttab,
        &system::fstab::acknowledged(target.config),
    ) {
        Ok(warnings) => found.warnings.extend(warnings),
        Err(e) => found.take("fstab lines the switch drops", Err(e)),
    }

    if let Some(rules) = rendered.state.files.get(system::firewall::RULES) {
        match is_root() {
            true => found.take(
                "the firewall's rules",
                system::firewall::validate(&String::from_utf8_lossy(&rendered.blobs[&rules.sha256])),
            ),
            false => found
                .unchecked
                .push("the firewall's rules (nft needs root)".to_string()),
        }
        found.take(
            "other firewalls",
            system::firewall::check_others(&rendered.state.services, |unit| {
                system::apply::is_enabled(&here, unit, false)
            }),
        );
        if system::apply::is_enabled(&here, "nftables.service", false)
            && rendered.state.services.get("nftables.service") != Some(&false)
        {
            found.warnings.push(
                "warning: nftables.service is enabled too, so /etc/nftables.conf (on Arch: drop everything but SSH) still applies on top of the archix firewall; set services[\"nftables.service\"] = false".to_string(),
            );
        }
    }

    found.unsafe_downgrades = unsafe_downgrades(changes, &rendered.state.unsafe_downgrades);
    found.take(
        "packages outside the config that a downgrade breaks",
        super::switch::check_partial(
            paths,
            planned.archive,
            lock,
            planned.installed,
            changes,
            &planned.declared.repos,
        ),
    );
    if planned.applies {
        boot_checks(planned, &mut found);
        package_checks(planned, &mut found);
    }
    found
}

/// A lock made for a newer CPU level than this machine runs was locked elsewhere.
pub(super) fn fits_cpu(lock: &HostLock, host: &str) -> Result<()> {
    let beyond = archix::archive::beyond_cpu(
        lock.packages
            .iter()
            .map(|(name, pkg)| (name.as_str(), pkg.file.as_str())),
        &archix::archive::architectures(),
    );
    if beyond.is_empty() {
        return Ok(());
    }
    let shown: Vec<&str> = beyond.iter().take(6).map(String::as_str).collect();
    anyhow::bail!(
        "archix.lock for {host} has packages built for a newer CPU level than this machine runs, so it was locked elsewhere: {}{}. Run `archix lock` on this machine; nothing was changed",
        shown.join(", "),
        if beyond.len() > shown.len() { ", ..." } else { "" }
    )
}

/// The move from Limine, and the boot images the switch would build.
fn boot_checks(planned: &Planned, found: &mut Found) {
    let Planned { paths, changes, .. } = *planned;
    let first = planned.rollbacks.lays_out() && !changes.is_empty();
    // A boot Limine started has no boot counting, fallback or rescue entry: package changes wait for a boot through systemd-boot.
    let mut limine_boot = match (first, planned.boot) {
        (false, Some(plan)) => system::limine::unprotected(paths, plan),
        _ => None,
    };
    if let Some(why) = &limine_boot {
        found.warnings.push(format!("warning: {why}"));
    }
    if changes.is_empty() || planned.allow_limine_boot {
        limine_boot = None;
    }
    if limine_boot.is_some() && system::limine::out_of_methods(paths) {
        found.refusals.push(anyhow::anyhow!(
            "{} not applied: this boot came through Limine and the firmware starts it whatever archix sets; nothing was changed",
            count(changes.len(), "package change", "package changes")
        ));
    }
    found.limine_boot = limine_boot.is_some();
    if let Some(plan) = planned.boot
        && !first
        && limine_boot.is_none()
    {
        found.take("boot image signing", system::boot::check_signing(plan));
        found.take(
            "room on the ESP",
            system::menu::precheck(paths, plan).map_err(|e| e.context("nothing was changed")),
        );
    }
}

/// File conflicts, and signatures when asked, of the packages the switch installs, when every one of their files is at hand.
fn package_checks(planned: &Planned, found: &mut Found) {
    let Planned {
        paths, lock, changes, ..
    } = *planned;
    let caches = [paths.packages(), paths.pacman_cache.clone()];
    let (mut install, mut remove, mut absent, mut builds) = (Vec::new(), Vec::new(), 0, 0);
    for change in changes {
        match change {
            Change::Install { name, .. }
            | Change::Upgrade { name, .. }
            | Change::Downgrade { name, .. }
            | Change::Rebuild { name, .. } => {
                let Some(pkg) = lock.packages.get(name) else { continue };
                if pkg.is_built() {
                    builds += 1;
                    continue;
                }
                let at_hand = caches
                    .iter()
                    .map(|cache| cache.join(&pkg.file))
                    .find(|path| usable(path, &pkg.sha256, planned.signatures));
                match at_hand {
                    Some(path) => install.push(path),
                    None => absent += 1,
                }
            }
            Change::Remove { name, .. } => remove.push(name.clone()),
            Change::Reason { .. } => {}
        }
    }
    if builds > 0 {
        found.unchecked.push(format!(
            "the {} this switch makes, and those packages' files",
            count(builds, "build", "builds")
        ));
    }
    if absent > 0 {
        found.unchecked.push(format!(
            "the files and signatures of {} still to download",
            count(absent, "package", "packages")
        ));
    }
    if builds > 0 || absent > 0 || install.is_empty() {
        return;
    }
    let here = super::switch::current_root(paths);
    found.take(
        "file conflicts",
        archix::conflicts::check(&install, &remove, &super::switch::installed_db(paths), &here),
    );
    match (planned.signatures, is_root()) {
        (false, _) => {}
        (true, false) => found
            .unchecked
            .push("package signatures (pacman's keyring needs root)".to_string()),
        (true, true) => found.take(
            "package signatures",
            check_packages(&Packages {
                reinstall: false,
                install,
                remove,
                versions: BTreeMap::new(),
                reasons: BTreeMap::new(),
            }),
        ),
    }
}

/// Whether the package file at `path` can be read as one, and with `whole` is the file the lock names.
fn usable(path: &Path, sha256: &str, whole: bool) -> bool {
    path.is_file()
        && archix::conflicts::contents(path).is_ok()
        && (!whole || archix::fetch::sha256(path).is_ok_and(|found| found == sha256))
}
