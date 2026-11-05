# Write replay of an in-place `archix switch`, then keep (`archix rollback N`), its `--cancel`, the keep again and the boot into the kept snapshot: every flush and mark, and subsets of each flush's ESP writes, must boot by itself, never as a try, into a root holding one of the values the run gave /etc/archix-vmtest, with the account made before the run, pacman's database and files as uncut and no db.lck.
from boot.common import HOST, bind, bump, ok, out, snapshots, switch, trying
from powercut import cut
from replay import replay

USER = "kept1"


def value():
    return out("cat /etc/archix-vmtest").strip()


def run(m, t):
    bind(m, t)
    s = replay.Scenario(m, t, "keep")
    with t.step("0: a snapshot to keep, an account made after it, a boot that counts as good, the marks in place"):
        cut.common.boot()
        bump(1)
        switch("a switch whose snapshot holds 0")
        kept = snapshots()[-1]
        m.succeed(f"useradd -m {USER}", what="adding an account after the snapshot")
        replay.install(m)
        m.reboot()
        cut.settle()
        t.require(trying() is None and value() == "1", f"the boot before the run should be the current system holding 1: {trying()}, {value()}")
    s.freeze()

    def body():
        cut.settle()
        replay.mark(m, "switch-start")
        bump(2)
        switch("a switch whose snapshot holds 1")
        replay.mark(m, "keep-start")
        t.log(m.succeed(f"{HOST} rollback {kept} --noconfirm 2>&1", what=f"keeping snapshot {kept}"))
        replay.mark(m, "cancel-start")
        t.log(m.succeed(f"{HOST} rollback --cancel 2>&1", what="cancelling the keep"))
        replay.mark(m, "keep-again")
        t.log(m.succeed(f"{HOST} rollback {kept} --noconfirm 2>&1", what=f"keeping snapshot {kept} again"))
        replay.idle(m)
        m.reboot()
        cut.settle()
        t.require(trying() is None and value() == "0", f"the boot after the keep should be snapshot {kept} as @, holding 0: {trying()}, {value()}")
        m.wait_until_succeeds(f"grep -q '\"armed\": true' {cut.common.STATE}", timeout=300, what="the good boot arming the slot")
        replay.mark(m, "end")

    with t.step("1: the switch, the keep, its cancel, the keep again and the boot after it, logged"):
        s.record(body)

    def extra():
        found = []
        if value() not in ("0", "1", "2"):
            found.append(f"the root holds {value()!r}, none of the values the run gave it (0, 1, 2)")
        if not ok(f"getent passwd {USER}"):
            found.append(f"the account {USER}, made before the run, is gone")
        return found

    def identify(scenario, state, tried):
        """The kept root, holding 0, is the uncut system after the run; the root before the keep, holding 1 or 2, the one before it."""
        name = "after" if value() == "0" else "before"
        return name if tried is None and scenario.baselines[name]["Q"] == state["Q"] else None

    spec = replay.Spec(identify, fallback=False, extra=extra)
    with t.step("2: the uncut systems' baselines: before the run and after it"):
        s.boot(s.overlay("before", s.base), s.pristine_vars, "the system before the run")
        s.capture("before")
        s.boot(s.overlay("after", s.ran), s.end_vars, "the system after the run")
        s.capture("after")
    s.control("before", s.overlay("before", s.base), s.pristine_vars, spec)
    s.control("after", s.overlay("after", s.ran), s.end_vars, spec)
    s.replay(spec)
    s.summary()
