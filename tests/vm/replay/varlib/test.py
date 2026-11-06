# Write replay of a plain install's first switch, which lays the root out for rollbacks, writes the first boot menu and schedules the move of /var/lib into @varlib, and of the boot after it, which makes the move: every flush and mark, and subsets of each flush's ESP writes, must boot by itself, never as a try, as the plain system or the laid-out one, with every file of /var/lib whole and no move left pending.
from boot.common import bind, ok, out, switch, trying
from powercut import cut
from replay import replay

SENTINEL = "/var/lib/aa-archix-replay"


def run(m, t):
    bind(m, t)
    s = replay.Scenario(m, t, "varlib")
    with t.step("0: the plain image with files in /var/lib and the marks in place"):
        s.start_from("/work/plain.img")
        cut.common.boot()
        t.require(not ok("[ -d /.archix/snapshots ]"), "the image should boot as a plain install, without rollbacks yet")
        # The config's efitools, which the image lacks, so the logged switch has less to install.
        m.succeed("pacman -S --noconfirm --noprogressbar efitools > /root/efitools.log 2>&1", timeout=900, what="installing efitools")
        m.succeed(f"mkdir -p {SENTINEL}/dir && head -c 64M /dev/urandom > {SENTINEL}/big && echo small > {SENTINEL}/dir/small && sha256sum {SENTINEL}/big {SENTINEL}/dir/small > /root/sentinel.sum", what="filling /var/lib")
        replay.install(m)
        m.reboot()
        cut.settle()
    s.freeze()

    def body():
        cut.settle()
        replay.mark(m, "switch-start")
        switch("the first switch, which schedules the move")
        replay.mark(m, "switched")
        replay.idle(m)
        m.reboot()
        cut.settle()
        t.require(trying() is None and out("findmnt -no FSROOT /var/lib").strip() == "/@varlib", f"the boot after the first switch should move /var/lib into @varlib: {out('findmnt /var/lib')}, try {trying()}")
        m.wait_until_succeeds(f"grep -q '\"armed\": true' {cut.common.STATE}", timeout=300, what="the good boot arming the slot")
        replay.mark(m, "end")

    with t.step("1: the first switch and the boot that moves /var/lib, logged"):
        s.record(body)

    def extra():
        found = []
        if not ok("sha256sum -c --quiet /root/sentinel.sum"):
            found.append("the files put in /var/lib before the switch changed")
        # A cut before the fstab's lines leaves the marker on a plain system, for the next switch to finish.
        if ok("[ -e /var/rollback/.split-pending ] && grep -q '[[:space:]]/var/lib[[:space:]]' /etc/fstab"):
            found.append("the move of /var/lib is still pending after the boot settled")
        if out("findmnt -no FSROOT /var/lib").strip() == "/@varlib" and out("findmnt -no FSROOT /var/lib/pacman").strip() != "/@/var/rollback/pacman":
            found.append(f"/var/lib is @varlib, but pacman's database isn't bound from the root: {out('findmnt /var/lib/pacman')}")
        return found

    # The first switch and the boot after it flush per package, about 2000 points, most of them outside the move.
    spec = replay.Spec(replay.by_packages, fallback=False, extra=extra, sample=([("split-entry-copied", "split-binds-ready")], 10))
    with t.step("2: the uncut systems' baselines: the plain install and the laid-out system"):
        s.boot(s.overlay("before", s.base), s.pristine_vars, "the plain install")
        s.capture("before")
        s.boot(s.overlay("after", s.ran), s.end_vars, "the laid-out system")
        s.capture("after")
    s.control("before", s.overlay("before", s.base), s.pristine_vars, spec)
    s.control("after", s.overlay("after", s.ran), s.end_vars, spec)
    s.replay(spec)
    s.summary()
