# revoke_unused under Secure Boot: an image a snapshot's entry booted leaves the ESP with that snapshot, the next switch adds it to dbx beside an entry archix didn't write, and the firmware then refuses it.
from boot.common import HOST, LOADER, bind, boot, bump, edit_config, entry, ok, out, snapshots, switch

SECURE = "'s|boot = { loader = \"systemd-boot\" }|boot = { loader = \"systemd-boot\", secure_boot = true }|'"
KEK = "/var/lib/sbctl/keys/KEK"


def secure_boot():
    return ok("bootctl status 2>/dev/null | grep -q 'Secure Boot: enabled (user)'")


def image(name):
    file = entry(name)
    return out(f"sed -nE 's#^(efi|uki) /#/efi/#p' /efi/loader/entries/{file}").strip() if file else ""


def run(m, t):
    bind(m, t)
    with t.step("setup: Secure Boot"):
        boot()
        m.succeed(f"{HOST} secure-boot setup 2>&1 | tail -5", 600, what="archix secure-boot setup")
        edit_config(SECURE)
        switch("the switch with Secure Boot")
        m.reboot()

    with t.step("r1: a snapshot's own image goes with it and the next switch revokes it"):
        boot()
        t.require(secure_boot(), "Secure Boot should be enforcing once set up")
        edit_config("'s|\"console=ttyS0\" }|\"console=ttyS0\", \"archix.revoketest\" }|'")
        switch("a switch to an image of its own")
        own = image("archix-current.conf")
        m.succeed(f"cp {own} /var/tmp/revoked.efi")
        m.succeed("hash-to-efi-sig-list /usr/lib/systemd/boot/efi/linuxx64.efi.stub /var/tmp/foreign.esl > /dev/null"
                  f" && sign-efi-sig-list -a -k {KEK}/KEK.key -c {KEK}/KEK.pem dbx /var/tmp/foreign.esl /var/tmp/foreign.auth > /dev/null"
                  " && efi-updatevar -a -f /var/tmp/foreign.auth dbx", what="a dbx entry archix didn't write")
        foreign = out("od -An -tx1 -j44 -N32 /var/tmp/foreign.esl | tr -d ' \\n'").strip()
        m.succeed("hash-to-efi-sig-list /var/tmp/revoked.efi /var/tmp/revoked.esl > /dev/null")
        target = out("od -An -tx1 -j44 -N32 /var/tmp/revoked.esl | tr -d ' \\n'").strip()
        edit_config("'s|, \"archix.revoketest\" }| }|; s|secure_boot = true }|secure_boot = true, revoke_unused = true }|'")
        switch("the switch back, with revoke_unused")
        holding = [n for n in snapshots() if image(f"archix-snapshot-{n}.conf") == own]
        t.log(f"snapshots booting {own}: {holding}")
        for number in holding:
            m.succeed(f"archix snapshot delete {number}", what=f"deleting snapshot {number}")
        bump(1)
        said = m.succeed(f"set -o pipefail; {HOST} switch --noconfirm 2>&1 | tail -20", what="a switch after the image's last entry went")
        t.log(said)
        t.check(not ok(f"[ -e {own} ]"), f"{own} should be off the ESP")
        dbx = out("efi-readvar -v dbx").lower()
        t.check(foreign != "" and foreign in dbx, f"the entry archix didn't write should still be in dbx: {dbx[-600:]}")
        t.check(target != "" and target in dbx, f"dbx should list the dropped image {target}: {dbx[-600:]}")
        t.check("nothing to revoke" in out("archix secure-boot revoke --dry-run 2>&1"), "revoke should find nothing left")
        m.succeed("mkdir -p /efi/EFI/revoketest && cp /var/tmp/revoked.efi /efi/EFI/revoketest/revoked.efi"
                  " && printf 'title revoked\\nefi /EFI/revoketest/revoked.efi\\n' > /efi/loader/entries/revoketest.conf"
                  f" && echo 'reboot-on-error yes' >> {LOADER} && sync -f /efi")
        m.select_entry("revoketest.conf")
        m.reboot()

    with t.step("r2: the firmware refuses the revoked image"):
        started = boot()
        t.check(started != "revoketest.conf", "the firmware should refuse the revoked image")
        t.check(secure_boot(), "Secure Boot should be enforcing")
