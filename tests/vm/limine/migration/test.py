# The move from Limine to systemd-boot, with a power cut at each of its steps: the switch to boot.loader = "systemd-boot" copies systemd-boot to the ESP (limine-loader-copied) and gives it a firmware entry after Limine's (limine-entry-created), tried once through BootNext; the first boot through it that counts as good puts it first (limine-order-set) and on the removable-media path (limine-fallback-written). After each cut nobody touches the machine until a boot comes up, through Limine or systemd-boot; switches and reboots then finish the move, which must end with systemd-boot first and on the removable path, Limine's loader kept beside it and Limine still installed.
import json

from boot.common import HOST, bind, boot, edit_config, ok, out
from limine.base import loader_info, through_limine
from powercut import cut

# In the switch that starts the move, and in the good boot through systemd-boot that finishes it; None is the control.
SWITCH = ["limine-loader-copied", "limine-entry-created"]
FINISH = ["limine-order-set", "limine-fallback-written"]
MIGRATION = "/var/lib/archix/boot/limine-migration.json"


def migration():
    text = out(f"cat {MIGRATION} 2>/dev/null")
    return json.loads(text) if text.strip() else {}


def fallback_is_systemd_boot():
    return ok("grep -aq 'systemd-boot' /efi/EFI/BOOT/BOOTX64.EFI")


def run(m, t):
    bind(m, t)

    def settle(point):
        try:
            boots, took = cut.settle()
        except Exception as e:
            cut.record(point, False, 0, 0, "-", None, [str(e)[:300]])
            t.require(False, f"no boot came up after the cut at {point}")
        return boots, took

    def move(point):
        cut.fresh("/work/disk.img")
        boot()
        t.require(through_limine(), f"each case starts on Limine, not {loader_info()}")
        edit_config("'s|loader = \"limine\"|loader = \"systemd-boot\"|'")
        boots = took = 0
        if point in SWITCH:
            cut.arm(point)
            cut.background(f"{HOST} switch --noconfirm", "/root/switch.log")
            reached = cut.reset_on_cut(timeout=1800)
            cut.disarm()
            boots, took = settle(reached)
            t.log(f"after the cut at {reached}: booted through {loader_info() or 'Limine'}, migration {migration()}")
        # Each switch makes the firmware try systemd-boot again until a boot through it counted as good.
        for attempt in range(3):
            if migration().get("phase") == "done":
                break
            m.succeed(f"{HOST} switch --noconfirm 2>&1 | tail -20", timeout=1800, what=f"a switch starting or retrying the move ({attempt + 1})")
            if point in FINISH and attempt == 0:
                cut.arm(point)
            m.reboot()
            if point in FINISH and attempt == 0:
                reached = cut.reset_on_cut(timeout=1800)
                cut.disarm()
                boots, took = settle(reached)
                t.log(f"after the cut at {reached}: booted through {loader_info() or 'Limine'}, migration {migration()}")
            else:
                settle(point or "control")
                m.wait_until_succeeds(f"grep -q '\"done\"' {MIGRATION} || [ \"$(tail -c +5 /sys/firmware/efi/efivars/LoaderInfo-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f 2>/dev/null | tr -d '\\000' | cut -c1-12)\" != systemd-boot ]", timeout=600, what="the good boot finishing the move")
        found = cut.problems(packages=False)
        if migration().get("phase") != "done":
            found.append(f"the move didn't finish: {migration()}")
        order = out("efibootmgr | sed -n 's/^BootOrder: //p'").strip()
        entry = migration().get("entry")
        if entry is None or not order.upper().startswith(f"{entry:04X}"):
            found.append(f"systemd-boot's entry {entry} should be first in the boot order: {order}")
        if not fallback_is_systemd_boot():
            found.append("the removable-media path should hold systemd-boot")
        if not ok("[ -f /efi/EFI/BOOT/BOOTX64.EFI.before-archix ]"):
            found.append("Limine's removable-media loader should be kept beside it")
        if not ok("pacman -Q limine"):
            found.append("Limine should stay installed")
        if "systemd-boot" not in loader_info():
            found.append(f"the last boot should come through systemd-boot, not {loader_info() or 'Limine'}")
        cut.record(point or "none", True, boots, took, cut.common.selected(), None, found, f"order {order}, loader {loader_info()}")
        t.check(not found, f"consistent after the cut at {point}: {found}")

    for point in SWITCH + FINISH + [None]:
        cut.point(f"{point or 'control'}: cut in the move from Limine to systemd-boot", lambda point=point: move(point))
    cut.summary()
