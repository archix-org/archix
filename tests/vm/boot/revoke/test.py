# revoke_unused under Secure Boot: keep = 1 drops a deployment with a UKI of its own, which goes into dbx beside an entry archix didn't write, and the firmware then refuses that UKI.
import re

from boot.common import HOST, LOADER, bind, boot, current, deployments, edit_config, ok, out, secure_boot, setup_secure_boot


def run(m, t):
    bind(m, t)
    with t.step("setup: Secure Boot"):
        setup_secure_boot()

    with t.step("7: keep = 1 and revoke_unused"):
        _, good, _ = boot()
        t.require(secure_boot(), "Secure Boot should be enforcing once set up")
        edit_config("'s|\"console=ttyS0\" }|\"console=ttyS0\", \"archix.revoketest\" }|'")
        m.succeed(f"{HOST} switch --noconfirm", what="switch to a deployment with a UKI of its own")
        dropped = current()
        own = out(f"ls /efi/loader/entries/archix-{dropped}.conf /efi/loader/entries/archix-{dropped}+*.conf 2> /dev/null | head -1").strip()
        revoked = out(f"sed -nE 's#^(efi|uki) /#/efi/#p' {own}").strip()
        m.succeed(f"cp {revoked} /var/tmp/revoked.efi", what=f"copying {revoked}")
        kek = "/var/lib/sbctl/keys/KEK"
        m.succeed("hash-to-efi-sig-list /usr/lib/systemd/boot/efi/linuxx64.efi.stub /var/tmp/foreign.esl > /dev/null", what="hashing the stand-in")
        m.succeed(f"sign-efi-sig-list -a -k {kek}/KEK.key -c {kek}/KEK.pem dbx /var/tmp/foreign.esl /var/tmp/foreign.auth > /dev/null", what="signing the stand-in")
        m.succeed("efi-updatevar -a -f /var/tmp/foreign.auth dbx", what="appending the stand-in to dbx")
        foreign = out("od -An -tx1 -j44 -N32 /var/tmp/foreign.esl | tr -d ' \\n'").strip()
        m.succeed("hash-to-efi-sig-list /var/tmp/revoked.efi /var/tmp/revoked.esl > /dev/null", what="hashing the copy")
        target = out("od -An -tx1 -j44 -N32 /var/tmp/revoked.esl | tr -d ' \\n'").strip()
        t.log("efivarfs before revoking: " + out("stat -f -c 'blocks=%b free=%f size=%S' /sys/firmware/efi/efivars"))
        edit_config("'s|, \"archix.revoketest\" }| }|; s|protected_entry = true }|protected_entry = true, revoke_unused = true },\\n  deployments = { keep = 1 }|'")
        m.succeed(f"{HOST} switch --noconfirm", what="switch with deployments.keep = 1")
        t.check(deployments() == f"{good} {current()} ", f"keep = 1 should leave only the booted and the current deployment: {deployments()}")
        generations = out("archix generations")
        t.check(any(re.match(rf"^ +{dropped} ", line) and "record" in line for line in generations.splitlines()), f"deployment {dropped}'s generation should stay as a record: {generations}")
        t.check(not ok(f"[ -e {revoked} ]"), f"dropping deployment {dropped} should take {revoked} off the ESP")
        dbx = out("efi-readvar -v dbx").lower()
        t.check(foreign != "" and foreign in dbx, f"the entry archix didn't write should still be in dbx: {dbx}")
        t.check(target != "" and target in dbx, f"dbx should list the dropped UKI {target}: {dbx}")
        t.log("efivarfs after revoking: " + out("stat -f -c 'blocks=%b free=%f size=%S' /sys/firmware/efi/efivars"))
        t.check("nothing to revoke" in out("archix secure-boot revoke --dry-run 2>&1"), "revoke should find nothing left")
        m.succeed("mkdir -p /efi/EFI/revoketest && cp /var/tmp/revoked.efi /efi/EFI/revoketest/revoked.efi")
        m.succeed("printf 'title revoked\\nefi /EFI/revoketest/revoked.efi\\n' > /efi/loader/entries/revoketest.conf")
        m.succeed(f"echo 'reboot-on-error yes' >> {LOADER}")
        before_revoked = current()
        m.select_entry("revoketest.conf")
        m.reboot()

    with t.step("8: the firmware refuses the revoked UKI"):
        selected, _, _ = boot()
        t.check(selected != "revoketest.conf", "the firmware should refuse the revoked UKI")
        t.check(current() == before_revoked, "the refused boot shouldn't change the current deployment")
        t.check(secure_boot(), "Secure Boot should be enforcing")
