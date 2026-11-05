# Write replay of a plain `pacman -S vim`, a transaction that touches neither the kernel nor the initramfs, so the step after it builds no boot image and nothing on the root is flushed but by archix's own flush before it drops the unfinished mark: every flush and mark, and subsets of each flush's ESP writes, must boot by itself as the system with or without vim whole or, while the root is marked unfinished, as the fallback slot in try mode.
from boot.common import bind, ok
from replay import replay


def run(m, t):
    bind(m, t)

    def download():
        m.succeed("pacman -Sw --noconfirm --noprogressbar vim > /root/download.log 2>&1", timeout=1800, what="downloading vim")
        t.require(not ok("pacman -Q vim"), "vim should not be installed before the transaction")

    def after():
        t.require(ok("pacman -Q vim"), "vim should be installed after the transaction")

    replay.transaction(m, t, "install", download, "pacman -S --noconfirm --noprogressbar vim", after)
