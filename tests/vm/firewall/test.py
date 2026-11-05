# The firewall over real network paths, in place: ssh from the host through a port forward logs in while the ssh service is allowed; over a second NIC that tests/vm/run.sh's l2.py container joins, from LAN and public addresses on IPv4 and IPv6, ssh's port answers everyone, a LAN-only service's only the LAN and an undeclared one no one. A switch dropping the services closes them at once, without a reboot, and after it; on btrfs a try boot of the snapshot from before brings its rules back; `archix revert` to the generation that allowed them opens them again live.
import shutil
import socket
import subprocess

from boot.common import bind, boot, edit_config, entries, entry, ok, out, trying
from suite import BTRFS, current_generation, lay_out, newest_snapshot, switch



def free_port():
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


PORT = free_port()
MACHINE = {"global_timeout": 7200}
OPEN = "lan4/22=open lan4/2201=open lan4/2202=closed lan6/22=open lan6/2201=open lan6/2202=closed wan4/22=open wan4/2201=closed wan4/2202=closed wan6/22=open wan6/2201=closed wan6/2202=closed"
SHUT = "lan4/22=closed lan4/2201=closed lan4/2202=closed lan6/22=closed lan6/2201=closed lan6/2202=closed wan4/22=closed wan4/2201=closed wan4/2202=closed wan6/22=closed wan6/2201=closed wan6/2202=closed"


def banner():
    try:
        with socket.create_connection(("127.0.0.1", PORT), timeout=10) as conn:
            conn.settimeout(10)
            return conn.recv(64).decode(errors="replace").strip()
    except OSError as e:
        return f"none ({e.__class__.__name__})"


def login():
    ssh = ["ssh", "-i", "/work/ssh-key", "-p", str(PORT), "-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null",
           "-o", "ConnectTimeout=10", "-o", "LogLevel=ERROR", "root@127.0.0.1", "cat /proc/sys/kernel/random/boot_id"]
    try:
        done = subprocess.run(ssh, capture_output=True, text=True, timeout=30)
    except subprocess.TimeoutExpired:
        return None
    return done.stdout.strip() if done.returncode == 0 else None


def reach():
    """What l2.py finds on the second NIC: which of sshd's ports 22 (ssh), 2201 (probe-lan, LAN only) and 2202 (undeclared) answer from LAN and public sources on both families."""
    with socket.socket(socket.AF_UNIX) as conn:
        conn.settimeout(120)
        conn.connect("/work/reach.sock")
        conn.sendall(b"reach\n")
        return conn.makefile("r").readline().strip()


def loaded():
    return out("nft list table inet archix 2>&1")


def probe_nic(t):
    """The second NIC's four static addresses, IPv6's past duplicate detection."""
    t.require(ok("nic=$(ip -o link | awk -F': ' '/52:54:00:12:34:77/ { print $2 }'); for i in $(seq 60); do [ -n \"$nic\" ] && [ \"$(ip -o addr show dev $nic | grep -c -e 192.168.77.2/ -e 198.51.100.2/ -e 'fd00:77::2/' -e '2001:db8::2/')\" = 4 ] && ! ip -o addr show dev $nic | grep -q tentative && exit 0; sleep 1; done; exit 1", 90),
              f"the second NIC should have its four addresses: {out('ip -o addr')}")


def is_open(t, m, what):
    t.check(ok("systemctl is-active -q archix-firewall.service") and "policy drop;" in loaded(), f"{what}: archix's input chain should be loaded with policy drop: {loaded()}")
    t.check('tcp dport 22 accept comment "ssh"' in loaded(), f"{what}: the loaded rules should open port 22 for ssh: {loaded()}")
    seen = banner()
    t.check(seen.startswith("SSH-") and login() == out("cat /proc/sys/kernel/random/boot_id").strip(), f"{what}: ssh from the host should log in to this boot: banner {seen!r}")
    probe_nic(t)
    t.check(reach() == OPEN, f"{what}: the second NIC should see {OPEN}: {reach()}")


