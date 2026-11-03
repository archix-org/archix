//! Hooks the integration tests use to crash archix at a chosen point or fake the booted generation; release builds compile them out.

/// Kills this process with SIGKILL when ARCHIX_TEST_CRASH names `point`, as a power cut or `kill -9` would.
pub fn crash_at(point: &str) {
    if cfg!(debug_assertions) && std::env::var("ARCHIX_TEST_CRASH").as_deref() == Ok(point) {
        unsafe { libc::kill(libc::getpid(), libc::SIGKILL) };
    }
}

/// A named step: in a debug build, runs ARCHIX_TEST_MARK with the name, cuts the power there if CUT_VARIABLE names it, then crashes there if ARCHIX_TEST_CRASH names it.
pub fn mark(step: &str) {
    if cfg!(debug_assertions) {
        if let Some(program) = std::env::var_os("ARCHIX_TEST_MARK") {
            let _ = std::process::Command::new(program).arg(step).status();
        }
        cut_if_armed(step);
    }
    crash_at(step);
}

/// The EFI variable that arms a power cut for the VM tests, also in the initramfs: its data (after the attributes) is STEP, or STEP#K for the Kth time one process reaches it.
pub const CUT_VARIABLE: &str = "ArchixTestCut-0b9c7b5e-5f43-4f0e-9a4e-6a2d7c1e3f21";

/// The step and the hit CUT_VARIABLE's data names.
pub fn armed_cut(data: &[u8]) -> Option<(String, u32)> {
    let text = String::from_utf8_lossy(data.get(4..)?);
    let text = text.trim_matches(|c: char| c == '\0' || c.is_whitespace());
    let (step, nth) = match text.split_once('#') {
        Some((step, nth)) => (step, nth.parse().ok()?),
        None => (text, 1),
    };
    (!step.is_empty() && nth > 0).then(|| (step.to_string(), nth))
}

static CUT_HITS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Disarms CUT_VARIABLE at the hit it names, says so on the console for the host to reset the VM, and resets the machine by SysRq if nobody has within 30 s.
fn cut_if_armed(step: &str) {
    let path = std::path::Path::new("/sys/firmware/efi/efivars").join(CUT_VARIABLE);
    let Some((armed, nth)) = std::fs::read(&path).ok().and_then(|data| armed_cut(&data)) else {
        return;
    };
    if armed != step || CUT_HITS.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1 != nth {
        return;
    }
    let _ = crate::system::revoke::set_immutable(&path, false);
    let _ = std::fs::remove_file(&path);
    let line = format!("archix-test: power cut at {step}#{nth}\n");
    let _ = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/console")
        .and_then(|mut console| std::io::Write::write_all(&mut console, line.as_bytes()));
    std::thread::sleep(std::time::Duration::from_secs(30));
    let _ = std::fs::write("/proc/sysrq-trigger", "b");
    loop {
        std::thread::sleep(std::time::Duration::from_secs(60));
    }
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

/// Where the initramfs leaves its records for the booted system, /run/archix, or ARCHIX_TEST_RUN in a debug build.
pub fn run_dir() -> std::path::PathBuf {
    if cfg!(debug_assertions)
        && let Some(dir) = std::env::var_os("ARCHIX_TEST_RUN")
    {
        return dir.into();
    }
    std::path::PathBuf::from(crate::system::initrd::RUN)
}

#[cfg(test)]
mod tests {
    use super::armed_cut;

    #[test]
    fn the_cut_variable_names_a_step_and_which_hit_of_it() {
        assert_eq!(
            armed_cut(b"\x07\0\0\0keep-exchanged"),
            Some(("keep-exchanged".to_string(), 1))
        );
        assert_eq!(
            armed_cut(b"\x07\0\0\0split-entry-copied#3\n"),
            Some(("split-entry-copied".to_string(), 3))
        );
        assert_eq!(armed_cut(b"\x07\0\0\0step#0"), None);
        assert_eq!(armed_cut(b"\x07\0\0\0"), None);
        assert_eq!(armed_cut(b"\x07\0"), None);
    }
}
