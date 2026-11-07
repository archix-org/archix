# The Limine tests' base, run once per run: the image boots through Limine from the ESP (its kernel and initramfs there, no systemd-boot), the first switch with boot.loader = "limine" sets the root up for rollbacks and writes no boot entries, the boot after comes up through Limine on the new layout, and the VM powers off; each test boots an overlay of the disk.
from boot.common import HOST, bind, boot, first_switch, ok, out

# systemd-boot's LoaderInfo, which only it sets; Limine sets LimineInfo-like variables of its own or none.
LOADER_INFO = "/sys/firmware/efi/efivars/LoaderInfo-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f"


def loader_info():
    return out(f"tail -c +5 {LOADER_INFO} 2>/dev/null | tr -d '\\000'").strip()


def through_limine():
    return "systemd-boot" not in loader_info()


def run(m, t):
    bind(m, t)
    with t.step("plain: Limine boots the image, and the first switch sets rollbacks up without writing boot entries"):
        m.start()
        boot()
        t.require(through_limine(), f"the image should boot through Limine, not {loader_info()}")
        t.check(not ok("[ -d /.archix/snapshots ]"), "the image should boot as a plain install, without rollbacks yet")
        status = first_switch("/root/first-switch.log", 900)
        t.log("first switch: " + out("tail -c 3000 /root/first-switch.log"))
        t.require(status == 0, f"the first switch on the booted system exited {status}")
        t.check(not ok("ls /efi/loader/entries/archix-* 2>/dev/null"), f"on Limine archix writes no boot entries: {out('ls -R /efi')}")
        t.check(ok("[ -f /efi/limine.conf ]") and ok("[ -f /efi/vmlinuz-linux ]"), "Limine's config and the kernel it boots stay on the ESP")
        m.reboot()

    with t.step("1: the layout boots through Limine"):
        boot()
        t.check(through_limine(), "the boot after the first switch should come through Limine")
        t.check(out("findmnt -no FSROOT /").strip() == "/@", f"the root should be @: {out('findmnt /')}")
        t.check(out("findmnt -no FSROOT /var/lib").strip() == "/@varlib", f"/var/lib should be @varlib: {out('findmnt /var/lib')}")
        t.check(out("findmnt -no FSROOT /.archix").strip() == "/@archix", f"/.archix should be @archix: {out('findmnt /.archix')}")
        m.shutdown()
