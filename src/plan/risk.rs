use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PkgRule {
    BootFiles,
    LibClosure,
    Dkms,
    Gpu,
    Chwd,
    Keyring,
    Mirrorlist,
    RunningKernel,
    Swap,
    Tooling,
    PamWhileLocked,
}

pub const PKG_RULES: [PkgRule; 11] = [
    PkgRule::BootFiles,
    PkgRule::LibClosure,
    PkgRule::Dkms,
    PkgRule::Gpu,
    PkgRule::Chwd,
    PkgRule::Keyring,
    PkgRule::Mirrorlist,
    PkgRule::RunningKernel,
    PkgRule::Swap,
    PkgRule::Tooling,
    PkgRule::PamWhileLocked,
];

impl PkgRule {
    pub fn text(self) -> &'static str {
        match self {
            PkgRule::BootFiles => "owns files under /boot, /efi, /usr/lib/modules, /usr/lib/initcpio or /usr/lib/firmware",
            PkgRule::LibClosure => "in the shared-library closure of pacman, sudo, systemd, login or the boot tools",
            PkgRule::Dkms => "a DKMS module",
            PkgRule::Gpu => "a GPU driver",
            PkgRule::Chwd => "managed by chwd",
            PkgRule::Keyring => "a keyring",
            PkgRule::Mirrorlist => "a mirrorlist",
            PkgRule::RunningKernel => "the running kernel's package",
            PkgRule::Swap => "a provider swap, conflict replacement or downgrade",
            PkgRule::Tooling => "archix, the AUR helper, base-devel or devtools",
            PkgRule::PamWhileLocked => "pam or libxcrypt while a screen locker runs: don't lock the screen until reboot",
        }
    }
}

/// One package line's facts the rules read: its file lists (old and new, no leading slash) and how it changes.
#[derive(Debug, Clone, Default)]
pub struct PkgFacts {
    pub name: String,
    pub files: Vec<String>,
    pub swap: bool,
}

#[derive(Debug, Clone, Default)]
pub struct RiskContext {
    pub closure: BTreeSet<String>,
    pub chwd: BTreeSet<String>,
    pub running_kernel: Option<String>,
    pub helper: Option<String>,
    pub screen_locker_running: bool,
}

const BOOT_DIRS: [&str; 5] = ["boot/", "efi/", "usr/lib/modules/", "usr/lib/initcpio/", "usr/lib/firmware/"];

pub fn is_dkms(files: &[String]) -> bool {
    files.iter().any(|f| {
        f.strip_prefix("usr/src/")
            .and_then(|rest| rest.split_once('/'))
            .is_some_and(|(_, file)| file == "dkms.conf")
    })
}

pub fn is_keyring(files: &[String]) -> bool {
    files
        .iter()
        .any(|f| f.starts_with("usr/share/pacman/keyrings/") && f.len() > "usr/share/pacman/keyrings/".len())
}

pub fn is_mirrorlist(name: &str, files: &[String]) -> bool {
    name.ends_with("mirrorlist")
        || files
            .iter()
            .any(|f| f.strip_prefix("etc/pacman.d/").is_some_and(|rest| rest.contains("mirrorlist")))
}

pub fn is_gpu_driver(name: &str) -> bool {
    let name = name.strip_prefix("lib32-").unwrap_or(name);
    name.starts_with("nvidia")
        || name.starts_with("xf86-video-")
        || name.starts_with("opencl-nvidia")
        || matches!(
            name,
            "mesa" | "vulkan-radeon" | "vulkan-intel" | "vulkan-nouveau" | "vulkan-swrast" | "intel-media-driver"
        )
}

pub fn is_tooling(name: &str, helper: Option<&str>) -> bool {
    matches!(name, "archix" | "archix-git" | "base-devel" | "devtools") || helper == Some(name)
}

pub fn applies(rule: PkgRule, p: &PkgFacts, ctx: &RiskContext) -> bool {
    match rule {
        PkgRule::BootFiles => p.files.iter().any(|f| BOOT_DIRS.iter().any(|d| f.starts_with(d) && f.len() > d.len())),
        PkgRule::LibClosure => ctx.closure.contains(&p.name),
        PkgRule::Dkms => is_dkms(&p.files),
        PkgRule::Gpu => is_gpu_driver(&p.name),
        PkgRule::Chwd => ctx.chwd.contains(&p.name),
        PkgRule::Keyring => is_keyring(&p.files),
        PkgRule::Mirrorlist => is_mirrorlist(&p.name, &p.files),
        PkgRule::RunningKernel => ctx.running_kernel.as_deref() == Some(p.name.as_str()),
        PkgRule::Swap => p.swap,
        PkgRule::Tooling => is_tooling(&p.name, ctx.helper.as_deref()),
        PkgRule::PamWhileLocked => ctx.screen_locker_running && matches!(p.name.as_str(), "pam" | "libxcrypt"),
    }
}

