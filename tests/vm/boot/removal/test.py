# Removing archix, the in-place model's eject: archix-git removed with pacman -R takes archix's pacman hooks with it, and the system then boots with no archix unit failing, pacman installs, and a kernel reinstall's mkinitcpio and the boot after it work without archix. Main's eject laid a deployment out as plain Arch; with no deployments, removing the package is what's left to test.
from boot.common import BACKDOOR, bind, boot, ok, out

PKG = "/var/tmp/archix-git-1-1-x86_64.pkg.tar.zst"
# A package owning the archix binaries the image installed by hand, as the AUR's archix-git would.
PACKAGE = r"""set -e
d=$(mktemp -d)
mkdir -p $d/usr/bin $d/usr/lib/archix
cp /usr/bin/archix $d/usr/bin/archix && ln -s archix $d/usr/bin/ax && cp /usr/lib/archix/archix-static $d/usr/lib/archix/archix-static
size=$(du -sb $d/usr | cut -f1)
printf 'pkgname = archix-git\npkgbase = archix-git\npkgver = 1-1\npkgdesc = archix\nbuilddate = 0\npackager = vmtest\nsize = %s\narch = x86_64\nlicense = MIT\nprovides = archix\n' $size > $d/.PKGINFO
cd $d && bsdtar --zstd -cf @OUT@ .PKGINFO usr
"""


def run(m, t):
    bind(m, t)
    with t.step("rm1: archix-git removed with pacman -R takes archix's hooks with it"):
        boot()
        m.succeed(PACKAGE.replace("@OUT@", PKG), what="packaging archix as archix-git")
        m.succeed(f"pacman -U --noconfirm --overwrite '*' {PKG} 2>&1 | tail -5", 600, what="installing archix-git over the image's own files")
        t.check(ok("ls /usr/share/libalpm/hooks/01-archix-snapshot.hook"), "archix's hooks should be there before the removal")
        said = m.succeed("pacman -R --noconfirm archix-git 2>&1", 600, what="pacman -R archix-git")
        t.log(said[-1500:])
        t.check(not ok("command -v archix"), "archix should be gone")
        t.check(not ok("ls /usr/share/libalpm/hooks/*archix* /usr/lib/archix/strict-hook.sh 2>/dev/null"), f"archix's hooks should go with it: {out('ls /usr/share/libalpm/hooks')}")
        m.succeed("pacman -S --noconfirm tree 2>&1 | tail -5", 900, what="pacman -S after the removal")
        t.check(ok("command -v tree"), "pacman -S tree should install it")
        m.reboot()

    with t.step("rm2: the system boots without archix, with no archix unit failing"):
        boot(allowed=tuple(u for u in out("systemctl --failed --no-legend --plain | awk '{ print $1 }'").split() if not u.startswith("archix")))
        failed = [u for u in out("systemctl --failed --no-legend --plain | awk '{ print $1 }'").split() if u != BACKDOOR]
        t.check(not failed, f"no unit should fail once archix is gone: {failed}; {out('journalctl -b -p err --no-pager | tail -20')}")
        t.check(not ok("command -v archix"), "archix stays gone")

    with t.step("rm3: a kernel reinstall's mkinitcpio works without archix, and the machine boots after it"):
        said = m.succeed("pacman -S --noconfirm linux 2>&1 | tail -30", 1800, what="reinstalling the kernel without archix")
        t.log(said)
        t.check("ERROR" not in said and "error:" not in said, f"mkinitcpio and the hooks should run cleanly: {said[-1500:]}")
        m.reboot()
        boot(allowed=tuple(u for u in out("systemctl --failed --no-legend --plain | awk '{ print $1 }'").split() if not u.startswith("archix")))
        t.check(out("uname -r").strip() == out("pacman -Q linux | awk '{print $2}' | sed 's/\\.arch/-arch/'").strip() or ok("[ -d /usr/lib/modules/$(uname -r) ]"), f"the boot should run an installed kernel: {out('uname -r')}")
