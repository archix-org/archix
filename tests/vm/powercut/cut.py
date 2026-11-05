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
COUNTED = "/sys/firmware/efi/efivars/LoaderBootCountPath-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f"
results = []


def arm(step):
    """Cuts the power the first time a process reaches mark step (or STEP#K, its Kth time) from now on, in this boot or a later one."""
    disarm()
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
    # QEMU truncates the serial log when it starts, so the mark new_boot takes must be 0, not the old log's length, or a cut's line early in the next VM is never seen; the old log is kept beside it.
    if m.serial.exists():
        kept = 1
        while (m.work / f"serial.{kept}.log").exists():
            kept += 1
        m.serial.rename(m.work / f"serial.{kept}.log")
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
            if common.m.resync():
                boots += 1
            # An uncounted boot never reaches boot-complete.target; it's settled once the system is up.
            counted = common.m.execute(f"[ -e {COUNTED} ]", timeout=20, quiet=True)[0] == 0
            up = "systemctl is-active boot-complete.target" if counted else "systemctl is-system-running --wait"
            status, text = common.m.execute(f"cat /run/archix/try 2>/dev/null; echo; {up}; tail -c +5 {common.EFIVAR} | tr -d '\\000'; echo; mountpoint -q /efi && ls {ENTRIES}", timeout=20 if counted else 300, quiet=True)
        except MachineError:
            continue
        lines = text.splitlines()
        tried = bool(lines and lines[0].strip())
        complete = lines[1:2] in (["active"], ["running"], ["degraded"])
        selected = lines[2].strip() if len(lines) > 2 else ""
        stem = selected.removesuffix(".conf")
        blessed = len(lines) > 3 and (selected in lines[3:] or not any(line.startswith(stem + "+") for line in lines[3:]))
        # An uncounted boot can't fall back, so it's settled once up even if its check left the entry only under a counted name, as dropping the second name a cut left does.
        if tried or (complete and (blessed or not counted)):
            common.m.execute("systemctl is-system-running --wait > /dev/null", 600, quiet=True)
            return boots, time.monotonic() - began
        if time.monotonic() - began > timeout:
            raise MachineError(f"no boot settled within {timeout} s of the cut")
        time.sleep(5)


LOCK = "/var/lib/pacman/db.lck"
MARK = "var/rollback/archix/pacman-unfinished.json"
# The uncut system's pacman -Dk and -Qkk, which each check after a cut is compared with.
base = None
# The last check's counts, which record() puts on the cut's line.
last = ""


def pacman_state():
    """pacman -Dk's and -Qkk's exits and lines, unfiltered and logged in full; the lines compared leave out only mtime mismatches and the per-package totals, which count them."""
    m = common.m
    dk_status, dk = m.execute("pacman -Dk 2>&1", 900)
    qkk_status, qkk = m.execute("pacman -Qkk 2>&1", 2400)
    lines = [line for line in (dk + "\n" + qkk).splitlines()
             if line.strip() and "(Modification time mismatch)" not in line and not re.search(r": \d+ total files, \d+ altered files?$", line)]
    return {"dk": dk_status, "qkk": qkk_status, "lines": lines, "raw": len(qkk.splitlines())}


def baseline(what):
    """Takes the uncut system's pacman state that the checks after the next cuts compare with."""
    global base
    base = pacman_state()
    common.t.log(f"baseline ({what}): pacman -Dk exit {base['dk']}, -Qkk exit {base['qkk']}, {len(base['lines'])} lines compared of {base['raw']}")


def unfinished_on_root():
    """Whether @, the root a try boot isn't running, holds the unfinished mark; None when @ can't be looked at."""
    status, text = common.m.execute(f"dev=$(findmnt -no SOURCE /.archix | sed 's/\\[.*//') && mkdir -p /run/archix-top && mount -o ro,subvolid=5 $dev /run/archix-top && {{ [ -e /run/archix-top/@/{MARK} ] && echo present || echo absent; umount /run/archix-top; }}", 60)
    return {"present": True, "absent": False}.get(text.strip().splitlines()[-1] if text.strip() else "", None)


