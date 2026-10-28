# A unit in the shared /usr/local that fails every boot: the new deployment's counted tries fail and the fallback to the previous one, which leaves /usr/local out, holds it out of the plain boots after, though that deployment is current; a release that fails comes back without it, the quarantine brings it back; then the unit breaks a blessed deployment's boot, and systemd-boot's menu, brought up with Space as a user does, starts the fallback UKI, which holds it out too.
import json
import shlex

from boot.common import HOST, bind, boot, current, ok, out

UNITS = "/usr/local/lib/systemd/system"
HOLD = "/var/lib/archix/boot/usr-local-hold"
QUARANTINE = "/var/usrlocal.quarantine"
# It fails in sysinit, before the driver's backdoor starts, so no failing boot passes for a booted one.
BROKEN = {
    "archix-vmtest-broken.service": "[Unit]\nDescription=a /usr/local unit that fails every boot\nDefaultDependencies=no\nAfter=local-fs.target\nBefore=sysinit.target\nFailureAction=reboot-force\n\n[Service]\nType=oneshot\nExecStart=/bin/sh -c 'cat /proc/sys/kernel/random/boot_id >> /var/lib/archix-vmtest/broken-boots; sync -f /var/lib/archix-vmtest/broken-boots; exit 1'\n",
    "sysinit.target.d/archix-vmtest-broken.conf": "[Unit]\nWants=archix-vmtest-broken.service\n",
    "boot-complete.target.d/archix-vmtest-broken.conf": "[Unit]\nRequires=archix-vmtest-broken.service\nAfter=archix-vmtest-broken.service\n",
}


def broken_boots():
    return len(out("cat /var/lib/archix-vmtest/broken-boots 2> /dev/null").split())


def hold():
    return out(f"cut -d' ' -f1 {HOLD} 2> /dev/null").strip()