def is_shut(t, m, what):
    t.check(ok("systemctl is-active -q archix-firewall.service") and "policy drop;" in loaded(), f"{what}: archix's input chain should be loaded with policy drop: {loaded()}")
    t.check("tcp dport" not in loaded(), f"{what}: the loaded rules should open no TCP port: {loaded()}")
    t.check(not banner().startswith("SSH-") and login() is None, f"{what}: ssh from the host should be refused")
    probe_nic(t)
    t.check(reach() == SHUT, f"{what}: the second NIC should see {SHUT}: {reach()}")
    t.check(ok("ssh-keyscan -T 10 127.0.0.1 2>/dev/null | grep -q ssh-"), f"{what}: sshd should still answer on the loopback, which the firewall accepts")


def run(m, t):
    bind(m, t)
    if not shutil.which("ssh"):
        subprocess.run(["pacman", "-S", "--noconfirm", "--needed", "openssh"], check=True, capture_output=True)
    # The forward's NIC on a subnet of its own: on net0's, the guest would answer it through net0.
    m.extra = ["-netdev", f"user,id=net2,net=10.0.3.0/24,hostfwd=tcp:127.0.0.1:{PORT}-:22", "-device", "virtio-net-pci,netdev=net2,romfile=",
               "-netdev", "stream,id=net1,server=on,addr.type=unix,addr.path=/work/l2.sock", "-device", "virtio-net-pci,netdev=net1,mac=52:54:00:12:34:77,romfile="]
    lay_out(m, t)
    with t.step("1: with ssh allowed, the host logs in, and ports answer by their services' sources"):
        m.wait_until_succeeds("systemctl is-active -q sshd.service", timeout=60)
        is_open(t, m, "allowed")
        t.check('ip6 saddr @archix_lan6 tcp dport 2201 accept comment "probe-lan"' in loaded(), f"the loaded rules should open 2201 to the LAN over IPv6: {loaded()}")
        rules = loaded()
        m.succeed("nft add table ip vmtest-other && nft add chain ip vmtest-other c && systemctl reload archix-firewall.service", what="another program's table, then a reload")
        t.check("table ip vmtest-other" in out("nft list tables"), "reloading archix's firewall should leave another program's table")
        t.check(loaded() == rules, "the reload shouldn't change archix's rules")
        allowed = current_generation()

    with t.step("2: a switch dropping the services closes them at once"):
        edit_config("'s|allowed_services = { \"ssh\", \"probe-lan\" }|allowed_services = {}|'")
        switch(m, t, "the switch dropping the services")
        before = newest_snapshot() if BTRFS else None
        t.check("table ip vmtest-other" in out("nft list tables"), "the switch's reload should leave another program's table")
        is_shut(t, m, "live after the switch")
        m.reboot()
        boot()
        is_shut(t, m, "after the reboot")

    if BTRFS:
        with t.step(f"3: a try boot of snapshot {before}, from before the switch, has its rules"):
            name = f"archix-snapshot-{before}.conf"
            t.require(entry(name), f"snapshot {before} should have an entry: {entries()}")
            m.select_entry(name)
            m.reboot()
            boot()
            t.check((trying() or {}).get("snapshot") == before, f"a try boot of snapshot {before}: {trying()}")
            m.wait_until_succeeds("systemctl is-active -q sshd.service", timeout=60)
            is_open(t, m, f"snapshot {before}")
            t.check(loaded() == rules, f"snapshot {before} should load the rules it had: {loaded()}")
            m.reboot()
            boot()
            is_shut(t, m, "back on the current system")

    with t.step(f"4: reverting to generation {allowed} opens the services again, live"):
        said = m.succeed(f"set -o pipefail; archix --host archix-vm revert {allowed} --noconfirm 2>&1 | tail -20", 1800, what=f"archix revert {allowed}")
        t.log(said)
        is_open(t, m, "live after the revert")
        m.reboot()
        boot()
        m.wait_until_succeeds("systemctl is-active -q sshd.service", timeout=60)
        is_open(t, m, "after the revert and a reboot")
