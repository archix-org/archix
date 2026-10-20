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

/// The kernel command line, or ARCHIX_TEST_CMDLINE in a debug build, so tests can boot a generation without a reboot.
pub fn kernel_cmdline() -> String {
    if cfg!(debug_assertions)
        && let Ok(cmdline) = std::env::var("ARCHIX_TEST_CMDLINE")
    {
        return cmdline;
    }
    std::fs::read_to_string("/proc/cmdline").unwrap_or_default()
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

/// The boot entry systemd-boot started, or ARCHIX_TEST_ENTRY in a debug build, where empty means the variable is missing.
pub fn selected_entry() -> Option<String> {
    if cfg!(debug_assertions)
        && let Ok(entry) = std::env::var("ARCHIX_TEST_ENTRY")
    {
        return (!entry.is_empty()).then_some(entry);
    }
    crate::system::boot::read_selected_entry()
}
