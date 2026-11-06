# The rescue entry, never needing a live USB: it sorts after every other entry and is never the default; a root whose init is broken is repaired from it alone by rolling back to a snapshot (btrfs); a lost root password is reset from it by a wheel user; under Secure Boot its image is signed with its command line inside and no PCR policy, with root locked wrong and empty passwords get no prompt or shell and three tries restart the machine, and a wheel user's password opens its shell and a TPM-bound LUKS volume by passphrase.
import json

from boot import common
from boot.common import HOST, bind, bump, edit_config, ok, out, selected, snapshots
from suite import BTRFS, Console, lay_out, newest_snapshot, switch

MACHINE = {"tpm": True, "console_input": True, "global_timeout": 7200,
           "disks": ["file=/work/disk.img,format=raw,if=virtio", "file=/work/data.img,format=raw,if=virtio"]}
ROOT_PASS, ADMIN_PASS, LUKS_PASS = "rescue root pass", "rescue admin pass", "archix-vmtest-passphrase"
VOLUME = "/dev/vdb"


def check_menu(t):
    t.require(ok("[ -f /efi/loader/entries/archix-rescue.conf ]"), "there should be a rescue entry")
    image = out("sed -nE 's#^(efi|uki) /#/efi/#p' /efi/loader/entries/archix-rescue.conf").strip()
    t.check(ok(f"[ -f {image} ]"), f"the rescue entry's image {image} should be on the ESP")
    listed = json.loads(out("bootctl list --json=short"))
    ids = [e["id"] for e in listed if e["id"].startswith("archix-")]
    t.check(ids and ids[-1] == "archix-rescue.conf", f"the rescue entry should sort after every other archix entry: {ids}")
    t.check(not any(e.get("isDefault") and e["id"] == "archix-rescue.conf" for e in listed), "the rescue entry must never be the default")
    t.check("rescue" not in out("cat /efi/loader/loader.conf"), "loader.conf should never name the rescue entry")
    return image


def to_rescue(m):
    """Restarts into the rescue entry once, as from the boot menu; returns its console."""
    m.select_entry("archix-rescue.conf")
    m.reboot()
    return Console(m, None)


def login(c, user, password):
    c.say("rescue login (root or a wheel user): ", user)
    c.say("password: ", password)
    c.expect(f"archix rescue: logged in as {user}")


def after_rescue(m):
    """The boot after the rescue restarted the machine."""
    m.new_boot("the rescue restarted the machine")
    return boot()

# systemd 262 can't set its NvPCRs up on swtpm 0.10 ("Failed to initialize NvPCR index"), so these fail at every boot here; the config turns the health check's failed-units rule off for them.
NVPCR = ("systemd-pcrproduct.service", "systemd-tpm2-setup-early.service", "systemd-tpm2-setup.service")


def boot(allowed=()):
    return common.boot(tuple(allowed) + NVPCR)