def held(why):
    return not ok("findmnt /usr/local") and ok("grep -q '^held ' /run/archix/usr-local") and why in out("cat /run/archix/usr-local")


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

    with t.step("ul2: the counted tries fail; the fallback to the previous deployment leaves /usr/local out and holds it out"):
        m.wait_for_shell(timeout=1500)
        selected, booted, _ = boot()
        t.check(booted == good, f"after the failed tries of deployment {bad}, systemd-boot should start {good}, not {selected}")
        t.check(ok(f"[ -f /efi/loader/entries/archix-{bad}+0-3.conf ]"), f"deployment {bad}'s entry should be marked bad: {out('ls /efi/loader/entries')}")
        t.check(broken_boots() >= 3, f"the unit in /usr/local should have failed each counted try: {broken_boots()} boots")
        t.check(not ok("findmnt /usr/local") and not ok(f"[ -e {UNITS}/archix-vmtest-broken.service ]"), "the fallback boot should leave /usr/local out")
        t.check(ok("grep -q 'older than the current' /run/archix/usr-local"), f"deploy-mount should say why: {out('cat /run/archix/usr-local')}")
        notes = out("cat /var/lib/archix/notes")
        t.check("left /usr/local unmounted" in notes and "/var/usrlocal" in notes, f"the login notes should say /usr/local is out and where it is: {notes}")
        t.check(f"{UNITS}/archix-vmtest-broken.service" in notes and "sudo archix usr-local quarantine" in notes and "--boot-loader-menu" in notes, f"the login note should name the unit, the command that clears it and how to bring up the menu: {notes}")
        t.check(current() == good, f"archix should make deployment {good} current after {bad} failed to boot")
        t.check(hold() == "held", f"the fallback boot should hold /usr/local out of the boots after: {out(f'cat {HOLD}')}")
        failures = broken_boots()
        m.reboot()

    with t.step("ul3: plain reboots of the current deployment keep /usr/local out"):
        for second in (False, True):
            selected, _, _ = boot()
            t.check(selected == f"archix-{good}.conf" and current() == good, f"a plain reboot should start deployment {good}'s own entry, not {selected}")
            t.check(broken_boots() == failures, f"no plain reboot should run the unit in /usr/local: {broken_boots()} boots, {failures} before")
            t.check(held("held out since"), f"deploy-mount should leave /usr/local out as held: {out('cat /run/archix/usr-local')}")
            if not second:
                m.reboot()
        status = out(f"{HOST} status")
        t.check("/usr/local: every boot leaves it out" in status, f"archix status should say /usr/local is held out: {status}")
        m.succeed("archix usr-local release", what="archix usr-local release")
        t.check(hold() == "trial", f"a release should let the next boot try /usr/local: {out(f'cat {HOLD}')}")
        m.reboot()

    with t.step("ul4: the released boot fails with the unit; the boot after leaves /usr/local out again"):
        m.wait_for_shell(timeout=600)
        selected, _, _ = boot()
        t.check(selected == f"archix-{good}.conf", f"the boot after the failed try should start deployment {good}'s entry, not {selected}")
        t.check(broken_boots() == failures + 1, f"the released boot should have bound /usr/local and failed once: {broken_boots()} boots, {failures} before")
        t.check(held("didn't reach multi-user.target"), f"the boot after a failed try should leave /usr/local out: {out('cat /run/archix/usr-local')}")
        t.check(hold() == "failed", f"the hold should say the try failed: {out(f'cat {HOLD}')}")
        said = m.succeed("archix usr-local quarantine", what="archix usr-local quarantine")
        t.check("archix-vmtest-broken.service" in said and ok(f"ls {QUARANTINE}/*/lib/systemd/system/archix-vmtest-broken.service"), f"the quarantine should move the unit out of /var/usrlocal: {said}")
        t.check(not ok("[ -e /var/usrlocal/lib/systemd/system/archix-vmtest-broken.service ]"), "the quarantined unit should be gone from /var/usrlocal")
        m.reboot()

    with t.step("ul5: with the unit quarantined, the current deployment binds /usr/local again"):
        selected, _, _ = boot()
        t.check(selected == f"archix-{good}.conf" and current() == good, f"deployment {good}'s own entry should boot, not {selected}")
        source = out("findmnt -no SOURCE /usr/local").strip()
        t.check(source.endswith("/var/usrlocal]"), f"/usr/local should be the shared /var/usrlocal again: {source}")
        t.check(broken_boots() == failures + 1, "no boot should fail once the unit is gone")
        m.wait_until_succeeds(f"[ ! -e {HOLD} ]", timeout=60, what="the boot that reached multi-user.target should lift the hold")
        t.check(not ok("grep -qs /usr/local /var/lib/archix/notes"), f"the boot with /usr/local should take its note away: {out('cat /var/lib/archix/notes')}")
        status = out(f"{HOST} status")
        t.check("/usr/local/lib/systemd" not in status and "/usr/local:" not in status, f"archix status should have nothing to say about /usr/local: {status}")
        entries = json.loads(out("bootctl list --json=short"))
        titles = [entry["showTitle"] for entry in entries]
        fallback = next((entry["showTitle"] for entry in entries if entry["id"] == f"archix-fallback-{good}.efi"), None)
        t.log(f"the menu: {titles}")
        t.require(fallback is not None, f"bootctl should list deployment {good}'s fallback UKI: {titles}")
        m.succeed(f"mv {QUARANTINE}/*/lib/systemd/system/* {UNITS}/")
        t.require(ok("[ -f /var/usrlocal/lib/systemd/system/archix-vmtest-broken.service ]"), "the unit should be back in /var/usrlocal")
        m.reboot()

    with t.step("ul6: the unit breaks the blessed deployment's boot; the menu brought up with Space starts its fallback UKI"):
        m.wait_for_console_text("Forcibly rebooting", timeout=300)
        m.pick_from_menu(titles, fallback, since=m.serial.stat().st_size)
        selected, _, _ = boot()
        t.check(selected == f"archix-fallback-{good}.efi", f"the menu should have started deployment {good}'s fallback UKI, not {selected}")
        t.check(broken_boots() == failures + 2, f"the boot of the current deployment with the unit back should have failed once: {broken_boots()} boots, {failures + 1} before")
        t.check(not ok("findmnt /usr/local") and ok("grep -q 'fallback UKI' /run/archix/usr-local"), f"the fallback UKI should leave /usr/local out: {out('cat /run/archix/usr-local')}")
        t.check(hold() == "held", f"the fallback UKI's boot should hold /usr/local out of the boots after: {out(f'cat {HOLD}')}")
        m.reboot()

    with t.step("ul7: the plain reboot after the menu's fallback keeps /usr/local out; the quarantine brings it back"):
        selected, _, _ = boot()
        t.check(selected == f"archix-{good}.conf", f"a plain reboot should start deployment {good}'s own entry, not {selected}")
        t.check(broken_boots() == failures + 2, "the plain reboot after the menu's fallback should not run the unit")
        t.check(held("held out since"), f"deploy-mount should leave /usr/local out as held after the fallback UKI: {out('cat /run/archix/usr-local')}")
        m.succeed("archix usr-local quarantine", what="the second archix usr-local quarantine")
        m.reboot()
        boot()
        t.check(ok("findmnt /usr/local") and broken_boots() == failures + 2, f"the boot after the second quarantine should bind /usr/local: {out('cat /run/archix/usr-local')}")
        m.wait_until_succeeds(f"[ ! -e {HOLD} ]", timeout=60, what="the boot after the second quarantine should lift the hold")
