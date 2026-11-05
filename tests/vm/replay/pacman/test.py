# Write replay of a real `pacman -Syu` (2026-10-10 to 2026-10-24: linux 7.2.9 to 7.2.11, systemd 262 to 262.1) and the boot after it: the step before the transaction (its snapshot, the current entry's tries, the unfinished mark), the extraction, the step after it (the boot images built and copied, the menu written and its counter reset, the root flushed, the mark dropped), the reboot, systemd-boot's count and systemd-bless-boot's rename; every flush and mark, and subsets of each flush's ESP writes, must boot by itself as the system before or after the update or, while the root is marked unfinished, as the fallback slot in try mode.
from boot.common import bind, out
from replay import replay


def run(m, t):
    bind(m, t)

    def download():
        m.succeed("sed -i 's|/2026/10/10/|/2026/10/24/|' /etc/pacman.d/mirrorlist && pacman -Syuw --noconfirm --noprogressbar > /root/download.log 2>&1", timeout=1800, what="downloading the update")
        listed = out("pacman -Qu")
        t.require("linux 7.2.9" in listed and "systemd 262" in listed, f"the update should bring a new kernel and systemd: {listed}")

    def after():
        t.require(out("uname -r").startswith("7.2.11"), f"the boot after the update should be its new kernel: {out('uname -r')}")

    replay.transaction(m, t, "pacman", download, "pacman -Syu --noconfirm --noprogressbar", after)
