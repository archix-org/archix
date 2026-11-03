# A masked hook: archix's snapshot hook linked to /dev/null in /etc/pacman.d/hooks makes plain pacman runs take no snapshot; `archix status` and the boot's notice say so, and removing the link brings the snapshots back.
from boot.common import bind, boot, ok, out, snapshots

MASK = "/etc/pacman.d/hooks/01-archix-snapshot.hook"
PKG = "/root/archix-test-plain-1-1-any.pkg.tar.zst"
BUILD = (
    "set -e; d=/root/archix-test-plain; rm -rf $d; mkdir -p $d/usr/share/archix-test; echo plain > $d/usr/share/archix-test/plain; "
    "printf 'pkgname = archix-test-plain\\npkgbase = archix-test-plain\\npkgver = 1-1\\npkgdesc = a plain package\\narch = any\\nsize = 4096\\n' > $d/.PKGINFO; "
    f"cd $d && bsdtar --zstd -cf {PKG} .PKGINFO usr"
)
SAYS = "01-archix-snapshot.hook is masked by /etc/pacman.d/hooks/01-archix-snapshot.hook, so plain pacman runs take no snapshot"


def run(m, t):
    bind(m, t)
    with t.step("1: the hook masked, a plain pacman run takes no snapshot, and status says why"):
        boot()
        m.succeed(f"mkdir -p /etc/pacman.d/hooks && ln -s /dev/null {MASK} && {BUILD}", what="masking the hook")
        before = snapshots()
        said = out(f"pacman -U --noconfirm {PKG} 2>&1")
        t.check(snapshots() == before and "holds the system as it was" not in said, f"no snapshot with the hook masked: {before} -> {snapshots()}: {said}")
        status = out("archix status 2>&1")
        t.check(SAYS in status, f"status should name the masked hook: {status}")
        m.reboot()

    with t.step("2: the boot's notice names it too; without the link, snapshots come back"):
        boot()
        notice = out("cat /run/motd.d/archix*.motd /run/issue.d/archix*.issue 2>/dev/null; journalctl -b -u archix-boot.service --no-pager")
        t.check(SAYS in notice, f"the boot's notice should name the masked hook: {notice[-1200:]}")
        m.succeed(f"rm {MASK}")
        before = snapshots()
        said = out(f"pacman -R --noconfirm archix-test-plain 2>&1")
        t.check(len(snapshots()) == len(before) + 1, f"with the link gone the hook snapshots again: {before} -> {snapshots()}: {said}")
        t.check(SAYS not in out("archix status 2>&1"), "status has nothing to say about the hook any more")
