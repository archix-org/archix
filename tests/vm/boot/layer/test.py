# Try mode's layer: a try boot that fills its layer file gets "no space left on device" and the notice said where the limit is; on a near-full disk the layer's file shrinks, and with no room for any it goes to memory, so the fallback still comes up.
import time

from boot.common import bind, boot, bump, entries, entry, ok, out, snapshots, switch, trying


def free_mib():
    """Free MiB once the space a deleted file held is back: btrfs frees it after a commit, the last try's layer file included, which the initramfs deletes."""
    last = None
    while True:
        now = int(out("btrfs filesystem sync / > /dev/null; df -B1M --output=avail / | tail -1").strip())
        if now == last:
            return now
        last = now
        time.sleep(2)


def try_boot(m, t, older, why):
    m.select_entry(f"archix-snapshot-{older}.conf")
    m.reboot()
    boot()
    tried = trying() or {}
    t.check(tried.get("snapshot") == older, f"a try boot of snapshot {older} {why}: {tried}")
    return tried


def run(m, t):
    bind(m, t)
    with t.step("1: a try boot fills its layer"):
        boot()
        bump(1)
        switch()
        older = snapshots()[-1]
        bump(2)
        switch()
        # The 7 GiB disk leaves a 4 GiB layer only just room, which the downloaded packages can take.
        m.succeed("rm -f /var/cache/pacman/pkg/*.pkg.tar.*")
        t.log(f"free before the try boot: {free_mib()} MiB")
        tried = try_boot(m, t, older, "with room on the disk")
        t.check(tried.get("layer") == "file" and tried.get("size") == 4 << 30, f"a 4 GiB layer file: {tried}")
        status, said = m.execute("dd if=/dev/zero of=/root/fill bs=1M count=6000 2>&1", 900)
        t.check(status != 0 and "No space left on device" in said, f"filling the layer should fail with ENOSPC: {status} {said[-300:]}")
        t.check(ok("grep -q 'no space left on device' /run/motd.d/archix.motd"), f"the notice says where the limit is: {out('cat /run/motd.d/archix.motd')}")
        m.reboot()

    with t.step("2: on a near-full disk the layer's file is smaller"):
        boot()
        m.succeed(f"head -c {max(free_mib() - 2048, 1)}M /dev/urandom > /root/filler && sync -f /root/filler", timeout=1800, what="filling the disk to 2 GiB free")
        t.log(f"free: {free_mib()} MiB; " + out("btrfs filesystem usage / | head -12"))
        tried = try_boot(m, t, older, "on a near-full disk")
        t.check(tried.get("layer") == "file" and tried.get("size") == 1 << 30, f"a 1 GiB layer file: {tried}")
        t.check(ok("grep -q '1024 MiB' /run/motd.d/archix.motd"), f"the notice gives its size: {out('cat /run/motd.d/archix.motd')}")
        m.reboot()

    with t.step("3: with no room for any file the layer is in memory, and the boot still comes up"):
        boot()
        m.succeed(f"head -c {max(free_mib() - 100, 1)}M /dev/urandom > /root/filler2 && sync -f /root/filler2", timeout=1800, what="filling the disk to 100 MiB free")
        t.log(f"free: {free_mib()} MiB")
        tried = try_boot(m, t, older, "on a full disk")
        t.check(tried.get("layer") not in (None, "file"), f"the layer should be in memory: {tried}")
        t.check(ok("grep -q 'in memory' /run/motd.d/archix.motd"), f"the notice says so: {out('cat /run/motd.d/archix.motd')}")
        m.reboot()

    with t.step("4: the current system boots again and the space comes back"):
        started = boot()
        t.check(started == "archix-current.conf" and trying() is None, f"a plain reboot starts the current entry: {started}")
        m.succeed("rm -f /root/filler /root/filler2")
        t.check(not ok("[ -e /.archix/try/layer.img ]"), "no layer file is left")
