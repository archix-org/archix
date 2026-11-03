# The unattended fallback: a required mount that's missing stops each counted boot in emergency mode, which with nobody logged in reboots itself after boot.unblessed_timeout, until systemd-boot falls back to the slot.
import time

from boot.common import bind, boot, edit_config, entries, entry, snapshots, switch, trying


def run(m, t):
    bind(m, t)
    with t.step("1: a switch declaring a required mount whose disk isn't there"):
        boot()
        edit_config("'s|  boot = { loader = \"systemd-boot\" },|  boot = { loader = \"systemd-boot\", unblessed_timeout = 1 },\\n  file_systems = { [\"/mnt/archix-missing\"] = { device = \"UUID=0badc0de-0000-4000-8000-000000000000\", fs_type = \"ext4\", required = true } },|'")
        switch("switch to a system whose boot stops in emergency mode")
        slot = snapshots()[-1]
        t.check(entry("archix-current.conf") == "archix-current+2.conf", f"two tries: {entries()}")
        began = time.monotonic()
        m.reboot()

    with t.step("2: two boots in emergency mode reboot themselves, then the slot boots"):
        m.wait_for_console_text(r"rebooting so the boot counter can fall back", timeout=900)
        m.wait_for_shell(timeout=1800)
        took = time.monotonic() - began
        started = boot()
        t.log(f"from the reboot into the bad system to the fallback's shell: {took:.0f} s")
        t.check(started == f"archix-snapshot-{slot}.conf", f"systemd-boot should fall back to the slot, archix-snapshot-{slot}.conf, not {started}")
        t.check(entry("archix-current.conf") == "archix-current+0-2.conf", f"the current entry should be bad: {entries()}")
        t.check((trying() or {}).get("snapshot") == slot, f"in try mode: {trying()}")
