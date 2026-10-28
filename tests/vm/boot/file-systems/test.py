# file_systems, swap and zram: a data disk, a LUKS volume with a key file, a bind mount and a tmpfs mount at the next boot, a swap file archix makes and zram; hibernating to that swap file and resuming; a switch with swap active; the boot with the data disk gone; /home on a disk of its own, required by default, stopping the boot when it's gone.
import os
import shlex
import subprocess

from boot.common import CONFIG, HOST, bind, boot, current, ok, out

KEY = "/var/lib/archix-vmtest/secret.key"
SOURCE = "/var/lib/archix-vmtest/bind-source"
EMERGENCY = r"Emergency Mode|emergency mode|root account is locked"


def mounts(data, luks):
    return (
        "file_systems = { "
        f'["/mnt/data"] = {{ device = "UUID={data}", fs_type = "ext4", check = true }}, '
        f'["/mnt/bound"] = {{ device = "{SOURCE}", options = {{ "bind" }} }}, '
        f'["/mnt/secret"] = {{ device = "/dev/mapper/vmsecret", fs_type = "ext4", encrypted = {{ device = "UUID={luks}", key_file = "{KEY}" }} }}, '
        '["/mnt/scratch"] = { device = "tmpfs", fs_type = "tmpfs", options = { "size=16M" } } }, '
        'swap = { { file = "/swap/swapfile", size = "1G" } }, '
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

    with t.step("fs2: the next boot mounts them, swaps on the file archix made and zram; hibernating to the swap file resumes the same boot; a switch with swap active"):
        boot()
        t.check(out("findmnt -no FSTYPE /mnt/data").strip() == "ext4" and out("cat /mnt/data/marker").strip() == "data", f"the data disk should be mounted at /mnt/data: {out('findmnt /mnt/data')}")
        t.check(out("cat /mnt/bound/marker").strip() == "bound", f"the bind mount should show its source: {out('findmnt /mnt/bound')}")
        t.check(out("findmnt -no SOURCE /mnt/secret").strip() == "/dev/mapper/vmsecret", f"the LUKS volume should be opened with its key file and mounted: {out('journalctl -b -u systemd-cryptsetup@vmsecret --no-pager | tail -5')}")
        t.check(out("findmnt -no FSTYPE /mnt/scratch").strip() == "tmpfs", "the tmpfs should be mounted at /mnt/scratch")
        active = swaps()
        t.check("/swap/swapfile" in active and "/dev/zram0" in active, f"swapon should show the swap file and zram: {active}; {out('journalctl -b -u archix-swapfile-swap-swapfile --no-pager | tail -5')}")
        if fs == "btrfs":
            t.check(ok("btrfs subvolume show /swap > /dev/null"), "on btrfs the swap file's directory should be a subvolume of its own")
        t.check(not ok("grep -qw resume= /proc/cmdline") and out("busctl call org.freedesktop.login1 /org/freedesktop/login1 org.freedesktop.login1.Manager CanHibernate").split()[-1:] == ['"yes"'], f"logind should offer hibernation to the swap file with no resume= on the command line: {out('cat /proc/cmdline')}")
        before = out("cat /proc/sys/kernel/random/boot_id").strip()
        m.succeed("echo kept > /dev/shm/archix-vmtest-hibernate && mkdir -p /run/systemd/system/archix-backdoor.service.d && printf '[Service]\\nRestart=always\\nRestartSec=1\\n' > /run/systemd/system/archix-backdoor.service.d/restart.conf && systemctl daemon-reload")
        resumed = m.hibernate()
        t.check(resumed == before and out("cat /dev/shm/archix-vmtest-hibernate 2>&1").strip() == "kept", f"the guest should resume the boot it hibernated ({before[:8]}, now {resumed[:8]}), RAM intact: {out('journalctl -b --no-pager | grep -i hibernat | tail -8')}")
        t.check(ok("journalctl -b --no-pager | grep -q 'hibernation exit'"), f"the kernel should log the resume: {out('journalctl -b -k --no-pager | grep -i hibernat | tail -5')}")
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
        m.shutdown()
        m.process = None

    subprocess.run(["qemu-img", "create", "-q", "-f", "qcow2", f"{m.work}/home.qcow2", "256M"], check=True)
    m.disks.append(f"file={m.work}/home.qcow2,format=qcow2,if=virtio")
    with t.step("fs4: /home moves to a disk of its own, required by default"):
        boot()
        m.succeed("printf 'label: gpt\\nsize=200MiB\\n' | sfdisk -q /dev/vdb && udevadm settle && mkfs.ext4 -q -L vmhome /dev/vdb1 && mount /dev/vdb1 /mnt && cp -a /home/. /mnt/ && echo home > /mnt/archix-vmtest-home && umount /mnt", what="moving /home to its own disk")
        home = out("blkid -s UUID -o value /dev/vdb1").strip()
        declare = 's|file_systems = { |file_systems = { ["/home"] = { device = "UUID=' + home + '", fs_type = "ext4" }, |'
        m.succeed(f"sed -i {shlex.quote(declare)} {CONFIG}")
        m.succeed(f"{HOST} switch --noconfirm", what="a switch declaring /home")
        fstab = out(f"cat /archix/deploy/{current()}/etc/fstab")
        t.check(f"UUID={home} /home ext4 defaults 0 0\n" in fstab, f"/home's line should have no nofail: {fstab}")
        m.reboot()
        boot()
        t.check(out("findmnt -no FSTYPE /home").strip() == "ext4" and out("cat /home/archix-vmtest-home").strip() == "home", f"/home should be its disk: {out('findmnt /home')}")
        m.shutdown()
        m.disks.pop()
        m.process = None

    with t.step("fs5: without the /home disk the boot stops in emergency mode instead of showing an empty /home"):
        found, shell = m.boot_stopping_at(EMERGENCY)
        console = m.console_since(0)
        t.check(not shell and ("home.mount" in console or "/home" in console), f"the boot should stop before multi-user without /home, not reach the shell: {found!r}")
        m.kill()
        m.stop()
        m.process = None
        m.connected = False

    m.disks.append(f"file={m.work}/home.qcow2,format=qcow2,if=virtio")
    with t.step("fs6: with the disk back the boot mounts /home again"):
        boot()
        t.check(out("cat /home/archix-vmtest-home").strip() == "home", f"/home should be its disk again: {out('findmnt /home')}")