pub fn package_rules_with(p: &PkgFacts, ctx: &RiskContext, enabled: impl Fn(PkgRule) -> bool) -> Vec<PkgRule> {
    PKG_RULES
        .into_iter()
        .filter(|rule| enabled(*rule) && applies(*rule, p, ctx))
        .collect()
}

pub fn package_risks(p: &PkgFacts, ctx: &RiskContext) -> Vec<String> {
    package_rules_with(p, ctx, |_| true)
        .into_iter()
        .map(|rule| rule.text().to_string())
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FileRule {
    Sudoers,
    Auth,
    Accounts,
    Initramfs,
    BootConfig,
    Modules,
    Pacman,
    Mounts,
    Network,
    Firewall,
    Sshd,
}

pub const FILE_RULES: [FileRule; 11] = [
    FileRule::Sudoers,
    FileRule::Auth,
    FileRule::Accounts,
    FileRule::Initramfs,
    FileRule::BootConfig,
    FileRule::Modules,
    FileRule::Pacman,
    FileRule::Mounts,
    FileRule::Network,
    FileRule::Firewall,
    FileRule::Sshd,
];

impl FileRule {
    pub fn text(self) -> &'static str {
        match self {
            FileRule::Sudoers => "sudoers",
            FileRule::Auth => "authentication (pam, security, nsswitch, shells)",
            FileRule::Accounts => "an account database",
            FileRule::Initramfs => "initramfs configuration",
            FileRule::BootConfig => "boot configuration",
            FileRule::Modules => "kernel module or console configuration",
            FileRule::Pacman => "pacman configuration",
            FileRule::Mounts => "mount table",
            FileRule::Network => "network configuration",
            FileRule::Firewall => "firewall rules",
            FileRule::Sshd => "sshd configuration",
        }
    }
}

fn in_dir(path: &str, dir: &str) -> bool {
    path.strip_prefix(dir)
        .is_some_and(|rest| rest.starts_with('/') && rest.len() > 1)
}

pub fn file_applies(rule: FileRule, path: &str) -> bool {
    match rule {
        FileRule::Sudoers => path == "/etc/sudoers" || in_dir(path, "/etc/sudoers.d"),
        FileRule::Auth => {
            in_dir(path, "/etc/pam.d")
                || in_dir(path, "/etc/security")
                || matches!(path, "/etc/nsswitch.conf" | "/etc/shells")
        }
        FileRule::Accounts => matches!(path, "/etc/passwd" | "/etc/group" | "/etc/shadow" | "/etc/gshadow"),
        FileRule::Initramfs => {
            path == "/etc/mkinitcpio.conf"
                || in_dir(path, "/etc/mkinitcpio.conf.d")
                || (in_dir(path, "/etc/mkinitcpio.d") && path.ends_with(".preset"))
        }
        FileRule::BootConfig => matches!(path, "/etc/kernel/cmdline" | "/etc/default/limine" | "/boot/limine.conf"),
        FileRule::Modules => {
            in_dir(path, "/etc/modprobe.d") || in_dir(path, "/etc/modules-load.d") || path == "/etc/vconsole.conf"
        }
        FileRule::Pacman => path == "/etc/pacman.conf" || in_dir(path, "/etc/pacman.d"),
        FileRule::Mounts => matches!(path, "/etc/fstab" | "/etc/crypttab"),
        FileRule::Network => {
            in_dir(path, "/etc/systemd/network")
                || in_dir(path, "/etc/NetworkManager")
                || in_dir(path, "/etc/netctl")
                || matches!(path, "/etc/hosts" | "/etc/resolv.conf" | "/etc/hostname")
        }
        FileRule::Firewall => {
            matches!(path, "/etc/nftables.conf")
                || in_dir(path, "/etc/nftables.d")
                || in_dir(path, "/etc/iptables")
                || in_dir(path, "/etc/firewalld")
                || in_dir(path, "/etc/ufw")
        }
        FileRule::Sshd => path == "/etc/ssh/sshd_config" || in_dir(path, "/etc/ssh/sshd_config.d"),
    }
}

pub fn file_rules_with(path: &str, enabled: impl Fn(FileRule) -> bool) -> Vec<FileRule> {
    FILE_RULES
        .into_iter()
        .filter(|rule| enabled(*rule) && file_applies(*rule, path))
        .collect()
}

pub fn file_risks(path: &str) -> Vec<String> {
    file_rules_with(path, |_| true)
        .into_iter()
        .map(|rule| rule.text().to_string())
        .collect()
}

