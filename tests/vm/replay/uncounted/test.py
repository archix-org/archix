# Write replay of an `archix switch` with boot counting off (boot.tries = 0) that changes the initramfs, so its boot step builds a new image and writes the current system's entry again: every flush and mark from the generation's record through the boot step's last flush, and subsets of each flush's ESP writes, must boot the current system by itself from a whole entry; with nothing counted there's no fallback, so a snapshot's boot is a failure.
import importlib.util

from boot.common import bind, edit_config, entries, ok, out, state, switch, trying
from powercut import cut
from replay import replay

_spec = importlib.util.spec_from_file_location("powercut_switch", "/vm/powercut/switch/test.py")
powercut_switch = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(powercut_switch)
# The switch's boot step: its record, the images it builds and copies, the menu it writes, its last flush.
WINDOW = ("switch-recorded", "switch-flushed")


def run(m, t):
    bind(m, t)
    s = replay.Scenario(m, t, "uncounted")
    with t.step("0: boot counting off, an initramfs that holds /etc/archix-vmtest, the marks in place"):
        cut.common.boot()
        edit_config("'s|  boot = { loader = \"systemd-boot\" },|  boot = { loader = \"systemd-boot\", tries = 0 },|'")
        edit_config("'s|  files = {|  files = {\\n    [\"/etc/mkinitcpio.conf.d/zz-archix-cut.conf\"] = { text = \"FILES=(/etc/archix-vmtest)\" },|'")
        switch("switch to boot counting off and an initramfs holding /etc/archix-vmtest")
        t.log(f"entries with counting off: {entries()}")
        t.require(not any("+" in name for name in entries()), f"with counting off no entry should count tries: {entries()}")
        replay.install(m)
        m.reboot()
        cut.settle()
        t.require(trying() is None, f"the boot before the switch should be the current system's: {trying()}")
    before = out("cat /etc/archix-vmtest").strip()
    s.freeze()

    def body():
        cut.settle()
        powercut_switch.declare(int(before) + 1, False)
        replay.mark(m, "switch-start")
        switch("the switch that rebuilds the initramfs with boot counting off")
        replay.mark(m, "switch-done")
        t.log(f"entries after the switch: {entries()}")
        replay.idle(m)
        m.reboot()
        cut.settle()
        t.require(trying() is None, f"the boot after the switch should be the current system's: try {trying()}")
        replay.mark(m, "end")

    with t.step("1: the switch and the boot after it, logged"):
        s.record(body)

    def extra():
        """The root holds the value from before the switch or after it; one whose boot state names an image the ESP lacks, as a cut between recording it and copying it leaves, boots its old entry, and archix status says the boot step isn't whole."""
        found = []
        now = out("cat /etc/archix-vmtest").strip()
        if now not in (before, str(int(before) + 1)):
            found.append(f"the root holds {now}, neither the {before} before the switch nor its {int(before) + 1}")
        main = ((state().get("current") or {}).get("main") or {}).get("name")
        if main and not ok(f"[ -f /efi/EFI/archix/{main} ]") and "boot step" not in out("archix status 2>&1"):
            found.append(f"the boot state names {main}, which the ESP lacks, and archix status doesn't say the boot step isn't whole")
        return found

    spec = replay.Spec(replay.by_packages, fallback=False, extra=extra, held=("switch-start", "switch-done"), sample=([WINDOW], 1_000_000), mark_any=True)
    with t.step("2: the uncut systems' baselines: before the switch and after it"):
        s.boot(s.overlay("before", s.base), s.pristine_vars, "the system before the switch")
        s.capture("before")
        s.boot(s.overlay("after", s.ran), s.end_vars, "the system after the switch")
        s.capture("after")
    s.control("after", s.overlay("after", s.ran), s.end_vars, spec)
    s.replay(spec)
    s.summary()
