# A unit in the shared /usr/local that fails every boot: the new deployment's counted tries fail, systemd-boot falls back to the previous one, which boots without /usr/local; the current one fails too while /usr/local is bound, and its fallback UKI boots without it; once the unit is gone from /var/usrlocal, the next boot binds /usr/local again.
import shlex

from boot.common import HOST, bind, boot, current, ok, out

UNITS = "/usr/local/lib/systemd/system"
# It fails in sysinit, before the driver's backdoor starts, so no failing boot passes for a booted one.
BROKEN = {
    "archix-vmtest-broken.service": "[Unit]\nDescription=a /usr/local unit that fails every boot\nDefaultDependencies=no\nAfter=local-fs.target\nBefore=sysinit.target\nFailureAction=reboot-force\n\n[Service]\nType=oneshot\nExecStart=/bin/sh -c 'cat /proc/sys/kernel/random/boot_id >> /var/lib/archix-vmtest/broken-boots; sync -f /var/lib/archix-vmtest/broken-boots; exit 1'\n",
    "sysinit.target.d/archix-vmtest-broken.conf": "[Unit]\nWants=archix-vmtest-broken.service\n",
    "boot-complete.target.d/archix-vmtest-broken.conf": "[Unit]\nRequires=archix-vmtest-broken.service\nAfter=archix-vmtest-broken.service\n",
}


def broken_boots():
    return len(out("cat /var/lib/archix-vmtest/broken-boots 2> /dev/null").split())


def run(m, t):
    bind(m, t)
    with t.step("ul1: a switch, then a unit in /usr/local that fails every boot"):
        _, good, _ = boot()
        m.succeed(f"{HOST} install tree --noconfirm", what="archix install tree")
        bad = current()
        t.require(ok(f"[ -f /efi/loader/entries/archix-{bad}+3.conf ]"), f"the new entry should count three boot tries: {out('ls /efi/loader/entries')}")
        m.succeed("mkdir -p /var/lib/archix-vmtest")
        for name, text in BROKEN.items():
            m.succeed(f"mkdir -p $(dirname {UNITS}/{name}) && printf %s {shlex.quote(text)} > {UNITS}/{name}")
        status = out(f"{HOST} status")
        t.check(f"{UNITS}/archix-vmtest-broken.service" in status and "no rollback undoes them" in status, f"archix status should warn about the boot-time files in /usr/local: {status}")
        verify = out("archix verify")
        t.check(f"{UNITS}/archix-vmtest-broken.service" in verify, f"archix verify should warn about the boot-time files in /usr/local: {verify}")
        m.reboot()

    with t.step("ul2: the counted tries fail; the fallback to the previous deployment leaves /usr/local out"):
        m.wait_for_shell(timeout=1500)
        selected, booted, _ = boot()
        t.check(booted == good, f"after the failed tries of deployment {bad}, systemd-boot should start {good}, not {selected}")
        t.check(ok(f"[ -f /efi/loader/entries/archix-{bad}+0-3.conf ]"), f"deployment {bad}'s entry should be marked bad: {out('ls /efi/loader/entries')}")
        t.check(broken_boots() >= 3, f"the unit in /usr/local should have failed each counted try: {broken_boots()} boots")
        t.check(not ok("findmnt /usr/local") and not ok(f"[ -e {UNITS}/archix-vmtest-broken.service ]"), "the fallback boot should leave /usr/local out")
        t.check(ok("grep -q 'older than the current' /run/archix/usr-local"), f"deploy-mount should say why: {out('cat /run/archix/usr-local')}")
        notes = out("cat /var/lib/archix/notes")
        t.check("left /usr/local unmounted" in notes and "/var/usrlocal" in notes, f"the login notes should say /usr/local is out and where it is: {notes}")
        t.check(current() == good, f"archix should make deployment {good} current after {bad} failed to boot")
        failures = broken_boots()
        m.select_entry(f"archix-{good}.conf")
        m.succeed(f"bootctl set-default archix-fallback-{good}.efi")
        m.reboot()

    with t.step("ul3: the current deployment fails with /usr/local too; its fallback UKI boots without it"):
        m.wait_for_shell(timeout=1500)
        selected, _, _ = boot()
        t.check(selected == f"archix-fallback-{good}.efi", f"after the failed boot of deployment {good}'s entry, systemd-boot should start its fallback UKI, not {selected}")
        t.check(broken_boots() == failures + 1, f"the boot of the current deployment {good} with /usr/local should have failed once: {broken_boots()} boots, {failures} before")
        t.check(not ok("findmnt /usr/local"), "the fallback UKI should leave /usr/local out")
        m.succeed("bootctl set-default ''")
        m.succeed("rm -r /var/usrlocal/lib/systemd/system/archix-vmtest-broken.service /var/usrlocal/lib/systemd/system/sysinit.target.d /var/usrlocal/lib/systemd/system/boot-complete.target.d")
        m.reboot()

    with t.step("ul4: with the unit gone from /var/usrlocal, the current deployment boots with /usr/local again"):
        selected, _, _ = boot()
        t.check(selected == f"archix-{good}.conf" and current() == good, f"deployment {good}'s own entry should boot, not {selected}")
        source = out("findmnt -no SOURCE /usr/local").strip()
        t.check(source.endswith("/var/usrlocal]"), f"/usr/local should be the shared /var/usrlocal again: {source}")
        t.check(broken_boots() == failures + 1, "no boot should fail once the unit is gone")
        t.check(not ok("grep -qs /usr/local /var/lib/archix/notes"), f"the boot with /usr/local should take its note away: {out('cat /var/lib/archix/notes')}")
        t.check("/usr/local/lib/systemd" not in out(f"{HOST} status"), "archix status should have nothing to warn about in /usr/local")