/// Kernel module dirs (`usr/lib/modules/<v>`) that hold a vmlinuz, by owning package.
pub fn kernels(pkgs: &BTreeMap<String, Vec<String>>) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (name, files) in pkgs {
        for file in files {
            if let Some(version) = file
                .strip_prefix("usr/lib/modules/")
                .and_then(|rest| rest.strip_suffix("/vmlinuz"))
                .filter(|v| !v.contains('/'))
            {
                out.insert(name.clone(), version.to_string());
            }
        }
    }
    out
}

/// SCOPE 3.2's DKMS refusal: a kernel in the final package set with no headers while a DKMS module is installed.
pub fn dkms_refusals(final_pkgs: &BTreeMap<String, Vec<String>>) -> Vec<String> {
    let modules: Vec<&String> = final_pkgs.iter().filter(|(_, f)| is_dkms(f)).map(|(n, _)| n).collect();
    if modules.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (kernel, version) in kernels(final_pkgs) {
        let build = format!("usr/lib/modules/{version}/build/");
        let has_headers = final_pkgs
            .values()
            .any(|files| files.iter().any(|f| f.starts_with(&build) && f.len() > build.len()));
        if !has_headers {
            out.push(format!(
                "kernel {kernel} would have no headers for the DKMS module{} {}; declare {kernel}-headers",
                if modules.len() == 1 { "" } else { "s" },
                modules.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
            ));
        }
    }
    out
}

/// Library paths in `ldd` output, as package file paths (/lib and /lib64 are usr/lib on Arch).
pub fn ldd_libraries(output: &str) -> Vec<String> {
    let mut libs = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        let path = match line.split_once("=>") {
            Some((_, rest)) => rest.split_whitespace().next().unwrap_or(""),
            None => line.split_whitespace().next().unwrap_or(""),
        };
        if !path.starts_with('/') {
            continue;
        }
        let path = path.trim_start_matches('/');
        let path = path
            .strip_prefix("lib64/")
            .or_else(|| path.strip_prefix("lib/"))
            .map(|rest| format!("usr/lib/{rest}"))
            .unwrap_or_else(|| path.replace("usr/lib64/", "usr/lib/"));
        libs.push(path);
    }
    libs
}

