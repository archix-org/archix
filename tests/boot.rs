//! Boot-side reports that stand without boot entries, which stage 3 writes again; run only inside the systemd-booted Arch container via `tests/arch/run.sh`.

mod common;

use common::*;

#[test]
#[ignore]
fn status_says_when_systemd_boot_is_too_old_to_count_boots() {
    let _system = lock_system();
    // Every LoaderFeatures bit below Type #1 UKI entries, as systemd-boot 257 reports them, and every one 262 has.
    let old = ("ARCHIX_TEST_LOADER_FEATURES", "65535");
    let new = ("ARCHIX_TEST_LOADER_FEATURES", "4194303");
    let setup = Setup::new(&["tree"], "");
    setup.first_switch();
    let status_old = setup.ok_env(&[old], &["status"]);
    let status_new = setup.ok_env(&[new], &["status"]);
    let warning = "boot counting is off: the systemd-boot that started this boot is older than 260";
    assert!(status_old.contains(warning), "{status_old}");
    assert!(!status_new.contains("boot counting is off"), "{status_new}");
}
