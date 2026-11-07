# The split boot tests' base, run once per run with this run's archix: the image boots as a plain install, the first switch sets the root up for rollbacks and writes the boot menu, the boot after counts as good, and the VM powers off; each test boots a copy-on-write overlay of the disk and a copy of the vars.
import os

from boot.common import HOST, bind, boot, counts_tries, edit_config, entries, first_switch, has_line, loader, ok, out, state

# On ext4 rollbacks are off: the base and the ext4 lane's test manage the config only.
BTRFS = os.environ.get("FS", "ext4") == "btrfs"


def run(m, t):
    bind(m, t)
    with t.step("plain: the first switch sets rollbacks up and writes the menu"):
        m.start()
        boot()
        t.check(not ok("[ -d /.archix/snapshots ]"), "the image should boot as a plain install, without rollbacks yet")
        if not BTRFS:
            edit_config("'/  rollback = { enable = true },/d'")
        status = first_switch("/root/first-switch.log", 900)
        t.log("first switch: " + out("tail -c 3000 /root/first-switch.log; ps -ef --forest | tail -15"))
        t.require(status == 0, f"the first switch on the booted system exited {status}")
        t.check(counts_tries("archix-current.conf", 2), f"the first switch should write the current system's entry with two tries: {entries()}")
        t.check(ok("ls /efi/EFI/archix/*.efi"), f"and its image: {out('ls /efi/EFI/archix')}")
        t.check(ok("[ -f /efi/loader/entries/archix-rescue.conf ]"), "and the rescue entry")
        text = loader()
        t.check(has_line(text, "preferred archix-*") and has_line(text, "default archix-current[.~]*"), f"loader.conf should prefer archix's entries in sort-key order: {text}")
        # LoaderConfigTimeout, not loader.conf: no test waits on the menu.
        m.succeed("bootctl set-timeout 0")
        m.reboot()

    with t.step("1: the layout boots and the boot counts as good"):
        started = boot()
        t.check(started == "archix-current.conf", f"the reboot should start the current system's entry, not {started}")
        m.wait_until_succeeds("[ -f /efi/loader/entries/archix-current.conf ]", timeout=300, what="systemd-bless-boot should count the boot as good")
        if not BTRFS:
            m.shutdown()
            return
        t.check(out("findmnt -no FSROOT /var/lib").strip() == "/@varlib", f"/var/lib should be @varlib: {out('findmnt /var/lib')}")
        t.check(out("findmnt -no FSROOT /.archix").strip() == "/@archix", f"/.archix should be @archix: {out('findmnt /.archix')}")
        armed = ok("for i in $(seq 60); do grep -q '\"armed\": true' /.archix/boot/state.json && exit 0; sleep 2; done; exit 1", 200)
        t.check(armed, "the good boot should arm the fallback slot: " + out("cat /.archix/boot/state.json; ls -la /.archix/boot /.archix/boot/entries /var/lib/archix/boot; journalctl -b --no-pager -u archix-boot.service -u archix-boot-complete.service -u systemd-bless-boot.service -u archix-health-check.service | tail -40"))
        t.log(f"state {state()}")
        m.shutdown()
