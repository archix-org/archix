# Power cuts in a real `pacman -Syu` (2026-10-10 to 2026-10-24: linux 7.2.9 to 7.2.11, systemd 262 to 262.1): during the step before the transaction as it gives the current entry its tries, during the extraction of every package, at each step of the boot image rebuild after it, and while the menu resets the current entry's boot counter on the ESP. A transaction cut off before the step after it finished is torn, so its boot mustn't count as good: after each cut nobody touches the machine until a boot settles, which must be the fallback slot, the snapshot before the transaction, in try mode; then the system is judged, and `archix rollback` to that snapshot puts it back for the next cut.
import re
import time

from boot.common import HOST, bind, edit_config, entries, out, snapshots, state, switch, trying
from powercut import cut

UPDATE = "pacman -Syu --noconfirm --noprogressbar"
LOG = "/root/update.log"
# The marks of the step after the transaction: the images built, recorded and copied to the ESP, then the menu written with the current entry's tries.
POST = ["boot-image-built", "boot-image-recorded", "boot-image-copy", "boot-image-copied", "menu-entry-written", "menu-synced"]
# The step before it renames the current entry to give it its tries, so the step after it finds the name already right and replaces none.
PRE = ["snapshot-before-record", "snapshot-before-rename", "menu-entry-written", "menu-entry-replaced", "menu-synced"]
EXTRACTED = r":: Processing package changes\.\.\."


class Before(list):
    """The snapshots before a cut, with whether the slot was armed and which it was."""

    armed = slot = None


def wait_update(m, t, regex, what, timeout=1800):
    """Waits for regex on the console from the update started in the background; fails at once, with its log, if pacman exits first."""
    pattern = re.compile(regex)
    deadline = time.monotonic() + timeout
    while not pattern.search(m.console_since(m.console_mark)):
        status, said = m.execute(f"tail -c 1500 {LOG} 2>/dev/null", quiet=True)
        if re.search(r"^exit \d+$", said, re.M):
            t.require(False, f"pacman ended before {what}: {said}")
        t.require(time.monotonic() < deadline, f"no {what} within {timeout} s: {said}")
        time.sleep(2)


def back_to(m, t, number, why):
    """Keeps the transaction's snapshot and reboots into it, dropping the root it replaces, so the next cut starts from the system before the update."""
    # A cut transaction leaves pacman's lock; archix refuses with it, as pacman does, and says to remove it once no pacman runs.
    m.succeed("pgrep -x pacman > /dev/null || rm -f /var/lib/pacman/db.lck")
    said = m.succeed(f"{HOST} rollback {number} --noconfirm 2>&1", what=f"keeping snapshot {number} {why}")
    t.log(said)
    replaced = cut.replaced(said)
    m.reboot()
    cut.settle()
    t.require(trying() is None and out("findmnt -no FSTYPE /").strip() != "overlay", f"after keeping snapshot {number} the current entry should boot it, not a try: {trying()}, entries {' '.join(entries())}; keep said: {said[-800:]}")
    found = cut.problems()
    t.check(not found, f"consistent after keeping snapshot {number}: {found}")
    if replaced:
        m.execute(f"{HOST} snapshot delete {replaced.group(1)} --noconfirm 2>&1")
    t.require(out("pacman -Q linux").split()[-1].startswith("7.2.9"), f"after the keep the system should be the one before the update: {out('pacman -Q linux systemd')}")


