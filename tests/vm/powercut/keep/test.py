# Power cuts at each step of keep (`archix rollback N`) around the exchange, and of `--cancel`: after each, nobody touches the machine until a boot settles, which must be either the root before the keep or snapshot N as @, with archix's records, the menu and the accounts whole, and keep working again.
from boot.common import HOST, bind, boot, bump, out, snapshots, switch, trying
from powercut import cut

KEEP = ["keep-cloned", "keep-before-exchange", "keep-exchanged", "keep-replaced-moved", "keep-recorded", "keep-var-lib", "keep-menu"]
CANCEL = ["cancel-before-exchange", "cancel-exchanged"]


def value():
    return int(out("cat /etc/archix-vmtest").strip())


def judge(m, t, point, allowed, user):
    """Follows the boots after the cut, checks the root is one of allowed (values of /etc/archix-vmtest) with user in it, and records the verdict; returns the value."""
    try:
        boots, took = cut.settle()
    except Exception as e:
        cut.record(point, False, 0, 0, "-", None, [str(e)[:300]])
        t.require(False, f"no boot came up after the cut at {point}")
    found = cut.problems()
    now = value()
    if now not in allowed:
        found.append(f"the root holds {now}, neither the system before the keep nor the kept snapshot ({allowed})")
    if not cut.common.ok(f"getent passwd {user}"):
        found.append(f"the account {user}, made before the keep, is gone")
    if trying() is not None:
        found.append(f"the boot after the cut is a try boot: {trying()}")
    entry = out(f"tail -c +5 {cut.common.EFIVAR} | tr -d '\\000'").strip()
    cut.record(point, True, boots, took, entry, None, found, f"root holds {now} of {allowed}, snapshots {snapshots()}")
    t.check(not found, f"consistent after the cut at {point}: {found}")
    return now


def run(m, t):
    bind(m, t)
    with t.step("0: the base boots"):
        boot()

    def keep(point, i):
        now = value()
        bump(now + 1)
        switch()
        kept = snapshots()[-1]
        user = f"cut{i}"
        m.succeed(f"useradd -m {user}", what="adding an account after the snapshot")
        cut.baseline(f"before the keep cut at {point}")
        cut.arm(point)
        cut.background(f"{HOST} rollback {kept} --noconfirm", "/root/keep.log")
        reached = cut.reset_on_cut(timeout=900)
        cut.disarm()
        t.check(reached == f"{point}#1", f"the cut should be at {point}, not {reached}")
        booted = judge(m, t, reached, (now, now + 1), user)
        again = m.execute(f"{HOST} switch --noconfirm 2>&1", 900)
        t.check(again[0] == 0, f"a switch should work after the cut, with the root holding {booted}: {again[1][-600:]}")

    def cancel(point, i):
        now = value()
        bump(now + 1)
        switch()
        kept = snapshots()[-1]
        user = f"cancel{i}"
        m.succeed(f"useradd -m {user}", what="adding an account after the snapshot")
        m.succeed(f"{HOST} rollback {kept} --noconfirm", what="keeping a snapshot")
        cut.baseline(f"before the --cancel cut at {point}")
        cut.arm(point)
        cut.background(f"{HOST} rollback --cancel", "/root/cancel.log")
        reached = cut.reset_on_cut(timeout=900)
        cut.disarm()
        t.check(reached == f"{point}#1", f"the cut should be at {point}, not {reached}")
        judge(m, t, reached, (now, now + 1), user)
        again = m.execute(f"{HOST} switch --noconfirm 2>&1", 900)
        t.check(again[0] == 0, f"a switch should work after the cut: {again[1][-600:]}")

    for i, point in enumerate(KEEP):
        cut.point(f"{point}: cut in a keep", lambda point=point, i=i: keep(point, i))
    for i, point in enumerate(CANCEL):
        cut.point(f"{point}: cut in a keep's --cancel", lambda point=point, i=i: cancel(point, i))
    cut.summary()
