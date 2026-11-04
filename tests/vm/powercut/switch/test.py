# Power cuts at each step of `archix switch` installing a package and changing the initramfs, and at the bless rename of the boot after one: a cut before the switch marks itself unfinished must leave the system from before it booting and counting as good; one between the mark and its removal must fall back by itself to the slot, the switch's snapshot, in try mode, with the mark still on @; one after must boot the new generation; a cut in the bless rename must still boot the new generation with its entry under one name.
import re

from boot.common import HOST, bind, edit_config, entries, out, snapshots, state, switch, trying
from powercut import cut

LOG = "/root/switch.log"
PACKAGE = "tree"
# Before the mark: the snapshot, then the menu giving the current entry its tries and the slot its entry (menu-synced ends each refresh; whether it writes an entry depends on the slot).
BEFORE = ["switch-snapshot", "menu-synced#1"]
# Between the mark and its removal: "extract" cuts once pacman starts on the package.
MARKED = ["switch-marked", "extract", "switch-packages", "switch-files", "switch-applied", "switch-recorded", "boot-image-built", "boot-image-copied", "menu-synced#2", "switch-flushed"]
AFTER = ["switch-finished"]
BLESS = ["bless-rename", "bless-renamed", "bless-synced"]


def value():
    return int(out("cat /etc/archix-vmtest").strip())


def declare(n, package):
    """The config at value n, with the package declared or not, and n in the initramfs so each switch builds a new image."""
    edit_config(f"'s|\\[\"/etc/archix-vmtest\"\\] = {{ text = \"[0-9]*\" }}|[\"/etc/archix-vmtest\"] = {{ text = \"{n}\" }}|'")
    edit_config(f"'s|\"git\"{', \"' + PACKAGE + '\"' if not package else ''} }}|\"git\"{', \"' + PACKAGE + '\"' if package else ''} }}|'")


