# file_systems, swap and zram: a data disk, a LUKS volume with a key file, a bind mount and a tmpfs mount at the next boot, a swap file archix makes and zram; a switch with swap active; the boot with the data disk gone.
import os
import shlex
import subprocess

from boot.common import CONFIG, HOST, bind, boot, current, ok, out

KEY = "/var/lib/archix-vmtest/secret.key"
SOURCE = "/var/lib/archix-vmtest/bind-source"


def mounts(data, luks):
    return (
        "file_systems = { "
        f'["/mnt/data"] = {{ device = "UUID={data}", fs_type = "ext4", check = true }}, '
        f'["/mnt/bound"] = {{ device = "{SOURCE}", options = {{ "bind" }} }}, '
        f'["/mnt/secret"] = {{ device = "/dev/mapper/vmsecret", fs_type = "ext4", encrypted = {{ device = "UUID={luks}", key_file = "{KEY}" }} }}, '
        '["/mnt/scratch"] = { device = "tmpfs", fs_type = "tmpfs", options = { "size=16M" } } }, '
        'swap = { { file = "/swap/swapfile", size = "64M" } }, '
        'zram = { enable = true, size = 64 },'
    )


def swaps():
    return out("swapon --show=NAME --noheadings").split()


def run(m, t):
    bind(m, t)
    subprocess.run(["qemu-img", "create", "-q", "-f", "qcow2", f"{m.work}/data.qcow2", "256M"], check=True)
    m.disks.append(f"file={m.work}/data.qcow2,format=qcow2,if=virtio")
    fs = os.environ.get("FS", "ext4")
    with t.step("fs1: a data disk, a LUKS volume and a bind source; the switch writes them into the next deployment's fstab"):
        boot()
        m.succeed("printf 'label: gpt\\nsize=96MiB\\nsize=96MiB\\n' | sfdisk -q /dev/vdb && udevadm settle && mkfs.ext4 -q -L vmdata /dev/vdb1", what="partitioning and formatting the data disk")
        m.succeed(f"mkdir -p /var/lib/archix-vmtest {SOURCE} && echo bound > {SOURCE}/marker && head -c 64 /dev/urandom > {KEY} && chmod 600 {KEY}")
        m.succeed(f"cryptsetup luksFormat -q --type luks2 --pbkdf pbkdf2 --pbkdf-force-iterations 1000 /dev/vdb2 {KEY} && cryptsetup open --key-file {KEY} /dev/vdb2 vmsecret && mkfs.ext4 -q /dev/mapper/vmsecret && cryptsetup close vmsecret", what="making the LUKS volume")
        m.succeed("mount /dev/vdb1 /mnt && echo data > /mnt/marker && umount /mnt")
        data = out("blkid -s UUID -o value /dev/vdb1").strip()
        luks = out("blkid -s UUID -o value /dev/vdb2").strip()
        declare = 's|hostname = "archix-vm",|hostname = "archix-vm", ' + mounts(data, luks) + "|"
        m.succeed(f"sed -i {shlex.quote(declare)} {CONFIG}")
        before = current()
        said = m.succeed(f"{HOST} switch --noconfirm", what="switch with file_systems, swap and zram")
        pending = current()
        fstab = out(f"cat /archix/deploy/{pending}/etc/fstab")
        t.check(f"UUID={data} /mnt/data ext4 nofail,x-systemd.device-timeout=30s 0 2" in fstab and "/dev/mapper/vmsecret /mnt/secret ext4 nofail 0 0" in fstab and "/swap/swapfile none swap nofail 0 0" in fstab, f"the next deployment's fstab should hold the declared lines: {fstab}")
        esp = [line for line in fstab.splitlines() if " /efi " in line]
        t.check(len(esp) == 1 and "x-systemd.automount" in esp[0] and "dmask=0077" in esp[0] and esp[0].endswith(" 2"), f"the ESP's line should keep archix's protections: {fstab}")
        t.check("fstab      mount /mnt/data" in said and "at the next boot" in said, f"the switch should say the mounts wait for the next boot: {said}")
        t.check(pending != before and not ok("findmnt /mnt/data") and "/swap/swapfile" not in swaps(), "the mounts and the swap should wait for the next boot")
        m.reboot()

    with t.step("fs2: the next boot mounts them, swaps on the file archix made and zram; a switch with swap active"):
        boot()
        t.check(out("findmnt -no FSTYPE /mnt/data").strip() == "ext4" and out("cat /mnt/data/marker").strip() == "data", f"the data disk should be mounted at /mnt/data: {out('findmnt /mnt/data')}")
        t.check(out("cat /mnt/bound/marker").strip() == "bound", f"the bind mount should show its source: {out('findmnt /mnt/bound')}")
        t.check(out("findmnt -no SOURCE /mnt/secret").strip() == "/dev/mapper/vmsecret", f"the LUKS volume should be opened with its key file and mounted: {out('journalctl -b -u systemd-cryptsetup@vmsecret --no-pager | tail -5')}")
        t.check(out("findmnt -no FSTYPE /mnt/scratch").strip() == "tmpfs", "the tmpfs should be mounted at /mnt/scratch")
        active = swaps()
        t.check("/swap/swapfile" in active and "/dev/zram0" in active, f"swapon should show the swap file and zram: {active}; {out('journalctl -b -u archix-swapfile-swap-swapfile --no-pager | tail -5')}")
        if fs == "btrfs":
            t.check(ok("btrfs subvolume show /swap > /dev/null"), "on btrfs the swap file's directory should be a subvolume of its own")
        m.succeed(f"sed -i 's|zram = {{ enable = true, size = 64 }},|zram = {{ enable = true, size = 64 }}, sysctl = {{ [\"vm.swappiness\"] = 30 }},|' {CONFIG}")
        before = current()
        m.succeed(f"{HOST} switch --noconfirm", what="a switch with the swap file active")
        t.check(current() != before, "the switch with swap active should make a new deployment")
        m.shutdown()
        m.disks.pop()
        m.process = None

    with t.step("fs3: without the data disk the boot goes on, leaving its mounts out"):
        boot()
        t.check(not ok("[ -e /dev/vdb ]"), "the data disk should be gone")
        t.check(not ok("findmnt /mnt/data") and not ok("findmnt /mnt/secret"), "the data disk's mounts should be left out")
        t.check(out("cat /mnt/bound/marker").strip() == "bound" and "/swap/swapfile" in swaps(), "the mounts on the root and the swap file should still be there")
        t.check(out("sysctl -n vm.swappiness").strip() == "30", "the deployment switched to with swap active should boot")