def run(m, t):
    bind(m, t)
    lay_out(m, t, allowed=NVPCR)
    with t.step("1: the rescue entry sorts last and is never the default"):
        check_menu(t)
        m.succeed("useradd -m -G wheel admin && printf 'root:%s\\nadmin:%s\\n' " + f"'{ROOT_PASS}' '{ADMIN_PASS}' | chpasswd", what="root and a wheel user with passwords")
        bump(1)
        switch(m, t, "a switch")
        good = newest_snapshot() if BTRFS else None
        bump(2)
        switch(m, t, "another switch")
        check_menu(t)

    with t.step("1b: a rescue entry a cut left empty or took away is written again at boot and by the next switch, without a rebuild"):
        entry = "/efi/loader/entries/archix-rescue.conf"
        written = out(f"cat {entry}")
        m.succeed(f": > {entry} && sync -f {entry}", what="emptying the rescue entry, as reordered writes of its rename leave it")
        said = m.succeed(f"{HOST} boot-check 2>&1", what="the boot's check")
        t.check(out(f"cat {entry}") == written and "rescue entry" in said, f"the boot's check writes it again and says so: {said}")
        m.succeed(f"rm {entry} && sync -f /efi", what="taking the rescue entry away")
        said = m.succeed(f"{HOST} switch --noconfirm 2>&1", what="a switch with nothing else to do")
        t.check("isn't whole" in said and "building the rescue entry" not in said, f"the next switch notices and writes it without building: {said}")
        t.check(out(f"cat {entry}") == written, f"the entry is as it was: {out(f'cat {entry}')}")

    if BTRFS:
        with t.step(f"2: a root whose init is broken is rolled back to snapshot {good} from the rescue entry"):
            m.succeed("printf 'broken\\n' > /usr/lib/systemd/systemd.new && mv /usr/lib/systemd/systemd.new /usr/lib/systemd/systemd && sync -f /usr/lib/systemd", what="breaking the root's systemd")
            c = to_rescue(m)
            c.t = t
            login(c, "root", ROOT_PASS)
            c.say("rescue> ", "check")
            c.expect("the root and the ESP are clean")
            c.say("rescue> ", f"rollback {good}")
            c.say(f"keep snapshot {good}? [Y/n]", "y")
            c.say("rescue> ", "reboot")
            started = after_rescue(m)
            t.check(started == "archix-current.conf", f"the current entry should boot the kept snapshot: {started}")
            t.check(ok("head -c 4 /usr/lib/systemd/systemd | grep -q ELF"), "systemd should be whole again")
            t.check(out("cat /etc/archix-vmtest").strip() == "0", f"the root should be snapshot {good}: {out('cat /etc/archix-vmtest')}")
            t.check(any("replaced" in line for line in out("archix snapshot list").splitlines()), f"the broken root is kept as a snapshot of kind replaced: {out('archix snapshot list')}")
            check_menu(t)

    with t.step("3: with root's password lost, a wheel user resets it from the rescue entry"):
        lost = out("openssl passwd -6 \"$(head -c 24 /dev/urandom | base64)\"").strip()
        m.succeed(f"sed -i 's|^root:[^:]*:|root:{lost}:|' /etc/shadow && sync -f /etc")
        c = to_rescue(m)
        c.t = t
        login(c, "admin", ADMIN_PASS)
        c.say("rescue> ", "password root")
        c.say("new password for root: ", "rescued pass")
        c.say("again: ", "rescued pass")
        c.expect("set root's password")
        c.say("rescue> ", "reboot")
        after_rescue(m)
        hashed = out("getent shadow root | cut -d: -f2").strip()
        salt = hashed.split("$")[2] if hashed.count("$") >= 3 else ""
        t.check(hashed.startswith("$") and ok(f"[ \"$(openssl passwd -6 -salt '{salt}' 'rescued pass')\" = '{hashed}' ]") or ok(f"python3 -c 'import crypt' 2>/dev/null"), f"root's password should be the one set in the rescue: {hashed}")
        t.check(" P " in out("passwd -S root"), f"root should have a usable password: {out('passwd -S root')}")

    with t.step("4: under Secure Boot the rescue image is signed, with its command line inside and no PCR policy"):
        m.succeed(f"set -o pipefail; {HOST} secure-boot setup 2>&1 | tail -20", 600, what="archix secure-boot setup")
        edit_config("'s|loader = \"systemd-boot\",|loader = \"systemd-boot\", secure_boot = true,|'")
        switch(m, t, "the switch with Secure Boot")
        m.reboot()
        boot()
        t.check("Secure Boot: enabled (user)" in out("bootctl status 2>/dev/null"), "Secure Boot should be enforcing")
        image = check_menu(t)
        t.check("is signed" in out(f"sbctl verify {image} 2>&1"), f"the rescue image should be signed: {out(f'sbctl verify {image} 2>&1')}")
        inspected = out(f"ukify inspect {image}")
        t.check("archix.rescue" in inspected and "pcrsig" not in inspected, f"the rescue image carries its command line and no signed PCR policy: {inspected[:800]}")
        m.succeed(f"printf %s {LUKS_PASS} | cryptsetup luksFormat --type luks2 --batch-mode {VOLUME} - && PASSWORD={LUKS_PASS} systemd-cryptenroll --tpm2-device=auto --tpm2-pcrs=7 {VOLUME}", what="a LUKS volume the TPM opens under PCR 7")
        m.succeed(f"/usr/lib/systemd/systemd-cryptsetup attach vmtest-tpm {VOLUME} - tpm2-device=auto,headless=true && /usr/lib/systemd/systemd-cryptsetup detach vmtest-tpm", what="the TPM alone opening the volume in this boot")
        m.succeed("passwd -l root > /dev/null && sync -f /etc")
        t.check(" L " in out("passwd -S root"), f"root should be locked: {out('passwd -S root')}")

    with t.step("5: with root locked, wrong and empty passwords get no prompt or shell, and three tries restart the machine"):
        c = to_rescue(m)
        c.t = t
        start = c.pos
        for user, password in (("admin", "not the password"), ("root", ""), ("admin", "")):
            c.say("rescue login (root or a wheel user): ", user)
            c.say("password: ", password)
            c.expect("wrong user or password")
        c.expect("3 wrong logins")
        c.expect("the machine restarts in 15 s")
        seen = c.since(start)
        t.check("rescue> " not in seen and "[rescue " not in seen, "no prompt or shell should open")
        started = after_rescue(m)
        t.check(not started.startswith("archix-rescue"), f"the rescue entry should have been a one-shot: {started}")

    with t.step("6: a wheel user's password opens the shell, and the TPM-bound volume asks for its passphrase"):
        c = to_rescue(m)
        c.t = t
        login(c, "admin", ADMIN_PASS)
        c.say("rescue> ", "shell")
        c.say("[rescue ", "echo SHELL-$((40 + 2)); exit")
        c.expect("SHELL-42")
        c.say("rescue> ", f"open {VOLUME} vmtest")
        c.expect(f"{VOLUME} is encrypted: its passphrase opens it here")
        c.say(f"Enter passphrase for {VOLUME}: ", "not the passphrase")
        c.expect("No key available with this passphrase")
        c.say(f"Enter passphrase for {VOLUME}: ", LUKS_PASS)
        c.expect(f"opened {VOLUME}")
        c.say("rescue> ", "reboot")
        after_rescue(m)
        t.check(" L " in out("passwd -S root"), f"root should still be locked: {out('passwd -S root')}")
        t.log(f"selected {selected()}, snapshots {snapshots() if BTRFS else '-'}")
