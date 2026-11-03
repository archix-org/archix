# Power cuts in the move of /var/lib into @varlib, which the first switch schedules and archix-split-var-lib.service makes at the next boot: after the copy of an entry, after its removal from the root, and once the binds are ready. Each cut starts from the plain image; nobody touches the machine until a boot settles, which must have every file of /var/lib whole in @varlib and pacman's database bound from the root.
from boot.common import HOST, bind, boot, out, switch
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

    for point in POINTS:
        cut.point(f"{point}: cut in the move of /var/lib", lambda point=point: split(point))
    cut.summary()
