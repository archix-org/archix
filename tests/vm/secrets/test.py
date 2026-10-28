# Declared secrets on a real boot: consumers read them (a wg-quick link, a NetworkManager profile under /run, LoadCredential=, EnvironmentFile=, a crypt_hash login), an older entry boots its own values, a lost identity fails closed without a fallback and is recovered with the admin key, a damaged ciphertext costs only its secret, a crash leaves no core, swap and hibernation; each scenario ends with the guest's leak sweep, the run with a grep of the raw root and swap partitions.
import json
import re
import secrets as random
import shlex
import subprocess

from driver import MachineError

HOST = "archix --host archix-vm"
CONFIG = "/etc/archix/init.lua"
RAMFS = "/run/vmtest-ramfs"
EFIVAR = "/sys/firmware/efi/efivars/LoaderEntrySelected-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f"
BACKDOOR = "archix-backdoor.service"
SECRETS_UNIT = "archix-secrets.service"
MACHINE = {"mem": "1536"}
m = t = None
markers = {}
original = ""


def out(cmd, timeout=900):
    return m.execute(cmd, timeout)[1]


def ok(cmd, timeout=900):
    return m.execute(cmd, timeout)[0] == 0


def current():
    return out("archix generations | awk '$1 == \"*\" { print $2 }'").strip()


def marker(name, length=32):
    value = "".join(random.choice("ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789") for _ in range(length))
    markers[name] = value
    return value


def wg_key(name):
    key = out("wg genkey").strip()
    markers[name] = key
    return key


def boot(allowed_failed=()):
    """Waits for the guest to reach multi-user.target with no failed unit but the backdoor's and `allowed_failed`; returns the entry systemd-boot started and the deployment the initramfs mounted."""
    m.execute("systemctl is-system-running --wait > /dev/null", 900)
    selected = out(f"tail -c +5 {EFIVAR} 2>/dev/null | tr -d '\\000'").strip()
    running = out("cat /run/archix/deployment 2>/dev/null").strip()
    t.log(f"booted entry {selected}, mounted deployment {running}, current {current()}")
    t.require(ok("systemctl is-active -q multi-user.target"), "the boot didn't reach multi-user.target")
    failed = [u for u in out("systemctl --failed --no-legend --plain | awk '{ print $1 }'").split() if u != BACKDOOR and u not in allowed_failed]
    for unit in failed:
        t.log(f"{unit}: " + out(f"journalctl -b -u {unit} --no-pager | tail -20"))
    t.require(not failed, f"units failed at this boot: {' '.join(failed)}")
    ok("mountpoint -q " + RAMFS + " || { mkdir -p " + RAMFS + " && mount -t ramfs -o mode=0700 ramfs " + RAMFS + "; }")
    return selected, running


def set_config(extra):
    """The image's config and `extra` (Lua table fields) as two modules, so their files, units and services merge."""
    table = original.strip().removeprefix("return").strip()
    m.succeed(f"cat > {CONFIG} <<'ARCHIX_EOF'\nreturn {{ imports = {{\n{table},\n{{\n{extra}\n}},\n}} }}\nARCHIX_EOF", what="writing the config")


def set_secret(name, value):
    """Encrypts `value` as secret `name` from stdin; the value travels through the ramfs, never a disk."""
    path = f"{RAMFS}/value"
    m.succeed(f"printf %s {shlex.quote(value)} > {path} && {HOST} secrets set {name} --file {path}; status=$?; rm -f {path}; exit $status", what=f"archix secrets set {name}")


def sweep(scenario, allowed=lambda name, place: False):
    """The guest's leak sweep for every marker so far; a hit `allowed` doesn't explain fails the step."""
    lines = "".join(f"{name} {value}\n" for name, value in markers.items())
    m.succeed(f"cat > {RAMFS}/markers <<'ARCHIX_EOF'\n{lines}ARCHIX_EOF")
    found = out(f"/opt/archix-vmtest/bin/archix-leak-sweep {RAMFS}/markers", timeout=1800)
    m.succeed(f"rm -f {RAMFS}/markers")
    for line in found.splitlines():
        parts = line.split(" ", 2)
        if len(parts) == 3 and parts[0] == "HIT":
            t.check(allowed(parts[1], parts[2]), f"{scenario}: marker {parts[1]} leaked into {parts[2]}")


