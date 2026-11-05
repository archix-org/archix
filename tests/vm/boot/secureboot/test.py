# Secure Boot: set up on a switched system, a boot blessed by systemd-bless-boot when archix bless meets a read-only ESP, the protected entry, the menu editor off, a switch whose images wouldn't be signed refused, and an edited entry's options line dropped by the signed image, with a login note.
import re

from boot.common import HOST, bind, boot, edit_config, entries, entry, loader, ok, out, snapshots, switch

SECURE = "'s|boot = { loader = \"systemd-boot\" }|boot = { loader = \"systemd-boot\", secure_boot = true, protected_entry = true }|'"
FAULT = "/var/lib/archix-vmtest/bless-fault"
# Around archix's own drop-in: with FAULT there the ESP is read-only for archix bless and writable again for systemd-bless-boot.
DROPINS = ("mkdir -p /etc/systemd/system/systemd-bless-boot.service.d"
           " && printf '[Service]\\nExecStartPre=/bin/sh -c \"if [ -e " + FAULT + " ]; then ls /efi > /dev/null && mount -o remount,ro /efi; fi\"\\n' > /etc/systemd/system/systemd-bless-boot.service.d/00-vmtest.conf"
           " && printf '[Service]\\nExecStartPre=/bin/sh -c \"if [ -e " + FAULT + " ]; then mount -o remount,rw /efi; fi\"\\n' > /etc/systemd/system/systemd-bless-boot.service.d/zz-vmtest.conf"
           " && systemctl daemon-reload")
PROTECTED = "archix-current-protected.conf"


def secure_boot():
    return ok("bootctl status 2>/dev/null | grep -q 'Secure Boot: enabled (user)'")


def run(m, t):
    bind(m, t)
    with t.step("sb1: Secure Boot set up, and archix bless set to meet a read-only ESP"):
        boot()
        m.succeed(f"{HOST} secure-boot setup 2>&1 | tail -5", 600, what="archix secure-boot setup")
        edit_config(SECURE)
        switch("the switch with Secure Boot")
        m.succeed(DROPINS, what="the bless-fault drop-ins")
        m.succeed(f"mkdir -p {FAULT.rsplit('/', 1)[0]} && touch {FAULT}")
        m.reboot()

    with t.step("sb2: enforcing, blessed by systemd-bless-boot past archix bless's failure, the protected entry, the editor off, an unsigned switch refused"):
        boot()
        t.check(secure_boot(), "Secure Boot should be enforcing")
        m.wait_until_succeeds("[ -f /efi/loader/entries/archix-current.conf ]", timeout=300, what="systemd-bless-boot should bless the counted entry")
        m.succeed(f"rm -f {FAULT}")
        bless = out("journalctl -b -u systemd-bless-boot.service --no-pager")
        t.check(re.search(r"Read-only file system", bless) is not None, f"archix bless should have failed on the read-only ESP: {bless[-800:]}")
        t.check(ok("systemctl is-active -q systemd-bless-boot.service"), f"systemd-bless-boot should succeed after archix bless failed: {bless[-800:]}")
        t.check(not ok("systemctl is-failed -q archix-boot-complete.service"), f"archix-boot-complete.service failed: {out('journalctl -b -u archix-boot-complete.service --no-pager | tail -5')}")
        t.check(not [e for e in entries() if e.startswith("archix-current") and "+" in e], f"no current entry should keep its counters: {entries()}")
        t.check(ok("archix secure-boot check"), "secure-boot check should pass")
        t.check(not ok("archix secure-boot check --anticheat"), "the anti-cheat check can't pass with a self-signed kernel")
        t.check(entry(PROTECTED) is not None, f"protected_entry should write {PROTECTED}: {entries()}")
        editor = [line for line in loader().splitlines() if line.split()[:1] == ["editor"]]
        t.check(editor == ["editor no"], f"under Secure Boot loader.conf should turn the menu editor off: {loader()}")
        image = out(f"sed -nE 's#^(efi|uki) /#/efi/#p' /efi/loader/entries/{entry('archix-current.conf')}").strip()
        t.check("is signed" in out(f"sbctl verify {image} 2>&1"), f"the current image should be signed: {out(f'sbctl verify {image} 2>&1')}")
        edit_config("'s|secure_boot = true|secure_boot = false|'")
        before = (out("archix generations"), snapshots())
        refused = m.execute(f"{HOST} install jq --noconfirm 2>&1")
        t.check(refused[0] != 0 and "wouldn't be signed" in refused[1], f"a switch whose images wouldn't be signed must be refused under Secure Boot: {refused[1][-600:]}")
        t.check((out("archix generations"), snapshots()) == before and not ok("command -v jq"), "the refused switch should change nothing")
        edit_config("'s|secure_boot = false|secure_boot = true|'")
        m.succeed(f"printf 'options %s archix.vmtest=edited\\n' \"$(cat /proc/cmdline)\" >> /efi/loader/entries/{entry(PROTECTED)} && sync -f /efi", what="adding an options line to the protected entry")
        m.select_entry(PROTECTED)
        m.reboot()

    with t.step("sb3: the signed image keeps its own command line over the edited entry's, and the login note says the entry changed"):
        started = boot()
        t.check(started == PROTECTED, f"systemd-boot should have started {PROTECTED}, not {started}")
        cmdline = out("cat /proc/cmdline")
        t.check("archix.vmtest=edited" not in cmdline and "lockdown=integrity" in cmdline, f"systemd-stub should drop the options line for the protected image's own command line: {cmdline}")
        notes = out("cat /var/lib/archix/notes")
        found = re.search(rf"the boot entry {re.escape(PROTECTED)}\S* isn't what archix wrote.*archix's copy is (\S+)\)", notes)
        t.check(found is not None, f"the login notes should say the entry was changed: {notes}")
        if found:
            m.succeed(f"cp {found.group(1)} /efi/loader/entries/{entry(PROTECTED)} && sync -f /efi", what="restoring the protected entry")
