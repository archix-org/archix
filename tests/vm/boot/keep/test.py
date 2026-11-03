# Keep: `archix rollback N` makes snapshot N the root from the next boot under the name @, carries accounts made since (from @, also in a try boot), refuses pacman and switches until the reboot, and `--cancel` swaps the running system back.
from boot.common import HOST, bind, boot, bump, entries, entry, ok, out, snapshots, switch, trying
from boot.hook.test import BUILD, PKG


def refused_pacman():
    """Whether plain pacman installing a package is refused, naming the pending keep."""
    return ok(BUILD) and not ok(f"pacman -U --noconfirm {PKG} > /tmp/pacman.log 2>&1") and ok("grep -q 'is pending' /tmp/pacman.log")


def run(m, t):
    bind(m, t)
    with t.step("1: keep the switch snapshot, refusals while pending, then --cancel"):
        boot()
        bump(1)
        switch()
        bump(2)
        switch()
        kept = snapshots()[-1]
        m.succeed("useradd -m -s /bin/bash keeper && echo keeper:pw | chpasswd", what="adding a user after the snapshot")
        said = m.succeed(f"{HOST} rollback {kept} --noconfirm 2>&1", what="keeping the snapshot")
        replaced = kept + 1
        t.check(f"the running system is snapshot {replaced}" in said and "are lost" in said, f"keep names the replaced root and what's lost: {said}")
        t.check(ok("grep -q 'is pending' /run/motd.d/archix-keep.motd"), "the pending notice is in the motd")
        t.check(not ok(f"{HOST} switch --noconfirm > /tmp/switch.log 2>&1") and ok("grep -q 'is pending' /tmp/switch.log"), f"a switch refuses: {out('cat /tmp/switch.log')}")
        t.check(refused_pacman(), f"pacman refuses while the keep is pending: {out('cat /tmp/pacman.log')}")
        m.succeed(f"{HOST} rollback --cancel", what="cancelling the keep")
        t.check(replaced not in snapshots(), f"--cancel drops the replaced snapshot: {snapshots()}")
        t.check(not ok("[ -e /run/archix/keep-pending ]"), "nothing is pending after --cancel")
        m.succeed(f"{HOST} rollback {kept} --noconfirm", what="keeping it again")
        m.reboot()

    with t.step("2: the kept snapshot boots as @, with the accounts made since"):
        started = boot()
        t.check(started == "archix-current.conf", f"the current entry should start, not {started}")
        t.check(out("findmnt -no FSROOT /").strip() == "/@", f"the root is mounted as @ by name: {out('findmnt /')}")
        t.check(out("cat /etc/archix-vmtest").strip() == "1", f"the root is the kept snapshot: {out('cat /etc/archix-vmtest')}")
        t.check(ok("getent passwd keeper") and ok("getent shadow keeper | grep -q '^keeper:\\$'"), "the user added after the snapshot comes along with its password")
        status = out(f"{HOST} status 2>&1")
        t.check("pending" not in status and "isn't the one archix last produced" not in status, f"status: nothing pending or foreign: {status}")
        listed = out("archix snapshot list")
        t.check(any(line.split()[:1] == [str(replaced)] and "replaced" in line for line in listed.splitlines()), f"the old root is snapshot {replaced}, of kind replaced: {listed}")
        m.wait_until_succeeds("[ -f /efi/loader/entries/archix-current.conf ]", timeout=300, what="the kept root's boot should count as good")
        m.succeed(f"{HOST} rollback {replaced} --noconfirm && {HOST} rollback --cancel", what="keeping the replaced root, then cancelling")
        m.reboot()

    with t.step("3: after --cancel the reboot keeps the running system"):
        started = boot()
        t.check(started == "archix-current.conf", f"the current entry should start, not {started}")
        t.check(out("cat /etc/archix-vmtest").strip() == "1", f"still the kept snapshot: {out('cat /etc/archix-vmtest')}")
        t.check(replaced in snapshots(), f"the earlier replaced root stays a snapshot: {snapshots()}")
        older = kept - 1
        t.check(entry(f"archix-snapshot-{older}.conf") is not None, f"snapshot {older} should have an entry: {entries()}")
        m.succeed("useradd -m -s /bin/bash before_try", what="adding a user to @ before the try boot")
        m.select_entry(f"archix-snapshot-{older}.conf")
        m.reboot()

    with t.step("4: keep from a try boot carries accounts from @, not the try's /etc"):
        boot()
        t.check((trying() or {}).get("snapshot") == older, f"a try boot of snapshot {older}: {trying()}")
        m.succeed("useradd -m -s /bin/bash in_try", what="adding a user in the try boot's throwaway /etc")
        said = m.succeed(f"{HOST} rollback {older} --noconfirm 2>&1", what="keeping the tried snapshot")
        t.check("the @ it replaces is snapshot" in said, f"keep from a try boot names the @ it replaces: {said}")
        m.reboot()

    with t.step("5: the tried snapshot is the root, with @'s accounts"):
        started = boot()
        t.check(started == "archix-current.conf" and trying() is None, f"the current entry should start, not in try mode: {started} {trying()}")
        t.check(out("cat /etc/archix-vmtest").strip() == "0", f"the root is snapshot {older}: {out('cat /etc/archix-vmtest')}")
        t.check(ok("getent passwd before_try"), "the user added to @ comes along")
        t.check(not ok("getent passwd in_try"), "the user added only in the try boot's layer doesn't")
        t.log(f"entries {entries()}")
