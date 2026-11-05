# The split boot tests' shared helpers, bound to one test's machine and steps by bind(); each test starts from base.py's disk, set up for rollbacks with the boot menu written and one good boot behind it.
import json
import re
import time

LOADER = "/efi/loader/loader.conf"
ENTRIES = "/efi/loader/entries"
CONFIG = "/etc/archix/init.lua"
EFIVAR = "/sys/firmware/efi/efivars/LoaderEntrySelected-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f"
STATE = "/.archix/boot/state.json"
HOST = "archix --host archix-vm"
BACKDOOR = "archix-backdoor.service"
ATTRIBUTED = "ATTRIBUTED: "
NIC_STATE ="for nic in /sys/class/net/en*; do echo \"$(basename $nic) carrier=$(cat $nic/carrier 2>&1) carrier_changes=$(cat $nic/carrier_changes) operstate=$(cat $nic/operstate) flags=$(cat $nic/flags)\"; done"
m = t = None


def bind(machine, test):
    global m, t
    m, t = machine, test


def out(cmd, timeout=900):
    return m.execute(cmd, timeout)[1]


def ok(cmd, timeout=900):
    return m.execute(cmd, timeout)[0] == 0


def loader():
    return out(f"cat {LOADER}")


def has_line(text, line):
    return line in text.splitlines()


def edit_config(sed):
    m.succeed(f"sed -i {sed} {CONFIG}")


def entries():
    """archix's entry files on the ESP, with their counters."""
    return out(f"ls {ENTRIES} | grep '^archix-' || true").split()


def entry(name):
    """The file of entry id name (archix-snapshot-3.conf), whatever its counters, or None."""
    stem = name.removesuffix(".conf")
    return next((f for f in entries() if f == name or f.startswith(stem + "+")), None)


def counts_tries(name, tries):
    """Whether entry id name counts tries untouched boots, under either spelling: a rewrite renames +2 to +2-0."""
    stem = name.removesuffix(".conf")
    return entry(name) in (f"{stem}+{tries}.conf", f"{stem}+{tries}-0.conf")


def selected():
    return out(f"tail -c +5 {EFIVAR} 2>/dev/null | tr -d '\\000'").strip()


def trying():
    """This boot's try record, or None outside try mode."""
    text = out("cat /run/archix/try 2>/dev/null")
    return json.loads(text) if text.strip() else None


def snapshots():
    """The snapshot numbers, oldest first."""
    return [int(line.split()[0]) for line in out("archix snapshot list").splitlines() if line.split() and line.split()[0].isdigit()]


def state():
    text = out(f"cat {STATE} 2>/dev/null")
    return json.loads(text) if text.strip() else {}


def switch(what="a switch"):
    m.succeed(f"{HOST} switch --noconfirm", what=what)


def add_unit(name, body, enable=True):
    """Declares unit name with body in the config, enabled by default; the next switch installs it."""
    text = body.replace("\n", "\\\\n").replace("\"", "\\\\\"")
    edit_config(f"'s|  units = {{|  units = {{\\n    [\"{name}\"] = \"{text}\",|'")
    if enable:
        edit_config(f"'s|  services = {{|  services = {{\\n    [\"{name}\"] = true,|'")


def wait_fallback(began, timeout=2400):
    """Waits, with nobody touching the machine, for the boots after a bad one to fall back: a boot in try mode; returns its seconds since began."""
    m.wait_for_console_text(r"archix: this boot is snapshot", timeout=timeout)
    m.connected = False
    m.wait_for_shell(timeout=900)
    return time.monotonic() - began


def bump(n):
    """Changes the managed file /etc/archix-vmtest, so a switch has something to apply and takes a snapshot first."""
    edit_config(f"'s|\\[\"/etc/archix-vmtest\"\\] = {{ text = \"[0-9]*\" }}|[\"/etc/archix-vmtest\"] = {{ text = \"{n}\" }}|'")


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
    """Judges a boot by the guest itself: multi-user.target reached, no failed unit but the backdoor's own and allowed, the NIC up; returns the entry systemd-boot started."""
    m.execute("systemctl is-system-running --wait > /dev/null", 900)
    started = selected()
    t.log(f"booted entry {started}, try {trying()}, entries {entries()}")
    settled(allowed)
    return started


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
