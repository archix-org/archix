# The fallback UKI under Secure Boot boots the current deployment without loader/entries, also as a collected deployment's copy, then eject, killed once and rerun, leaves plain Arch with the deployment's packages.
from boot.common import bind, boot, current, diff, has_line, loader, ok, out, secure_boot, setup_secure_boot


def run(m, t):
    bind(m, t)
    with t.step("setup: Secure Boot and tree"):
        setup_secure_boot(install="tree")

    with t.step("fallback: only the current deployment's fallback UKI, signed; loader/entries deleted"):
        boot()
        t.require(secure_boot(), "Secure Boot should be enforcing once set up")
        c = current()
        uki = out("ls /efi/EFI/Linux | tr '\\n' ' '")
        t.check(out("ls /efi/EFI/Linux | grep '^archix-fallback-'").strip() == f"archix-fallback-{c}.efi", f"only deployment {c}'s fallback UKI should be on the ESP: {uki}")
        verify = out("sbctl verify 2>&1 | grep fallback")
        t.check(any(f"archix-fallback-{c}.efi" in line and "is signed" in line for line in verify.splitlines()), f"the fallback UKI should be signed: {verify}")
        m.succeed("rm -rf /efi/loader/entries && sync -f /efi")
        m.reboot()

    with t.step("9: without loader/entries the fallback UKI boots the current deployment"):
        selected, _, running = boot()
        c = current()
        t.check(selected == f"archix-fallback-{c}.efi", f"without loader/entries systemd-boot should start the fallback UKI of deployment {c}, not {selected}")
        t.check(running == c, f"the fallback UKI should boot deployment {c}, not {running}")
        t.check(ok("command -v tree"), f"deployment {c} should have tree")
        t.check(not ok("findmnt /usr/local") and not ok("[ -e /usr/local/bin/archix-vmtest-declared ]"), "the fallback UKI should leave /usr/local out")
        t.check(ok("grep -q 'fallback UKI' /run/archix/usr-local && grep -q 'left /usr/local unmounted' /var/lib/archix/notes"), f"the fallback UKI's boot should say why /usr/local is out: {out('cat /run/archix/usr-local /var/lib/archix/notes')}")
        m.succeed(f"cp /efi/EFI/Linux/archix-fallback-{c}.efi /efi/EFI/Linux/archix-fallback-999.efi")
        m.succeed("printf 'uki /EFI/Linux/archix-fallback-999.efi\\n' > /var/lib/archix/boot/entries/archix-fallback-999.efi && sync -f /efi")
        m.select_entry("archix-fallback-999.efi")
        m.reboot()

    with t.step("10: the fallback UKI of a collected deployment boots current; eject killed after copying"):
        selected, _, running = boot(match=False)
        c = current()
        t.check(selected == "archix-fallback-999.efi", f"systemd-boot should have started archix-fallback-999.efi, not {selected}")
        t.check(running == c, f"the fallback of a deployment that's gone should boot current, {c}, not {running}")
        t.check(ok(f"journalctl -b --no-pager | grep -q 'booting deployment {c} (current;'"), "deploy-mount should say it booted current instead")
        m.succeed("rm /efi/EFI/Linux/archix-fallback-999.efi /var/lib/archix/boot/entries/archix-fallback-999.efi")
        packages = out("pacman -Q")
        qkk = out("pacman -Qkk 2>&1")
        m.succeed("archix eject --noconfirm > /dev/console 2>&1 &")
        phase = "grep -qx 'phase copied' /var/lib/archix/eject 2> /dev/null"
        t.require(ok(f"for _ in $(seq 3000); do {phase} && break; sleep 0.1; done; {phase}", 600), f"the eject never finished copying the trees: {out('cat /var/lib/archix/eject 2>&1')}")
        m.execute("pkill -9 -f 'archix eject'; sync -f /efi; sync -f /")
        t.check(ok("[ -d /archix/deploy ]"), "the killed eject shouldn't have got as far as deleting /archix")
        phases = out("tr '\\n' ' ' < /var/lib/archix/eject")
        t.log(f"killed the eject after it copied the trees ({phases}); loader.conf: {loader()}")
        m.reboot()

    with t.step("11: eject finishes after the kill and the restart"):
        selected, _, _ = boot()
        t.log(f"after the killed eject systemd-boot started {selected}")
        m.succeed("archix eject --noconfirm", what="running eject again should finish it")
        t.check(not ok("[ -e /archix ]"), f"eject should delete /archix: {out('ls -A /archix 2>&1')}")
        t.check("archix" not in out("findmnt -no SOURCE /usr"), f"/usr should be the root's own now: {out('findmnt -no SOURCE /usr')}")
        t.check(has_line(loader(), "default arch-linux.efi"), f"eject should make the plain UKI the default: {loader()}")
        t.check(not ok("ls /efi/loader/entries/archix-* /efi/EFI/Linux/archix-fallback-* > /dev/null 2>&1"), "eject left archix's boot entries")
        t.check(not ok("[ -e /var/lib/archix/eject ]"), "the finished eject left its state")
        t.check(ok("command -v tree"), "the running system lost tree")
        t.check(ok("[ -x /usr/local/bin/archix-vmtest-declared ] && [ ! -e /var/usrlocal ]"), f"eject should make /var/usrlocal the root's own /usr/local: {out('ls -la /usr/local/bin /var/usrlocal 2>&1')}")
        m.reboot()

    with t.step("12: plain Arch boots with the deployment's packages; plain pacman works"):
        selected, _, running = boot()
        t.check(selected == "arch-linux.efi", f"plain Arch should boot through arch-linux.efi, not {selected}")
        t.check(running == "" and not ok("[ -e /archix ]"), "a plain boot mounts no deployment")
        t.check("archix" not in out("findmnt -no SOURCE /usr"), "/usr should be the root's own")
        t.check(ok("[ -x /usr/local/bin/archix-vmtest-declared ]") and not ok("findmnt /usr/local"), "plain Arch should keep /usr/local's files in its own /usr/local")
        t.check(secure_boot(), "Secure Boot should still be enforcing")
        now = out("pacman -Q")
        t.check(now == packages, f"plain Arch has other packages than the deployment had: {diff(packages, now)}")
        now = out("pacman -Qkk 2>&1")
        t.check(now == qkk, f"pacman -Qkk differs from the deployment's: {diff(qkk, now)}")
        t.check(ok("systemctl is-active -q archix-protected.service"), "the ProtectSystem=strict service should run on plain Arch")
        for unit in ("archix-boot", "archix-boot-complete", "archix-deploy-activate", "archix-flatpak"):
            t.check(not ok(f"[ -e /etc/systemd/system/{unit}.service ]"), f"eject should remove {unit}.service")
        m.succeed("pacman -Syu --noconfirm", what="plain pacman -Syu after the eject")
        m.succeed("pacman -S --noconfirm cowsay", what="plain pacman -S after the eject")
        t.check(ok("command -v cowsay"), "pacman -S cowsay should install it")
        m.shutdown()
