# An old snapshot's initramfs reading records a newer archix wrote: the snapshot records and the boot state get a higher format and fields this archix doesn't know, as a later release might add, and the snapshot's entry, whose image this build made, still boots it in try mode and the current entry the current system.
from boot.common import STATE, bind, boot, bump, entries, entry, ok, out, snapshots, switch, trying

NEWER = "sed -i -E '0,/\"format\": [0-9]+,/s//\"format\": 99,\\n  \"from_a_newer_archix\": {\"kind\": \"unknown\", \"list\": [1, 2]},/'"


def run(m, t):
    bind(m, t)
    with t.step("1: two snapshots, then every record rewritten as a newer archix might"):
        boot()
        bump(1)
        switch()
        older = snapshots()[-1]
        bump(2)
        switch()
        m.succeed(f"for f in /.archix/snapshots/*/record.json {STATE}; do {NEWER} $f; grep -q '\"format\": 99' $f; done", what="rewriting the records")
        t.log(out(f"cat /.archix/snapshots/{older}/record.json"))
        m.select_entry(f"archix-snapshot-{older}.conf")
        m.reboot()

    with t.step("2: the snapshot's entry boots it from the newer records"):
        started = boot()
        t.check(started == f"archix-snapshot-{older}.conf" and (trying() or {}).get("snapshot") == older, f"snapshot {older} in try mode: {started} {trying()}")
        t.check(out("cat /etc/archix-vmtest").strip() == "0", "the snapshot's own files")
        t.check(ok("archix snapshot list > /dev/null"), f"listing snapshots reads them: {out('archix snapshot list 2>&1')}")
        m.reboot()

    with t.step("3: the current entry boots the current system"):
        started = boot()
        t.check(started == "archix-current.conf" and trying() is None, f"the current system: {started} {trying()}")
        t.check(out("cat /etc/archix-vmtest").strip() == "2", "the current system's files")
        t.log(f"entries {entries()}")
