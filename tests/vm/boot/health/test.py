# The health check must not fall back where nothing broke: on a machine without a display manager, and with a unit that failed at the last blessed boot too and fails again.
from boot.common import bind, boot, bump, counts_tries, edit_config, entries, entry, ok, out, state, switch, trying

FAILING = "archix-test-failing.service"


def counted_boot(m, t, allowed=()):
    """A switch gives the current entry its tries; the boot after it must pass its check and be blessed, not fall back."""
    bump(int(out("cat /etc/archix-vmtest").strip()) + 1)
    switch()
    t.check(counts_tries("archix-current.conf", 2), f"two tries: {entries()}")
    m.reboot()
    started = boot(allowed)
    t.check(started == "archix-current.conf" and trying() is None, f"the current entry should boot, not a fallback: {started} {trying()}")
    m.wait_until_succeeds("[ -f /efi/loader/entries/archix-current.conf ]", timeout=300, what="the counted boot should be blessed")
    said = out("journalctl -b -u archix-health-check.service --no-pager")
    t.check("passed its health check" in said, f"the health check should pass: {said[-800:]}")
    return said


def run(m, t):
    bind(m, t)
    with t.step("1: no display manager: the counted boot passes"):
        boot()
        t.check(not ok("[ -e /etc/systemd/system/display-manager.service ]"), "the image has no display manager")
        counted_boot(m, t)

    with t.step("2: a unit that fails at an uncounted boot of the current system"):
        m.succeed(f"printf '[Unit]\\nDescription=fails at every boot\\n\\n[Service]\\nType=oneshot\\nExecStart=/usr/bin/false\\n\\n[Install]\\nWantedBy=multi-user.target\\n' > /etc/systemd/system/{FAILING} && systemctl enable {FAILING}", what="a unit that fails, enabled by hand")
        m.reboot()
        boot(allowed=(FAILING,))
        t.check(ok(f"systemctl is-failed -q {FAILING}"), "the unit fails at this uncounted boot")

    with t.step("3: the switch notes it as failing already, and the counted boot after, where it fails again, still passes"):
        counted_boot(m, t, allowed=(FAILING,))
        t.check(FAILING in state().get("blessed_failed_units", []), f"the switch should note the failed unit: {state()}")
        t.check(ok(f"systemctl is-failed -q {FAILING}"), "the unit failed this boot too")
