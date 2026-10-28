# The split boot tests' base, run once per run with this run's archix: the image boots as a plain install, the first switch lays it out, and the VM powers off; each test boots a copy-on-write overlay of the disk and a copy of the vars.
import time

from boot.common import HOST, bind, boot, current, deployments, hidden_usr, ok, out


def run(m, t):
    bind(m, t)
    with t.step("plain: the first switch on a booted system"):
        m.start()
        boot()
        t.check(not ok("[ -e /archix ]"), "the image should boot as a plain install, not laid out yet")
        t.require(ok("systemctl is-active -q archix-protected.service"), "the ProtectSystem=strict service should run before the first switch")
        pid = out("systemctl show -P MainPID archix-protected.service").strip()
        m.succeed(f"{HOST} switch --noconfirm", what="the first switch on the booted system")
        t.check(current() == "2" and deployments() == "1 2 ", f"the first switch should lay out deployment 1 and build 2: {deployments()}")
        time.sleep(5)
        t.check(ok("systemctl is-active -q archix-protected.service") and out("systemctl show -P MainPID archix-protected.service").strip() == pid,
                f"the ProtectSystem=strict service lost its /usr during the first layout: {out('journalctl -u archix-protected.service -b --no-pager | tail -5')}")
        t.check(hidden_usr() > 0, "the first switch pruned the live /usr, which running services may still use")
        usr = out("findmnt -no SOURCE /usr")
        t.check("archix/deploy/1/usr" in usr, f"the running system should be bound to deployment 1: {usr}")
        t.check(not ok("touch /boot/archix-vmtest"), "/boot should be read-only once laid out")
        # LoaderConfigTimeout, not loader.conf, which archix rewrites: no test waits on the menu.
        m.succeed("bootctl set-timeout 0")
        m.shutdown()