def shadow_place(place):
    return place.endswith("/etc/shadow") or place.endswith("/etc/shadow-") or "/var/lib/archix/etc-merge/" in place


def secrets_config(machine_key, admin, items, consumers, more=""):
    return f'secrets = {{ machine_key = "{machine_key}", admins = {{ "{admin}" }}, items = {{ {items} }}{more} }},\n{consumers}'


SWAP = "/dev/vda3"
SWAP_UNIT = "dev-vda3.swap"
SWAP_CONFIG = f'units = {{ ["{SWAP_UNIT}"] = "[Swap]\\nWhat={SWAP}\\n\\n[Install]\\nWantedBy=swap.target\\n" }}, services = {{ ["{SWAP_UNIT}"] = true }},'


ITEMS = """
    wg0 = { file = "secrets/wg0.age", mode = "0600" },
    nm = { file = "secrets/nm.age", path = "/run/NetworkManager/system-connections/home.nmconnection", mode = "0600" },
    api = { file = "secrets/api.age", owner = "alice", group = "alice", mode = "0400" },
    nb = { file = "secrets/nb.age" },
    pw = { file = "secrets/pw.age", kind = "crypt_hash" },
    swapcheck = { file = "secrets/swapcheck.age" },
"""

CONSUMERS = """
  users = { alice = { groups = { "wheel" }, hashed_password = archix.secret("pw") } },
  files = {
    ["/etc/wireguard/wg0.conf"] = { secret = "wg0" },
    ["/etc/netbird/env"] = { text = "NB_SETUP_KEY=" .. archix.secret("nb") .. "\\n", mode = "0600" },
  },
  units = {
    ["archix-test-cred.service"] = "[Service]\\nType=oneshot\\nLoadCredential=api:" .. archix.secret_path("api") .. "\\nExecStart=/bin/sh -c 'cmp -s \\"$CREDENTIALS_DIRECTORY/api\\" /run/archix/secrets/api'\\n",
    ["archix-test-env.service"] = "[Service]\\nType=oneshot\\nEnvironmentFile=/etc/netbird/env\\nExecStart=/bin/sh -c 'test -n \\"$NB_SETUP_KEY\\" && grep -qxF \\"NB_SETUP_KEY=$NB_SETUP_KEY\\" /etc/netbird/env'\\n",
  },
"""


def nm_profile(key):
    return f"[connection]\nid=home\ntype=wireguard\ninterface-name=wghome\nautoconnect=false\n\n[wireguard]\nprivate-key={key}\n\n[ipv4]\nmethod=disabled\n\n[ipv6]\nmethod=disabled\n"


def values_hash(name):
    return out(f"sha256sum < /run/archix/secrets/{name} 2>/dev/null | cut -d' ' -f1").strip()


def sha(text):
    import hashlib
    return hashlib.sha256(text.encode()).hexdigest()


def raw_partitions():
    """The root and swap partitions of the shut-down disk image, as (name, offset, size)."""
    table = json.loads(subprocess.run(["sfdisk", "-J", f"{m.work}/disk.img"], check=True, capture_output=True, text=True).stdout)
    parts = table["partitiontable"]["partitions"]
    return [("root", parts[1]["start"] * 512, parts[1]["size"] * 512), ("swap", parts[2]["start"] * 512, parts[2]["size"] * 512)]


def raw_grep(name, offset, size, needles):
    """Which of `needles` (name to bytes) occur in the disk image's bytes from offset for size, freed blocks included."""
    found = set()
    longest = max(len(v) for v in needles.values())
    with open(f"{m.work}/disk.img", "rb") as disk:
        disk.seek(offset)
        tail = b""
        left = size
        while left > 0:
            chunk = disk.read(min(left, 64 << 20))
            if not chunk:
                break
            left -= len(chunk)
            data = tail + chunk
            for key, needle in needles.items():
                if needle in data:
                    found.add(key)
            tail = data[-longest:]
    return found


