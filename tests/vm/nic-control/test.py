# The NIC no-carrier control: NIC_REBOOTS warm reboots and NIC_RESETS system_resets of the boot suites' image, plain Arch with archix removed (NIC_ARM=plain) or laid out by base.py's first switch (deployed), each boot judged as boot() judges one and a NIC without carrier reported before any recovery.
import os
import re

from boot.common import NIC_STATE, bind, ok, out, settled

ARM = os.environ.get("NIC_ARM", "plain")
REBOOTS = int(os.environ.get("NIC_REBOOTS") or 30)
RESETS = int(os.environ.get("NIC_RESETS") or 6)
MACHINE = {"global_timeout": max(int(os.environ.get("VM_TIMEOUT") or 2400), 120 * (REBOOTS + RESETS + 1))}
ARCHIX = "/usr/bin/archix /usr/lib/archix /etc/archix /var/lib/archix /etc/systemd/system/archix-protected.service /etc/systemd/system/multi-user.target.wants/archix-protected.service"


def plan():
    """Which boots are resets: NIC_RESETS of them spread evenly through the NIC_REBOOTS + NIC_RESETS boots after the first."""
    total = REBOOTS + RESETS
    resets = {round((k + 1) * total / (RESETS + 1)) for k in range(RESETS)} if RESETS else set()
    return ["reset" if n in resets else "reboot" for n in range(1, total + 1)]


def run(m, t):
    bind(m, t)
    boots, good = [], []

    def judged(n, how):
        boots.append(how)
        m.execute("systemctl is-system-running --wait > /dev/null", 900)
        settled()
        if ARM == "deployed":
            t.require(ok("[ -e /run/archix/deployment ]"), "the deployed arm booted without a deployment")
        nic = out(NIC_STATE).strip()
        took = re.search(r"Startup finished in .*", out("systemd-analyze 2>&1 | head -1"))
        t.log(f"NIC-CONTROL arm={ARM} boot={n} how={how} ok: {nic}; {took.group(0) if took else 'no systemd-analyze'}")
        good.append(how)

    try:
        with t.step(f"{ARM} 0: the first, cold boot"):
            m.start()
            if ARM == "plain":
                t.require(not ok("[ -e /archix ]"), "the plain arm's image should not be laid out")
                m.succeed(f"systemctl disable --now archix-protected.service && rm -rf {ARCHIX} && systemctl daemon-reload")
                t.require(out("ls /usr/bin /usr/lib/systemd/system /etc/systemd/system /usr/lib | grep archix | grep -vc archix-backdoor || true").strip() == "0", f"archix left in the plain arm: {out('ls -d /usr/bin/archix* /usr/lib/systemd/system/archix* /etc/systemd/system/archix* /usr/lib/archix* 2>&1')}")
                # base.py sets the same for the deployed arm, so neither waits on the menu.
                m.succeed("bootctl set-timeout 0")
            else:
                t.require(ok("[ -e /run/archix/deployment ]"), "the deployed arm should boot a deployment")
            judged(0, "cold")
            t.log(f"the plan after the cold boot: {' '.join(plan())}")
        for n, how in enumerate(plan(), 1):
            with t.step(f"{ARM} {n}: {'a warm reboot' if how == 'reboot' else 'QMP system_reset'}"):
                if how == "reboot":
                    m.reboot()
                else:
                    m.crash()
                judged(n, how)
    finally:
        t.log(f"NIC-CONTROL arm={ARM} boots={len(boots)} good={len(good)} reboots={good.count('reboot')} resets={good.count('reset')} planned=1+{REBOOTS}+{RESETS}")
