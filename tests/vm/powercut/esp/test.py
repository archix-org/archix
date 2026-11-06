# A cut after a plain `pacman -Syu` replaced the running kernel (linux 7.2.9 to 7.2.11), before the step after it built the new image: the old image boots a root without its modules, at the default boot.unblessed_timeout and tries; the ESP must still mount, the health check fail on the mark, and the machine fall back to the slot unattended, whose time from the cut is measured for the docs.
import time

from boot.common import bind, entries, out, snapshots, state, trying
from powercut import cut

UPDATE = "pacman -Syu --noconfirm --noprogressbar"
LOG = "/root/update.log"
POINT = "boot-image-built"


def run(m, t):
    bind(m, t)
    with t.step("0: the defaults, the update downloaded, a boot that counts as good"):
        cut.common.boot()
        m.succeed("sed -i 's|/2026/10/10/|/2026/10/24/|' /etc/pacman.d/mirrorlist && pacman -Syuw --noconfirm --noprogressbar > /root/download.log 2>&1", timeout=1800, what="downloading the update")
        t.require("linux 7.2.9" in out("pacman -Qu"), f"the update should replace the running kernel: {out('pacman -Qu')}")
        m.reboot()
        cut.settle()
        t.require(trying() is None, f"the current system should boot: {trying()}")
        cut.baseline("the system before the update")

    with t.step("1: the cut once the transaction replaced the running kernel"):
        before = snapshots()
        armed, slot = state().get("armed"), state().get("slot")
        cut.arm(POINT)
        cut.background(UPDATE, LOG)
        reached = cut.reset_on_cut(timeout=1800)
        began = time.monotonic()
        t.check(reached == f"{POINT}#1", f"the cut should be at {POINT}: {reached}")

    with t.step("2: the torn root boots its old kernel, and unattended the machine falls back to the slot"):
        m.wait_for_shell(timeout=900)
        cut.disarm()
        release = out("uname -r").strip()
        t.log(f"booted {release} from {cut.common.selected()}")
        mounted = cut.common.ok("ls /efi/loader/entries > /dev/null && findmnt -no FSTYPE /efi | grep -qx vfat")
        modules = cut.common.ok(f"[ -e /usr/lib/modules/{release}/modules.dep ]")
        efi = out("systemctl status efi.mount --no-pager 2>&1 | tail -3")
        tried = None
        while tried is None:
            t.require(time.monotonic() - began < 3600, f"no fallback within an hour of the cut: entries {entries()}")
            boots, _ = cut.settle(timeout=3600)
            tried = (trying() or {}).get("snapshot")
        took = time.monotonic() - began
        # The torn boot reboots as soon as its check fails, so its line is read from the console.
        said = ([line for line in m.console_since(0).splitlines() if "failed its health check" in line] or [""])[-1]
        t.log(f"the health check: {said}")
        new = [n for n in snapshots() if n not in before]
        want = new[0] if armed and new else slot
        t.log(f"FALLBACK-TIME default unblessed_timeout: {took:.0f} s from the cut to the slot's boot, snapshot {tried}, {boots} boot(s) after the first; ESP mounted on the torn boot: {mounted}")
        t.check(release.startswith("7.2.9"), f"the old image should boot: {release}")
        t.check(mounted, f"the ESP should mount on the torn boot: {efi}")
        t.check(modules, f"the booted kernel's modules should be back on the torn boot: {release}")
        t.check("marked bad" in said and "reboots into the fallback now" in said, f"the health check should mark the entry bad and reboot at once: {said}")
        t.check(took < 300, f"a boot the mark says is torn should fall back in about a minute, not after boot.unblessed_timeout: {took:.0f} s")
        t.check(str(tried) == str(want), f"the fallback should be the slot, snapshot {want}: {tried}")
        found = cut.problems(snapshot=new[0] if new else None, lock=cut.kind(tried) == "pacman", mark_on_root=True)
        t.check(not found, f"the fallback is consistent: {found}")
