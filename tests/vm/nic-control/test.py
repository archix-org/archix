# The NIC no-carrier control: NIC_REBOOTS warm reboots and NIC_RESETS system_resets of the boot suites' image, plain Arch with archix removed (NIC_ARM=plain) or laid out by base.py's first switch (deployed), each boot judged as boot() judges one and a NIC without carrier reported before any recovery.
import os
import re

from driver import MachineError
from boot.common import NIC_STATE, bind, ok, out, settled

ARM = os.environ.get("NIC_ARM", "plain")
REBOOTS = int(os.environ.get("NIC_REBOOTS") or 30)
RESETS = int(os.environ.get("NIC_RESETS") or 6)
HANG = int(os.environ.get("NIC_HANG") or 300)
MACHINE = {"global_timeout": max(int(os.environ.get("VM_TIMEOUT") or 2400), 120 * (REBOOTS + RESETS + 1))}
# Linux runs with CR0 0x80050033 (PE, WP, AM, PG and the rest), OVMF without AM.
LINUX_CR0 = "80050033"
ANSI = re.compile(r"\x1b\[[0-9;?=!]*[A-Za-z]|\x1b\][^\x1b]*\x1b\\|\x1b[78]")
ARCHIX = "/usr/bin/archix /usr/lib/archix /etc/archix /var/lib/archix /etc/systemd/system/archix-protected.service /etc/systemd/system/multi-user.target.wants/archix-protected.service"


def plan():
    """Which boots are resets: NIC_RESETS of them spread evenly through the NIC_REBOOTS + NIC_RESETS boots after the first."""
    total = REBOOTS + RESETS
    resets = {round((k + 1) * total / (RESETS + 1)) for k in range(RESETS)} if RESETS else set()
    return ["reset" if n in resets else "reboot" for n in range(1, total + 1)]


def hang_phase(console, registers):
    """Where a boot that never reached the shell stopped, from the console since the reboot or reset and each vCPU's CR0 and RIP; docs/CI.md's hang table counts by these."""
    text = ANSI.sub("", console)
    linux = [cpu for cpu in registers.split("CPU#")[1:] if f"CR0={LINUX_CR0}" in cpu]
    started = text.rfind("BdsDxe: starting")
    if started < 0:
        return "shutdown" if linux else "firmware-before-BdsDxe"
    after = text[started:]
    if not linux:
        return "loader-after-BdsDxe"
    if "Welcome to" in after:
        return "after-switch-root"
    if "Booting initrd of" in after:
        mount = "archix: mount the deployment this boot belongs to"
        return "initramfs-archix-deploy-mount" if after.count(mount) == 1 else "initramfs"
    return "early-kernel"


def run(m, t):
    bind(m, t)
    boots, good, hangs = [], [], []

    def judged(n, how):
        boots.append(how)
        # A boot that never reaches the shell is another fault than the NIC's: it's reported, counted and reset, and the arm goes on.
        for _ in range(3):
            try:
                m.wait_for_shell(HANG)
                break
            except MachineError as e:
                if not m.alive():
                    raise
                hangs.append(n)
                registers = m.hmp("info registers -a")
                cpus = " ".join(re.findall(r"(?:RIP|CR0|CR3)=\S+", registers))
                phase = hang_phase(m.console_since(m.console_mark), registers)
                t.log(f"NIC-CONTROL arm={ARM} boot={n} how={how} HANG phase={phase}; {cpus}")
                t.log(f"hang {n}: {e}; qemu: {m.hmp('info status').strip()}")
                m.crash()
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
        t.log(f"NIC-CONTROL arm={ARM} boots={len(boots)} good={len(good)} hangs={len(hangs)}{' at ' + ','.join(map(str, hangs)) if hangs else ''} reboots={good.count('reboot')} resets={good.count('reset')} planned=1+{REBOOTS}+{RESETS}")
