# The missing-snapshot guard: an entry whose snapshot was deleted by hand boots the newest surviving snapshot whose entry hasn't failed, in try mode, and with none left the rescue entry; never an entry that failed.
from boot.common import bind, boot, bump, entries, entry, ok, out, snapshots, state, switch, trying


def run(m, t):
    bind(m, t)
    with t.step("1: two snapshots with entries, the newer one deleted by hand"):
        boot()
        bump(1)
        switch()
        older = snapshots()[-1]
        bump(2)
        switch()
        newer = snapshots()[-1]
        t.check(entry(f"archix-snapshot-{older}.conf") and entry(f"archix-snapshot-{newer}.conf"), f"both should have entries: {entries()}")
        m.succeed(f"btrfs -q subvolume delete /.archix/snapshots/{newer}/root && rm -rf /.archix/snapshots/{newer}", what="deleting a snapshot by hand")
        m.select_entry(f"archix-snapshot-{newer}.conf")
        m.reboot()

    with t.step("2: its entry boots the newest snapshot that survived, in try mode"):
        started = boot()
        t.check(started == f"archix-snapshot-{newer}.conf", f"systemd-boot should start the picked entry, not {started}")
        tried = trying() or {}
        t.check(tried.get("snapshot") == older and f"snapshot {newer}" in (tried.get("why") or ""), f"the initramfs should boot snapshot {older} instead, saying why: {tried}")
        t.check(ok(f"grep -q 'snapshot {newer}, which the boot entry named, is gone' /run/motd.d/archix.motd"), f"the notice should say why: {out('cat /run/motd.d/archix.motd')}")
        # Every survivor with an entry, the base's own snapshot too: its first switch stops for the layout's reboot, and the switch after takes one.
        survivors = [n for n in snapshots() if n != newer and entry(f"archix-snapshot-{n}.conf")]
        for n in survivors:
            name = entry(f"archix-snapshot-{n}.conf")
            m.succeed(f"mv /efi/loader/entries/{name} /efi/loader/entries/archix-snapshot-{n}+0-1.conf", what=f"marking snapshot {n}'s entry as failed")
        m.succeed("archix boot-check > /dev/null", what="recording the failed entries")
        t.check(all(n in state().get("failed", []) for n in survivors), f"archix should record every failed entry {survivors}: {state()}")
        m.select_entry(f"archix-snapshot-{newer}.conf")
        m.reboot()

    with t.step("3: with no snapshot left whose entry hasn't failed, the rescue entry"):
        m.wait_for_console_text(r"archix rescue", timeout=600)
        m.crash()
        started = boot()
        t.check(started == "archix-current.conf", f"after the rescue, the current entry should start again, not {started}")