def problems(snapshot=None, packages=True, lock=False, mark=False, mark_on_root=None, compare=True):
    """What's inconsistent on the booted system: archix status, the snapshot store, the menu's images, the boot state, pacman's database and files against the baseline, db.lck (present only if lock), the unfinished mark in the booted root (only if mark) and on @ (mark_on_root, unless None); snapshot must exist (its entry goes once a boot is blessed, as it's no longer the slot)."""
    global last
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
    ids = [re.sub(r"\+[0-9-]+\.conf$", ".conf", name) for name in common.entries()]
    twice = sorted({i for i in ids if ids.count(i) > 1})
    if twice:
        found.append(f"entries on the ESP under two names: {twice} in {common.entries()}")
    try:
        state = json.loads(out(f"cat {common.STATE} 2>/dev/null || echo {{}}"))
        main = ((state.get("current") or {}).get("main") or {}).get("name")
        # A try boot can't see the image the current root built and recorded before a cut kept it from the ESP; back_to checks again after the keep.
        if state and (not main or not common.ok(f"[ -f /efi/EFI/archix/{main} ]")) and not common.ok("[ -s /run/archix/try ]"):
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
    counts = []
    if packages:
        now = pacman_state()
        if base is None or not compare:
            counts.append(f"dk={now['dk']} qkk={now['qkk']} lines={len(now['lines'])} (not compared)")
        else:
            new = [line for line in now["lines"] if line not in base["lines"]]
            gone = [line for line in base["lines"] if line not in now["lines"]]
            counts.append(f"dk={now['dk']}/{base['dk']} qkk={now['qkk']}/{base['qkk']} lines={len(now['lines'])}/{len(base['lines'])} differing={len(new) + len(gone)}")
            if now["dk"] != base["dk"]:
                found.append(f"pacman -Dk exits {now['dk']}, the uncut system {base['dk']}")
            if new or gone:
                found.append(f"pacman -Dk/-Qkk differ from the uncut system in {len(new) + len(gone)} line(s): new {new[:15]}, gone {gone[:15]}")
    held = common.ok(f"[ -e {LOCK} ]")
    counts.append(f"db.lck={'present' if held else 'absent'}")
    if held != lock:
        found.append(f"db.lck is {'present' if held else 'absent'} on the booted root; expected {'present' if lock else 'absent'}")
    marked = common.ok(f"[ -e /{MARK} ]")
    counts.append(f"mark={'present' if marked else 'absent'}")
    if marked != mark:
        found.append(f"the unfinished mark is {'present' if marked else 'absent'} on the booted root; expected {'present' if mark else 'absent'}")
    if mark_on_root is not None:
        on_root = unfinished_on_root()
        counts.append(f"mark-on-@={ {True: 'present', False: 'absent', None: 'unread'}[on_root] }")
        if on_root != mark_on_root:
            found.append(f"the unfinished mark on @ is {on_root}; expected {'present' if mark_on_root else 'absent'}")
    last = " ".join(counts)
    common.t.log(f"checks: {last}")
    return found


def kind(number):
    """A snapshot's kind from its record: switch, pacman, manual or replaced."""
    try:
        return json.loads(out(f"cat /.archix/snapshots/{number}/record.json")).get("kind")
    except ValueError:
        return None


def replaced(said):
    """The number keep's output gives the root it replaced, as a match whose group(1) it is, or None."""
    return re.search(r"(?:the running system|the @ it replaces) is snapshot (\d+)", said)


def record(point, booted, boots, seconds, entry, tried, found, note=""):
    global last
    line = f"CUT point={point} booted={'yes' if booted else 'NO'} boots={boots} took={seconds:.0f}s entry={entry} try={tried} consistent={'yes' if not found else 'NO'}"
    if last:
        line += f" checks={last!r}"
    if note:
        line += f" note={note!r}"
    if found:
        line += f" problems={found!r}"
    last = ""
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
