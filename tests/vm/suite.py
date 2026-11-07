# Shared by the suites with an image of their own (tests/vm/run.sh's suite_tests): the first switch on the plain image, as boot/base.py makes it, without powering off.
import os
import time

from boot import common
from boot.common import HOST, boot, edit_config, entries, ok, out, snapshots

BTRFS = os.environ.get("FS", "ext4") == "btrfs"


def lay_out(m, t, timeout=1800, allowed=()):
    """Boots the plain image, makes the first switch (rollbacks on btrfs, off on ext4), reboots, and waits for that boot to count as good."""
    with t.step("plain: the first switch sets the system up"):
        m.start()
        boot(allowed)
        t.check(not ok("[ -d /.archix/snapshots ]"), "the image should boot as a plain install")
        if not BTRFS:
            edit_config("'/  rollback = { enable = true },/d'")
        status = common.first_switch("/root/first-switch.log", timeout, allowed)
        t.require(status == 0, f"the first switch exited {status}: {out('tail -c 3000 /root/first-switch.log')}")
        t.check(common.counts_tries("archix-current.conf", 2), f"the first switch should write the current entry with two tries: {entries()}")
        m.succeed("bootctl set-timeout 0")
        m.reboot()
    with t.step("plain: the switched system boots and counts as good"):
        started = boot(allowed)
        t.check(started == "archix-current.conf", f"the current entry should start, not {started}")
        m.wait_until_succeeds("[ -f /efi/loader/entries/archix-current.conf ]", timeout=300, what="the boot should count as good")
        if BTRFS:
            t.check(out("findmnt -no FSROOT /var/lib").strip() == "/@varlib", f"/var/lib should be @varlib: {out('findmnt /var/lib')}")


def switch(m, t, what, timeout=3600):
    """A switch whose output goes to the console and /root/switch.log; returns its output's tail."""
    status, said = m.execute(f"set -o pipefail; timeout {timeout - 60} {HOST} switch --noconfirm 2>&1 | tee /root/switch.log | tail -c 4000", timeout)
    t.require(status == 0, f"{what} exited {status}: {said[-3000:]}")
    if common.LAID_OUT_FIRST in said:
        said += common.finish_layout(what)[-4000:]
    return said


def update(m, t, date):
    m.succeed(f"set -o pipefail; {HOST} update --to {date} --noconfirm 2>&1 | tail -20", 1800, what=f"archix update --to {date}")


def add_packages(m, *names):
    """Adds packages to the config's packages list, after "base"."""
    listed = "".join(f'"{name}", ' for name in names)
    m.succeed(f"sed -i 's|{{ \"base\", |{{ \"base\", {listed}|' /etc/archix/init.lua")


def drop_packages(m, *names):
    for name in names:
        m.succeed(f"sed -i 's|\"{name}\", ||' /etc/archix/init.lua")


def current_generation():
    return int(out("archix generations | awk '$1 == \"*\" && $2 ~ /^[0-9]+$/ { print $2; exit }'").strip() or 0)


def newest_snapshot():
    return snapshots()[-1]


class Console:
    """A conversation at the serial console, for prompts the backdoor doesn't reach: expect waits for literal text after what was matched last, send types a line."""

    def __init__(self, m, t):
        self.m, self.t = m, t
        self.pos = m.console_mark

    def text(self):
        with open(self.m.serial, "rb") as f:
            f.seek(self.pos)
            return f.read().decode(errors="replace")

    def expect(self, literal, timeout=900):
        deadline = time.monotonic() + timeout
        while True:
            found = self.text().find(literal)
            if found >= 0:
                self.pos += len(self.text()[: found + len(literal)].encode(errors="replace"))
                self.t.log(f"console: {literal!r}")
                return
            if not self.m.alive():
                raise self.m.gone(f"waiting for {literal!r} on the console")
            if time.monotonic() > deadline:
                self.t.require(False, f"{literal!r} didn't show on the console within {timeout} s: ...{self.text()[-1500:]}")
            time.sleep(0.3)

    def say(self, expect, send):
        self.expect(expect)
        self.m.type(send)

    def since(self, start):
        with open(self.m.serial, "rb") as f:
            f.seek(start)
            return f.read(self.pos - start).decode(errors="replace")


def images_consistent(t):
    """Every archix entry starts an image on the ESP, and every image there is started by an entry."""
    listing = out("for e in /efi/loader/entries/archix-current*.conf /efi/loader/entries/archix-snapshot-*.conf; do [ -f \"$e\" ] || continue; sed -nE 's#^(efi|uki) /#/efi/#p' \"$e\"; done | sort -u")
    started = set(listing.split())
    images = set(out("ls /efi/EFI/archix/*.efi 2>/dev/null").split())
    t.check(started <= images, f"entries start images that are gone: {sorted(started - images)}")
    t.check(images <= started, f"images no entry starts are left on the ESP: {sorted(images - started)}")
