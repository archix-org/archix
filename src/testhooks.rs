//! Hooks the integration tests use to crash archix at a chosen point or fake the booted generation; release builds compile them out.

/// Kills this process with SIGKILL when ARCHIX_TEST_CRASH names `point`, as a power cut or `kill -9` would.
pub fn crash_at(point: &str) {
    if cfg!(debug_assertions) && std::env::var("ARCHIX_TEST_CRASH").as_deref() == Ok(point) {
        unsafe { libc::kill(libc::getpid(), libc::SIGKILL) };
    }
}

/// A named step of a deployment switch: in a debug build, runs ARCHIX_TEST_MARK with the name, as the power-cut harness logs steps, then crashes there if ARCHIX_TEST_CRASH names it.
pub fn mark(step: &str) {
    if cfg!(debug_assertions)
        && let Some(program) = std::env::var_os("ARCHIX_TEST_MARK")
    {
        let _ = std::process::Command::new(program).arg(step).status();
    }
    crash_at(step);
}

/// Whether ARCHIX_TEST_BREAK names `rule` in a debug build, so the power-cut harness can be seen catching a commit that breaks it.
pub fn broken(rule: &str) -> bool {
    cfg!(debug_assertions) && std::env::var("ARCHIX_TEST_BREAK").as_deref() == Ok(rule)
}

/// Fails with an error when ARCHIX_TEST_FAIL names `step` in a debug build, as a write error there would.
pub fn fail_at(step: &str) -> anyhow::Result<()> {
    if cfg!(debug_assertions) && std::env::var("ARCHIX_TEST_FAIL").as_deref() == Ok(step) {
        anyhow::bail!("failing at {step} for a test");
    }
    Ok(())
}

/// The kernel command line, or ARCHIX_TEST_CMDLINE in a debug build, so tests can boot a generation without a reboot.
pub fn kernel_cmdline() -> String {
    if cfg!(debug_assertions)
        && let Ok(cmdline) = std::env::var("ARCHIX_TEST_CMDLINE")
    {
        return cmdline;
    }
    std::fs::read_to_string("/proc/cmdline").unwrap_or_default()
}

/// The running kernel's release, or ARCHIX_TEST_KERNEL in a debug build, where empty means unknown.
pub fn kernel_release() -> Option<String> {
    let release = match std::env::var("ARCHIX_TEST_KERNEL") {
        Ok(release) if cfg!(debug_assertions) => release,
        _ => std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default(),
    };
    Some(release.trim().to_string()).filter(|release| !release.is_empty())
}

/// Whether `archix rescue` asks for its login outside an initramfs too, with ARCHIX_TEST_RESCUE_LOGIN=1 in a debug build.
pub fn rescue_login() -> bool {
    cfg!(debug_assertions) && std::env::var("ARCHIX_TEST_RESCUE_LOGIN").as_deref() == Ok("1")
}

/// A caching proxy of archive.archlinux.org that requests for it go to instead, ARCHIX_TEST_ARCHIVE in a debug build, so the test suites don't depend on reaching the archive.
pub fn archive_proxy() -> Option<String> {
    if !cfg!(debug_assertions) {
        return None;
    }
    std::env::var("ARCHIX_TEST_ARCHIVE").ok().filter(|url| !url.is_empty())
}

/// The day the newest archive snapshot is searched back from, ARCHIX_TEST_TODAY in a debug build, so a test sees the same newest snapshot every day.
pub fn archive_today() -> Option<crate::date::Date> {
    if !cfg!(debug_assertions) {
        return None;
    }
    std::env::var("ARCHIX_TEST_TODAY").ok()?.parse().ok()
}

/// The root whose /archix holds the deployments, or ARCHIX_TEST_ROOT in a debug build, so tests can switch a machine laid out in a directory.
pub fn root() -> std::path::PathBuf {
    if cfg!(debug_assertions)
        && let Some(root) = std::env::var_os("ARCHIX_TEST_ROOT")
    {
        return std::path::PathBuf::from(root);
    }
    std::path::PathBuf::from("/")
}

/// systemd-boot's LoaderFeatures, or ARCHIX_TEST_LOADER_FEATURES in a debug build.
pub fn loader_features() -> u64 {
    if cfg!(debug_assertions)
        && let Ok(features) = std::env::var("ARCHIX_TEST_LOADER_FEATURES")
    {
        return features.parse().unwrap_or(0);
    }
    crate::system::boot::read_loader_features()
}

/// Where systemd-boot's EFI variables are: ARCHIX_TEST_EFIVARS in a debug build, none in a chroot or on a test root, whose variables would be the host's.
pub fn efivars() -> Option<std::path::PathBuf> {
    if cfg!(debug_assertions)
        && let Some(dir) = std::env::var_os("ARCHIX_TEST_EFIVARS")
    {
        return Some(std::path::PathBuf::from(dir));
    }
    if crate::system::secureboot::in_chroot() || root() != std::path::Path::new("/") {
        return None;
    }
    Some(std::path::PathBuf::from("/sys/firmware/efi/efivars"))
}

/// Whether boot.esp must be a mounted FAT filesystem: always, but on a test root, whose ESP is a plain directory, only with ARCHIX_TEST_CHECK_ESP, and never with ARCHIX_TEST_PLAIN_ESP in a debug build.
pub fn check_esp() -> bool {
    let plain =
        PLAIN_ESP.load(std::sync::atomic::Ordering::Relaxed) || std::env::var_os("ARCHIX_TEST_PLAIN_ESP").is_some();
    if cfg!(test) || (cfg!(debug_assertions) && plain) {
        return false;
    }
    root() == std::path::Path::new("/") || std::env::var_os("ARCHIX_TEST_CHECK_ESP").is_some()
}

static PLAIN_ESP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Lets a test that calls archix in its own process use a plain directory as the ESP, as a test root or ARCHIX_TEST_PLAIN_ESP does; a no-op in a release build.
pub fn allow_plain_esp() {
    PLAIN_ESP.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Whether systemd-boot is counting this boot's tries (it sets LoaderBootCountPath then), or ARCHIX_TEST_COUNTED_BOOT in a debug build.
pub fn counted_boot() -> bool {
    if cfg!(debug_assertions)
        && let Ok(value) = std::env::var("ARCHIX_TEST_COUNTED_BOOT")
    {
        return value == "1";
    }
    std::path::Path::new("/sys/firmware/efi/efivars/LoaderBootCountPath-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f").exists()
}

/// The boot entry systemd-boot started, or ARCHIX_TEST_ENTRY in a debug build, where empty means the variable is missing.
pub fn selected_entry() -> Option<String> {
    if cfg!(debug_assertions)
        && let Ok(entry) = std::env::var("ARCHIX_TEST_ENTRY")
    {
        return (!entry.is_empty()).then_some(entry);
    }
    crate::system::boot::read_selected_entry()
}