/// The packages owning each binary and every library its `ldd` output names: SCOPE 3.2's closure.
pub fn closure(binaries: &[(String, String)], owner: &dyn Fn(&str) -> Option<String>) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for (binary, ldd) in binaries {
        out.extend(owner(binary.trim_start_matches('/')));
        for lib in ldd_libraries(ldd) {
            out.extend(owner(&lib));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(name: &str, files: &[&str]) -> PkgFacts {
        PkgFacts {
            name: name.into(),
            files: files.iter().map(|f| f.to_string()).collect(),
            swap: false,
        }
    }

    fn ctx() -> RiskContext {
        RiskContext {
            closure: ["zlib-ng-compat".to_string()].into(),
            chwd: ["chwd-profile-pkg".to_string()].into(),
            running_kernel: Some("linux-cachyos".into()),
            helper: Some("paru".into()),
            screen_locker_running: true,
        }
    }

    fn pkg_fixtures() -> Vec<(PkgRule, PkgFacts)> {
        vec![
            (PkgRule::BootFiles, facts("amd-ucode", &["boot/amd-ucode.img"])),
            (PkgRule::LibClosure, facts("zlib-ng-compat", &["usr/lib/libz.so.1"])),
            (PkgRule::Dkms, facts("zfs-dkms", &["usr/src/zfs-2.2/dkms.conf"])),
            (PkgRule::Gpu, facts("vulkan-radeon", &[])),
            (PkgRule::Chwd, facts("chwd-profile-pkg", &[])),
            (PkgRule::Keyring, facts("cachyos-keyring", &["usr/share/pacman/keyrings/cachyos.gpg"])),
            (PkgRule::Mirrorlist, facts("cachyos-mirrorlist", &[])),
            (PkgRule::RunningKernel, facts("linux-cachyos", &[])),
            (PkgRule::Swap, PkgFacts { swap: true, ..facts("zlib", &[]) }),
            (PkgRule::Tooling, facts("paru", &[])),
            (PkgRule::PamWhileLocked, facts("pam", &[])),
        ]
    }

    #[test]
    fn every_package_category_has_a_fixture_that_only_its_rule_flags() {
        let fixtures = pkg_fixtures();
        assert_eq!(fixtures.len(), PKG_RULES.len());
        for (rule, p) in &fixtures {
            assert_eq!(package_rules_with(p, &ctx(), |_| true), vec![*rule], "{p:?}");
        }
    }

    #[test]
    fn disabling_any_package_rule_fails_its_fixture() {
        for (rule, p) in pkg_fixtures() {
            assert!(
                package_rules_with(&p, &ctx(), |r| r != rule).is_empty(),
                "{rule:?} is not load-bearing for {p:?}"
            );
        }
    }

    #[test]
    fn ordinary_packages_are_unflagged() {
        assert!(package_risks(&facts("ripgrep", &["usr/bin/rg", "usr/lib/modules-load.d/x"]), &ctx()).is_empty());
        let unlocked = RiskContext { screen_locker_running: false, ..ctx() };
        assert!(package_risks(&facts("pam", &[]), &unlocked).is_empty());
        assert!(package_risks(&facts("x", &["usr/lib/modules/"]), &ctx()).is_empty());
    }

    fn file_fixtures() -> Vec<(FileRule, &'static str)> {
        vec![
            (FileRule::Sudoers, "/etc/sudoers.d/10-wheel"),
            (FileRule::Auth, "/etc/pam.d/system-login"),
            (FileRule::Accounts, "/etc/shadow"),
            (FileRule::Initramfs, "/etc/mkinitcpio.d/linux.preset"),
            (FileRule::BootConfig, "/etc/kernel/cmdline"),
            (FileRule::Modules, "/etc/modprobe.d/nvidia.conf"),
            (FileRule::Pacman, "/etc/pacman.d/mirrorlist"),
            (FileRule::Mounts, "/etc/crypttab"),
            (FileRule::Network, "/etc/systemd/network/20-wired.network"),
            (FileRule::Firewall, "/etc/nftables.conf"),
            (FileRule::Sshd, "/etc/ssh/sshd_config"),
        ]
    }

    #[test]
    fn every_file_category_has_a_fixture_that_only_its_rule_flags() {
        let fixtures = file_fixtures();
        assert_eq!(fixtures.len(), FILE_RULES.len());
        for (rule, path) in &fixtures {
            assert_eq!(file_rules_with(path, |_| true), vec![*rule], "{path}");
        }
    }

    #[test]
    fn disabling_any_file_rule_fails_its_fixture() {
        for (rule, path) in file_fixtures() {
            assert!(file_rules_with(path, |r| r != rule).is_empty(), "{rule:?} not load-bearing for {path}");
        }
    }

    #[test]
    fn unlisted_files_are_unflagged() {
        for path in ["/etc/foo.conf", "/etc/pam.dx", "/etc/mkinitcpio.d/notes.txt", "/etc/sudoersx"] {
            assert!(file_risks(path).is_empty(), "{path}");
        }
    }

    fn files(list: &[&str]) -> Vec<String> {
        list.iter().map(|f| f.to_string()).collect()
    }

    #[test]
    fn dkms_refuses_a_kernel_without_headers() {
        let mut pkgs = BTreeMap::new();
        pkgs.insert("linux".to_string(), files(&["usr/lib/modules/6.1-arch1/vmlinuz"]));
        pkgs.insert("linux-headers".to_string(), files(&["usr/lib/modules/6.1-arch1/build/Makefile"]));
        pkgs.insert("linux-lts".to_string(), files(&["usr/lib/modules/6.6-lts/vmlinuz"]));
        assert!(dkms_refusals(&pkgs).is_empty());
        pkgs.insert("nvidia-open-dkms".to_string(), files(&["usr/src/nvidia-560/dkms.conf"]));
        let refusals = dkms_refusals(&pkgs);
        assert_eq!(refusals.len(), 1);
        assert!(refusals[0].contains("linux-lts"), "{refusals:?}");
    }

    #[test]
    fn closure_maps_ldd_output_to_owning_packages() {
        let ldd = "\tlinux-vdso.so.1 (0x00007ffd)\n\tlibz.so.1 => /usr/lib/libz.so.1 (0x00007f)\n\tlibc.so.6 => /usr/lib/libc.so.6 (0x1)\n\t/lib64/ld-linux-x86-64.so.2 => /usr/lib64/ld-linux-x86-64.so.2 (0x2)\n";
        let owners: BTreeMap<&str, &str> = [
            ("usr/bin/pacman", "pacman"),
            ("usr/lib/libz.so.1", "zlib-ng-compat"),
            ("usr/lib/libc.so.6", "glibc"),
            ("usr/lib/ld-linux-x86-64.so.2", "glibc"),
        ]
        .into();
        let set = closure(&[("/usr/bin/pacman".into(), ldd.into())], &|p| owners.get(p).map(|s| s.to_string()));
        assert_eq!(set, ["glibc", "pacman", "zlib-ng-compat"].map(String::from).into());
    }
}
