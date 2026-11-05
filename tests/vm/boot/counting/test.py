# Boot counting and the slot: a kernel that panics gets its two tries, then systemd-boot falls back to the slot, the snapshot of how the system last booted fine, read-only in try mode.
from boot.common import HOST, bind, boot, counts_tries, edit_config, entries, entry, ok, out, snapshots, switch, trying


def run(m, t):
    bind(m, t)
    with t.step("1: a switch to a kernel command line that panics"):
        boot()
        edit_config("'s|\"console=ttyS0\" }|\"console=ttyS0\", \"init=/usr/bin/false\", \"panic=3\" }|'")
        switch("switch to a kernel command line that can't boot")
        slot = snapshots()[-1]
        t.check(counts_tries("archix-current.conf", 2), f"the current entry should count two tries: {entries()}")
        t.check(entry(f"archix-snapshot-{slot}.conf") == f"archix-snapshot-{slot}+1.conf", f"the snapshot taken before the switch should have an entry with a try of its own: {entries()}")
        t.check(ok(f"grep -q '^sort-key archix-1$' /efi/loader/entries/archix-snapshot-{slot}+1.conf"), "it's the slot, sorted right after the current entry")
        m.reboot()

    with t.step("2: after two failed boots systemd-boot falls back to the slot, read-only"):
        m.wait_for_shell(timeout=1500)
        started = boot()
        t.check(started == f"archix-snapshot-{slot}.conf", f"after two failed boots systemd-boot should start the slot's entry, archix-snapshot-{slot}.conf, not {started}")
        t.check(entry("archix-current.conf") == "archix-current+0-2.conf", f"the current entry should be bad: {entries()}")
        tried = trying() or {}
        t.check(tried.get("snapshot") == slot, f"the initramfs should boot snapshot {slot} in try mode: {tried}")
        t.check(out("findmnt -no FSTYPE /").strip() == "overlay", f"the root should be the snapshot under a throwaway layer: {out('findmnt /')}")
        t.check(ok("grep -q 'try mode' /run/motd.d/archix.motd") and ok("grep -q 'try mode' /run/issue.d/archix.issue"), f"the login message and motd should carry the notice: {out('cat /run/motd.d/archix.motd')}")
        t.check(ok(f"archix status | grep -q 'snapshot {slot}, read-only'"), f"status should say so: {out('archix status')}")
        refused = out("pacman -Rdd --noconfirm efitools 2>&1")
        t.check(f"booted snapshot {slot} read-only; `archix rollback {slot}` to keep, or reboot" in refused, f"pacman should refuse in try mode: {refused}")
        switched = out(f"{HOST} switch --noconfirm 2>&1")
        t.check(f"snapshot {slot} in try mode" in switched, f"a switch should refuse in try mode: {switched}")
        m.succeed("echo try > /etc/archix-try-write && echo try > /var/lib/archix-try-write", what="writing to the layer")
        m.reboot()

    with t.step("3: the next boot, the slot again, has none of the try's changes"):
        started = boot()
        t.check(started == f"archix-snapshot-{slot}.conf", f"with the current entry bad, the slot should start again, not {started}")
        t.check(not ok("[ -e /etc/archix-try-write ]") and not ok("[ -e /var/lib/archix-try-write ]"), "the last try's changes should be gone")
