# The split boot tests' shared helpers, bound to one test's machine and steps by bind(); each test starts from base.py's laid-out disk.
import difflib
import re
import time

LOADER = "/efi/loader/loader.conf"
CONFIG = "/etc/archix/init.lua"
STATE = "/var/lib/archix-vmtest"
EFIVAR = "/sys/firmware/efi/efivars/LoaderEntrySelected-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f"
HOST = "archix --host archix-vm"
BACKDOOR = "archix-backdoor.service"
NIC_STATE = "for nic in /sys/class/net/en*; do echo \"$(basename $nic) carrier=$(cat $nic/carrier 2>&1) carrier_changes=$(cat $nic/carrier_changes) operstate=$(cat $nic/operstate) flags=$(cat $nic/flags)\"; done"
SECURE_BOOT_CONFIG = "'s|boot = { loader = \"systemd-boot\" }|boot = { loader = \"systemd-boot\", secure_boot = true, protected_entry = true }|'"
m = t = None


def bind(machine, test):
    global m, t
    m, t = machine, test


def diff(old, new):
    return "".join(difflib.unified_diff(old.splitlines(True), new.splitlines(True)))[:600]


def number(text):
    words = text.split()
    return int(words[0]) if words and words[0].isdigit() else -1


def out(cmd, timeout=900):
    return m.execute(cmd, timeout)[1]


def ok(cmd, timeout=900):
    return m.execute(cmd, timeout)[0] == 0


def current():
    return out("archix generations | awk '$1 == \"*\" { print $2 }'").strip()


def deployments():
    return out("ls /archix/deploy 2>/dev/null | sort -n | tr '\\n' ' '")


def listing():
    return out("ls -A /archix/deploy | tr '\\n' ' '")


def loader():
    return out(f"cat {LOADER}")


def has_line(text, line):
    return line in text.splitlines()


def hidden_usr():
    return number(out("mkdir -p /run/vmtest-root && mount --bind / /run/vmtest-root && ls -A /run/vmtest-root/usr | wc -l; umount /run/vmtest-root"))


def findings(n):
    return out(f"/opt/archix-vmtest/bin/archix-check-deployment {n} | grep -E ': /(usr|opt|boot)/' || true")


def edit_config(sed):
    m.succeed(f"sed -i {sed} {CONFIG}")


def secure_boot():
    return ok("bootctl status 2> /dev/null | grep -q 'Secure Boot: enabled (user)'")


def network(wait=True):
    if wait and ok("/usr/lib/systemd/systemd-networkd-wait-online --timeout=90 > /dev/null 2>&1", 150):
        return
    t.log("networkctl: " + out("networkctl list --no-pager 2>&1"))
    t.log("networkd: " + out("journalctl -b -u systemd-networkd --no-pager | tail -30"))
    t.log("nic at the report: " + out(NIC_STATE) + out("for nic in /sys/class/net/en*; do ip -d link show $(basename $nic) 2>&1; done"))
    t.log("kernel: " + out("journalctl -b -k --no-pager | grep -i -E 'virtio|net|enp|eth|link' | tail -30"))
    dev = re.search(r"^(\S+).*\[virtio-net\]", m.hmp("info virtio"), re.M)
    t.log("qemu: " + m.hmp("info network") + "".join(m.hmp(f"info virtio-queue-status {dev.group(1)} {q}") for q in (0, 1) if dev))
    m.execute("for nic in /sys/class/net/en*; do ip link set $(basename $nic) down; ip link set $(basename $nic) up; done")
    if ok("/usr/lib/systemd/systemd-networkd-wait-online --timeout=30 > /dev/null 2>&1", 60):
        t.log("nic: a link down/up brought it back")
    else:
        t.log("nic: still not configured 30 s after a link down/up; the host's set_link off and on. nic after the down/up: " + out(NIC_STATE))
        m.hmp("set_link net0 off")
        time.sleep(2)
        m.hmp("set_link net0 on")
        configured = ok("/usr/lib/systemd/systemd-networkd-wait-online --timeout=40 > /dev/null 2>&1", 70)
        t.log(f"nic: after the host's set_link off/on {'it is' if configured else 'still not'} configured; qemu: {m.hmp('info network')}")
    t.log("nic at the end: " + out(NIC_STATE))
    t.require(False, "networkd didn't configure the NIC's link within 90 s of this stage starting")


def boot(match=True):
    """Judges a boot by the guest itself, not by the backdoor having connected: multi-user.target reached, no failed unit but the backdoor's own, the NIC up, the mounted deployment the one systemd-boot started."""
    m.execute("systemctl is-system-running --wait > /dev/null", 900)
    selected = out(f"tail -c +5 {EFIVAR} 2>/dev/null | tr -d '\\000'").strip()
    found = re.search(r"archix-(?:fallback-)?([0-9]+)", selected)
    booted = found.group(1) if found else ""
    running = out("cat /run/archix/deployment 2>/dev/null").strip()
    t.log(f"booted entry {selected}, mounted deployment {running}, current {current()}, deployments {deployments()}")
    settled()
    if match:
        t.require(booted == running, f"the initramfs mounted deployment {running}, but systemd-boot started {selected}")
    return selected, booted, running


def settled():
    """multi-user.target reached, no failed unit but the backdoor's own and the NIC configured, as boot() and tests/vm/nic-control judge a boot."""
    t.require(ok("systemctl is-active -q multi-user.target"), f"the boot didn't reach multi-user.target: {out('systemctl list-jobs --no-pager 2>&1 | tail -10')}")
    failed = [unit for unit in out("systemctl --failed --no-legend --plain | awk '{ print $1 }'").split() if unit != BACKDOOR]
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


def setup_secure_boot(install=""):
    """Enrols archix's Secure Boot keys and switches with secure_boot and protected_entry, installing install with it; the next boot enforces it."""
    boot()
    m.succeed("archix secure-boot setup")
    edit_config(SECURE_BOOT_CONFIG)
    m.succeed(f"{HOST} {'install ' + install if install else 'switch'} --noconfirm")
    m.reboot()
