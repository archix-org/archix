# Write replay of an `archix switch` that installs a package and changes the initramfs, as `powercut switch` cuts it (switch-snapshot, switch-marked, switch-packages, -files, -applied, -recorded, the boot images, switch-flushed, switch-finished), root's config.git store, and the boot after it with its bless rename: every flush and mark, and subsets of each flush's ESP writes, must boot by itself as the system before or after the switch or, while @ is marked unfinished, as the slot, the switch's snapshot, in try mode.
import importlib.util

from boot.common import bind, edit_config, ok, out, state, switch, trying
from powercut import cut
from replay import replay

# The powercut switch test's config edits, so both suites switch the same way.
_spec = importlib.util.spec_from_file_location("powercut_switch", "/vm/powercut/switch/test.py")
powercut_switch = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(powercut_switch)
PACKAGE = powercut_switch.PACKAGE


def run(m, t):
    bind(m, t)
    s = replay.Scenario(m, t, "switch")
    with t.step("0: an initramfs that holds /etc/archix-vmtest, the package downloaded, a boot that counts as good, the marks in place"):
        cut.common.boot()
        edit_config("'s|  boot = { loader = \"systemd-boot\" },|  boot = { loader = \"systemd-boot\", unblessed_timeout = 2, health_check = { reboot_on_failure = true } },|'")
        edit_config("'s|  files = {|  files = {\\n    [\"/etc/mkinitcpio.conf.d/zz-archix-cut.conf\"] = { text = \"FILES=(/etc/archix-vmtest)\" },|'")
        switch("switch to an initramfs holding /etc/archix-vmtest")
        m.succeed(f"pacman -Sw --noconfirm --noprogressbar {PACKAGE} > /root/download.log 2>&1", timeout=900, what=f"downloading {PACKAGE}")
        replay.install(m)
        m.reboot()
        cut.settle()
        t.require(trying() is None and state().get("armed"), f"the boot before the switch should be the current system's, blessed and armed: {trying()}, {state()}")
    before = out("cat /etc/archix-vmtest").strip()
    s.freeze()

    def body():
        cut.settle()
        powercut_switch.declare(int(before) + 1, True)
        replay.mark(m, "switch-start")
        switch(f"the switch that installs {PACKAGE} and rebuilds the initramfs")
        replay.mark(m, "switch-done")
        replay.idle(m)
        m.reboot()
        cut.settle()
        t.require(trying() is None and ok(f"pacman -Q {PACKAGE}"), f"the boot after the switch should be the new generation: try {trying()}")
        m.wait_until_succeeds(f"grep -q '\"armed\": true' {cut.common.STATE}", timeout=300, what="the good boot arming the slot")
        replay.mark(m, "end")

    with t.step("1: the switch and the boot after it, logged"):
        s.record(body)

    def extra():
        """The root's value and the package go together: the system before the switch, or after it."""
        if trying() is not None:
            return []
        now, installed = out("cat /etc/archix-vmtest").strip(), ok(f"pacman -Q {PACKAGE}")
        if (now, installed) not in ((before, False), (str(int(before) + 1), True)):
            return [f"the root holds {now} with {PACKAGE} {'installed' if installed else 'missing'}: neither the system before the switch ({before}, without it) nor after it"]
        return []

    spec = replay.Spec(replay.by_packages, fallback=True, extra=extra, held=("switch-start", "switch-done"))
    with t.step("2: the uncut systems' baselines: before the switch, after it, and the slot in try mode"):
        s.boot(s.overlay("before", s.base), s.pristine_vars, "the system before the switch")
        s.capture("before")
        s.boot(s.overlay("after", s.ran), s.end_vars, "the system after the switch")
        s.capture("after")
        slot = state().get("slot")
        t.require(slot is not None, f"the system after the switch should have a fallback slot: {state()}")
        m.select_entry(f"archix-snapshot-{slot}.conf")
        m.reboot()
        cut.settle()
        t.require(str((trying() or {}).get("snapshot")) == str(slot), f"the slot's entry should start snapshot {slot} in try mode: {trying()}")
        s.capture("try")
    s.control("before", s.overlay("before", s.base), s.pristine_vars, spec)
    s.control("after", s.overlay("after", s.ran), s.end_vars, spec)
    s.replay(spec)
    s.summary()
