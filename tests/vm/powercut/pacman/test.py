# Power cuts in a real `pacman -Syu` (2026-10-10 to 2026-10-24: linux 7.2.9 to 7.2.11, systemd 262 to 262.1): during package extraction, at each step of the boot image rebuild after it, and while the menu resets the current entry's boot counter on the ESP. After each cut nobody touches the machine until a boot settles; then the system is judged, and `archix rollback` to the transaction's snapshot puts it back for the next cut.
import re

from boot.common import HOST, bind, edit_config, entries, out, snapshots, switch, trying
from powercut import cut

UPDATE = "pacman -Syu --noconfirm --noprogressbar"
LOG = "/root/update.log"
# The marks of the step after the transaction: the images built, recorded and copied to the ESP, then the menu written with the current entry's tries.
POST = ["boot-image-built", "boot-image-recorded", "boot-image-copy", "boot-image-copied", "menu-entry-written", "menu-entry-replaced", "menu-synced"]


def back_to(m, t, number, why):
    """Keeps the transaction's snapshot and reboots into it, dropping the root it replaces, so the next cut starts from the system before the update."""
    # A cut transaction leaves pacman's lock; archix refuses with it, as pacman does, and says to remove it once no pacman runs.
    m.succeed("pgrep -x pacman > /dev/null || rm -f /var/lib/pacman/db.lck")
    said = m.succeed(f"{HOST} rollback {number} --noconfirm 2>&1", what=f"keeping snapshot {number} {why}")
    replaced = cut.replaced(said)
    m.reboot()
    cut.settle()
    if replaced:
        m.execute(f"{HOST} snapshot delete {replaced.group(1)} --noconfirm 2>&1")
    t.check(out("pacman -Q linux").split()[-1].startswith("7.2.9"), f"after the keep the system should be the one before the update: {out('pacman -Q linux systemd')}")


def judge(m, t, point, before, packages):
    """Follows the boots after the cut and records the verdict; returns the transaction's snapshot, which must survive with its entry."""
    try:
        boots, took = cut.settle()
    except Exception as e:
        cut.record(point, False, 0, 0, "-", None, [str(e)[:300]])
        t.require(False, f"no boot came up after the cut at {point}")
    tried = (trying() or {}).get("snapshot")
    new = [n for n in snapshots() if n not in before]
    pre = new[0] if new else None
    found = cut.problems(snapshot=pre, packages=packages and tried is None)
    if pre is None:
        found.append(f"the hook's snapshot is gone: {before} -> {snapshots()}")
    entry = out(f"tail -c +5 {cut.common.EFIVAR} | tr -d '\\000'").strip()
    cut.record(point, True, boots, took, entry, tried, found, f"linux {out('uname -r').strip()}, entries {' '.join(entries())}")
    t.check(not found, f"consistent after the cut at {point}: {found}")
    return pre


def run(m, t):
    bind(m, t)
    with t.step("0: a two-minute unblessed timeout, the update downloaded"):
        cut.common.boot()
        edit_config("'s|  boot = { loader = \"systemd-boot\" },|  boot = { loader = \"systemd-boot\", unblessed_timeout = 2 },|'")
        switch("switch to a two-minute unblessed timeout")
        m.succeed("sed -i 's|/2026/10/10/|/2026/10/24/|' /etc/pacman.d/mirrorlist && pacman -Syuw --noconfirm --noprogressbar > /root/download.log 2>&1", timeout=1800, what="downloading the update")
        listed = out("pacman -Qu")
        t.require("linux 7.2.9" in listed and "systemd 262" in listed, f"the update should bring a new kernel and systemd: {listed}")
        names = out("pacman -Su --print --print-format %n --noconfirm").split()
        count = len(names)
        t.log(f"{count} packages in the transaction: {' '.join(names)}")

    def extract(k):
        before = snapshots()
        cut.background(UPDATE, LOG)
        m.wait_for_console_text(rf"(upgrading|installing) {re.escape(names[k - 1])}\.\.\.", timeout=1800)
        m.crash()
        pre = judge(m, t, f"extract-{k}", before, False)
        if pre is not None:
            back_to(m, t, pre, "after the cut")

    def post(point):
        before = snapshots()
        cut.background(UPDATE, LOG)
        m.wait_for_console_text(r"archix: snapshot \d+ holds the system as it was before this transaction", timeout=1800)
        cut.arm(point)
        reached = cut.reset_on_cut(timeout=1800)
        cut.disarm()
        pre = judge(m, t, reached, before, True)
        if pre is not None:
            back_to(m, t, pre, "after the cut")

    for k in sorted({1, max(2, count // 4), count // 2, (3 * count) // 4, count}):
        cut.point(f"extract {k}/{count}: cut once pacman starts on {names[k - 1]}", lambda k=k: extract(k))
    for name in POST:
        cut.point(f"{name}: cut in the step after the transaction", lambda name=name: post(name))

    def control():
        before = snapshots()
        m.succeed(f"{UPDATE} > {LOG} 2>&1", timeout=1800, what="the whole update")
        m.reboot()
        judge(m, t, "none", before, True)
        t.check(out("uname -r").startswith("7.2.11"), f"the new kernel should boot: {out('uname -r')}")

    cut.point("control: the update without a cut boots its new kernel", control)
    cut.summary()
