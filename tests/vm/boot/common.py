# The VM suites' shared helpers, bound to one test's machine and steps by bind().
import re
import time

CONFIG = "/etc/archix/init.lua"
EFIVAR = "/sys/firmware/efi/efivars/LoaderEntrySelected-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f"
HOST = "archix --host archix-vm"
BACKDOOR = "archix-backdoor.service"
ATTRIBUTED = "ATTRIBUTED: "
NIC_STATE ="for nic in /sys/class/net/en*; do echo \"$(basename $nic) carrier=$(cat $nic/carrier 2>&1) carrier_changes=$(cat $nic/carrier_changes) operstate=$(cat $nic/operstate) flags=$(cat $nic/flags)\"; done"
# What the switch stub prints until the rescope's rewrite (docs/SCOPE.md 2.2), exiting 3.
STUB = "switch is being rewritten for the rescope"
m = t = None


def bind(machine, test):
    global m, t
    m, t = machine, test


def out(cmd, timeout=900):
    return m.execute(cmd, timeout)[1]


def ok(cmd, timeout=900):
    return m.execute(cmd, timeout)[0] == 0


def edit_config(sed):
    m.succeed(f"sed -i {sed} {CONFIG}")


def selected():
    """The entry the firmware's loader started this boot from, if systemd-boot set LoaderEntrySelected."""
    return out(f"tail -c +5 {EFIVAR} 2>/dev/null | tr -d '\\000'").strip()


def not_stub(status, said, what):
    """Fails the step loudly when what ran into the switch stub, naming it, rather than as an unexplained exit 3."""
    t.require(not (status == 3 and STUB in said), f"{what}: `archix switch` is a stub until the rescope's rewrite (docs/SCOPE.md 2.2; tests/ci/rescope-pending.txt): {said[-300:]}")


def switch(what="a switch", timeout=3600):
    """A switch, its output on the console and in /root/switch.log; returns its output's tail."""
    status, said = m.execute(f"set -o pipefail; timeout {timeout - 60} {HOST} switch --noconfirm 2>&1 | tee /root/switch.log | tail -c 4000", timeout)
    not_stub(status, said, what)
    t.require(status == 0, f"{what} exited {status}: {said[-3000:]}")
    return said


def carrier_race(networkctl, nic_state):
    """The NICs networkd calls no-carrier while the kernel has their carrier after at least one change: docs/CI.md's networkd carrier race, read before any recovery."""
    kernel = {line.split()[0]: dict(word.split("=", 1) for word in line.split()[1:] if "=" in word) for line in nic_state.splitlines() if line.strip()}
    named = {words[1]: words[3] for words in (line.split() for line in networkctl.splitlines()) if len(words) >= 4 and words[0].isdigit()}
    return [nic for nic, state in kernel.items() if named.get(nic) == "no-carrier" and state.get("carrier") == "1" and state.get("carrier_changes", "").isdigit() and int(state["carrier_changes"]) >= 1]


def network(wait=True):
    if wait and ok("/usr/lib/systemd/systemd-networkd-wait-online --timeout=90 > /dev/null 2>&1", 150):
        return
    networkctl = out("networkctl list --no-pager 2>&1")
    t.log("networkctl: " + networkctl)
    t.log("networkd: " + out("journalctl -b -u systemd-networkd --no-pager | tail -30"))
    nic_state = out(NIC_STATE)
    t.log("nic at the report: " + nic_state + out("for nic in /sys/class/net/en*; do ip -d link show $(basename $nic) 2>&1; done"))
    t.log("kernel: " + out("journalctl -b -k --no-pager | grep -i -E 'virtio|net|enp|eth|link' | tail -30"))
    dev = re.search(r"^(\S+).*\[virtio-net\]", m.hmp("info virtio"), re.M)
    t.log("qemu: " + m.hmp("info network") + "".join(m.hmp(f"info virtio-queue-status {dev.group(1)} {q}") for q in (0, 1) if dev))
    raced = carrier_race(networkctl, nic_state)
    m.execute("for nic in /sys/class/net/en*; do ip link set $(basename $nic) down; ip link set $(basename $nic) up; done")
    if ok("/usr/lib/systemd/systemd-networkd-wait-online --timeout=30 > /dev/null 2>&1", 60):
        t.log("nic: a link down/up brought it back")
        if raced:
            t.attribute(f"{ATTRIBUTED}networkd-carrier-race on {' '.join(raced)} (known guest-stack flake, docs/CI.md#the-networkd-carrier-race)")
    else:
        t.log("nic: still not configured 30 s after a link down/up; the host's set_link off and on. nic after the down/up: " + out(NIC_STATE))
        m.hmp("set_link net0 off")
        time.sleep(2)
        m.hmp("set_link net0 on")
        configured = ok("/usr/lib/systemd/systemd-networkd-wait-online --timeout=40 > /dev/null 2>&1", 70)
        t.log(f"nic: after the host's set_link off/on {'it is' if configured else 'still not'} configured; qemu: {m.hmp('info network')}")
    t.log("nic at the end: " + out(NIC_STATE))
    t.require(False, "networkd didn't configure the NIC's link within 90 s of this stage starting")


def boot(allowed=()):
    """Judges a boot by the guest itself: multi-user.target reached, no failed unit but the backdoor's own and allowed, the NIC up; returns the running kernel."""
    m.execute("systemctl is-system-running --wait > /dev/null", 900)
    kernel = out("uname -r").strip()
    t.log(f"booted {kernel} from entry {selected() or 'unknown'}")
    settled(allowed)
    return kernel


def settled(allowed=()):
    """multi-user.target reached, no failed unit but the backdoor's own and allowed, and the NIC configured, as boot() and tests/vm/nic-control judge a boot."""
    t.require(ok("systemctl is-active -q multi-user.target"), f"the boot didn't reach multi-user.target: {out('systemctl list-jobs --no-pager 2>&1 | tail -10')}")
    failed = [unit for unit in out("systemctl --failed --no-legend --plain | awk '{ print $1 }'").split() if unit != BACKDOOR and unit not in allowed]
    if failed:
        for unit in failed:
            t.log(f"{unit}: " + out(f"journalctl -b -u {unit} --no-pager | tail -20"))
        t.log("networkctl: " + out("networkctl list --no-pager 2>&1"))
        if "systemd-networkd-wait-online.service" in failed:
            t.log(f"units failed at this boot: {' '.join(failed)}")
            # The NIC report, QEMU's view included, while the guest is still in the state that failed it.
            network(wait=False)
        t.require(False, f"units failed at this boot: {' '.join(failed)}")
    network()
    reporting = link_status_nics()
    t.check(not reporting, f"a NIC reports its link status, so its carrier comes up only as networkd brings the link up, which networkd can miss (docs/CI.md#the-networkd-carrier-race): {reporting}")


def link_status_nics():
    """The NICs whose virtio device offers link status (VIRTIO_NET_F_STATUS, feature bit 16): virtio-net then holds the carrier off until the link is opened, while without it (QEMU's status=off) the carrier is up from the probe, before networkd starts."""
    text = out("for nic in /sys/class/net/en*; do [ -e $nic/device/features ] && echo \"$(basename $nic) $(cut -c17 $nic/device/features)\"; done; true")
    return [line.split()[0] for line in text.splitlines() if line.split()[1:] == ["1"]]