def judge(m, t, point, before, fallback=True):
    """Follows the boots after the cut and records the verdict; with fallback, the boot that settles must be the slot in try mode: the snapshot the step before the transaction took if the last boot counted as good, else the slot before it. Returns the transaction's snapshot."""
    try:
        boots, took = cut.settle()
    except Exception as e:
        cut.record(point, False, 0, 0, "-", None, [str(e)[:300]])
        t.require(False, f"no boot came up after the cut at {point}")
    tried = (trying() or {}).get("snapshot")
    new = [n for n in snapshots() if n not in before]
    pre = new[0] if new else None
    # A pacman snapshot is taken in PreTransaction, while pacman holds db.lck, so a try boot of one has it; @, which the cut tore, still holds the unfinished mark.
    if fallback:
        found = cut.problems(snapshot=pre, lock=tried is not None and cut.kind(tried) == "pacman", mark_on_root=True)
    else:
        found = cut.problems(snapshot=pre, compare=False)
    if pre is None:
        found.append(f"the hook's snapshot is gone: {before} -> {snapshots()}")
    slot = (pre if before.armed else before.slot) if fallback else None
    if fallback and str(tried) != str(slot):
        found.append(f"a cut-off transaction's boot should fall back to the slot, snapshot {slot}, in try mode, but this boot is {tried or 'the current system'}: {out('journalctl -b -u archix-health-check --no-pager | tail -5')}")
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
        # A boot that counts as good arms the slot, so the snapshot before the transaction becomes it, as after each cut's keep.
        m.reboot()
        cut.settle()
        t.require(trying() is None, f"the boot after the switch should be the current system's: {trying()}")
        cut.baseline("the system before the update")

    def start():
        before = Before(snapshots())
        menu = state()
        before.armed, before.slot = menu.get("armed"), menu.get("slot")
        m.execute(f"rm -f {LOG}")
        cut.background(UPDATE, LOG)
        return before

    def pre_hook(point):
        cut.arm(point)
        before = start()
        reached = cut.reset_on_cut(timeout=1800)
        cut.disarm()
        try:
            boots, took = cut.settle()
        except Exception as e:
            cut.record(f"pre-{reached}", False, 0, 0, "-", None, [str(e)[:300]])
            t.require(False, "no boot came up after the cut in the step before the transaction")
        tried = (trying() or {}).get("snapshot")
        # The cut killed pacman in its PreTransaction hooks, so the root it left keeps db.lck, and the mark comes after these points.
        found = cut.problems(lock=tried is None or cut.kind(tried) == "pacman")
        if not out("pacman -Q linux").split()[-1].startswith("7.2.9") or tried:
            found.append(f"a cut before the transaction changed anything should leave the current system booting as it was: {out('pacman -Q linux')}, try {trying()}")
        added = [n for n in snapshots() if n not in before]
        cut.record(f"pre-{reached}", True, boots, took, out(f"tail -c +5 {cut.common.EFIVAR} | tr -d '\\000'").strip(), tried, found, f"snapshots taken {added}, entries {' '.join(entries())}")
        t.check(not found, f"consistent after the cut in the step before the transaction: {found}")
        if tried:
            back_to(m, t, tried, "after the fallback")
            return
        m.succeed("pgrep -x pacman > /dev/null || rm -f /var/lib/pacman/db.lck")
        for number in added:
            m.execute(f"{HOST} snapshot delete {number} --noconfirm 2>&1")

    def extract(k):
        before = start()
        wait_update(m, t, rf"(upgrading|installing) {re.escape(names[k - 1])}\.\.\.", f"pacman starting on {names[k - 1]}")
        m.crash()
        pre = judge(m, t, f"extract-{k}", before)
        if pre is not None:
            back_to(m, t, pre, "after the cut")

    def post(point):
        before = start()
        wait_update(m, t, EXTRACTED, "the extraction")
        cut.arm(point)
        try:
            reached = cut.reset_on_cut(timeout=1800)
        except Exception:
            cut.disarm()
            m.wait_until_succeeds(f"grep -q '^exit ' {LOG}", timeout=1800, what="the update's end")
            new = [n for n in snapshots() if n not in before]
            if new:
                back_to(m, t, new[0], f"after an update the cut at {point} missed")
            t.require(False, f"the update never reached {point}: {out(f'tail -c 800 {LOG}')}")
        cut.disarm()
        pre = judge(m, t, reached, before)
        if pre is not None:
            back_to(m, t, pre, "after the cut")

    for name in PRE:
        cut.point(f"pre-transaction {name}: cut as the step before the transaction gives the current entry its tries", lambda name=name: pre_hook(name))
    for k in range(1, count + 1):
        cut.point(f"extract {k}/{count}: cut once pacman starts on {names[k - 1]}", lambda k=k: extract(k))
    for name in POST:
        cut.point(f"{name}: cut in the step after the transaction", lambda name=name: post(name))

    def control():
        before = snapshots()
        # A cut step whose pacman outlived it would hold the lock this one needs.
        m.wait_until_succeeds("! pgrep -x pacman", timeout=1800, what="no pacman left running")
        status, said = m.execute(f"{UPDATE} > {LOG} 2>&1; status=$?; tail -c 3000 {LOG}; exit $status", 1800)
        t.require(status == 0, f"the whole update failed (exit {status}): {said}")
        m.reboot()
        judge(m, t, "none", before, fallback=False)
        t.check(out("uname -r").startswith("7.2.11"), f"the new kernel should boot: {out('uname -r')}")
        t.check(trying() is None, f"the update without a cut should boot the current system, blessed: {trying()}")

    cut.point("control: the update without a cut boots its new kernel", control)
    cut.summary()
