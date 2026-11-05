# CachyOS as it installs: its repos ahead of Arch's with x86-64-v3 builds, linux-cachyos, booted by Limine. The first switch sets rollbacks up and leaves Limine's menu alone; the lock takes CachyOS's builds and archix keeps copies; a switch on a full disk is refused before it changes anything; keep across an initramfs change is refused on Limine; a revert with CachyOS's mirror unreachable and pacman's cache empty reinstalls CachyOS's builds from archix's copies; the move to systemd-boot keeps Limine first until a boot through systemd-boot counts as good, then hands over, after which keep works.
import time

from boot.common import HOST, bind, boot, edit_config, entries, entry, ok, out, selected, snapshots
from suite import add_packages, current_generation, drop_packages, newest_snapshot, switch

MACHINE = {"mem": "4096", "cpus": "4", "global_timeout": 10800}
LOADER_INFO = "/sys/firmware/efi/efivars/LoaderInfo-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f"


def arch_of(name):
    return out(f"pacman -Qi {name} 2>/dev/null | sed -n 's/^Architecture *: //p'").strip()


def through_systemd_boot():
    """Whether systemd-boot started this boot: Limine sets the Boot Loader Interface's variables too, under its own name."""
    return "systemd-boot" in out(f"tail -c +5 {LOADER_INFO} 2>/dev/null | tr -d '\\000'")


def boot_order():
    """The firmware's boot order as labels, first first, and BootNext's label or None."""
    said = out("efibootmgr")
    labels = {}
    order, after = [], None
    for line in said.splitlines():
        if line.startswith("BootOrder:"):
            order = line.split(":", 1)[1].strip().split(",")
        elif line.startswith("BootNext:"):
            after = line.split(":", 1)[1].strip()
        elif line.startswith("Boot") and line[4:8].isalnum() and len(line) > 9 and line[8] in "* ":
            labels[line[4:8]] = line[9:].split("\t")[0].strip()
    return [labels.get(n, n) for n in order], labels.get(after) if after else None


def migration_done():
    return ok("grep -q '\"done\"' /var/lib/archix/boot/limine-migration.json")


def packages():
    return out("pacman -Q")


