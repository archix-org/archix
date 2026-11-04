# A root replaced as limine-snapper-restore replaces it, on a machine Limine boots: @ moved aside and a writable snapshot of an older read-only snapshot (snapper's, taken outside archix) put in its place. At the next boot archix reports a root it didn't produce, says this is expected on Limine, refuses switches and keeps, and `archix resync` adopts it, carrying the accounts made since from the root it replaced; then switches work again.
from boot.common import HOST, bind, boot, bump, ok, out, switch

TOP = "/run/archix-test-top"
MOUNT = f"dev=$(findmnt -no SOURCE / | sed 's/\\[.*//') && mkdir -p {TOP} && mount -o subvolid=5 $dev {TOP}"


def value():
    return int(out("cat /etc/archix-vmtest").strip())


def run(m, t):
    bind(m, t)
    with t.step("1: snapper's snapshot, then changes, then the root replaced by it"):
        boot()
        bump(1)
        switch()
        m.succeed(f"{MOUNT} && btrfs -q subvolume snapshot -r {TOP}/@ {TOP}/@snapper-1 && umount {TOP}", what="a read-only snapshot of @ outside archix")
        bump(2)
        switch()
        m.succeed("useradd -m archix-carried && echo archix-carried:pw | chpasswd", what="an account made after that snapshot")
        m.succeed(f"{MOUNT} && mv {TOP}/@ {TOP}/@restore-backup && btrfs -q subvolume snapshot {TOP}/@snapper-1 {TOP}/@ && umount {TOP}", what="replacing @ as limine-snapper-restore does")
        m.reboot()

    with t.step("2: the replaced root is reported and refuses switches; resync adopts it and carries accounts"):
        boot()
        t.check(value() == 1, f"Limine boots the restored root, which holds 1, not {value()}")
        status = out("archix status 2>&1")
        t.check("isn't the one archix last produced" in status and "limine-snapper-restore" in status and "archix resync" in status, f"status should report the replaced root, say it's expected on Limine and offer resync: {status[-800:]}")
        code, said = m.execute(f"{HOST} switch --noconfirm 2>&1")
        t.check(code != 0 and "isn't the one archix last produced" in said and "nothing was changed" in said, f"a switch should refuse until resync: {said[-600:]}")
        code, said = m.execute(f"{HOST} rollback 1 --noconfirm 2>&1")
        t.check(code != 0 and "isn't the one archix last produced" in said, f"a keep should refuse until resync: {said[-600:]}")
        said = m.succeed("archix resync 2>&1", what="resync")
        t.log(said)
        t.check("@restore-backup" in said, f"resync should carry accounts from the root it replaced, found by its recorded UUID as @restore-backup: {said[-600:]}")
        t.check(ok("getent passwd archix-carried"), "the account made after snapper's snapshot comes over")
        t.check("isn't the one archix last produced" not in out("archix status 2>&1"), "status has nothing to report after resync")
        bump(3)
        switch("a switch after resync")
        t.check(value() == 3, f"switches work again: {value()}")
