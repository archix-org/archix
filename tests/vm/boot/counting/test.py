# Boot counting: a deployment whose kernel panics gets three tries, then systemd-boot falls back to the last good one and archix makes it current again.
import re

from boot.common import HOST, bind, boot, current, edit_config, has_line, loader, ok, out


def run(m, t):
    bind(m, t)
    with t.step("6: a kernel that panics gets three tries"):
        _, booted, _ = boot()
        good = booted
        edit_config("'s|\"console=ttyS0\" }|\"console=ttyS0\", \"init=/usr/bin/false\", \"panic=3\" }|'")
        m.succeed(f"{HOST} switch --noconfirm", what="switch to a deployment that can't boot")
        bad = current()
        t.check(ok(f"[ -f /efi/loader/entries/archix-{bad}+3.conf ]"), f"the new entry should count three boot tries: {out('ls /efi/loader/entries')}")
        text = loader()
        t.check(has_line(text, f"preferred archix-{bad}.conf") and has_line(text, f"default archix-{good}.conf"), f"loader.conf should prefer deployment {bad} and fall back to {good}: {text}")
        m.reboot()

    with t.step("7: systemd-boot falls back after three failed boots"):
        m.wait_for_shell(timeout=1500)
        _, booted, _ = boot()
        t.log(out("ls /efi/loader/entries"))
        t.check(booted == good, f"after three failed boots of deployment {bad}, systemd-boot should start {good}, not {booted}")
        t.check(current() == good, f"archix should make deployment {good} current after {bad} failed to boot")
        t.check(ok(f"[ -f /efi/loader/entries/archix-{bad}+0-3.conf ]"), f"deployment {bad}'s entry should be marked bad: {out('ls /efi/loader/entries')}")
        t.check(ok(f"grep -q \"generation {bad} didn't finish booting\" /var/lib/archix/notes"), f"the login note should say {bad} didn't boot: {out('cat /var/lib/archix/notes')}")
        t.check(not re.search(r"^preferred", loader(), re.M), f"falling back to {good} should drop preferred: {loader()}")
