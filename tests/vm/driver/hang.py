# Where a boot that never reached the guest's shell stopped, and whether docs/CI.md's hang table attributes it; `python3 hang.py` checks both against the captures in hang-captures.json.
import json
import re
import sys
from pathlib import Path

# Linux runs with CR0 80050033 (PE, WP, AM, PG and the rest), OVMF and its SMI handler with 80010033.
LINUX_CR0 = "80050033"
ANSI = re.compile(r"\x1b\[[0-9;?=!]*[A-Za-z]|\x1b\][^\x1b]*\x1b\\|\x1b[78]")
LINUX_OUTPUT = re.compile(r"Booting initrd of|^\[ *[0-9]+\.[0-9]+\]|\[  OK  \]|^ *(Starting|Stopping) ", re.M)
DEPLOY_MOUNT = "archix: mount the deployment this boot belongs to"
TABLE = "docs/CI.md#hangs-on-githubs-nested-vms"
CAPTURES = Path(__file__).with_name("hang-captures.json")
PHASES = ("shutdown", "firmware-before-BdsDxe", "loader-after-BdsDxe", "firmware-smm-under-linux", "early-kernel", "initramfs", "initramfs-archix-deploy-mount", "after-switch-root")


def phase(console, registers):
    """The phase, one of PHASES, from the console since the reboot or reset and QEMU's `info registers -a` (or its RIP=, CR0= and CR3= words)."""
    text = ANSI.sub("", console)
    linux = LINUX_CR0 in re.findall(r"CR0=0*([0-9a-fA-F]+)", registers)
    started, restarted = text.rfind("BdsDxe: starting"), text.rfind("reboot: Restarting system")
    if started < restarted or started < 0 and not LINUX_OUTPUT.search(text):
        # A reset that the firmware never got as far as starting the boot entry after, or a reset with nothing on the console yet.
        return "shutdown" if linux else "firmware-before-BdsDxe"
    after = text[max(started, 0):]
    if not linux:
        # Linux had written to the console since the entry started, and no reset since: every vCPU is in the firmware's SMI handler.
        return "firmware-smm-under-linux" if LINUX_OUTPUT.search(after) else "loader-after-BdsDxe"
    if started < 0:
        return "shutdown"
    if "Welcome to" in after:
        return "after-switch-root"
    if "Booting initrd of" in after:
        return "initramfs-archix-deploy-mount" if after.count(DEPLOY_MOUNT) == 1 else "initramfs"
    return "early-kernel"


def attribution(where):
    """What docs/CI.md's rule makes of a hang in phase where: an ATTRIBUTED: line, or a NOT ATTRIBUTED: one for a plain failure."""
    if where in ("firmware-before-BdsDxe", "loader-after-BdsDxe", "early-kernel"):
        return f"ATTRIBUTED: nested-virt-hang ({where}; {TABLE})"
    if where in ("initramfs", "after-switch-root"):
        return f"ATTRIBUTED: nested-virt-hang ({where}, which plain Arch shows too in nic-control's table; re-check if nic-control stops showing it; {TABLE})"
    if where == "initramfs-archix-deploy-mount":
        return f"NOT ATTRIBUTED: the boot hung in archix's deploy-mount, which is archix's to look into ({TABLE})"
    if where == "firmware-smm-under-linux":
        return f"NOT ATTRIBUTED: the vCPUs sat in the firmware's SMI handler while Linux ran, which nic-control has seen only with archix ({TABLE})"
    return f"NOT ATTRIBUTED: a hang in {where}, which nic-control's table doesn't hold ({TABLE})"


def check():
    captures = json.loads(CAPTURES.read_text())
    for capture in captures:
        got = phase(capture["console"], capture["registers"])
        assert got == capture["phase"], f"{capture.get('job', capture['source'])} {capture.get('arm', '')} {capture.get('boot', '')}: {got}, not {capture['phase']}"
    placed = sorted({c["phase"] for c in captures}, key=PHASES.index)
    attributed = {"firmware-before-BdsDxe", "loader-after-BdsDxe", "early-kernel", "initramfs", "after-switch-root"}
    for where in PHASES:
        assert attribution(where).startswith("ATTRIBUTED: ") == (where in attributed), where
    print(f"hang.py: {len(captures)} captures placed as recorded ({', '.join(placed)}); attributed: {', '.join(sorted(attributed, key=PHASES.index))}")


if __name__ == "__main__":
    sys.exit(check())
