# Strict mode's paths out on a real system: while on, plain pacman refuses; it lets pacman through again with archix's binary gone, its flag gone, the config gone, after strict = false and a switch, and when the transaction is pacman -R archix itself, after which plain pacman works.
from boot.common import CONFIG, bind, boot, edit_config, ok, out, switch
from boot.hook.test import BUILD, PKG

FLAG = "/var/lib/archix/strict"
REFUSED = "strict mode is on, so packages change only through archix"
ARCHIX_PKG = "/root/archix-0-1-x86_64.pkg.tar.zst"
PACKAGE_ARCHIX = (
    "set -e; d=/root/archix-pkg; rm -rf $d; mkdir -p $d/usr/bin $d/usr/lib/archix; cp /usr/bin/archix $d/usr/bin/; cp /usr/lib/archix/archix-static $d/usr/lib/archix/; "
    "printf 'pkgname = archix\\npkgbase = archix\\npkgver = 0-1\\npkgdesc = archix for the test\\narch = x86_64\\nsize = 4096\\n' > $d/.PKGINFO; "
    f"cd $d && bsdtar --zstd -cf {ARCHIX_PKG} .PKGINFO usr"
)


def plain(m, t, allowed, why):
    """Installs then removes the plain test package with plain pacman; checks it's let through, or refused naming strict mode."""
    status, said = m.execute(f"pacman -U --noconfirm {PKG} 2>&1", 300)
    if allowed:
        t.check(status == 0, f"plain pacman should work {why}: {said[-500:]}")
        m.execute("pacman -R --noconfirm archix-test-plain > /dev/null 2>&1")
    else:
        t.check(status != 0 and REFUSED in said, f"plain pacman should be refused {why}: {said[-500:]}")


def run(m, t):
    bind(m, t)
    with t.step("1: archix as a package, strict mode on"):
        boot()
        m.succeed(f"{BUILD} && {PACKAGE_ARCHIX} && pacman -U --noconfirm --overwrite '*' {ARCHIX_PKG} > /dev/null", what="installing archix as a package")
        edit_config("'s|^return {|return { strict = true,|'")
        switch("switch to strict mode")
        flag = out(f"ls /var/lib/archix/ | grep -i strict").strip()
        t.require(flag, f"the switch should write strict mode's flag: {out('ls /var/lib/archix')}")
        plain(m, t, False, "with strict mode on")

    with t.step("2: archix's binary gone, its flag gone, the config gone: pacman works each time"):
        m.succeed("mv /usr/bin/archix /root/archix.moved")
        plain(m, t, True, "with archix's binary gone")
        m.succeed("mv /root/archix.moved /usr/bin/archix")
        plain(m, t, False, "with the binary back")
        m.succeed(f"mv /var/lib/archix/{flag} /root/flag.moved")
        plain(m, t, True, "with the flag gone")
        m.succeed(f"mv /root/flag.moved /var/lib/archix/{flag}")
        config = out(f"cat /var/lib/archix/{flag}").strip()
        m.succeed(f"mv {config}/init.lua /root/init.lua.moved")
        plain(m, t, True, "with the config gone")
        m.succeed(f"mv /root/init.lua.moved {config}/init.lua")
        plain(m, t, False, "with everything back")

    with t.step("3: pacman -R archix works while strict mode is on, and plain pacman after it"):
        status, said = m.execute("pacman -R --noconfirm archix 2>&1", 300)
        t.check(status == 0, f"removing archix should work in strict mode: {said[-600:]}")
        plain(m, t, True, "after archix is removed")
        m.succeed(f"pacman -U --noconfirm {ARCHIX_PKG} > /dev/null", what="installing archix again")
        plain(m, t, False, "with archix back and its flag still there")

    with t.step("4: strict = false and a switch turn it off"):
        edit_config("'s|^return { strict = true,|return {|'")
        switch("switch with strict mode off")
        plain(m, t, True, "with strict mode off")
