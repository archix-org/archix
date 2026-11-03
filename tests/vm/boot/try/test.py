# Try mode by hand: picking an older snapshot's entry boots it read-only with a throwaway layer over the root and /var/lib; the next boot is the current system's, with none of it.
from boot.common import HOST, bind, boot, bump, entries, entry, ok, out, snapshots, switch, trying


def run(m, t):
    bind(m, t)
    with t.step("1: two switches, then the older snapshot picked once"):
        boot()
        bump(1)
        switch()
        older = snapshots()[-1]
        bump(2)
        switch()
        t.check(entry(f"archix-snapshot-{older}.conf") is not None, f"snapshot {older} should have an entry: {entries()}")
        m.select_entry(f"archix-snapshot-{older}.conf")
        m.reboot()

    with t.step("2: the snapshot boots read-only with a throwaway layer"):
        started = boot()
        t.check(started == f"archix-snapshot-{older}.conf", f"the picked entry should start, not {started}")
        tried = trying() or {}
        t.check(tried.get("snapshot") == older and tried.get("layer") == "file", f"a try boot of snapshot {older} on a layer file in @archix/try: {tried}")
        t.check(out("cat /etc/archix-vmtest").strip() == "0", f"the root should be the system before the first switch: {out('cat /etc/archix-vmtest')}")
        t.check(out("findmnt -no FSTYPE /var/lib").strip() == "overlay", f"/var/lib should be @varlib under the layer: {out('findmnt /var/lib')}")
        t.check(ok("lsattr /.archix/try/layer.img | grep -q '^[^ ]*C'"), f"the layer file is NOCOW: {out('lsattr /.archix/try/layer.img')}")
        m.succeed("echo try > /etc/archix-try-write && echo try > /var/lib/archix-try-write && echo try > /usr/archix-try-write", what="writing to the layer")
        t.check(not ok("[ -e /.archix/snapshots/%d/root/etc/archix-try-write ]" % older), "the snapshot itself stays read-only and unchanged")
        m.reboot()

    with t.step("3: the next boot is the current system's, without the try's changes"):
        started = boot()
        t.check(started == "archix-current.conf", f"a plain reboot should start the current entry, not {started}")
        t.check(trying() is None, "and not in try mode")
        t.check(out("cat /etc/archix-vmtest").strip() == "2", "the current system as the last switch left it")
        t.check(not ok("[ -e /etc/archix-try-write ]") and not ok("[ -e /var/lib/archix-try-write ]") and not ok("[ -e /usr/archix-try-write ]"), "the try's changes should be gone")
        t.check(not ok("[ -e /.archix/try/layer.img ]"), "the layer file should be gone")
