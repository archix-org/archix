# A broken glibc update days after the last kernel update: a kernel update makes a pacman snapshot with an entry and boots fine, a later plain pacman transaction installs a glibc whose libc.so.6 is garbage, so nothing dynamic runs any more (pacman's own hooks and archix's step after the transaction among them), and the machine is reset as a user would; with nobody touching it, it must fall back to the slot, the snapshot just before glibc, not to the older kernel snapshot.
import time

from boot.common import bind, boot, entries, entry, ok, out, snapshots, state, trying, wait_fallback

BUILD = r"""set -e
d=/root/glibc-broken; rm -rf $d; mkdir -p $d
v=$(pacman -Q glibc | cut -d' ' -f2); new=${v%-*}-$(( ${v##*-} + 1 ))
desc=$(ls -d /var/lib/pacman/local/glibc-[0-9]*)/desc
cd / && pacman -Qlq glibc | grep -v '/$' | sed 's|^/||' | bsdtar -cf - -T - | bsdtar -xf - -C $d
printf 'garbage, not a library\n' > $d/usr/lib/libc.so.6
field() { awk -v f="%$1%" '$0 == f { on = 1; next } /^%/ { on = 0 } on && NF { print $1 }' $desc; }
{ printf 'pkgname = glibc\npkgbase = glibc\npkgver = %s\npkgdesc = a broken glibc\narch = x86_64\nsize = 1\n' $new
  field DEPENDS | sed 's/^/depend = /'; field PROVIDES | sed 's/^/provides = /'; field BACKUP | sed 's/^/backup = /'; } > $d/.PKGINFO
cd $d && bsdtar --zstd -cf /root/glibc-broken.pkg.tar.zst .PKGINFO $(ls)
echo $new"""


def run(m, t):
    bind(m, t)
    with t.step("1: a kernel update through plain pacman, then a good boot"):
        boot()
        m.succeed("sed -i 's|/2026/10/10/|/2026/10/17/|' /etc/pacman.d/mirrorlist && pacman -Syu --noconfirm --noprogressbar > /root/kernel.log 2>&1", timeout=1800, what="the kernel update")
        kernel = snapshots()[-1]
        t.check(entry(f"archix-snapshot-{kernel}.conf") is not None, f"the kernel update's snapshot should have an entry: {entries()}")
        m.reboot()
        boot()
        t.check(out("uname -r").startswith("7.2.10"), f"the new kernel boots: {out('uname -r')}")
        m.wait_until_succeeds("grep -q '\"armed\": true' /.archix/boot/state.json", timeout=300, what="the good boot should arm the slot")

    with t.step("2: a broken glibc installed by plain pacman, then a reset"):
        m.reboot()
        boot()
        version = m.succeed(BUILD, what="building the broken glibc").strip().splitlines()[-1]
        t.log(f"broken glibc {version}")
        before = snapshots()
        m.succeed("setsid -f bash -c 'pacman -U --noconfirm --noprogressbar /root/glibc-broken.pkg.tar.zst > /dev/console 2>&1; echo s > /proc/sysrq-trigger; echo ARCHIX-GLIBC-DONE > /dev/console' < /dev/null > /dev/null 2>&1", what="installing it")
        m.wait_for_console_text(r"ARCHIX-GLIBC-DONE", timeout=900)
        time.sleep(15)
        began = time.monotonic()
        m.crash()

    with t.step("3: with nobody there the slot boots, the system just before glibc"):
        took = wait_fallback(began, timeout=3600)
        started = boot()
        t.log(f"UNATTENDED glibc: from the reset into the broken system to the fallback's shell: {took:.0f} s")
        slot = [n for n in snapshots() if n not in before]
        t.require(slot, f"the transaction should have made a snapshot: {before} -> {snapshots()}")
        t.check(started == f"archix-snapshot-{slot[0]}.conf", f"systemd-boot should fall back to the slot, archix-snapshot-{slot[0]}.conf, not {started} (the kernel snapshot is {kernel})")
        t.check((trying() or {}).get("snapshot") == slot[0], f"in try mode: {trying()}")
        t.check(out("pacman -Q glibc").split()[-1] != version, f"the slot has the glibc from before: {out('pacman -Q glibc')}")
        t.log(f"state {state()} entries {entries()}")
