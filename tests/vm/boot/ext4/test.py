# The ext4 lane, rollbacks off (config management only): switches apply live, `archix revert` goes back to an older generation live, the boot after a switch is counted and blessed through the health check, and nothing offers a rollback.
from boot.common import HOST, bind, boot, bump, entries, entry, ok, out, switch


def run(m, t):
    bind(m, t)
    with t.step("1: switches apply live on ext4"):
        boot()
        t.check(out("findmnt -no FSTYPE /").strip() == "ext4", f"the root is ext4: {out('findmnt /')}")
        bump(1)
        switch()
        t.check(out("cat /etc/archix-vmtest").strip() == "1", "the switch applied live")
        bump(2)
        switch()
        t.check(out("cat /etc/archix-vmtest").strip() == "2", "the second switch applied live")
        t.check(not ok("[ -d /.archix/snapshots ]"), "no snapshots on ext4")
        status = out("archix status 2>&1")
        t.log(status)

    with t.step("2: revert to the previous generation applies live"):
        t.log(out("archix generations"))
        said = m.succeed(f"{HOST} revert --noconfirm 2>&1", what="archix revert")
        t.check(out("cat /etc/archix-vmtest").strip() == "1", f"the revert applied live: {said[-500:]}")
        refused = out("archix rollback 1 --noconfirm 2>&1")
        t.check("rollback" in refused and not ok("archix rollback 1 --noconfirm"), f"rollback isn't available on ext4: {refused}")

    with t.step("3: the boot after a switch is counted and blessed"):
        bump(3)
        switch()
        t.check(entry("archix-current.conf") == "archix-current+2.conf", f"the switch gives the current entry two tries: {entries()}")
        m.reboot()
        started = boot()
        t.check(started == "archix-current.conf", f"the current entry boots: {started}")
        m.wait_until_succeeds("[ -f /efi/loader/entries/archix-current.conf ]", timeout=300, what="the counted boot should be blessed")
        said = out("journalctl -b -u archix-health-check.service --no-pager")
        t.check("passed its health check" in said, f"the health check runs and passes: {said[-600:]}")
        t.check(out("cat /etc/archix-vmtest").strip() == "3", "the switched system")
        t.check(not any(e.startswith("archix-snapshot-") for e in entries()), f"no snapshot entries: {entries()}")
