# Shared by the suites with an image of their own (tests/vm/run.sh's suite_tests): the first switch on the plain image, then the boot after it.
import os

from boot import common
from boot.common import HOST, boot

BTRFS = os.environ.get("FS", "ext4") == "btrfs"


def lay_out(m, t, timeout=1800, allowed=()):
    """Boots the plain image, makes the first switch and reboots into what it applied."""
    with t.step("plain: the first switch on the plain image"):
        m.start()
        boot(allowed)
        status, _ = m.execute(f"set -o pipefail; timeout {timeout - 100} {HOST} switch --noconfirm 2>&1 | tee /root/first-switch.log > /dev/console", timeout)
        said = common.out("tail -c 3000 /root/first-switch.log")
        common.not_stub(status, said, "the first switch")
        t.require(status == 0, f"the first switch exited {status}: {said}")
        m.succeed("bootctl set-timeout 0")
        m.reboot()
    with t.step("plain: the switched system boots"):
        boot(allowed)


def switch(m, t, what, timeout=3600):
    return common.switch(what, timeout)


def update(m, t, date):
    m.succeed(f"set -o pipefail; {HOST} update --to {date} --noconfirm 2>&1 | tail -20", 1800, what=f"archix update --to {date}")


def add_packages(m, *names):
    """Adds packages to the config's packages list, after "base"."""
    listed = "".join(f'"{name}", ' for name in names)
    m.succeed(f"sed -i 's|{{ \"base\", |{{ \"base\", {listed}|' /etc/archix/init.lua")


def drop_packages(m, *names):
    for name in names:
        m.succeed(f"sed -i 's|\"{name}\", ||' /etc/archix/init.lua")
