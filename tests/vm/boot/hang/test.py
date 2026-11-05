# The unattended fallback from a hang: a unit that never finishes keeps each counted boot from boot-complete.target, boot.unblessed_timeout reboots it with nobody there, and systemd-boot falls back to the slot.
import time

from boot.common import add_unit, bind, boot, counts_tries, edit_config, entries, entry, ok, snapshots, switch, trying, wait_fallback


def run(m, t):
    bind(m, t)
    with t.step("1: a switch to a system with a unit that hangs at every boot"):
        boot()
        edit_config("'s|  boot = { loader = \"systemd-boot\" },|  boot = { loader = \"systemd-boot\", unblessed_timeout = 1 },|'")
        add_unit("archix-test-hang.service", "[Unit]\nDescription=a unit that never finishes at boot\nConditionPathExists=!/run/archix-test-no-hang\n\n[Service]\nType=oneshot\nExecStart=/usr/bin/sleep infinity\n\n[Install]\nWantedBy=multi-user.target")
        m.succeed("touch /run/archix-test-no-hang", what="keeping the switch's start of the unit from hanging")
        switch("switch to a system whose boot hangs")
        slot = snapshots()[-1]
        t.check(counts_tries("archix-current.conf", 2), f"two tries: {entries()}")
        began = time.monotonic()
        m.reboot()

    with t.step("2: the hung boots reboot themselves, then the slot boots"):
        took = wait_fallback(began)
        started = boot()
        t.log(f"UNATTENDED hang: from the reboot into the hanging system to the fallback's shell: {took:.0f} s")
        t.check(started == f"archix-snapshot-{slot}.conf", f"systemd-boot should fall back to the slot, archix-snapshot-{slot}.conf, not {started}")
        t.check(entry("archix-current.conf") == "archix-current+0-2.conf", f"the current entry should have used its tries: {entries()}")
        t.check((trying() or {}).get("snapshot") == slot, f"in try mode: {trying()}")
        t.check(not ok("systemctl cat archix-test-hang.service > /dev/null 2>&1"), "the slot is the system before the hanging unit")