def run(m, t):
    bind(m, t)
    with t.step("1: Limine boots CachyOS; the first switch sets rollbacks up and leaves Limine's menu alone"):
        m.start()
        boot()
        t.check("cachyos" in out("uname -r"), f"the image should boot linux-cachyos: {out('uname -r')}")
        t.check(not through_systemd_boot(), "the image should boot through Limine, not systemd-boot")
        m.succeed("efibootmgr --create --disk /dev/vda --part 1 --label Limine --loader '\\EFI\\limine\\BOOTX64.EFI' > /dev/null", what="giving Limine a firmware entry, as CachyOS's installer does")
        limine_conf = out("sha256sum /efi/limine.conf /efi/vmlinuz-linux-cachyos /efi/initramfs-linux-cachyos.img")
        said = switch(m, t, "the first switch on Limine")
        t.log(said[-1500:])
        t.check(not [e for e in entries() if e != "archix-rescue.conf"], f"on Limine archix writes no entries of its own: {entries()}")
        t.check(out("sha256sum /efi/limine.conf /efi/vmlinuz-linux-cachyos /efi/initramfs-linux-cachyos.img") == limine_conf, "Limine's menu, kernel and initramfs on the ESP stay as they were")
        t.check("the root is set up for rollbacks" in said, f"the first switch sets rollbacks up: {said[-800:]}")
        m.reboot()
        boot()
        t.check(not through_systemd_boot(), "Limine still boots the machine")
        t.check(out("findmnt -no FSROOT /var/lib").strip() == "/@varlib", f"/var/lib should be @varlib after the boot: {out('findmnt /var/lib')}")
        t.check(out("findmnt -no FSROOT /.archix").strip() == "/@archix", f"@archix should be mounted after the reboot: {out('findmnt /.archix')}")
        t.check(out("findmnt -no FSROOT /var/lib/pacman").strip() == "/@/var/rollback/pacman", f"pacman's database should be bound from the root: {out('findmnt /var/lib/pacman')}")

    with t.step("2: CachyOS's v3 builds, locked from its repos, with archix's own copies"):
        for name in ("coreutils", "htop", "jq"):
            t.check(arch_of(name) == "x86_64_v3", f"{name} should be CachyOS's x86_64_v3 build, not {arch_of(name)}")
        for name, repo in (("coreutils", "cachyos-core-v3"), ("htop", "cachyos-extra-v3")):
            t.check(ok(f"grep -q '^{name} = .*repo = \"{repo}\"' /etc/archix/archix.lock"), f"the lock should take {name} from {repo}: {out(f'grep ^{name} /etc/archix/archix.lock')}")
        # The first switch kept its copies before @archix was mounted; the boot after moves them there.
        t.check(ok("ls /.archix/packages/htop-*.pkg.tar.zst") and not ok("ls -d /var/cache/archix/pkg"), f"archix's copy of CachyOS's htop should be in @archix: {out('ls /.archix/packages /var/cache/archix/pkg 2>&1 | head -20')}")

    with t.step("3: a switch on a full disk is refused before it changes anything"):
        add_packages(m, "tree")
        before = (packages(), snapshots(), out("cat /etc/archix/archix.lock"))
        m.succeed("fallocate -l $(( $(df --output=avail -B1 / | tail -1) - 20 * 1024 * 1024 )) /var/tmp/archix-fill")
        status, said = m.execute(f"{HOST} switch --noconfirm > /run/full.log 2>&1; echo $?; tail -c 1500 /run/full.log", 1800)
        m.succeed("rm -f /var/tmp/archix-fill")
        t.log(said)
        t.check(said.split("\n", 1)[0].strip() != "0", "a switch with 20 MB free should fail")
        t.check("nothing was changed" in said, f"the refusal should say nothing was changed: {said[-600:]}")
        t.check((packages(), snapshots()) == before[:2], f"the refused switch changed the packages or the snapshots: {snapshots()} vs {before[1]}")
        drop_packages(m, "tree")

    with t.step("4: on Limine, keeping a snapshot from before an initramfs change is refused"):
        older = current_generation()
        m.succeed("mkdir -p /etc/archix/files && printf 'MODULES+=(vfat)\\n' > /etc/archix/files/archix-vmtest.conf")
        edit_config("'s|  files = {|  files = {\\n    [\"/etc/mkinitcpio.conf.d/archix-vmtest.conf\"] = { source = \"files/archix-vmtest.conf\" },|'")
        drop_packages(m, "htop", "jq")
        add_packages(m, "tree")
        switch(m, t, "the switch changing the initramfs")
        kept = newest_snapshot()
        t.check(not ok("pacman -Q htop") and ok("pacman -Q tree"), "the switch applied live")
        status, said = m.execute(f"{HOST} rollback {kept} --noconfirm 2>&1", 600)
        t.log(said)
        t.check(status != 0 and "initramfs" in said and "systemd-boot" in said, f"keep across an initramfs change should be refused on Limine, pointing at systemd-boot: {said[-600:]}")
        t.check(not ok("[ -e /run/archix/keep-pending ]"), "nothing is pending after the refusal")

    with t.step(f"5: a revert to generation {older} with CachyOS's mirror unreachable reinstalls its builds from archix's copies"):
        m.succeed("cp /etc/hosts /root/hosts && echo '127.0.0.1 mirror.cachyos.org' >> /etc/hosts && rm -rf /var/cache/pacman/pkg/*")
        t.check(not ok("curl -sf -m 10 -o /dev/null https://mirror.cachyos.org/"), "mirror.cachyos.org should be unreachable now")
        said = m.succeed(f"set -o pipefail; {HOST} revert {older} --noconfirm 2>&1 | tail -30", 2400, what=f"archix revert {older} offline")
        t.log(said)
        for name in ("htop", "jq"):
            t.check(arch_of(name) == "x86_64_v3", f"the revert should reinstall CachyOS's {name}, not {arch_of(name)!r}")
        m.succeed("cp /root/hosts /etc/hosts")

    with t.step("6: the move to systemd-boot keeps Limine first until a boot through systemd-boot counts as good"):
        edit_config("'s|loader = \"limine\"|loader = \"systemd-boot\"|'")
        said = switch(m, t, "the switch to systemd-boot")
        t.log(said[-1500:])
        order, after = boot_order()
        t.log(f"boot order {order}, next {after}: {out('efibootmgr')}")
        t.check(order[:1] == ["Limine"], f"Limine should stay first in the boot order: {order}")
        t.check(after is not None and after != "Limine", f"the firmware should try systemd-boot once at the next boot: next {after}")
        t.check(entry("archix-current.conf") is not None, f"the switch should write systemd-boot's menu: {entries()}")
        m.reboot()
        started = boot()
        t.check(through_systemd_boot() and started == "archix-current.conf", f"systemd-boot should start the current entry: {started}")
        # Under Limine archix can't tell systemd-boot counts boots, so that entry had no tries; the switch from systemd-boot gives it some, and the good boot through them finishes the move.
        for attempt in range(3):
            if migration_done():
                break
            switch(m, t, f"a switch from systemd-boot retrying the move ({attempt + 1})")
            m.reboot()
            boot()
            m.wait_until_succeeds("[ -f /efi/loader/entries/archix-current.conf ]", timeout=300, what="the boot through systemd-boot should count as good")
            for _ in range(60):
                if migration_done():
                    break
                time.sleep(5)
        order, _ = boot_order()
        t.check(migration_done(), f"the move should be done: {out('cat /var/lib/archix/boot/limine-migration.json')}")
        t.check(order[:1] != ["Limine"] and "Limine" in order, f"systemd-boot first, Limine kept after it: {order}")
        t.check(ok("grep -aq systemd-boot /efi/EFI/BOOT/BOOTX64.EFI") and ok("[ -f /efi/EFI/BOOT/BOOTX64.EFI.before-archix ]"), "the removable-media path should be systemd-boot's, Limine's kept beside it")
        m.reboot()
        started = boot()
        t.check(through_systemd_boot() and started == "archix-current.conf", f"systemd-boot boots by default now: {started}")

    with t.step(f"7: on systemd-boot, keeping snapshot {kept} works"):
        m.succeed(f"set -o pipefail; {HOST} rollback {kept} --noconfirm 2>&1 | tail -20", 1800, what=f"keeping snapshot {kept}")
        m.reboot()
        started = boot()
        t.check(started == "archix-current.conf", f"the kept root boots from the current entry: {started}")
        t.check(ok("pacman -Q htop") and not ok("[ -e /etc/mkinitcpio.conf.d/archix-vmtest.conf ]"), f"the root is snapshot {kept}, from before the initramfs change: {out('pacman -Q htop tree 2>&1')}")
        t.log(f"selected {selected()}")
