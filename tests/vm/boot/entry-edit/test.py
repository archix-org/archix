# An older deployment's boot entry edited on the ESP, once to pass another command line and once to start another UKI: each boot mounts the current deployment instead and says why in the login notes; the entry as archix wrote it boots its deployment again.
from boot.common import bind, boot, current, ok, out

RECORDS = "/var/lib/archix/boot/entries"
NOTES = "/var/lib/archix/notes"


def entry_file(n):
    return out(f"ls /efi/loader/entries/archix-{n}.conf /efi/loader/entries/archix-{n}+*.conf 2> /dev/null | head -1").strip()


def run(m, t):
    bind(m, t)
    with t.step("ee1: an older deployment's entry gets an options line"):
        boot()
        c = current()
        older = next((n for n in out("ls /archix/deploy | sort -rn").split() if n != c and entry_file(n)), "")
        t.require(older != "", f"a deployment older than {c} should have an entry: {out('ls /efi/loader/entries')}")
        entry = f"archix-{older}.conf"
        m.succeed(f"printf 'options %s archix.vmtest=edited\\n' \"$(cat /proc/cmdline)\" >> {entry_file(older)} && sync -f /efi", what="adding an options line to the older entry")
        m.select_entry(entry)
        m.reboot()

    with t.step("ee2: the edited command line boots the current deployment, with a login note"):
        selected, _, running = boot(match=False)
        t.check(selected == entry, f"systemd-boot should have started {entry}, not {selected}")
        t.check(ok("grep -qw archix.vmtest=edited /proc/cmdline"), f"without Secure Boot the options line should be the command line: {out('cat /proc/cmdline')}")
        t.check(running == c, f"the edited entry should boot the current deployment {c}, not {running}")
        t.check(ok("grep -q \"isn't the one archix built\" /run/archix/entry-refused"), f"deploy-mount should record why: {out('cat /run/archix/entry-refused 2>&1')}")
        note = f"systemd-boot started the boot entry {entry}, but archix booted the current deployment {c} instead"
        login = out("bash -l -c true 2>&1")
        t.check(note in login and "archix.vmtest=edited" in login, f"the login notes should say so: {login}")
        t.check(current() == c, f"deployment {c} should stay current: {current()}")
        m.succeed(f"cp {RECORDS}/{entry} {entry_file(older)} && uki=$(sed -n 's|^efi /|/efi/|p; s|^uki /|/efi/|p' {RECORDS}/{entry}) && cp $uki /efi/EFI/archix/vmtest-copy.efi", what="restoring the entry and copying its UKI")
        m.succeed(f"sed -i -E 's#^(efi|uki) .*#efi /EFI/archix/vmtest-copy.efi#' {entry_file(older)} && rm -f {NOTES} && sync -f /efi", what="pointing the entry at the copy")
        m.select_entry(entry)
        m.reboot()

    with t.step("ee3: an entry that starts another UKI boots the current deployment, with a login note"):
        selected, _, running = boot(match=False)
        t.check(selected == entry, f"systemd-boot should have started {entry}, not {selected}")
        t.check(running == c, f"the entry pointed at another UKI should boot the current deployment {c}, not {running}")
        why = "it started /EFI/archix/vmtest-copy.efi, but archix wrote it to start /EFI/archix/"
        notes = out(f"cat {NOTES}")
        t.check(f"{note}: {why}" in notes, f"the login notes should say which UKI it started: {notes}")
        m.succeed(f"cp {RECORDS}/{entry} {entry_file(older)} && rm /efi/EFI/archix/vmtest-copy.efi && sync -f /efi", what="restoring the entry")
        m.select_entry(entry)
        m.reboot()

    with t.step("ee4: the entry as archix wrote it boots its deployment"):
        _, _, running = boot()
        t.check(running == older, f"the restored entry should boot deployment {older}, not {running}")
        t.check(not ok("[ -e /run/archix/entry-refused ]"), f"nothing should be refused: {out('cat /run/archix/entry-refused 2>&1')}")
        t.check(current() == older, f"booting deployment {older}'s own entry should make it current, not {current()}")