def run(m, t):
    bind(m, t)
    with t.step("0: a two-minute unblessed timeout and an initramfs that holds /etc/archix-vmtest"):
        cut.common.boot()
        edit_config("'s|  boot = { loader = \"systemd-boot\" },|  boot = { loader = \"systemd-boot\", unblessed_timeout = 2 },|'")
        edit_config("'s|  files = {|  files = {\\n    [\"/etc/mkinitcpio.conf.d/zz-archix-cut.conf\"] = { text = \"FILES=(/etc/archix-vmtest)\" },|'")
        switch("switch to a two-minute unblessed timeout")
        m.reboot()
        cut.settle()
        t.require(trying() is None, f"the boot after the switch should be the current system's: {trying()}")
        cut.baseline("the system before the cut switches")

    def blessed():
        """Reboots into the current system, which arms the slot if its entry was counted and counts as good."""
        m.reboot()
        cut.settle()
        t.require(trying() is None, f"the current system should boot: {trying()}, entries {entries()}")

    def back_to(number, why):
        """Keeps snapshot number, the system before the switch, and boots it as the current system."""
        said = m.succeed(f"{HOST} rollback {number} --noconfirm 2>&1", what=f"keeping snapshot {number} {why}")
        replaced = cut.replaced(said)
        m.reboot()
        cut.settle()
        t.require(trying() is None, f"after keeping snapshot {number} the current entry should boot it: {trying()}, entries {' '.join(entries())}")
        found = cut.problems()
        t.check(not found, f"consistent after keeping snapshot {number}: {found}")
        if replaced:
            m.execute(f"{HOST} snapshot delete {replaced.group(1)} --noconfirm 2>&1")

    def cycle(point):
        now = value()
        before = snapshots()
        menu = state()
        armed, slot = menu.get("armed"), menu.get("slot")
        declare(now + 1, True)
        m.execute(f"rm -f {LOG}")
        if point != "extract":
            cut.arm(point)
        cut.background(f"{HOST} switch --noconfirm", LOG)
        if point == "extract":
            m.wait_for_console_text(rf"installing {PACKAGE}\.\.\.", timeout=1800)
            m.crash()
            reached = "extract"
        else:
            reached = cut.reset_on_cut(timeout=1800)
        cut.disarm()
        try:
            boots, took = cut.settle()
        except Exception as e:
            cut.record(reached, False, 0, 0, "-", None, [str(e)[:300]])
            t.require(False, f"no boot came up after the cut at {reached}")
        tried = (trying() or {}).get("snapshot")
        new = [n for n in snapshots() if n not in before]
        taken = new[0] if new else None
        installed = cut.common.ok(f"pacman -Q {PACKAGE}")
        if point in BEFORE:
            found = cut.problems()
            if tried is not None or value() != now or installed:
                found.append(f"a cut before the switch changed anything should leave the system before it booting: try {tried}, value {value()}, {PACKAGE} installed {installed}")
        elif point in AFTER:
            found = cut.problems()
            if tried is not None or value() != now + 1 or not installed:
                found.append(f"a cut after the switch finished should boot the new generation: try {tried}, value {value()}, {PACKAGE} installed {installed}")
        else:
            found = cut.problems(snapshot=taken, mark_on_root=True)
            # The switch's snapshot is the slot only after a boot that counted as good; else the slot from before stays.
            want = taken if armed else slot
            if taken is None or tried != want:
                found.append(f"a cut between the mark and its removal should fall back to the slot, snapshot {want} (armed {armed}, the switch's is {taken}), in try mode, but this boot is {tried or 'the current system'}: {out('journalctl -b -1 -u archix-health-check --no-pager | tail -5')}")
            if value() != now or installed:
                found.append(f"the slot should hold the system before the switch: value {value()}, {PACKAGE} installed {installed}")
        entry = out(f"tail -c +5 {cut.common.EFIVAR} | tr -d '\\000'").strip()
        cut.record(reached, True, boots, took, entry, tried, found, f"value {value()} of {now} -> {now + 1}, entries {' '.join(entries())}")
        t.check(not found, f"consistent after the cut at {reached}: {found}")
        if tried is not None:
            back_to(tried, "after the fallback")
        declare(value(), cut.common.ok(f"pacman -Q {PACKAGE}"))
        if cut.common.ok(f"pacman -Q {PACKAGE}"):
            declare(value(), False)
            switch(f"taking {PACKAGE} out again")
        blessed()

    def bless(point):
        now = value()
        declare(now + 1, False)
        switch("a switch whose next boot blesses its entry")
        cut.arm(point)
        m.reboot()
        reached = cut.reset_on_cut(timeout=900)
        cut.disarm()
        try:
            boots, took = cut.settle()
        except Exception as e:
            cut.record(reached, False, 0, 0, "-", None, [str(e)[:300]])
            t.require(False, f"no boot came up after the cut at {reached}")
        tried = (trying() or {}).get("snapshot")
        found = cut.problems()
        if tried is not None or value() != now + 1:
            found.append(f"a cut in the bless rename should still boot the new generation: try {tried}, value {value()}")
        current = [name for name in entries() if re.match(r"archix-current(\+[0-9-]+)?\.conf$", name)]
        if len(current) != 1:
            found.append(f"the current entry should be on the ESP under one name: {entries()}")
        entry = out(f"tail -c +5 {cut.common.EFIVAR} | tr -d '\\000'").strip()
        cut.record(reached, True, boots, took, entry, tried, found, f"entries {' '.join(entries())}")
        t.check(not found, f"consistent after the cut at {reached}: {found}")
        blessed()

    for point in BEFORE + MARKED + AFTER:
        cut.point(f"{point}: cut in a switch", lambda point=point: cycle(point))
    for point in BLESS:
        cut.point(f"{point}: cut in the bless of the boot after a switch", lambda point=point: bless(point))
    cut.summary()
