# Secure Boot: set up on a laid-out system, a boot blessed by systemd-bless-boot when archix bless meets a read-only ESP, protected entries, and unsigned switches refused.
import re

from boot.common import HOST, SECURE_BOOT_CONFIG, STATE, bind, boot, current, deployments, edit_config, has_line, listing, loader, ok, out, secure_boot


def run(m, t):
    bind(m, t)
    with t.step("4b: Secure Boot set up"):
        boot()
        m.succeed("archix secure-boot setup", what="archix secure-boot setup")
        edit_config(SECURE_BOOT_CONFIG)
        m.succeed(f"{HOST} switch --noconfirm", what="switch with Secure Boot")
        m.succeed(f"mkdir -p {STATE} && touch {STATE}/bless-fault")
        m.reboot()

    with t.step("5: Secure Boot, blessing on a read-only ESP, unsigned switches refused"):
        _, booted, _ = boot()
        t.check(secure_boot(), "Secure Boot should be enforcing")
        t.check(not ok("systemctl is-failed -q archix-boot-complete.service"), f"archix-boot-complete.service failed: {out('journalctl -u archix-boot-complete.service -b --no-pager | tail -5')}")
        c = current()
        text = loader()
        t.check(has_line(text, f"default archix-{c}.conf") and not re.search(r"^preferred", text, re.M), f"once deployment {c} booted fine, it should be the default and nothing preferred: {text}")
        m.succeed(f"rm -f {STATE}/bless-fault")
        bless = out("journalctl -b -u systemd-bless-boot.service --no-pager")
        tail = "\n".join(bless.splitlines()[-8:])
        t.check(re.search(rf"renaming archix-{booted}\+[0-9-]+\.conf to archix-{booted}\.conf.*Read-only file system", bless),
                f"archix bless should have failed to rename the counted entry on the read-only ESP: {tail}")
        t.check(ok("systemctl is-active -q systemd-bless-boot.service"), f"systemd-bless-boot should succeed after archix bless failed: {tail}")
        entries = out("ls /efi/loader/entries | tr '\\n' ' '")
        t.check(ok(f"[ -f /efi/loader/entries/archix-{booted}.conf ]"), f"systemd-bless-boot should have blessed deployment {booted}'s entry: {entries}")
        t.check("+" not in entries, f"no entry should keep or gain boot counters: {entries}")
        t.check(current() == booted, f"deployment {booted} should stay current after archix bless failed, not {current()}")
        t.check(not ok("grep -qs \"couldn't count this boot\" /var/lib/archix/notes"), f"the login notes shouldn't say the boot wasn't counted: {out('cat /var/lib/archix/notes')}")
        t.check(ok("archix secure-boot check"), "secure-boot check should pass")
        t.check(not ok("archix secure-boot check --anticheat"), "the anti-cheat check can't pass with a self-signed kernel")
        t.check(ok(f"[ -f /efi/loader/entries/archix-{current()}-protected.conf ]"), "protected_entry should write a protected entry")
        edit_config("'s|secure_boot = true|secure_boot = false|'")
        before, kept = current(), deployments()
        t.require(not ok(f"{HOST} install jq --noconfirm"), "an unsigned switch must be refused under Secure Boot")
        t.check(current() == before, "the refused switch changed the current deployment")
        t.check(deployments() == kept, f"the refused switch left a deployment behind: {deployments()}")
        t.check(not ok("ls -d /archix/deploy/.staging-*"), f"the refused switch left its staging tree: {listing()}")
