# A snapshot's boot entry edited on the ESP, once to pass another command line and once to start another image: without Secure Boot the ESP is unsigned, so each boot does what the edited entry says, in try mode, and the login notes say the entry isn't what archix wrote; the entry as archix recorded it boots with no note. On main an edited entry booted the current deployment instead; the in-place model boots snapshots only read-only, so the note is what's kept.
import re

from boot.common import bind, boot, bump, entries, entry, ok, out, snapshots, switch, trying

NOTES = "/var/lib/archix/notes"


def record_of(notes):
    found = re.search(r"isn't what archix wrote.*archix's copy is (\S+)\)", notes)
    return found.group(1) if found else None


def run(m, t):
    bind(m, t)
    with t.step("ee1: a snapshot's entry gets an options line"):
        boot()
        bump(1)
        switch()
        older = snapshots()[-1]
        name = f"archix-snapshot-{older}.conf"
        t.require(entry(name), f"snapshot {older} should have an entry: {entries()}")
        m.succeed(f"printf 'options %s archix.vmtest=edited\\n' \"$(cat /proc/cmdline)\" >> /efi/loader/entries/{entry(name)} && rm -f {NOTES} && sync -f /efi", what="adding an options line to the entry")
        m.select_entry(name)
        m.reboot()

    with t.step("ee2: the edited command line boots, in try mode, with a login note"):
        started = boot()
        t.check(started == name and (trying() or {}).get("snapshot") == older, f"{name} should start snapshot {older} in try mode: {started} {trying()}")
        t.check(ok("grep -qw archix.vmtest=edited /proc/cmdline"), f"without Secure Boot the options line is the command line: {out('cat /proc/cmdline')}")
        notes = out(f"cat {NOTES} 2>/dev/null; bash -l -c true 2>&1")
        record = record_of(notes)
        t.check(f"the boot entry {name}" in notes and record is not None, f"the login notes should say the entry isn't what archix wrote: {notes}")
        image = out(f"sed -nE 's#^(efi|uki) /#/efi/#p' {record}").strip() if record else ""
        m.reboot()
        boot()
        m.succeed(f"cp {record} /efi/loader/entries/{entry(name)} && cp {image} /efi/EFI/archix/vmtest-copy.efi"
                  f" && sed -i -E 's#^(efi|uki) .*#uki /EFI/archix/vmtest-copy.efi#' /efi/loader/entries/{entry(name)} && rm -f {NOTES} && sync -f /efi", what="pointing the entry at a copy of its image")
        m.select_entry(name)
        m.reboot()

    with t.step("ee3: an entry that starts another image boots, with a login note"):
        started = boot()
        t.check(started == name, f"systemd-boot should have started {name}, not {started}")
        notes = out(f"cat {NOTES} 2>/dev/null")
        t.check(f"the boot entry {name}" in notes and "isn't what archix wrote" in notes, f"the login notes should say so: {notes}")
        m.reboot()
        boot()
        m.succeed(f"cp {record} /efi/loader/entries/{entry(name)} && rm /efi/EFI/archix/vmtest-copy.efi && rm -f {NOTES} && sync -f /efi", what="restoring the entry")
        m.select_entry(name)
        m.reboot()

    with t.step("ee4: the entry as archix wrote it boots its snapshot with no note"):
        started = boot()
        t.check(started == name and (trying() or {}).get("snapshot") == older, f"the restored entry should boot snapshot {older}: {started} {trying()}")
        t.check("isn't what archix wrote" not in out(f"cat {NOTES} 2>/dev/null"), "no note for the restored entry")
