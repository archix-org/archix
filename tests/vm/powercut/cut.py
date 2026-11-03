# The power-cut tests' shared helpers: a cut armed at one of archix's marks through an EFI variable the debug build reads (testhooks::CUT_VARIABLE), the QMP reset when the guest says it reached it, and the verdict on the boots after: did one come up by itself, and are archix's records, the menu and pacman's database whole.
import json
import re
import shlex
import subprocess
import time

from boot import common
from boot.common import ENTRIES, out
from driver.machine import MachineError
from driver.steps import StepFailed

VARIABLE = "/sys/firmware/efi/efivars/ArchixTestCut-0b9c7b5e-5f43-4f0e-9a4e-6a2d7c1e3f21"
SAID = r"archix-test: power cut at (\S+)"
FALLBACK = r"rebooting so the boot counter can fall back"
results = []


def arm(step):
    """Cuts the power the first time a process reaches mark step (or STEP#K, its Kth time) from now on, in this boot or a later one."""
    common.m.succeed(f"printf '\\x07\\x00\\x00\\x00%s' {shlex.quote(step)} > {VARIABLE}", what=f"arming the cut at {step}")


def disarm():
    common.m.execute(f"[ ! -e {VARIABLE} ] || {{ chattr -i {VARIABLE}; rm -f {VARIABLE}; }}")


def background(cmd, log):
    """Starts cmd in the guest, its output in log and on the console, and returns at once."""
    common.m.succeed(f"setsid -f bash -c {shlex.quote(f'set -o pipefail; {{ {cmd}; }} 2>&1 | tee {log} > /dev/console; echo \"exit $?\" >> {log}')} < /dev/null > /dev/null 2>&1")


def reset_on_cut(timeout=1800):
    """Waits for the guest to say it reached the armed step, then resets it; returns the step."""
    said = common.m.wait_for_console_text(SAID, timeout=timeout)
    common.m.crash()
    return said.split()[-1]


def fresh(backing):
    """Stops the VM and gives it a new overlay of backing with new firmware variables, for a cut in a system that hasn't run yet."""
    m = common.m
    m.stop()
    subprocess.run(["qemu-img", "create", "-q", "-f", "qcow2", "-b", backing, "-F", "raw", str(m.work / "disk.qcow2")], check=True)
    (m.work / "vars.fd").unlink(missing_ok=True)
    m.process = m.shell = None
    m.ready, m.seen, m.boot_id, m.connected = [], 0, None, False
    m.new_boot(f"a fresh overlay of {backing}")


def newer_boot():
    with common.m.lock:
        return [b for b in common.m.ready[common.m.seen:] if b != common.m.boot_id]


def settle(timeout=2400):
    """Follows the boots after a cut until one settles, blessed or in try mode, as nobody touches the machine: a counted boot that fails reboots itself; returns (boots, seconds), or raises when no boot comes up."""
    began = time.monotonic()
    boots = 1
    common.m.wait_for_shell(timeout=900)
    while True:
        if newer_boot():
            common.m.connected = False
            common.m.new_boot("the guest rebooted itself")
            common.m.wait_for_shell(timeout=900)
            boots += 1
            continue
        try:
            status, text = common.m.execute(f"cat /run/archix/try 2>/dev/null; echo; systemctl is-active boot-complete.target; tail -c +5 {common.EFIVAR} | tr -d '\\000'; echo; mountpoint -q /efi && ls {ENTRIES}", timeout=20, quiet=True)
        except MachineError:
            continue
        lines = text.splitlines()
        tried = bool(lines and lines[0].strip())
        complete = "active" in lines[1:2]
        selected = lines[2].strip() if len(lines) > 2 else ""
        stem = selected.removesuffix(".conf")
        blessed = len(lines) > 3 and (selected in lines[3:] or not any(line.startswith(stem + "+") for line in lines[3:]))
        if tried or (complete and blessed):
            common.m.execute("systemctl is-system-running --wait > /dev/null", 600, quiet=True)
            return boots, time.monotonic() - began
        if time.monotonic() - began > timeout:
            raise MachineError(f"no boot settled within {timeout} s of the cut")
        time.sleep(5)


