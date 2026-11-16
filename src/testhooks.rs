//! Hooks the integration tests use to crash archix at a chosen point or fake the booted generation; release builds compile them out.

/// Kills this process with SIGKILL when ARCHIX_TEST_CRASH names `point`, as a power cut or `kill -9` would.
pub fn crash_at(point: &str) {
    if cfg!(debug_assertions) && std::env::var("ARCHIX_TEST_CRASH").as_deref() == Ok(point) {
        unsafe { libc::kill(libc::getpid(), libc::SIGKILL) };
    }
}

/// A named step: in a debug build, runs ARCHIX_TEST_MARK with the name, then crashes there if ARCHIX_TEST_CRASH names it.
pub fn mark(step: &str) {
    if cfg!(debug_assertions)
        && let Some(program) = std::env::var_os("ARCHIX_TEST_MARK")
    {
        let _ = std::process::Command::new(program).arg(step).status();
    }
    crash_at(step);
}

/// Runs ARCHIX_TEST_AFTER_CAPTURE in a shell in a debug build, right after a root command captured the config, as the config's owner racing the command would.
pub fn after_capture() {
    if cfg!(debug_assertions)
        && let Some(script) = std::env::var_os("ARCHIX_TEST_AFTER_CAPTURE")
    {
        let _ = std::process::Command::new("sh").arg("-c").arg(script).status();
    }
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

/// A caching proxy of archive.archlinux.org that requests for it go to instead, ARCHIX_TEST_ARCHIVE in a debug build, so the test suites don't depend on reaching the archive.
pub fn archive_proxy() -> Option<String> {
    if !cfg!(debug_assertions) {
        return None;
    }
    std::env::var("ARCHIX_TEST_ARCHIVE").ok().filter(|url| !url.is_empty())
}

/// The day the newest archive snapshot is searched back from and after which /packages/ listings are cut, ARCHIX_TEST_TODAY in a debug build, so a test sees the same archive every day.
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

/// Where `archix secrets edit` puts the plain text instead of a private ramfs, ARCHIX_TEST_SECRETS_SCRATCH in a debug build, so a test can see it refuse a directory on disk.
pub fn secrets_scratch() -> Option<std::path::PathBuf> {
    if !cfg!(debug_assertions) {
        return None;
    }
    std::env::var_os("ARCHIX_TEST_SECRETS_SCRATCH").map(std::path::PathBuf::from)
}

/// Writes how much memory this process has locked (VmLck, kB) to ARCHIX_TEST_VMLCK in a debug build, so a test can see a phase run locked.
pub fn record_locked_memory() {
    if cfg!(debug_assertions)
        && let Some(path) = std::env::var_os("ARCHIX_TEST_VMLCK")
    {
        let _ = std::fs::write(path, crate::secrets::memory::locked_kb().to_string());
    }
}

/// archix's own binary, or ARCHIX_TEST_SELF in a debug build, so library tests can start archix's child commands.
pub fn own_exe() -> std::io::Result<std::path::PathBuf> {
    if cfg!(debug_assertions)
        && let Some(exe) = std::env::var_os("ARCHIX_TEST_SELF")
    {
        return Ok(exe.into());
    }
    // A chroot or a deleted binary leaves current_exe() unreachable, but the kernel's own link still execs it.
    let own = std::path::Path::new("/proc/self/exe");
    if own.exists() {
        return Ok(own.to_path_buf());
    }
    std::env::current_exe()
}
