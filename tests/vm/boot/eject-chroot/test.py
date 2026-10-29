# Eject from a live USB's arch-chroot: a plain Arch live system without archix (build-live.sh, booted by QEMU's -kernel as a USB stick's own kernel boots) mounts the laid-out root and its ESP, binds the current deployment with its static archix's mount-deployment and runs `archix eject` in arch-chroot; the disk then boots as plain Arch with the deployment's packages, a clean pacman -Qkk, and pacman -Syu from the archive snapshot.
import os
import subprocess
from pathlib import Path

from boot.common import STOCK_QKK, bind, boot, current, diff, has_line, ok, out, qkk_problems, qkk_warnings

LIVE = "root=LABEL=archixlive rw console=ttyS0"


def run(m, t):
    bind(m, t)
    with t.step("ec1: the laid-out system boots its current deployment, then shuts down"):
        _, _, running = boot()
        t.require(running == current(), f"deployment {running} should be current")
        packages = out("pacman -Q")
        qkk = out("pacman -Qkk 2>&1")
        m.shutdown()
        m.process = None

    with t.step("ec2: from the live system, mount-deployment and `archix eject` in arch-chroot"):
        base = Path(m.work).parent
        t.require((base / "live.img").is_file() and (base / "live-vmlinuz").is_file(), "build-image.sh should have built the live system (LIVE_DISK)")
        subprocess.run(["qemu-img", "create", "-q", "-f", "qcow2", "-b", str(base / "live.img"), "-F", "raw", f"{m.work}/live.qcow2"], check=True)
        m.disks.append(f"file={m.work}/live.qcow2,format=qcow2,if=virtio")
        m.extra = ["-kernel", str(base / "live-vmlinuz"), "-initrd", str(base / "live-initramfs.img"), "-append", LIVE]
        t.require(out("findmnt -no LABEL /").strip() == "archixlive", f"the live system should run from its own disk: {out('findmnt /')}")
        t.check(not ok("command -v archix || [ -e /archix ]"), "the live system should have no archix of its own")
        stock = qkk_warnings()
        t.check(stock == STOCK_QKK, f"the live system, Arch as pacstrap leaves it, should have exactly the pacman -Qkk warnings plain Arch is allowed: {sorted(stock)}")
        subvol = "-o subvol=@ " if os.environ.get("FS") == "btrfs" else ""
        m.succeed(f"mount {subvol}$(blkid -L archixroot) /mnt", what="mounting the archix root on the live system")
        esp = out("cat /mnt/var/lib/archix/boot/esp").strip()
        m.succeed(f"mount $(blkid -L ESP) /mnt{esp}", what="mounting the ESP where boot/esp says")
        n = out("readlink /mnt/archix/current").strip().split("/")[-1]
        t.check(n == running, f"current should still be deployment {running}, not {n}")
        m.succeed(f"/mnt/archix/deploy/{n}/usr/lib/archix/archix-static mount-deployment /mnt", what="mount-deployment from the live system")
        status, said = m.execute("arch-chroot /mnt env -u JOURNAL_STREAM archix eject --noconfirm 2>&1", 1800)
        t.log(said[-2000:])
        t.require(status == 0, f"archix eject in arch-chroot failed: {said[-1500:]}")
        t.check(f"deployment {n}, which this chroot runs" in said and "umount -R" in said, f"eject should say it lays out the bound deployment and what to do after: {said[-800:]}")
        t.check(not ok("[ -e /mnt/archix ]"), f"eject should delete /archix: {out('ls -A /mnt/archix 2>&1')}")
        t.check(not ok("mountpoint -q /mnt/usr") and not ok("mountpoint -q /mnt/etc"), f"eject should take the deployment binds off the live system's mounts too: {out('findmnt -R /mnt')}")
        t.check(out("arch-chroot /mnt pacman -Q 2>&1") == packages, f"the root's own trees should hold the deployment's packages: {diff(packages, out('arch-chroot /mnt pacman -Q 2>&1'))}")
        conf = out(f"cat /mnt{esp}/loader/loader.conf")
        t.check(has_line(conf, "default arch-linux.efi") and ok(f"[ -s /mnt{esp}/EFI/Linux/arch-linux.efi ]"), f"eject should make a plain UKI the default: {conf}")
        t.check(not ok(f"ls /mnt{esp}/loader/entries/archix-* /mnt{esp}/EFI/Linux/archix-fallback-* > /dev/null 2>&1") and not ok(f"[ -e /mnt{esp}/EFI/archix ]"), "eject left archix's boot entries")
        t.check(not ok("grep -rqs archix-deploy /mnt/etc/mkinitcpio.conf /mnt/etc/mkinitcpio.conf.d"), "the plain initramfs config should have no archix hook")
        t.check(not ok("awk '!/^#/ && ($2 ~ /^\\/archix/ || $4 ~ /archix/)' /mnt/etc/fstab | grep -q ."), f"fstab should mount nothing of archix's: {out('cat /mnt/etc/fstab')}")
        m.succeed("umount -R /mnt", what="unmounting the root from the live system")
        m.shutdown()
        m.process = None
        m.disks.pop()
        m.extra = []

    with t.step("ec3: plain Arch boots with the deployment's packages; pacman -Qkk is clean and pacman -Syu works"):
        selected, _, mounted = boot(match=False)
        t.check(selected == "arch-linux.efi", f"plain Arch should boot through arch-linux.efi, not {selected}")
        t.check(mounted == "" and not ok("[ -e /archix ]"), "a plain boot mounts no deployment")
        t.check("archix" not in out("findmnt -no SOURCE /usr"), "/usr should be the root's own")
        now = out("pacman -Q")
        t.check(now == packages, f"plain Arch has other packages than deployment {running} had: {diff(packages, now)}")
        now = out("pacman -Qkk 2>&1")
        t.check(now == qkk, f"pacman -Qkk differs from the deployment's: {diff(qkk, now)}")
        problems = qkk_problems()
        t.check(problems == [], f"pacman -Qkk should find nothing on plain Arch that stock Arch doesn't: {problems}")
        t.check(ok("[ -x /usr/local/bin/archix-vmtest-declared ]") and not ok("findmnt /usr/local"), "plain Arch should keep /usr/local's files in its own /usr/local")
        t.check(ok("systemctl is-active -q archix-protected.service"), "the ProtectSystem=strict service should run on plain Arch")
        for unit in ("archix-boot", "archix-deploy-activate", "archix-etc-merge", "archix-etc-merge-boot"):
            t.check(not ok(f"[ -e /etc/systemd/system/{unit}.service ]"), f"eject should remove {unit}.service")
        m.succeed("pacman -Syu --noconfirm", what="plain pacman -Syu after the eject")
        m.succeed("pacman -S --noconfirm tree", what="plain pacman -S after the eject")
        t.check(ok("command -v tree"), "pacman -S tree should install it")
        m.shutdown()
