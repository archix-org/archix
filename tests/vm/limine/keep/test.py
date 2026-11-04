# Keep on Limine, which boots the kernel and initramfs on the ESP rather than the root's: a keep of a snapshot whose kernel differs from the current one is refused before anything changes, naming why; a keep of one with the same kernel goes through, and Limine boots the kept root at the next boot.
from boot.common import HOST, bind, boot, bump, out, snapshots, switch

# What CachyOS's Limine hooks do after a kernel update: the new kernel and its initramfs onto the ESP, where Limine boots them.
TO_ESP = "v=$(pacman -Ql linux | awk '$2 ~ /\\/usr\\/lib\\/modules\\/[^/]+\\/vmlinuz$/ { print $2 }' | cut -d/ -f5) && cp /usr/lib/modules/$v/vmlinuz /efi/vmlinuz-linux && mkinitcpio -k $v -S autodetect -g /efi/initramfs-linux.img > /root/esp-initramfs.log 2>&1"


def value():
    return int(out("cat /etc/archix-vmtest").strip())


def run(m, t):
    bind(m, t)
    with t.step("1: a keep across a kernel change is refused before anything changes"):
        boot()
        bump(1)
        switch()
        older = snapshots()[-1]
        kernel = out("pacman -Q linux").strip()
        m.succeed("sed -i 's|/2026/10/10/|/2026/10/17/|' /etc/pacman.d/mirrorlist && pacman -Sy --noconfirm --noprogressbar linux > /root/kernel.log 2>&1", timeout=1800, what="updating the kernel")
        t.require(out("pacman -Q linux").strip() != kernel, f"the kernel should have changed from {kernel}")
        m.succeed(TO_ESP, timeout=900, what="the new kernel onto the ESP")
        before = snapshots()
        status, said = m.execute(f"{HOST} rollback {older} --noconfirm 2>&1")
        t.log(f"keep of snapshot {older} across {kernel} -> {out('pacman -Q linux').strip()}: exit {status}: {said[-600:]}")
        t.check(status != 0 and "Limine boots the kernel and initramfs on the ESP" in said and "nothing was changed" in said, f"the keep should be refused naming the kernel: {said[-600:]}")
        t.check(snapshots() == before and not keep_pending(), f"the refused keep should change nothing: {before} -> {snapshots()}")
        m.reboot()

    with t.step("2: a keep with the same kernel goes through, and Limine boots the kept root"):
        boot()
        bump(2)
        switch()
        same = snapshots()[-1]
        bump(3)
        switch()
        said = m.succeed(f"{HOST} rollback {same} --noconfirm 2>&1", what=f"keeping snapshot {same}")
        t.log(said)
        m.reboot()
        boot()
        # A switch's snapshot holds the system from before it: the one taken by the switch to 2 holds 1, on the new kernel.
        t.check(value() == 1, f"the kept root holds the system from before the switch to 2, not {value()}")
        t.check(out("findmnt -no FSROOT /").strip() == "/@", f"the kept root boots under the name @: {out('findmnt /')}")


def keep_pending():
    return out("[ -e /run/archix/keep-pending ] && echo yes || true").strip() == "yes"
