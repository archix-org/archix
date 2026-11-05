# A live-USB recovery: two switches, then the ESP wiped (no loader, entries or images) and the root's init damaged, so the disk starts no kernel; a plain Arch disk, as a live USB, takes the static archix from the disk, `archix chroot`s into the root with its layout, puts systemd-boot back and keeps the snapshot from before the damage, which boots and switches again. btrfs only: on ext4 there's no snapshot to keep, and `archix chroot` mounts a btrfs layout.
import subprocess

from boot.common import bind, boot, bump, entries, ok, out
from suite import BTRFS, lay_out, newest_snapshot, switch

ARCHIX = ["file=/work/disk.img,format=raw,if=none,id=archix"]
BOTH = ["file=/work/rescue.img,format=raw,if=none,id=rescue", "file=/work/disk.img,format=raw,if=none,id=archix"]
MACHINE = {"disks": ARCHIX, "global_timeout": 7200}


def disks(m, rescue):
    m.disks = BOTH if rescue else ARCHIX
    m.extra = (["-device", "virtio-blk-pci,drive=rescue,bootindex=0"] if rescue else []) + ["-device", "virtio-blk-pci,drive=archix,bootindex=1"]


def powered_off(m, timeout):
    try:
        m.process.wait(timeout)
    except subprocess.TimeoutExpired:
        return False
    m.stop()
    m.process = None
    m.connected = False
    return True


def run(m, t):
    bind(m, t)
    if not BTRFS:
        t.log("recovery runs on btrfs only: on ext4 there's no snapshot to keep, and archix chroot mounts a btrfs layout")
        return
    disks(m, False)
    lay_out(m, t)
    with t.step("1: two switches, then the ESP wiped and the root's init damaged"):
        bump(1)
        switch(m, t, "the switch to mark 1")
        bump(2)
        switch(m, t, "the switch to mark 2")
        kept = newest_snapshot()
        t.check(out(f"cat /.archix/snapshots/{kept}/root/etc/archix-vmtest").strip() == "1", f"snapshot {kept} should hold mark 1")
        m.succeed(f"mkdir -p /var/lib/archix-vmtest && echo {kept} > /var/lib/archix-vmtest/kept")
        m.succeed("rm -rf /efi/* && [ -z \"$(ls -A /efi)\" ]", what="wiping the ESP")
        m.succeed("printf 'damaged\\n' > /usr/lib/systemd/systemd.new && mv /usr/lib/systemd/systemd.new /usr/lib/systemd/systemd && sync -f /usr/lib/systemd /efi /var/lib/archix-vmtest", what="damaging the root's systemd")
        m.shutdown()

    with t.step("2: the damaged disk starts no kernel"):
        m.process = None
        m.new_boot("booting the wiped disk")
        m.start()
        m.console_mark = 0
        t.check(not powered_off(m, 180), "the wiped disk should neither boot nor power off")
        seen = m.console_since(m.console_mark)
        m.stop()
        m.process = None
        t.log(f"the wiped disk's console: {seen[-600:]}")
        t.check("Linux version" not in seen, "the wiped disk should start no kernel")

    with t.step(f"3: from a plain Arch disk, archix chroot and keeping snapshot {kept} recover it"):
        disks(m, True)
        m.new_boot("booting the rescue disk")
        m.start()
        m.console_mark = 0
        m.wait_for_console_text(r"VMTEST: rescue: (recovered|FAIL.*)", timeout=2400)
        said = m.console_since(m.console_mark)
        t.log("\n".join(line for line in said.splitlines() if "VMTEST" in line))
        t.require("VMTEST: rescue: recovered" in said, "the recovery on the rescue disk failed (above)")
        t.require(powered_off(m, 300), "the rescue disk should power off")

    with t.step(f"4: the disk boots snapshot {kept}, kept, and switches again"):
        disks(m, False)
        m.new_boot("booting the recovered disk")
        started = boot()
        t.check(started == "archix-current.conf", f"the current entry the recovery wrote should start: {started} {entries()}")
        t.check(out("cat /etc/archix-vmtest").strip() == "1" and ok("head -c 4 /usr/lib/systemd/systemd | grep -q ELF"), "the root should be the kept snapshot, mark 1, with its systemd whole")
        t.check(not [line for line in out("systemctl --failed --no-legend").splitlines() if "archix" in line], f"no archix unit should fail: {out('systemctl --failed --no-legend')}")
        t.log(out("archix status 2>&1 | head -20"))
        bump(3)
        switch(m, t, "the switch after the recovery")
        m.reboot()
        boot()
        t.check(out("cat /etc/archix-vmtest").strip() == "3", "the switch after the recovery boots")
