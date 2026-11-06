# Power cuts in the move of /var/lib into @varlib, which the first switch schedules and archix-split-var-lib.service makes at the next boot: after the copy of an entry, after its removal from the root, and once the binds are ready. Each cut starts from the plain image; nobody touches the machine until a boot settles, which must have every file of /var/lib whole in @varlib and pacman's database bound from the root.
from boot.common import HOST, bind, boot, entries, out, switch
from powercut import cut

POINTS = ["split-entry-copied#1", "split-entry-removed#1", "split-entry-copied#3", "split-entry-removed#4", "split-binds-ready"]
SENTINEL = "/var/lib/aa-archix-cut"
SUM = "find /var/lib -xdev -path /var/lib/systemd -prune -o -type f -print0 2>/dev/null | sort -z | xargs -0 sha256sum | grep -v -E '  /var/lib/(pacman|dkms|archix|systemd)/' | sha256sum | cut -c1-16"


def run(m, t):
    bind(m, t)

    def split(point):
        cut.fresh("/work/plain.img")
        boot()
        m.succeed(f"mkdir -p {SENTINEL}/dir && head -c 64M /dev/urandom > {SENTINEL}/big && echo small > {SENTINEL}/dir/small && sha256sum {SENTINEL}/big {SENTINEL}/dir/small > /root/sentinel.sum", what="filling /var/lib")
        switch("the first switch, which schedules the move")
        before = out(SUM).strip()
        listed = out("ls -A /var/lib").split()
        cut.baseline(f"before the cut at {point}")
        cut.arm(point)
        m.reboot()
        reached = cut.reset_on_cut(timeout=900)
        t.check(reached == (point if "#" in point else f"{point}#1"), f"the cut should be at {point}, not {reached}")
        try:
            boots, took = cut.settle()
        except Exception as e:
            cut.record(reached, False, 0, 0, "-", None, [str(e)[:300]])
            t.require(False, f"no boot came up after the cut at {reached}")
        found = cut.problems()
        if out("findmnt -no FSROOT /var/lib").strip() != "/@varlib":
            found.append(f"/var/lib isn't @varlib: {out('findmnt /var/lib')}")
        if out("findmnt -no FSROOT /var/lib/pacman").strip() != "/@/var/rollback/pacman":
            found.append(f"pacman's database isn't bound from the root: {out('findmnt /var/lib/pacman')}")
        if not cut.common.ok("sha256sum -c --quiet /root/sentinel.sum"):
            found.append("the files put in /var/lib before the move changed")
        after = out(SUM).strip()
        if after != before:
            found.append(f"/var/lib's files differ from before the move ({before} -> {after}): {out('ls -A /var/lib')} (before: {' '.join(listed)})")
        if cut.common.ok("[ -e /var/rollback/.split-pending ]"):
            found.append("the move is still marked pending after the boot")
        cut.record(reached, True, boots, took, cut.common.selected(), None, found, f"/var/lib had {len(listed)} entries")
        t.check(not found, f"consistent after the cut at {reached}: {found}")
        m.reboot()
        cut.common.boot()

    def layout(point):
        cut.fresh("/work/plain.img")
        boot()
        m.succeed(f"mkdir -p {SENTINEL}/dir && echo small > {SENTINEL}/dir/small && sha256sum {SENTINEL}/dir/small > /root/sentinel.sum && sync -f /root/sentinel.sum", what="files in /var/lib, on disk before the cut")
        packages = set(out("pacman -Qq").split())
        cut.arm(point)
        cut.background(f"{HOST} switch --noconfirm", "/root/switch.log")
        reached = cut.reset_on_cut(timeout=900)
        t.check(reached == f"{point}#1", f"the cut should be at {point}, not {reached}")
        boots, took = cut.settle()
        found = []
        if not packages <= set(out("pacman -Qq").split()):
            found.append("packages went missing from pacman's database after the cut")
        lines = cut.common.ok("grep -q '[[:space:]]/var/lib[[:space:]]' /etc/fstab")
        if point == "layout-fstab-written":
            if not lines or out("findmnt -no FSROOT /var/lib").strip() != "/@varlib":
                found.append(f"the fstab's lines were on disk, so this boot should have moved /var/lib: {out('findmnt /var/lib')}")
        else:
            if lines or not cut.common.ok("[ -e /var/rollback/.split-pending ]"):
                found.append("a cut before the fstab's lines should leave the marker and no lines")
            if cut.common.ok("findmnt /var/lib"):
                found.append(f"/var/lib should still be on the root: {out('findmnt /var/lib')}")
        switch("the next switch, which finishes laying the root out")
        m.reboot()
        cut.settle()
        if out("findmnt -no FSROOT /var/lib").strip() != "/@varlib":
            found.append(f"/var/lib isn't @varlib after the next switch's boot: {out('findmnt /var/lib')}")
        if out("findmnt -no FSROOT /var/lib/pacman").strip() != "/@/var/rollback/pacman":
            found.append(f"pacman's database isn't bound from the root: {out('findmnt /var/lib/pacman')}")
        if cut.common.ok("[ -e /var/rollback/.split-pending ]"):
            found.append("the move is still marked pending after the next switch's boot")
        if not packages <= set(out("pacman -Qq").split()):
            found.append("packages went missing from pacman's database after the next switch")
        if not cut.common.ok("sha256sum -c --quiet /root/sentinel.sum"):
            found.append("the files put in /var/lib before the cut changed")
        cut.record(reached, True, boots, took, cut.common.selected(), None, found)
        t.check(not found, f"the next switch finishes the move after the cut at {reached}: {found}")

    def whole(what):
        """What keeps the system the rerun left from being an uncut first switch's after its boot: the menu, the layout, pacman's database and files."""
        found = cut.problems()
        if not any(name.startswith("archix-current") for name in entries()):
            found.append(f"no current entry on the ESP: {entries()}")
        if out("findmnt -no FSROOT /var/lib").strip() != "/@varlib":
            found.append(f"/var/lib isn't @varlib: {out('findmnt /var/lib')}")
        if out("findmnt -no FSROOT /var/lib/pacman").strip() != "/@/var/rollback/pacman":
            found.append(f"pacman's database isn't bound from the root: {out('findmnt /var/lib/pacman')}")
        if cut.common.ok("[ -e /var/rollback/.split-pending ]"):
            found.append("the move of /var/lib is still pending")
        status = out(f"{HOST} status 2>&1")
        if "boot step" in status:
            found.append(f"archix status says the boot step isn't done {what}: {status[-400:]}")
        return found

    with t.step("first: an uncut first switch and its boot, the baseline of the reruns"):
        cut.fresh("/work/plain.img")
        boot()
        t.require(not cut.common.ok("pacman -Q efitools"), "the plain image lacks efitools, so the first switch runs a transaction")
        switch("the uncut first switch")
        m.reboot()
        cut.settle()
        cut.baseline("an uncut first switch after its boot")
        found = whole("after an uncut first switch")
        t.require(not found, f"the uncut first switch should be whole: {found}")

    def rerun(point):
        cut.fresh("/work/plain.img")
        boot()
        if point != "extract":
            cut.arm(point)
        cut.background(f"{HOST} switch --noconfirm", "/root/switch.log")
        if point == "extract":
            m.wait_for_console_text(r"installing efitools\.\.\.", timeout=1800)
            m.crash()
            reached = "extract"
        else:
            reached = cut.reset_on_cut(timeout=1800)
            t.check(reached == f"{point}#1", f"the cut should be at {point}, not {reached}")
        cut.disarm()
        boots, took = cut.settle()
        said = m.succeed(f"{HOST} switch --noconfirm 2>&1", timeout=1800, what="the user's switch after the cut")
        m.reboot()
        cut.settle()
        found = whole("after the rerun")
        cut.record(reached, True, boots, took, cut.common.selected(), None, found, f"the rerun said: {said.strip().splitlines()[-1][:200] if said.strip() else ''}")
        t.check(not found, f"a rerun after the cut at {reached} ends whole: {found}")

    for point in ["extract", "switch-packages", "layout-split-armed", "switch-recorded", "boot-image-copied", "menu-entry-written", "switch-boot-done"]:
        cut.point(f"{point}: cut in the first switch, then the user switches again", lambda point=point: rerun(point))
    for point in ["layout-subvolumes", "layout-split-armed", "layout-fstab-written"]:
        cut.point(f"{point}: cut in the first switch's layout", lambda point=point: layout(point))
    for point in POINTS:
        cut.point(f"{point}: cut in the move of /var/lib", lambda point=point: split(point))
    cut.summary()
