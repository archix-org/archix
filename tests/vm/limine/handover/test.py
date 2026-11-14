# The handover from Limine to systemd-boot as the firmware really did it, read from `archix boot-info` after each reboot: with BootNext honoured the boot comes through systemd-boot; with BootNext gone before the reboot, as firmware that ignores it behaves, archix says at boot, in status and in the next switch that Limine started the boot and its protection isn't in effect, holds the package change back and puts systemd-boot first in the boot order with Limine second, which gets the machine onto systemd-boot; with that undone too, as firmware that follows neither, the switch is refused until --allow-limine-boot.
import json

from boot.common import HOST, bind, boot, edit_config, ok, out
from limine.base import loader_info, through_limine
from powercut import cut

MIGRATION = "/var/lib/archix/boot/limine-migration.json"
SAYS = "this boot came through Limine, not archix's systemd-boot: boot counting, fallback and the rescue entry are not in effect"


def migration():
    text = out(f"cat {MIGRATION} 2>/dev/null")
    return json.loads(text) if text.strip() else {}


def info():
    """`archix boot-info` as a dict of its keys, each with the list of its values."""
    found = {}
    for line in out("archix boot-info").splitlines():
        key, _, value = line.partition(": ")
        found.setdefault(key, []).append(value)
    return found


def first(found, key):
    return (found.get(key) or [""])[0]


