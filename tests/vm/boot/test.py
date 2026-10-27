# The boot suite (tests/vm/vmtest.sh) on the driver: the first layout on a booted system, deployments, boot entries, boot counting, Secure Boot, the /etc merge, the fallback UKI and eject.
import difflib
import re
import time

LOADER = "/efi/loader/loader.conf"
CONFIG = "/etc/archix/init.lua"
STATE = "/var/lib/archix-vmtest"
EFIVAR = "/sys/firmware/efi/efivars/LoaderEntrySelected-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f"
HOST = "archix --host archix-vm"


def diff(old, new):
    return "".join(difflib.unified_diff(old.splitlines(True), new.splitlines(True)))[:600]


def number(text):
    words = text.split()
    return int(words[0]) if words and words[0].isdigit() else -1


def run(m, t):
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
        return out(f"archix-check-deployment {n} | grep -E ': /(usr|opt|boot)/' || true")

    def edit_config(sed):
        m.succeed(f"sed -i {sed} {CONFIG}")

    def secure_boot():
        return ok("bootctl status 2> /dev/null | grep -q 'Secure Boot: enabled (user)'")

    def network():
        if ok("/usr/lib/systemd/systemd-networkd-wait-online --timeout=90 > /dev/null 2>&1", 150):
            return
        nic_state = "for nic in /sys/class/net/en*; do echo \"$(basename $nic) carrier=$(cat $nic/carrier 2>&1) carrier_changes=$(cat $nic/carrier_changes) operstate=$(cat $nic/operstate) flags=$(cat $nic/flags)\"; done"
        t.log("networkctl: " + out("networkctl list --no-pager 2>&1"))
        t.log("networkd: " + out("journalctl -b -u systemd-networkd --no-pager | tail -30"))
        t.log("nic at the report: " + out(nic_state) + out("for nic in /sys/class/net/en*; do ip -d link show $(basename $nic) 2>&1; done"))
        t.log("kernel: " + out("journalctl -b -k --no-pager | grep -i -E 'virtio|net|enp|eth|link' | tail -30"))
        dev = re.search(r"^(\S+).*\[virtio-net\]", m.hmp("info virtio"), re.M)
        t.log("qemu: " + m.hmp("info network") + "".join(m.hmp(f"info virtio-queue-status {dev.group(1)} {q}") for q in (0, 1) if dev))
        m.execute("for nic in /sys/class/net/en*; do ip link set $(basename $nic) down; ip link set $(basename $nic) up; done")
        if ok("/usr/lib/systemd/systemd-networkd-wait-online --timeout=30 > /dev/null 2>&1", 60):
            t.log("nic: a link down/up brought it back")
        else:
            t.log("nic: still not configured 30 s after a link down/up; the host's set_link off and on")
            m.hmp("set_link net0 off")
            time.sleep(2)
            m.hmp("set_link net0 on")
            configured = ok("/usr/lib/systemd/systemd-networkd-wait-online --timeout=40 > /dev/null 2>&1", 70)
            t.log(f"nic: after the host's set_link off/on {'it is' if configured else 'still not'} configured; qemu: {m.hmp('info network')}")
        t.log("nic at the end: " + out(nic_state))
        t.require(False, "networkd didn't configure the NIC's link within 90 s of this stage starting")

    def boot(match=True):
        m.execute("systemctl is-system-running --wait > /dev/null", 900)
        selected = out(f"tail -c +5 {EFIVAR} 2>/dev/null | tr -d '\\000'").strip()
        found = re.search(r"archix-(?:fallback-)?([0-9]+)", selected)
        booted = found.group(1) if found else ""
        running = out("cat /run/archix/deployment 2>/dev/null").strip()
        t.log(f"booted entry {selected}, mounted deployment {running}, current {current()}, deployments {deployments()}")
        failed = out("systemctl --failed --no-legend --plain | awk '{ print $1 }'").split()
        if failed:
            for unit in failed:
                t.log(f"{unit}: " + out(f"journalctl -b -u {unit} --no-pager | tail -20"))
            t.log("networkctl: " + out("networkctl list --no-pager 2>&1"))
            t.require(False, f"units failed at this boot: {' '.join(failed)}")
        network()
        if match:
            t.require(booted == running, f"the initramfs mounted deployment {running}, but systemd-boot started {selected}")
        return selected, booted, running

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
        m.reboot()

    with t.step("1: the layout boots deployment 2 read-only; a switch builds deployment 3 beside it"):
        boot()
        t.check(current() == "2", "the first switch should make deployment 2 current")
        t.check(deployments() == "1 2 ", f"the first switch should lay out deployment 1 and build 2: {deployments()}")
        usr = out("findmnt -no SOURCE /usr")
        t.check("archix/deploy/2/usr" in usr, f"/usr should be deployment 2's: {usr}")
        t.check(not ok("touch /usr/archix-vmtest"), "/usr should be read-only")
        t.check(not ok("touch /boot/archix-vmtest"), "/boot should be read-only")
        options = out("findmnt -no OPTIONS /archix")
        t.check("ro" in options.strip().split(","), f"/archix should be a read-only mount: {options}")
        rescue = out("sed -nE 's#^(efi|uki) /#/efi/#p' /efi/loader/entries/archix-rescue.conf 2> /dev/null").strip()
        t.check(rescue != "" and ok(f"[ -f {rescue} ]"), f"the first switch should leave the rescue entry and its UKI: {out('ls /efi/loader/entries')}")
        t.check("archix-rescue" not in loader(), f"the rescue entry must never be loader.conf's default: {loader()}")
        for attempt in ("chmod -R 700 /archix/deploy/1/usr/bin", "chown -R nobody /archix/deploy/1/usr/bin",
                        "setfacl -R -m u:nobody:rwx /archix/deploy/1/usr/bin", "setcap cap_net_raw+ep /archix/deploy/1/usr/bin/pacman"):
            t.check(not ok(attempt), f"{attempt} got through /archix")
        changed = findings(1)
        t.check(changed == "", f"deployment 1 changed through /archix: {changed}")
        t.check(ok("archix verify"), "archix verify should find deployments 1 and 2 as their packages installed them")
        t.check(hidden_usr() == 0, "the first boot after the layout should have emptied the root's own /usr, which deployments hide")
        t.check(ok("systemctl is-active -q archix-protected.service"), "the ProtectSystem=strict service should run on deployment 2")
        attrs = out(f"od -An -tu4 -N4 {EFIVAR} 2>/dev/null | tr -d ' '").strip()
        t.check(attrs.isdigit() and int(attrs) & 1 == 0, f"LoaderEntrySelected should be volatile, but its attributes are {attrs or 'missing'}")
        t.check(ok("systemctl is-active -q archix-marker.service"), "deployment 2 should start the marker")
        edit_config("'/-- marker/d'")
        m.succeed(f"{HOST} install tree --noconfirm", what="archix install tree")
        t.check(current() == "3", "installing tree should make deployment 3 current")
        t.check(ok("[ -x /archix/deploy/3/usr/bin/tree ]"), "deployment 3 should have tree")
        t.check(not ok("[ -e /archix/deploy/2/usr/bin/tree ]"), "the switch changed deployment 2")
        t.check(deployments() == "2 3 ", f"keep = 2 should keep only the booted deployment 2 and the new 3: {deployments()}")
        t.check(not ok("command -v tree"), "tree should arrive with the next boot, not now")
        status = out(f"{HOST} status")
        t.check("waiting for a reboot (deployment 3): tree " in status, f"status should name tree as waiting for a reboot: {status}")
        t.check(ok("grep -q 'waiting for a reboot (deployment 3)' /var/lib/archix/notes"), "the login notes should say updates wait for a reboot")
        t.check(ok("systemctl is-active -q archix-marker.service"), "the running system should keep the marker until the next boot")
        text = loader()
        t.check(has_line(text, "preferred archix-3.conf") and has_line(text, "default archix-2.conf"), f"loader.conf should prefer deployment 3 and fall back to 2: {text}")
        m.reboot()

    with t.step("2: deployment 3 boots with tree and without the marker"):
        _, booted, _ = boot()
        t.check(booted == "3" and current() == "3", "the restart should boot deployment 3")
        t.check(ok("command -v tree"), "deployment 3 should have tree")
        t.check(not ok("systemctl is-enabled -q archix-marker.service"), "deployment 3 drops the marker")
        t.check(not ok("grep -qs 'waiting for a reboot' /var/lib/archix/notes"), "the boot of deployment 3 should take the waiting-updates note away")
        m.select_entry("archix-2.conf")
        m.reboot()

    with t.step("3: booting an older entry makes it current; a rollback repoints to 3"):
        _, booted, _ = boot()
        t.check(booted == "2", "should have booted deployment 2's entry")
        t.check(current() == "2", "booting deployment 2's entry should make it current")
        text = loader()
        t.check(has_line(text, "default archix-2.conf") and not re.search(r"^preferred", text, re.M), f"deployment 2 should be the default now: {text}")
        t.check(not ok("command -v tree"), "deployment 2 has no tree")
        t.check(ok("systemctl is-active -q archix-marker.service"), "deployment 2 should start the marker")
        m.succeed("archix rollback 3 --noconfirm", what="archix rollback 3")
        t.check(current() == "3", "rolling back to deployment 3 should make it current")
        t.check(has_line(loader(), "default archix-3.conf"), f"rolling back to deployment 3 should make it the default: {loader()}")
        t.check(deployments() == "2 3 ", f"a rollback to a kept deployment builds nothing: {deployments()}")
        m.reboot()

    with t.step("4: the rollback boots 3; a power cut between a rollback's boot default and its current"):
        _, booted, _ = boot()
        t.check(booted == "3", "the rollback should boot deployment 3")
        t.check(ok("command -v tree"), "deployment 3 should have tree")
        baseline = findings(2)
        m.succeed(f"sed -i -e '/^preferred /d' -e 's/^default .*/default archix-2.conf/' {LOADER}")
        t.require(has_line(loader(), "default archix-2.conf"), f"loader.conf should name deployment 2: {loader()}")
        t.require(current() == "3", "current should still be 3")
        m.succeed("sync -f /efi")
        m.crash()

    with t.step("4b: after the cut deployment 2 boots whole and current; Secure Boot set up"):
        _, booted, _ = boot()
        t.check(booted == "2", f"after a crash between the rollback's two writes, deployment 2's entry should boot, not {booted}")
        t.check(current() == "2", "booting deployment 2 should have made it current after the crash")
        t.check(has_line(loader(), "default archix-2.conf"), f"deployment 2 should stay the default: {loader()}")
        now = findings(2)
        t.check(now == baseline, f"deployment 2's files changed: {diff(baseline, now)}")
        t.check(ok("systemctl is-active -q archix-marker.service"), "deployment 2 should start the marker")
        m.succeed("archix secure-boot setup", what="archix secure-boot setup")
        edit_config("'s|boot = { loader = \"systemd-boot\" }|boot = { loader = \"systemd-boot\", secure_boot = true, protected_entry = true }|'")
        m.succeed(f"{HOST} switch --noconfirm", what="switch with Secure Boot")
        m.succeed(f"mkdir -p {STATE} && touch {STATE}/bless-fault")
        m.reboot()

    with t.step("5: Secure Boot, blessing on a read-only ESP, unsigned switches refused; a power cut mid-switch"):
        _, booted, _ = boot()
        t.check(secure_boot(), "Secure Boot should be enforcing")
        t.check(not ok("systemctl is-failed -q archix-boot-complete.service"), f"archix-boot-complete.service failed: {out('journalctl -u archix-boot-complete.service -b --no-pager | tail -5')}")
        c = current()
        text = loader()
        t.check(has_line(text, f"default archix-{c}.conf") and not re.search(r"^preferred", text, re.M), f"once deployment {c} booted fine, it should be the default and nothing preferred: {text}")
        m.succeed(f"rm -f {STATE}/bless-fault")
        bless = out("journalctl -b -u systemd-bless-boot.service --no-pager")
        tail = "\n".join(bless.splitlines()[-8:])
        t.check(re.search(rf"renaming archix-{booted}\+[0-9-]+\.conf to archix-{booted}\.conf.*Read-only file system", bless),
                f"archix bless should have failed to rename the counted entry on the read-only ESP: {tail}")
        t.check(ok("systemctl is-active -q systemd-bless-boot.service"), f"systemd-bless-boot should succeed after archix bless failed: {tail}")
        entries = out("ls /efi/loader/entries | tr '\\n' ' '")
        t.check(ok(f"[ -f /efi/loader/entries/archix-{booted}.conf ]"), f"systemd-bless-boot should have blessed deployment {booted}'s entry: {entries}")
        t.check("+" not in entries, f"no entry should keep or gain boot counters: {entries}")
        t.check(current() == booted, f"deployment {booted} should stay current after archix bless failed, not {current()}")
        t.check(not ok("grep -qs \"couldn't count this boot\" /var/lib/archix/notes"), f"the login notes shouldn't say the boot wasn't counted: {out('cat /var/lib/archix/notes')}")
        t.check(ok("archix secure-boot check"), "secure-boot check should pass")
        t.check(not ok("archix secure-boot check --anticheat"), "the anti-cheat check can't pass with a self-signed kernel")
        t.check(ok(f"[ -f /efi/loader/entries/archix-{current()}-protected.conf ]"), "protected_entry should write a protected entry")
        edit_config("'s|secure_boot = true|secure_boot = false|'")
        before, kept = current(), deployments()
        t.require(not ok(f"{HOST} install jq --noconfirm"), "an unsigned switch must be refused under Secure Boot")
        t.check(current() == before, "the refused switch changed the current deployment")
        t.check(deployments() == kept, f"the refused switch left a deployment behind: {deployments()}")
        t.check(not ok("ls -d /archix/deploy/.staging-*"), f"the refused switch left its staging tree: {listing()}")
        edit_config("'s|secure_boot = false|secure_boot = true|'")
        m.succeed(f"{HOST} install jq --noconfirm > /dev/console 2>&1 &")
        m.wait_until_succeeds("ls -d /archix/deploy/.staging-*", timeout=600, interval=0.2, what="the switch never started staging")
        time.sleep(1)
        m.crash()

    with t.step("6: the cut switch left the old deployment; a kernel that panics gets three tries"):
        _, booted, _ = boot()
        t.check(booted == before and current() == before, f"after a power cut mid-switch, deployment {before} should still boot and be current")
        t.check(deployments() == kept, f"the cut switch committed a deployment: {listing()}; deployments before {kept}, now {deployments()}")
        m.succeed(f"{HOST} install jq --noconfirm", what="the switch after the power cut")
        t.check(number(current()) > number(before), "installing jq should make a newer deployment")
        t.check(not ok("ls -d /archix/deploy/.staging-*"), f"the next switch should clear the cut switch's staging tree: {listing()}")
        good = booted
        edit_config("'s|\"console=ttyS0\" }|\"console=ttyS0\", \"init=/usr/bin/false\", \"panic=3\" }|'")
        m.succeed(f"{HOST} switch --noconfirm", what="switch to a deployment that can't boot")
        bad = current()
        t.check(ok(f"[ -f /efi/loader/entries/archix-{bad}+3.conf ]"), f"the new entry should count three boot tries: {out('ls /efi/loader/entries')}")
        text = loader()
        t.check(has_line(text, f"preferred archix-{bad}.conf") and has_line(text, f"default archix-{good}.conf"), f"loader.conf should prefer deployment {bad} and fall back to {good}: {text}")
        m.reboot()

    with t.step("7: systemd-boot falls back after three failed boots; keep = 1 and revoke_unused"):
        m.wait_for_shell(timeout=1500)
        _, booted, _ = boot()
        t.log(out("ls /efi/loader/entries"))
        t.check(booted == good, f"after three failed boots of deployment {bad}, systemd-boot should start {good}, not {booted}")
        t.check(current() == good, f"archix should make deployment {good} current after {bad} failed to boot")
        t.check(ok(f"[ -f /efi/loader/entries/archix-{bad}+0-3.conf ]"), f"deployment {bad}'s entry should be marked bad: {out('ls /efi/loader/entries')}")
        t.check(ok(f"grep -q \"generation {bad} didn't finish booting\" /var/lib/archix/notes"), f"the login note should say {bad} didn't boot: {out('cat /var/lib/archix/notes')}")
        t.check(not re.search(r"^preferred", loader(), re.M), f"falling back to {good} should drop preferred: {loader()}")
        edit_config("'s|, \"init=/usr/bin/false\", \"panic=3\" }| }|'")
        edit_config("'s|\"console=ttyS0\" }|\"console=ttyS0\", \"archix.revoketest\" }|'")
        m.succeed(f"{HOST} switch --noconfirm", what="switch to a deployment with a UKI of its own")
        dropped = current()
        own = out(f"ls /efi/loader/entries/archix-{dropped}.conf /efi/loader/entries/archix-{dropped}+*.conf 2> /dev/null | head -1").strip()
        revoked = out(f"sed -nE 's#^(efi|uki) /#/efi/#p' {own}").strip()
        m.succeed(f"cp {revoked} /var/tmp/revoked.efi", what=f"copying {revoked}")
        kek = "/var/lib/sbctl/keys/KEK"
        m.succeed("hash-to-efi-sig-list /usr/lib/systemd/boot/efi/linuxx64.efi.stub /var/tmp/foreign.esl > /dev/null", what="hashing the stand-in")
        m.succeed(f"sign-efi-sig-list -a -k {kek}/KEK.key -c {kek}/KEK.pem dbx /var/tmp/foreign.esl /var/tmp/foreign.auth > /dev/null", what="signing the stand-in")
        m.succeed("efi-updatevar -a -f /var/tmp/foreign.auth dbx", what="appending the stand-in to dbx")
        foreign = out("od -An -tx1 -j44 -N32 /var/tmp/foreign.esl | tr -d ' \\n'").strip()
        m.succeed("hash-to-efi-sig-list /var/tmp/revoked.efi /var/tmp/revoked.esl > /dev/null", what="hashing the copy")
        target = out("od -An -tx1 -j44 -N32 /var/tmp/revoked.esl | tr -d ' \\n'").strip()
        t.log("efivarfs before revoking: " + out("stat -f -c 'blocks=%b free=%f size=%S' /sys/firmware/efi/efivars"))
        edit_config("'s|, \"archix.revoketest\" }| }|; s|protected_entry = true }|protected_entry = true, revoke_unused = true },\\n  deployments = { keep = 1 }|'")
        m.succeed(f"{HOST} switch --noconfirm", what="switch with deployments.keep = 1")
        t.check(deployments() == f"{good} {current()} ", f"keep = 1 should leave only the booted and the current deployment: {deployments()}")
        generations = out("archix generations")
        t.check(any(re.match(rf"^ +{dropped} ", line) and "record" in line for line in generations.splitlines()), f"deployment {dropped}'s generation should stay as a record: {generations}")
        t.check(not ok(f"[ -e {revoked} ]"), f"dropping deployment {dropped} should take {revoked} off the ESP")
        dbx = out("efi-readvar -v dbx").lower()
        t.check(foreign != "" and foreign in dbx, f"the entry archix didn't write should still be in dbx: {dbx}")
        t.check(target != "" and target in dbx, f"dbx should list the dropped UKI {target}: {dbx}")
        t.log("efivarfs after revoking: " + out("stat -f -c 'blocks=%b free=%f size=%S' /sys/firmware/efi/efivars"))
        t.check("nothing to revoke" in out("archix secure-boot revoke --dry-run 2>&1"), "revoke should find nothing left")
        m.succeed("mkdir -p /efi/EFI/revoketest && cp /var/tmp/revoked.efi /efi/EFI/revoketest/revoked.efi")
        m.succeed("printf 'title revoked\\nefi /EFI/revoketest/revoked.efi\\n' > /efi/loader/entries/revoketest.conf")
        m.succeed(f"echo 'reboot-on-error yes' >> {LOADER}")
        before_revoked = current()
        m.select_entry("revoketest.conf")
        m.reboot()

    with t.step("8: the firmware refuses the revoked UKI"):
        selected, _, _ = boot()
        t.check(selected != "revoketest.conf", "the firmware should refuse the revoked UKI")
        t.check(current() == before_revoked, "the refused boot shouldn't change the current deployment")
        t.check(secure_boot(), "Secure Boot should be enforcing")
        m.succeed(f"rm -rf /efi/EFI/revoketest /efi/loader/entries/revoketest.conf && sed -i '/^reboot-on-error/d' {LOADER}")
        m.reboot()

    with t.step("etc-merge: the watcher carries live /etc changes into the pending deployment"):
        _, booted, _ = boot()
        m.succeed(f"{HOST} install zsh --noconfirm", what="archix install zsh")
        pending = current()
        t.require(pending != booted, "installing zsh should leave a deployment pending")
        t.check(ok(f"grep -qx /bin/zsh /archix/deploy/{pending}/etc/shells"), "zsh's scriptlet should add it to the staged /etc/shells")
        merge_active = "systemctl is-active -q archix-etc-merge.service"
        t.check(ok(f"for _ in $(seq 50); do {merge_active} && break; sleep 0.2; done; {merge_active}"),
                f"the commit should start archix-etc-merge.service: {out('systemctl status archix-etc-merge.service --no-pager | tail -5')}")
        m.succeed("echo 'root:etc-merge-vmtest' | chpasswd", what="chpasswd")
        shadow = out("grep '^root:' /etc/shadow").strip()
        m.succeed("mkdir -p /etc/NetworkManager/system-connections && (umask 077 && printf '[connection]\\nid=vmtest\\ntype=wifi\\n\\n[wifi-security]\\npsk=vmtest-secret\\n' > /etc/NetworkManager/system-connections/.vmtest.nmconnection.T3MP)")
        m.succeed("mv /etc/NetworkManager/system-connections/.vmtest.nmconnection.T3MP /etc/NetworkManager/system-connections/vmtest.nmconnection")
        m.succeed("mkdir -p /etc/vmtest-dir/sub && echo 'made after the switch' > /etc/vmtest-dir/sub/file && echo /bin/vmtest-shell >> /etc/shells")
        m.execute(f"for _ in $(seq 50); do grep -qx /bin/vmtest-shell /archix/deploy/{pending}/etc/shells && break; sleep 0.2; done")
        # The copy lands before the merge records it for status, which a status read 0.1 s later missed on btrfs.
        counted = f"grep -q '/etc changes carried into the pending deployment {pending}' <<< \"$s\" && grep -q '/etc/shells: changed on the running system' <<< \"$s\""
        status = out(f"for _ in $(seq 50); do s=$({HOST} status 2>&1); {counted} && break; sleep 0.2; done; echo \"$s\"")
        if not t.check(f"/etc changes carried into the pending deployment {pending}" in status and "/etc/shells: changed on the running system" in status,
                       "status should count the carried changes and the conflict"):
            t.log("status: " + status)
            t.log("journal: " + out("journalctl -b -u archix-etc-merge.service --no-pager | tail -30"))
            t.log("state: " + out("ls -la /var/lib/archix/etc-merge 2>&1"))
        m.reboot()

    with t.step("etc-merge-2: the pending deployment boots with the carried changes; a power cut in the merge's window"):
        _, booted, _ = boot()
        t.check(booted == pending, f"the restart should boot deployment {pending}")
        t.check(ok(f"grep -qxF '{shadow}' /etc/shadow"), f"the password changed after the switch didn't reach deployment {pending}")
        nm = "/etc/NetworkManager/system-connections/vmtest.nmconnection"
        mode = out(f"stat -c %a {nm} 2>&1").strip()
        t.check(ok(f"grep -qx psk=vmtest-secret {nm}") and mode == "600", f"the connection should arrive whole and 0600: {mode}")
        t.check(out("cat /etc/vmtest-dir/sub/file 2>/dev/null").strip() == "made after the switch", f"the new directory didn't reach deployment {pending}")
        t.check(ok("grep -qx /bin/vmtest-shell /etc/shells"), "the live edit to /etc/shells should win")
        t.check(ok("grep -qx /bin/zsh /etc/shells.pacnew"), f"deployment {pending}'s /etc/shells should be /etc/shells.pacnew: {out('ls /etc/shells* 2>&1')}")
        status = out(f"{HOST} status 2>&1")
        t.check("were carried from deployment" in status, f"status should report the merge after the boot: {status}")
        t.check(not ok("grep -q \"didn't shut down cleanly\" /var/lib/archix/notes 2>/dev/null"), "a clean reboot shouldn't leave the power-cut note")
        m.succeed(f"{HOST} install bc --noconfirm", what="archix install bc")
        pending = current()
        m.execute("for _ in $(seq 50); do systemctl is-active -q archix-etc-merge.service && break; sleep 0.2; done")
        m.succeed("echo 'written just before the power went' > /etc/vmtest-power-cut && sync -f /etc")
        m.crash()

    with t.step("etc-merge-3: a power cut inside the window leaves a note and the old /etc's last change"):
        _, booted, _ = boot()
        t.check(booted == pending, f"after the power cut deployment {pending} should boot")
        t.check(out("cat /etc/vmtest-power-cut 2>/dev/null").strip() == "written just before the power went", "the boot should carry what the old /etc held on disk")
        t.check(ok(f"grep -q \"didn't shut down cleanly before deployment {pending} first booted\" /var/lib/archix/notes"), f"the login note should name the window: {out('cat /var/lib/archix/notes')}")
        t.check("didn't shut down cleanly" in out(f"{HOST} status 2>&1"), "status should name the window")
        m.reboot()

    with t.step("fallback: only the current deployment's fallback UKI, signed; loader/entries deleted"):
        boot()
        c = current()
        uki = out("ls /efi/EFI/Linux | tr '\\n' ' '")
        t.check(out("ls /efi/EFI/Linux | grep '^archix-fallback-'").strip() == f"archix-fallback-{c}.efi", f"only deployment {c}'s fallback UKI should be on the ESP: {uki}")
        verify = out("sbctl verify 2>&1 | grep fallback")
        t.check(any(f"archix-fallback-{c}.efi" in line and "is signed" in line for line in verify.splitlines()), f"the fallback UKI should be signed: {verify}")
        m.succeed("rm -rf /efi/loader/entries && sync -f /efi")
        m.reboot()

    with t.step("9: without loader/entries the fallback UKI boots the current deployment"):
        selected, _, running = boot()
        c = current()
        t.check(selected == f"archix-fallback-{c}.efi", f"without loader/entries systemd-boot should start the fallback UKI of deployment {c}, not {selected}")
        t.check(running == c, f"the fallback UKI should boot deployment {c}, not {running}")
        t.check(ok("command -v tree"), f"deployment {c} should have tree")
        m.succeed(f"cp /efi/EFI/Linux/archix-fallback-{c}.efi /efi/EFI/Linux/archix-fallback-999.efi")
        m.succeed("printf 'uki /EFI/Linux/archix-fallback-999.efi\\n' > /var/lib/archix/boot/entries/archix-fallback-999.efi && sync -f /efi")
        m.select_entry("archix-fallback-999.efi")
        m.reboot()

    with t.step("10: the fallback UKI of a collected deployment boots current; eject killed after copying"):
        selected, _, running = boot(match=False)
        c = current()
        t.check(selected == "archix-fallback-999.efi", f"systemd-boot should have started archix-fallback-999.efi, not {selected}")
        t.check(running == c, f"the fallback of a deployment that's gone should boot current, {c}, not {running}")
        t.check(ok(f"journalctl -b --no-pager | grep -q 'booting deployment {c} (current;'"), "deploy-mount should say it booted current instead")
        m.succeed("rm /efi/EFI/Linux/archix-fallback-999.efi /var/lib/archix/boot/entries/archix-fallback-999.efi")
        packages = out("pacman -Q")
        qkk = out("pacman -Qkk 2>&1")
        m.succeed("archix eject --noconfirm > /dev/console 2>&1 &")
        phase = "grep -qx 'phase copied' /var/lib/archix/eject 2> /dev/null"
        t.require(ok(f"for _ in $(seq 3000); do {phase} && break; sleep 0.1; done; {phase}", 600), f"the eject never finished copying the trees: {out('cat /var/lib/archix/eject 2>&1')}")
        m.execute("pkill -9 -f 'archix eject'; sync -f /efi; sync -f /")
        t.check(ok("[ -d /archix/deploy ]"), "the killed eject shouldn't have got as far as deleting /archix")
        phases = out("tr '\\n' ' ' < /var/lib/archix/eject")
        t.log(f"killed the eject after it copied the trees ({phases}); loader.conf: {loader()}")
        m.reboot()

    with t.step("11: eject finishes after the kill and the restart"):
        selected, _, _ = boot()
        t.log(f"after the killed eject systemd-boot started {selected}")
        m.succeed("archix eject --noconfirm", what="running eject again should finish it")
        t.check(not ok("[ -e /archix ]"), f"eject should delete /archix: {out('ls -A /archix 2>&1')}")
        t.check("archix" not in out("findmnt -no SOURCE /usr"), f"/usr should be the root's own now: {out('findmnt -no SOURCE /usr')}")
        t.check(has_line(loader(), "default arch-linux.efi"), f"eject should make the plain UKI the default: {loader()}")
        t.check(not ok("ls /efi/loader/entries/archix-* /efi/EFI/Linux/archix-fallback-* > /dev/null 2>&1"), "eject left archix's boot entries")
        t.check(not ok("[ -e /var/lib/archix/eject ]"), "the finished eject left its state")
        t.check(ok("command -v tree"), "the running system lost tree")
        m.reboot()

    with t.step("12: plain Arch boots with the deployment's packages; plain pacman works"):
        selected, _, running = boot()
        t.check(selected == "arch-linux.efi", f"plain Arch should boot through arch-linux.efi, not {selected}")
        t.check(running == "" and not ok("[ -e /archix ]"), "a plain boot mounts no deployment")
        t.check("archix" not in out("findmnt -no SOURCE /usr"), "/usr should be the root's own")
        t.check(secure_boot(), "Secure Boot should still be enforcing")
        now = out("pacman -Q")
        t.check(now == packages, f"plain Arch has other packages than the deployment had: {diff(packages, now)}")
        now = out("pacman -Qkk 2>&1")
        t.check(now == qkk, f"pacman -Qkk differs from the deployment's: {diff(qkk, now)}")
        t.check(ok("systemctl is-active -q archix-protected.service"), "the ProtectSystem=strict service should run on plain Arch")
        for unit in ("archix-boot", "archix-boot-complete", "archix-deploy-activate", "archix-flatpak"):
            t.check(not ok(f"[ -e /etc/systemd/system/{unit}.service ]"), f"eject should remove {unit}.service")
        m.succeed("pacman -Syu --noconfirm", what="plain pacman -Syu after the eject")
        m.succeed("pacman -S --noconfirm cowsay", what="plain pacman -S after the eject")
        t.check(ok("command -v cowsay"), "pacman -S cowsay should install it")
        m.shutdown()
