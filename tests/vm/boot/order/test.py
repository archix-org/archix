# Fallback order: a switch, then a plain pacman kernel update, then a plain pacman transaction that breaks the boot. The slot is the switch's snapshot, taken first after the last good boot, so it boots first although the kernel snapshot's entry is newer; once the slot's entry has failed too, the kernel snapshot boots, newest before older.
import time

from boot.common import bind, boot, bump, edit_config, entries, entry, ok, out, snapshots, switch, trying, wait_fallback
from boot.pacman.test import BUILD, PKG


def run(m, t):
    bind(m, t)
    with t.step("1: a switch, a kernel update and a broken package, no boot between"):
        boot()
        edit_config("'s|  boot = { loader = \"systemd-boot\" },|  boot = { loader = \"systemd-boot\", unblessed_timeout = 1 },|'")
        bump(1)
        switch()
        slot = snapshots()[-1]
        m.succeed("sed -i 's|/2026/10/10/|/2026/10/17/|' /etc/pacman.d/mirrorlist && pacman -Syu --noconfirm --noprogressbar > /root/kernel.log 2>&1", timeout=1800, what="the kernel update")
        kernel = snapshots()[-1]
        m.succeed(BUILD, what="building the broken package")
        m.succeed(f"pacman -U --noconfirm {PKG} > /root/broken.log 2>&1", what="installing it")
        t.check(entry(f"archix-snapshot-{slot}.conf") and entry(f"archix-snapshot-{kernel}.conf"), f"the slot and the kernel snapshot should have entries: {entries()}")
        t.log("sort keys: " + out("grep -H sort-key /efi/loader/entries/archix-*.conf"))
        began = time.monotonic()
        m.reboot()

    with t.step("2: the broken boots fall back to the slot, the switch's snapshot"):
        took = wait_fallback(began)
        started = boot()
        t.log(f"from the reboot to the fallback's shell: {took:.0f} s")
        t.check(started == f"archix-snapshot-{slot}.conf", f"the slot, archix-snapshot-{slot}.conf, should boot before the newer kernel snapshot {kernel}, not {started}")
        t.check(out("cat /etc/archix-vmtest").strip() == "0", "the slot predates the switch")
        name = entry(f"archix-snapshot-{slot}.conf")
        m.succeed(f"mv /efi/loader/entries/{name} /efi/loader/entries/archix-snapshot-{slot}+0-1.conf", what="marking the slot's entry failed")
        m.reboot()

    with t.step("3: with the slot's entry failed too, the newest snapshot entry boots next"):
        started = boot()
        t.check(started == f"archix-snapshot-{kernel}.conf", f"the kernel snapshot, the newest entry left, should boot, not {started}")
        t.check((trying() or {}).get("snapshot") == kernel and out("uname -r").startswith("7.2.9"), f"snapshot {kernel}, the system before the kernel update, in try mode with its own image's kernel: {trying()} {out('uname -r')}")