def problems(snapshot=None, packages=True):
    """What's inconsistent on the booted system: archix status, the snapshot store, the menu's images, the boot state, and pacman's database and files; snapshot must exist (its entry goes once a boot is blessed, as it's no longer the slot)."""
    found = []
    if not common.ok("mountpoint -q /efi"):
        found.append(f"the ESP isn't mounted: {out('systemctl status efi.mount --no-pager 2>&1 | tail -5')}")
    status, text = common.m.execute("archix status 2>&1", quiet=True)
    if status:
        found.append(f"archix status exited {status}: {text[-400:]}")
    if "isn't the one archix last produced" in text:
        found.append("archix calls the root foreign")
    missing = out(f"cd {ENTRIES} && for f in archix-*.conf; do awk '$1 == \"uki\" || $1 == \"efi\" {{ print $2 }}' $f | while read -r image; do [ -f \"/efi$image\" ] || echo \"$f -> $image\"; done; done")
    if missing.strip():
        found.append(f"entries start images that aren't there: {missing.strip()}")
    try:
        state = json.loads(out(f"cat {common.STATE} 2>/dev/null || echo {{}}"))
        main = ((state.get("current") or {}).get("main") or {}).get("name")
        if state and (not main or not common.ok(f"[ -f /efi/EFI/archix/{main} ]")):
            found.append(f"the boot state's current image {main} isn't on the ESP")
    except ValueError as e:
        found.append(f"the boot state doesn't parse: {e}")
    broken = out("shopt -s nullglob; for d in /.archix/snapshots/[0-9]*; do [ -d $d/root ] && [ -s $d/record.json ] || echo $d; done 2>/dev/null")
    if broken.strip():
        found.append(f"snapshots without their root or record: {broken.strip()}")
    if snapshot is not None:
        if snapshot not in common.snapshots():
            found.append(f"snapshot {snapshot} is gone")
    if common.ok("btrfs subvolume list / | grep -q 'path @archix-keep$'") and not common.ok("[ -e /run/archix/keep-pending ]"):
        found.append("an unfinished keep's clone is left")
    if packages:
        status, text = common.m.execute("pacman -Dk > /dev/null 2>&1 || echo 'warning: pacman -Dk finds database errors'; pacman -Qkk 2>&1 > /dev/null | grep '^warning: ' | grep -v -E ' /etc/| /var/log/|\\((Modification time|Permissions|UID|GID) mismatch\\)' | head -20", 600, quiet=True)
        if text.strip():
            found.append(f"pacman finds the system broken: {text.strip()[-600:]}")
    return found


def replaced(said):
    """The number keep's output gives the root it replaced, as a match whose group(1) it is, or None."""
    return re.search(r"(?:the running system|the @ it replaces) is snapshot (\d+)", said)


def record(point, booted, boots, seconds, entry, tried, found, note=""):
    line = f"CUT point={point} booted={'yes' if booted else 'NO'} boots={boots} took={seconds:.0f}s entry={entry} try={tried} consistent={'yes' if not found else 'NO'}"
    if note:
        line += f" note={note!r}"
    if found:
        line += f" problems={found!r}"
    results.append(line)
    common.t.log(line)
    print(line, flush=True)


failed = []


def point(name, body):
    """Runs one cut as a step of its own; a failed one is listed at the end rather than stopping the cuts after it."""
    try:
        with common.t.step(name):
            body()
    except StepFailed as e:
        failed.append(f"{name}: {e}")


def summary():
    """Prints each cut's verdict and fails unless every step passed."""
    print("CUTS:", flush=True)
    for line in results:
        print("  " + line, flush=True)
    with common.t.step("every cut"):
        common.t.require(not failed, f"{len(failed)} cut(s) failed:\n" + "\n".join(failed))
