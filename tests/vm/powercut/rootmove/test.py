# A root on btrfs's top level (subvolid 5): rollbacks refused until `archix snapshot setup --move-root` moves it into @ in the initramfs at the next boot, with power cuts at each step of that move. Each cut starts from that image; nobody touches the machine until a boot settles, which must be the root in @ with every file, the subvolume nested in it moved along and the top-level subvolume beside it untouched; then rollbacks are set up and boot.
from boot.common import HOST, bind, boot, edit_config, out, switch
from powercut import cut

POINTS = ["move-root-snapshotted", "move-root-fstab", "move-root-nested-moved", "move-root-cleaned", None]
FILES = "sha256sum /usr/bin/bash /usr/lib/libc.so.6 /etc/passwd /etc/archix-vmtest /root/keep/big /usr/lib/modules/*/vmlinuz | sha256sum | cut -c1-16"


def run(m, t):
    bind(m, t)

    def move(point):
        cut.fresh("/work/top.img")
        boot()
        t.check(out("findmnt -no FSROOT /").strip() == "/", f"the image's root should be the top level: {out('findmnt /')}")
        edit_config("'s|rollback = { enable = true }|rollback = { enable = false }|'")
        switch("a switch that installs archix's boot, rollbacks off")
        status, said = m.execute(f"sed -i 's|rollback = {{ enable = false }}|rollback = {{ enable = true }}|' /etc/archix/init.lua && {HOST} switch --noconfirm 2>&1")
        t.check(status != 0 and "top level" in said, f"rollbacks on the top level should be refused, naming why: {status} {said[-400:]}")
        m.succeed("btrfs -q subvolume create /srv/nested && echo nested > /srv/nested/file && btrfs -q subvolume create /beside && echo beside > /beside/file && mkdir -p /root/keep && echo kept > /root/keep/file && head -c 256M /dev/urandom > /root/keep/big", what="a nested subvolume and files")
        m.succeed("btrfs subvolume list / > /root/subvolumes-before")
        before = out(FILES).strip()
        said = m.succeed("archix snapshot setup --move-root 2>&1", what="scheduling the move")
        t.log(said)
        cut.baseline(f"before the cut at {point or 'none'}")
        if point:
            cut.arm(point)
        m.reboot()
        reached = cut.reset_on_cut(timeout=900) if point else "none"
        try:
            boots, took = cut.settle()
        except Exception as e:
            cut.record(reached, False, 0, 0, "-", None, [str(e)[:300]])
            t.require(False, f"no boot came up after the cut at {reached}")
        found = cut.problems(packages=True)
        if out("findmnt -no FSROOT /").strip() != "/@":
            found.append(f"the root isn't @: {out('findmnt /')}")
        after = out(FILES).strip()
        if after != before:
            found.append(f"the root's files differ from before the move ({before} -> {after})")
        if out("cat /srv/nested/file").strip() != "nested" or not cut.common.ok("btrfs subvolume show /srv/nested > /dev/null"):
            found.append(f"the nested subvolume didn't come along: {out('ls -la /srv; btrfs subvolume list /')}")
        if out("cat /beside/file").strip() != "beside" or "beside" not in out("btrfs subvolume list / | awk '{ print $NF }'").split():
            found.append(f"the subvolume beside the root moved or isn't mounted: {out('findmnt /beside; btrfs subvolume list /')}")
        if out("cat /root/keep/file").strip() != "kept":
            found.append("a file in /root is gone")
        t.log("fstab after the move: " + out("cat /etc/fstab; findmnt /beside"))
        listed = out("mount -o subvolid=5 $(findmnt -no SOURCE / | sed 's/\\[.*//') /mnt && ls -A /mnt; umount /mnt")
        t.log(f"the top level after the move: {listed}")
        if "@" not in listed.split():
            found.append(f"no @ at the top level: {listed}")
        cut.record(reached, True, boots, took, cut.common.selected(), None, found, f"subvolumes now: {out('btrfs subvolume list / | tr \"\\n\" \";\"')}")
        t.check(not found, f"consistent after the cut at {reached}: {found}")
        status, said = m.execute(f"{HOST} switch --noconfirm 2>&1")
        t.check(status != 0 and "/beside" in said and "file_systems" in said, f"the first switch should ask for the fstab line the move added for /beside to be declared: {status} {said[-600:]}")
        device = out("findmnt -no UUID /beside").strip()
        edit_config(f"'s|^return {{|return {{ file_systems = {{ [\"/beside\"] = {{ device = \"UUID={device}\", fs_type = \"btrfs\", options = {{ \"subvol=/beside\" }}, required = true }} }},|'")
        switch("the switch after the move with /beside declared, which lays the root out for rollbacks")
        m.reboot()
        boot()
        t.check(out("findmnt -no FSROOT /var/lib").strip() == "/@varlib", f"/var/lib should be @varlib after the next boot: {out('findmnt /var/lib')}")
        t.check(out("findmnt -no FSROOT /.archix").strip() == "/@archix" and cut.common.ok("archix snapshot list"), f"rollbacks should be set up now: {out('findmnt /.archix; archix snapshot list 2>&1')}")
        t.check(out("cat /beside/file").strip() == "beside", f"the subvolume beside the root stays mounted after switches: {out('cat /etc/fstab; findmnt /beside')}")

    for point in POINTS:
        cut.point(f"{point or 'control'}: cut in the move of the root into @", lambda point=point: move(point))
    cut.summary()
