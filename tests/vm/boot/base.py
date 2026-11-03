# The split boot tests' base, run once per run with this run's archix: the image boots as a plain install, the first switch sets the root up for rollbacks and writes the boot menu, the boot after counts as good, and the VM powers off; each test boots a copy-on-write overlay of the disk and a copy of the vars.
from boot.common import HOST, bind, boot, entries, has_line, loader, ok, out, state


def run(m, t):
    bind(m, t)
    with t.step("plain: the first switch sets rollbacks up and writes the menu"):
        m.start()
        boot()
        t.check(not ok("[ -d /.archix/snapshots ]"), "the image should boot as a plain install, without rollbacks yet")
        m.succeed(f"{HOST} switch --noconfirm", what="the first switch on the booted system")
        t.check("archix-current+2.conf" in entries(), f"the first switch should write the current system's entry with two tries: {entries()}")
        t.check(ok("ls /efi/EFI/archix/*.efi"), f"and its image: {out('ls /efi/EFI/archix')}")
        t.check(ok("[ -f /efi/loader/entries/archix-rescue.conf ]"), "and the rescue entry")
        text = loader()
        t.check(has_line(text, "preferred archix-*") and has_line(text, "default archix-current.conf"), f"loader.conf should prefer archix's entries in sort-key order: {text}")
        # LoaderConfigTimeout, not loader.conf: no test waits on the menu.
        m.succeed("bootctl set-timeout 0")
        m.reboot()

    with t.step("1: the layout boots and the boot counts as good"):
        started = boot()
        t.check(started == "archix-current.conf", f"the reboot should start the current system's entry, not {started}")
        t.check(out("findmnt -no FSROOT /var/lib").strip() == "/@varlib", f"/var/lib should be @varlib: {out('findmnt /var/lib')}")
        t.check(out("findmnt -no FSROOT /.archix").strip() == "/@archix", f"/.archix should be @archix: {out('findmnt /.archix')}")
        m.wait_until_succeeds("[ -f /efi/loader/entries/archix-current.conf ]", timeout=300, what="systemd-bless-boot should count the boot as good")
        m.wait_until_succeeds(f"grep -q '\"armed\": true' /.archix/boot/state.json", timeout=120, what="the good boot should arm the fallback slot")
        t.log(f"state {state()}")
        m.shutdown()
