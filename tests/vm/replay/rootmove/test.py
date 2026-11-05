# Write replay of the move of a root on btrfs's top level into @, which `archix snapshot setup --move-root` schedules and the initramfs makes at the next boot: every flush, and subsets of each flush's ESP writes, must boot by itself into the root in @ with every file, the subvolume nested in it moved along and the one beside it at the top level untouched.
from boot.common import bind, edit_config, ok, out, switch
from powercut import cut
from replay import replay

FILES = "sha256sum /usr/bin/bash /usr/lib/libc.so.6 /etc/passwd /etc/archix-vmtest /root/keep/big /usr/lib/modules/*/vmlinuz | sha256sum | cut -c1-16"


def run(m, t):
    bind(m, t)
    s = replay.Scenario(m, t, "rootmove")
    with t.step("0: a root on the top level with a nested subvolume, one beside it and files, the move scheduled, the marks in place"):
        s.start_from("/work/top.img")
        cut.common.boot()
        t.require(out("findmnt -no FSROOT /").strip() == "/", f"the image's root should be the top level: {out('findmnt /')}")
        edit_config("'s|rollback = { enable = true }|rollback = { enable = false }|'")
        switch("a switch that installs archix's boot, rollbacks off")
        m.succeed("btrfs -q subvolume create /srv/nested && echo nested > /srv/nested/file && btrfs -q subvolume create /beside && echo beside > /beside/file && mkdir -p /root/keep && echo kept > /root/keep/file && head -c 256M /dev/urandom > /root/keep/big", what="a nested subvolume and files")
        m.succeed(f"{FILES} > /root/files-before")
        edit_config("'s|rollback = { enable = false }|rollback = { enable = true }|'")
        t.log(m.succeed("archix snapshot setup --move-root 2>&1", what="scheduling the move"))
        replay.install(m)
    s.freeze()

    def body():
        cut.settle()
        t.require(out("findmnt -no FSROOT /").strip() == "/@", f"the boot should have moved the root into @: {out('findmnt /')}")
        replay.mark(m, "end")

    with t.step("1: the boot that moves the root into @, logged"):
        s.record(body)

    def extra():
        found = []
        if out("findmnt -no FSROOT /").strip() != "/@":
            found.append(f"the root isn't @: {out('findmnt /')}")
        if out(FILES).strip() != out("cat /root/files-before").strip():
            found.append("the root's files differ from before the move")
        if out("cat /srv/nested/file").strip() != "nested" or not ok("btrfs subvolume show /srv/nested > /dev/null"):
            found.append(f"the nested subvolume didn't come along: {out('ls -la /srv; btrfs subvolume list /')}")
        if out("cat /beside/file").strip() != "beside":
            found.append(f"the subvolume beside the root moved or isn't mounted: {out('findmnt /beside; btrfs subvolume list /')}")
        if out("cat /root/keep/file").strip() != "kept":
            found.append("a file in /root is gone")
        return found

    spec = replay.Spec(replay.by_packages, fallback=False, extra=extra)
    with t.step("2: the uncut systems' baselines: a boot of the scheduled move, and the moved system"):
        s.boot(s.overlay("before", s.base), s.pristine_vars, "the scheduled move")
        s.capture("before")
        s.boot(s.overlay("after", s.ran), s.end_vars, "the moved system")
        s.capture("after")
    s.control("before", s.overlay("before", s.base), s.pristine_vars, spec)
    s.control("after", s.overlay("after", s.ran), s.end_vars, spec)
    s.replay(spec)
    s.summary()