def run(machine, test):
    global m, t, original
    m, t = machine, test
    with t.step("s1: consumers read their secrets on a real boot"):
        m.start()
        boot()
        original = out(f"cat {CONFIG}")
        said = m.succeed("archix secrets init", what="archix secrets init")
        machine_key = re.search(r'machine_key = "(age1[0-9a-z]+)"', said).group(1)
        m.succeed("age-keygen -o /root/admin.key 2>/dev/null", what="making an admin key")
        admin = out("age-keygen -y /root/admin.key").strip()
        set_config(secrets_config(machine_key, admin, ITEMS, CONSUMERS))
        wg = wg_key("wg0")
        set_secret("wg0", f"[Interface]\nPrivateKey = {wg}\nAddress = 10.99.0.1/24\n")
        nm_key = wg_key("nm")
        set_secret("nm", nm_profile(nm_key))
        set_secret("api", marker("api", 24))
        nb = marker("nb", 12)
        set_secret("nb", nb)
        password = marker("password", 20)
        pw_hash = out(f"openssl passwd -6 {shlex.quote(password)}").strip()
        markers.pop("password")
        markers["pw"] = pw_hash
        set_secret("pw", pw_hash)
        set_secret("swapcheck", marker("swapcheck", 32))
        switched = m.succeed(f"{HOST} switch --noconfirm", what="the switch with secrets")
        t.check("secret     wg0: new" in switched and pw_hash not in switched, f"the switch names the secrets, never a value: {switched[-1500:]}")
        first = current()
        m.reboot()
        boot()
        t.check(ok(f"systemctl is-active -q {SECRETS_UNIT}"), f"{SECRETS_UNIT} should have decrypted every secret: {out(f'journalctl -b -u {SECRETS_UNIT} --no-pager | tail')}")
        t.check(out("findmnt -no FSTYPE /run/archix/secrets").strip() == "ramfs", "the secrets should be on a ramfs")
        t.check(out("readlink /etc/wireguard/wg0.conf").strip() == "/run/archix/secrets/wg0", "wg0.conf should be a link into the ramfs")
        up = m.execute("wg-quick up wg0 2>&1")
        t.check(up[0] == 0 and out("wg show wg0 private-key").strip() == wg, f"wg-quick should bring wg0 up through the link: {up[1][-600:]}")
        t.log(f"wg-quick on the link: {'warns' if 'world accessible' in up[1] else 'no warning'} about permissions")
        m.execute("wg-quick down wg0")
        t.check(out("findmnt -no FSTYPE /run/NetworkManager/system-connections/home.nmconnection").strip() == "ramfs", "the NetworkManager profile should be the ramfs file, bound there")
        m.succeed("nmcli connection reload")
        t.check(out("nmcli -g connection.id connection show home 2>&1").strip() == "home", f"NetworkManager should load the profile from /run: {out('nmcli connection show 2>&1')}")
        t.check(ok("systemctl start archix-test-cred.service"), f"LoadCredential= should get api: {out('journalctl -b -u archix-test-cred --no-pager | tail -5')}")
        t.check(ok("systemctl start archix-test-env.service"), f"EnvironmentFile= should get nb through the template: {out('journalctl -b -u archix-test-env --no-pager | tail -5')}")
        t.check(out("stat -c '%a %U' /run/archix/secrets/api").strip() == "400 alice", "api should be alice's, 0400")
        # The password goes in once su has turned echo off and asked for it.
        login = m.execute(f"(sleep 3; printf '%s\\n' {shlex.quote(password)}) | SHELL=/bin/sh setpriv --reuid=nobody --regid=nobody --clear-groups script -qec 'su alice -c \"id -un\"' /dev/null 2>&1")
        t.check(login[1].split()[-1:] == ["alice"], f"alice should log in with su and the password the crypt_hash secret sets: {login[1][-300:].replace(password, '<password>')}")
        t.check(not ok("su nobody -s /bin/sh -c 'cat /run/archix/secrets/wg0' 2>/dev/null"), "another user can't read a root secret")
        sweep("s1", lambda name, place: name == "pw" and shadow_place(place))

    with t.step("s2: an older entry boots its own values"):
        new_api = marker("api2", 24)
        set_secret("api", new_api)
        m.succeed(f"{HOST} switch --noconfirm", what="the switch with a new api value")
        second = current()
        m.reboot()
        boot()
        t.check(values_hash("api") == sha(new_api), "the new deployment boots the new value")
        m.select_entry(f"archix-{first}.conf")
        m.reboot()
        selected, running = boot()
        t.check(running == first, f"the older entry should boot deployment {first}, not {running}")
        t.check(values_hash("api") == sha(markers["api"]), "the older deployment boots its own, older value")
        t.check(values_hash("wg0") == sha(f"[Interface]\nPrivateKey = {markers['wg0']}\nAddress = 10.99.0.1/24\n"), "and every other secret as it was")
        m.succeed(f"archix rollback {second} --noconfirm")
        m.reboot()
        boot()
        sweep("s2", lambda name, place: name == "pw" and shadow_place(place))

    with t.step("s3: without the identity the boot goes on, consumers fail closed, counting doesn't fall back; the admin key recovers"):
        m.succeed(f"{HOST} install tree --noconfirm", what="archix install tree")
        counted = current()
        t.require(ok(f"ls /efi/loader/entries/archix-{counted}+3.conf"), "the new entry should count its tries")
        m.succeed("mv /var/lib/archix/secrets/identity /root/identity.lost")
        m.reboot()
        selected, running = boot(allowed_failed=(SECRETS_UNIT,))
        t.check(running == counted, f"the new deployment should boot though no secret decrypts, not {running}")
        t.check(ok(f"systemctl is-failed -q {SECRETS_UNIT}"), f"{SECRETS_UNIT} should fail")
        t.check(not ok("systemctl start archix-test-cred.service"), "LoadCredential= fails closed")
        t.check(not ok("cat /etc/wireguard/wg0.conf > /dev/null 2>&1"), "the link dangles")
        notes = out("cat /var/lib/archix/notes")
        t.check("no secret was decrypted at this boot" in notes, f"the login note should say so: {notes}")
        m.wait_until_succeeds(f"ls /efi/loader/entries/archix-{counted}.conf", timeout=120, what="the boot should be blessed, as secrets aren't part of boot counting")
        t.check(current() == counted, f"current should stay {counted}")
        said = m.succeed("archix secrets init", what="a new identity after the loss")
        new_key = re.search(r'machine_key = "(age1[0-9a-z]+)"', said).group(1)
        admin = out("age-keygen -y /root/admin.key").strip()
        set_config(secrets_config(new_key, admin, ITEMS, CONSUMERS))
        m.succeed(f"ARCHIX_AGE_IDENTITY=/root/admin.key {HOST} secrets rekey", what="the admin key re-encrypts to the new machine key")
        m.succeed(f"{HOST} switch --noconfirm", what="the switch after the rekey")
        m.succeed("rm /root/identity.lost")
        m.reboot()
        boot()
        t.check(ok(f"systemctl is-active -q {SECRETS_UNIT}") and values_hash("api") == sha(markers["api2"]), "after the recovery every secret decrypts again")
        sweep("s3", lambda name, place: name == "pw" and shadow_place(place))

    with t.step("s4: a damaged ciphertext costs only its secret"):
        sha_nb = json.loads(out("cat /usr/lib/archix/secrets/manifest.json"))["secrets"]["nb"]["sha256"]
        m.succeed(f"mount -o remount,bind,rw /usr && printf X | dd of=/usr/lib/archix/secrets/{sha_nb}.age bs=1 seek=200 conv=notrunc 2>/dev/null; mount -o remount,bind,ro /usr")
        m.reboot()
        boot(allowed_failed=(SECRETS_UNIT, "archix-test-env.service"))
        t.check(not ok("test -e /run/archix/secrets/nb") and ok("test -s /run/archix/secrets/api"), "only the damaged secret is missing")
        status = out(f"{HOST} status")
        t.check("secret nb is missing at this boot" in status, f"status should name it: {status}")
        m.succeed(f"{HOST} remove tree --noconfirm", what="a switch that rebuilds the deployment's files")
        m.reboot()
        boot()
        t.check(ok("test -s /run/archix/secrets/nb"), "the next deployment has it whole again")
        sweep("s4", lambda name, place: name == "pw" and shadow_place(place))

    with t.step("s9: a crash of the decrypt step with values in memory leaves no core dump"):
        crashed = m.execute("strace -f -o /dev/null -e inject=linkat,rename:signal=SEGV:when=1 archix secrets activate > /dev/null 2>&1; echo $?")
        t.check(crashed[1].strip() == "139", f"the step should die of SIGSEGV with its values in memory: exit {crashed[1].strip()}")
        m.execute("sleep 3")
        t.check(not ok("ls /var/lib/systemd/coredump/*archix* 2>/dev/null"), f"no core of archix should be stored: {out('ls -l /var/lib/systemd/coredump; coredumpctl list --no-pager 2>&1 | tail -3')}")
        m.succeed(f"systemctl restart {SECRETS_UNIT}")
        sweep("s9", lambda name, place: name == "pw" and shadow_place(place))

    with t.step("s8: swap under memory pressure; hibernation refused while it's unencrypted"):
        m.succeed(f"mkswap -q {SWAP}")
        set_config(secrets_config(new_key, admin, ITEMS, CONSUMERS) + "\n" + SWAP_CONFIG)
        m.succeed(f"{HOST} switch --noconfirm", what="the switch with swap")
        m.reboot()
        boot()
        t.check(SWAP in out("swapon --show=NAME --noheadings"), f"the swap should be on: {out('swapon --show')}")
        dropin = out("cat /run/systemd/sleep.conf.d/zz-archix-pending-deployment.conf 2>&1")
        t.check("AllowHibernation=no" in dropin and "secrets" in dropin, f"hibernation should be off for the secrets: {dropin}")
        can = out("busctl call org.freedesktop.login1 /org/freedesktop/login1 org.freedesktop.login1.Manager CanHibernate 2>&1")
        t.check('"yes"' not in can, f"logind shouldn't offer hibernation: {can}")
        status = out(f"{HOST} status")
        t.check("hibernation is off while secrets are declared and swap isn't encrypted" in status, f"status should say why: {status}")
        used = m.succeed("mount -t tmpfs -o size=2400M tmpfs /mnt && (dd if=/dev/urandom of=/mnt/fill bs=1M count=1700 2>/dev/null; true); free -m | awk '/Swap/ { print $3 }'; rm -f /mnt/fill; umount /mnt", timeout=1800, what="memory pressure")
        t.check(int(used.strip() or 0) > 100, f"the pressure should have used swap: {used.strip()} MiB")
        set_config(secrets_config(new_key, admin, ITEMS, CONSUMERS, more=", allow_hibernation = true") + "\n" + SWAP_CONFIG)
        m.succeed(f"{HOST} switch --noconfirm", what="the switch allowing hibernation")
        m.reboot()
        boot()
        t.check(not ok("test -e /run/systemd/sleep.conf.d/zz-archix-pending-deployment.conf"), "allow_hibernation lets it hibernate")
        sweep("s8", lambda name, place: name == "pw" and shadow_place(place))
        m.shutdown()

    with t.step("raw: the root and swap partitions hold no value, freed blocks included"):
        needles = {name: value.encode() for name, value in markers.items()}
        for name, offset, size in raw_partitions():
            found = raw_grep(name, offset, size, needles)
            for key in sorted(found):
                consumer = key in ("nm", "nb")
                if name == "root" and key == "pw":
                    continue
                if name == "swap" and consumer:
                    t.log(f"swap holds {key}, which a long-running consumer (NetworkManager, or systemd for EnvironmentFile=) keeps in its own memory; that's outside archix's invariant")
                    continue
                t.check(False, f"the raw {name} partition holds marker {key}")
            t.log(f"raw {name}: {len(found)} of {len(needles)} markers found: {sorted(found)}")
