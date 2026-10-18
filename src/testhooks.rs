//! Hooks the integration tests use to crash archix at a chosen point or fake the booted generation; release builds compile them out.

/// Kills this process with SIGKILL when ARCHIX_TEST_CRASH names `point`, as a power cut or `kill -9` would.
pub fn crash_at(point: &str) {
    if cfg!(debug_assertions) && std::env::var("ARCHIX_TEST_CRASH").as_deref() == Ok(point) {
        unsafe { libc::kill(libc::getpid(), libc::SIGKILL) };
    }
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