def run(m, t):
    bind(m, t)

    def start():
        """A fresh machine on Limine, with a firmware entry for it as CachyOS's installer makes, switched to boot.loader = "systemd-boot"; returns the entry archix made and the boot order before."""
        cut.fresh("/work/disk.img")
        boot()
        t.require(through_limine(), f"each case starts on Limine, not {loader_info()}")
        m.succeed("efibootmgr -q --create --disk /dev/vda --part 1 --label Limine --loader '\\EFI\\limine\\BOOTX64.EFI'", what="giving Limine a firmware entry")
        before = first(info(), "firmware-order")
        t.check(first(info(), "loader") == "limine" and first(info(), "protection").startswith("none"), f"on Limine boot-info names it and says nothing protects the boot: {info()}")
        edit_config("'s|loader = \"limine\"|loader = \"systemd-boot\"|'")
        said = m.succeed(f"set -o pipefail; {HOST} switch --noconfirm 2>&1 | tail -20", timeout=1800, what="the switch that starts the move")
        t.log(said)
        found = info()
        entry = f"{migration().get('entry', 0xFFFF):04X}"
        t.check("read back from the firmware" in said, f"the switch says it read the firmware's state back: {said[-600:]}")
        t.check(first(found, "firmware-next") == entry, f"BootNext should name archix's entry {entry}: {found}")
        t.check(first(found, "firmware-order") == f"{before},{entry}", f"archix's entry goes after the others, {before}: {found}")
        uuid = out("lsblk -dno PARTUUID /dev/vda1").strip()
        mine = [line for line in found.get("firmware-entry", []) if line.startswith(entry)]
        t.check(len(mine) == 1 and f"partition {uuid} " in mine[0] and "systemd-bootx64.efi" in mine[0], f"entry {entry} should name the ESP by its PARTUUID {uuid} and systemd-boot's file: {mine}")
        t.check("problem" not in found and "armed in this boot by BootNext" in first(found, "migration"), f"boot-info should find the firmware as archix set it: {found}")
        t.check(ok("grep -q '^efi /EFI/limine/BOOTX64.EFI' /efi/loader/entries/limine-before-archix.conf"), "systemd-boot's menu should have an entry that starts Limine")
        return entry, before

    def finish(entry):
        """The good boot through systemd-boot finishes the move, and the boot after comes through it too."""
        for attempt in range(2):
            if migration().get("phase") == "done":
                break
            if attempt:
                m.succeed(f"set -o pipefail; {HOST} switch --noconfirm 2>&1 | tail -20", timeout=1800, what="a switch from systemd-boot retrying the move")
                m.reboot()
                boot()
            m.wait_until_succeeds(f"grep -q '\"done\"' {MIGRATION}", timeout=600, what="the boot through systemd-boot counting as good and finishing the move")
        m.reboot()
        boot()
        found = info()
        t.check(first(found, "loader") == "systemd-boot" and first(found, "firmware-order").startswith(entry), f"after the move systemd-boot is first and starts the boot: {found}")
        t.check(first(found, "migration").startswith("done") and ok("pacman -Q limine"), f"the move is done and Limine stays installed: {found}")

    def limine_boot(what):
        """A boot Limine started although systemd-boot was to be tried: what archix says about it."""
        boot()
        found = info()
        t.require(first(found, "loader") == "limine", f"{what}: Limine should have started this boot: {found}")
        t.check(first(found, "protection").startswith("none"), f"boot-info should say nothing protects this boot: {found}")
        status = out("archix status 2>&1")
        t.check(SAYS in status, f"status should say Limine started the boot: {status[:1500]}")
        t.check(SAYS in out("cat /run/motd.d/archix.motd 2>/dev/null"), f"the login note should say so: {out('cat /run/motd.d/archix.motd 2>&1')[:800]}")
        return status

    with t.step("1: with BootNext honoured the boot after the switch comes through systemd-boot, by LoaderInfo"):
        entry, _ = start()
        m.reboot()
        boot()
        found = info()
        t.log(f"boot-info: {found}")
        t.check("systemd-boot" in loader_info(), f"LoaderInfo should be systemd-boot's: {loader_info()}")
        t.check(first(found, "loader") == "systemd-boot" and first(found, "loader-info").startswith("systemd-boot"), f"boot-info should name systemd-boot: {found}")
        t.check(first(found, "firmware-current") == entry, f"the firmware should have started entry {entry}: {found}")
        t.check(first(found, "loader-entry").startswith("archix-current") and first(found, "protection").startswith("in effect"), f"systemd-boot should have started archix's entry, protected: {found}")
        t.check(SAYS not in out("archix status 2>&1"), "status has no warning on a boot through systemd-boot")
        finish(entry)

    with t.step("2: firmware that ignores BootNext: reported, the package change held back, and systemd-boot first in the boot order gets there"):
        entry, before = start()
        m.succeed("efibootmgr -q -N", what="dropping BootNext, as firmware that ignores it")
        m.reboot()
        status = limine_boot("after BootNext was dropped")
        t.check("ignored BootNext" in status, f"status should say the firmware ignored BootNext: {status[:1500]}")
        t.check(migration().get("missed") == ["next"], f"the miss should be recorded: {migration()}")
        m.succeed("sed -i 's|{ \"base\", |{ \"base\", \"tree\", |' /etc/archix/init.lua")
        code, said = m.execute(f"set -o pipefail; {HOST} switch --noconfirm 2>&1 | tail -30", 1800)
        t.log(said)
        t.check(SAYS in said and "1 package change waits:" in said and "--allow-limine-boot" in said, f"the switch should say Limine started the boot and hold the package back: {said[-1500:]}")
        t.check(not ok("pacman -Q tree"), "no package should change on a boot Limine started")
        found = info()
        t.check(first(found, "firmware-order") == f"{entry},{before}", f"systemd-boot's entry first, the others after it: {found}")
        t.check("problem" not in found, f"the firmware should hold what archix set: {found}")
        m.succeed("efibootmgr -q -N", what="dropping BootNext again")
        m.reboot()
        boot()
        found = info()
        t.require(first(found, "loader") == "systemd-boot", f"first in the boot order, systemd-boot should start the boot: {found}")
        t.check(first(found, "protection").startswith("in effect"), f"and the boot is protected: {found}")
        m.succeed(f"set -o pipefail; {HOST} switch --noconfirm 2>&1 | tail -20", timeout=1800, what="the switch applying the package on a boot through systemd-boot")
        t.check(ok("pacman -Q tree"), "the package change applies once systemd-boot started the boot")
        m.reboot()
        boot()
        finish(entry)

    with t.step("3: firmware that follows neither BootNext nor the boot order: the switch is refused until --allow-limine-boot"):
        entry, before = start()
        m.succeed("efibootmgr -q -N")
        m.reboot()
        limine_boot("after BootNext was dropped")
        m.succeed("sed -i 's|{ \"base\", |{ \"base\", \"tree\", |' /etc/archix/init.lua")
        m.succeed(f"set -o pipefail; {HOST} switch --noconfirm 2>&1 | tail -20", timeout=1800, what="the switch putting systemd-boot first")
        m.succeed(f"efibootmgr -q -N && efibootmgr -q -o {before},{entry}", what="undoing BootNext and the boot order, as firmware that follows neither")
        m.reboot()
        status = limine_boot("after BootNext and the boot order were undone")
        t.check("firmware's boot menu" in status, f"status should point at the firmware's boot menu: {status[:1500]}")
        t.check(migration().get("missed") == ["next", "first"], f"both misses should be recorded: {migration()}")
        code, said = m.execute(f"set -o pipefail; {HOST} switch --noconfirm 2>&1 | tail -30", 1800)
        t.log(said)
        t.check(code != 0 and "nothing was changed" in said and SAYS in said, f"the switch should be refused, saying why: {said[-1500:]}")
        t.check(not ok("pacman -Q tree"), "the refused switch changes no package")
        m.succeed(f"set -o pipefail; {HOST} switch --noconfirm --allow-limine-boot 2>&1 | tail -20", timeout=1800, what="the switch with --allow-limine-boot")
        t.check(ok("pacman -Q tree"), "--allow-limine-boot applies the package")
        t.check(first(info(), "loader") == "limine" and ok("[ -f /efi/limine.conf ]"), "Limine still boots the machine")
