# The write-replay power-cut tests' shared steps (docs/vm-testing.md): a scenario runs once with QEMU's blklogwrites driver logging every write, flush and archix mark of its disk; then each flush and mark of the log, and seeded subsets of each flush's ESP writes, is replayed onto a copy of the disk it started from, booted with nobody touching it, and judged against the uncut systems.
import hashlib
import json
import os
import random
import re
import shlex
import shutil
import struct
import subprocess
import tarfile
import threading
import time
from collections import Counter

from boot import common
from boot.common import edit_config, out, state, switch, trying
from driver.machine import OVMF_VARS, Machine, Timeline
from driver.steps import Test
from powercut import cut
from replay import fat, writelog

MARKER = "/usr/local/bin/archix-replay-mark"
# The marks the driver and the replay's own units write; every other one is archix's, after a write it has flushed.
OURS = {"idle", "boot-up", "shutdown", "end", "bless-start", "bless-done", "pacman-start", "pacman-done", "switch-start", "switch-done", "switched", "keep-start", "cancel-start", "keep-again"}
ESP_DIRS = ["loader/entries", "EFI/archix"]
EMPTY = hashlib.sha256(b"").hexdigest()
PEEK = "/run/archix-replay-peek"
UNFINISHED = "var/rollback/archix/pacman-unfinished.json"
LOCK = "var/rollback/pacman/db.lck"
SUBSETS = int(os.environ.get("REPLAY_SUBSETS") or 16)
EVERY = int(os.environ.get("REPLAY_EVERY") or 1)
ONLY = [word for word in (os.environ.get("REPLAY_ONLY") or "").split(",") if word]
# START..END[,START..END]: only the points from mark START (NAME, or NAME#K for its Kth in the log) through the first flush after mark END (NAME#K counted from START), as a mutation's window.
WINDOWS = [word.split("..", 1) for word in (os.environ.get("REPLAY_WINDOW") or "").split(",") if ".." in word]
ESP_ONLY = os.environ.get("REPLAY_ESP_ONLY") == "1"
# How many points boot at once, each in a VM of its own; one while the host's I/O pressure (full avg60) is over IO_PRESSURE.
JOBS = max(1, int(os.environ.get("REPLAY_JOBS") or 1))
IO_PRESSURE = 10.0
# What a batch leaves free of the host's memory: QEMU takes a guest's memory as it touches it, so a VM just started counts for little yet.
MEMORY_SPARE_MIB = 8192
SEED = os.environ.get("REPLAY_SEED") or os.environ.get("GITHUB_RUN_ID") or time.strftime("%Y%m%d")
SETTLE = 1500
SUMMARY = re.compile(r"^\S+: \d+ total files, \d+ (altered|missing) files$")

MARK_SCRIPT = r"""#!/bin/sh
# archix-replay-mark STEP: writes STEP into the GPT gap of the disk the write replay logs, O_DIRECT and unsynced, so it adds no flush; archix runs it at each of its marks through ARCHIX_TEST_MARK.
mkdir -p /run/archix-replay
exec 9>> /run/archix-replay/lock
flock 9
n=$(cat /run/archix-replay/seq 2> /dev/null || echo 0)
echo $((n + 1)) > /run/archix-replay/seq
printf 'ARCHIX-REPLAY-MARK %s-%s %s\n' "$n" "$(cut -c1-8 /proc/sys/kernel/random/boot_id)" "$1" | dd of=/dev/vda bs=512 seek=$((1024 + n % 1024)) count=1 conv=sync,notrunc oflag=direct status=none
"""
# The log's drive: the qcow2 overlay the scenario runs on, every request to it logged in order.
LOGGED = "driver=blklogwrites,file.driver=qcow2,file.file.filename={disk},log.driver=file,log.filename={log},log-sector-size=512,if=virtio,discard=ignore"
PLAIN = "file={disk},format=qcow2,if=virtio,discard=ignore"


def heredoc(path, text, mode="0644"):
    return f"mkdir -p {shlex.quote(os.path.dirname(path))} && install -m{mode} /dev/stdin {shlex.quote(path)} <<'ARCHIX_REPLAY_EOF'\n{text}ARCHIX_REPLAY_EOF"


def install(m):
    """Puts the mark program in the guest, ARCHIX_TEST_MARK in every unit's environment, marks around systemd-bless-boot and at each boot and shutdown, udev's watch off the disk, so a mark's write makes no change event, and the kernel's timed writeback off."""
    m.succeed(heredoc(MARKER, MARK_SCRIPT, "0755"))
    m.succeed(heredoc("/etc/udev/rules.d/99-archix-replay.rules", 'ACTION!="remove", SUBSYSTEM=="block", KERNEL=="vda", OPTIONS+="nowatch"\n'))
    # No timed writeback, so nothing reaches the disk but what a flush or memory pressure sends: a 30 s timer can't stand in for a flush archix left out.
    m.succeed(heredoc("/etc/sysctl.d/99-archix-replay.conf", "vm.dirty_writeback_centisecs = 0\nvm.dirty_expire_centisecs = 360000\n"))
    m.succeed(heredoc("/etc/systemd/system.conf.d/archix-replay.conf", f"[Manager]\nDefaultEnvironment=ARCHIX_TEST_MARK={MARKER}\n"))
    m.succeed(heredoc("/etc/systemd/system/systemd-bless-boot.service.d/00-archix-replay.conf", f"[Service]\nExecStartPre=-{MARKER} bless-start\nExecStartPost=-{MARKER} bless-done\n"))
    m.succeed(heredoc("/etc/systemd/system/archix-replay-boot.service", f"[Unit]\nDescription=archix write replay: mark this boot and its shutdown\nDefaultDependencies=no\nAfter=local-fs.target\nConflicts=shutdown.target\nBefore=shutdown.target\n\n[Service]\nType=oneshot\nRemainAfterExit=yes\nExecStart={MARKER} boot-up\nExecStop={MARKER} shutdown\n\n[Install]\nWantedBy=sysinit.target\n"))
    m.succeed("systemctl enable archix-replay-boot.service")


def mark(m, step):
    """Marks step in the log from the driver, as archix marks its own."""
    m.succeed(f"{MARKER} {shlex.quote(step)}")


# Longer than btrfs's 30 s commit interval, so with timed writeback off a commit lands after the command, as on a machine left running.
IDLE = 40


def idle(m):
    """Leaves the guest idle for IDLE seconds after the logged command: btrfs then commits what it holds in memory without writing new files' data, the state a flush left out leaves exposed."""
    mark(m, "idle")
    time.sleep(IDLE)


def use(m, drive, vars_from, how):
    """Stops the VM and makes its next start boot drive with a fresh copy of the firmware variables in vars_from."""
    m.stop()
    shutil.copy(vars_from, m.work / "vars.fd")
    m.disks = [drive]
    m.process = m.shell = None
    m.ready, m.seen, m.boot_id, m.connected = [], 0, None, False
    m.new_boot(how)


def qemu_img(*args):
    subprocess.run(["qemu-img", *[str(a) for a in args]], check=True)


class Scenario:
    """One test's logged run and its replay: the disk it started from, its log, the uncut systems' baselines and every point's verdict."""

    def __init__(self, m, t, name):
        self.m, self.t, self.name = m, t, name
        self.work = m.work
        self.base = self.work / "disk.qcow2"
        self.image = self.work / "replay.img"
        self.ran = self.work / "ran.qcow2"
        self.log = self.work / "log.bin"
        self.pristine_vars = self.work / "pristine-vars.fd"
        self.end_vars = self.work / "end-vars.fd"
        self.outputs = self.work / "pacman"
        self.outputs.mkdir(exist_ok=True)
        self.lines = open(self.work / "replay.log", "a", buffering=1)
        self.baselines = {}
        self.esp = {}
        self.esp_before = set()
        self.results = []
        self.failed = []

    def start_from(self, image):
        """Makes the scenario start from raw image, as cut.fresh() does, with new firmware variables."""
        self.base = self.work / "start.qcow2"
        self.m.stop()
        self.base.unlink(missing_ok=True)
        qemu_img("create", "-q", "-f", "qcow2", "-b", image, "-F", "raw", self.base)
        use(self.m, PLAIN.format(disk=self.base), OVMF_VARS, f"a fresh overlay of {image}")

    def note(self, line):
        self.lines.write(line + "\n")
        self.t.log(line)
        print(line, flush=True)

    def freeze(self):
        """Powers the prepared system off and keeps its disk as a raw image the replay writes onto, and its firmware variables, which every replayed boot starts from."""
        self.m.shutdown()
        qemu_img("convert", "-O", "raw", self.base, self.image)
        shutil.copy(self.work / "vars.fd", self.pristine_vars)

    def record(self, body):
        """Boots the prepared system with every write logged, runs body, and powers it off; the system it leaves is ran.qcow2."""
        qemu_img("create", "-q", "-f", "qcow2", "-b", self.base, "-F", "qcow2", self.ran)
        self.log.write_bytes(b"")
        # blklogwrites fails a request of over 1022 vectors with EIO, and virtio-blk merges neighbours up to 1024; unmerged, a request has at most seg_max (254).
        self.m.extra = [*self.m.extra, "-global", "virtio-blk-pci.request-merging=off"]
        use(self.m, LOGGED.format(disk=self.ran, log=self.log), self.pristine_vars, "the logged run")
        began = time.monotonic()
        body()
        self.m.shutdown()
        shutil.copy(self.work / "vars.fd", self.end_vars)
        self.parts = writelog.layout(str(self.image))
        self.entries = writelog.read(str(self.log), self.parts)
        self.points = writelog.points(self.entries)
        kinds = Counter(e.where or ("flush" if e.flags & writelog.FLUSH else "?") for e in self.entries)
        marks = [e.mark.split(" ", 1)[-1] for e in self.entries if e.mark]
        self.note(f"REPLAY-LOG test={self.name} entries={len(self.entries)} bytes={self.log.stat().st_size} took={time.monotonic() - began:.0f}s kinds={dict(kinds)} points={len(self.points)} esp-windows={sum(1 for p in self.points if p.window)} marks={len(marks)}: {' '.join(marks)}")
        for p in self.points:
            self.lines.write(f"  {p.label()} names={','.join(p.names)} esp-window={len(p.window)}\n")

    def boot(self, drive, vars_from, how):
        use(self.m, drive, vars_from, how)
        return cut.settle(timeout=SETTLE)

    def capture(self, name, identity=None):
        """The booted uncut system's pacman outputs and ESP files as baseline name."""
        state = self.pacman(f"baseline-{name}")
        state["identity"] = identity
        self.baselines[name] = state
        files = self.esp_files()
        if name == "before":
            self.esp_before = set(files)
        for key, digests in files.items():
            self.esp.setdefault(key, set()).update(digests)
        self.note(f"REPLAY-BASELINE test={self.name} name={name} packages={len(state['Q'].splitlines())} Dk-lines={sum(lines(state['Dk'], 'Dk').values())} Qkk-lines={sum(lines(state['Qkk'], 'Qkk').values())} esp-files={len(self.esp)}")

    def pacman(self, label):
        """Unfiltered `pacman -Dk` and `pacman -Qkk`, and `pacman -Q`, on the booted system, each kept in the test's pacman/ directory."""
        state = {}
        for kind, cmd, timeout in (("Dk", "pacman -Dk", 600), ("Qkk", "pacman -Qkk", 1800), ("Q", "pacman -Q", 120)):
            text = common.m.execute(f"{cmd} 2>&1; echo \"exit $?\"", timeout, quiet=True)[1]
            (self.outputs / f"{file_name(label)}.{kind}.txt").write_text(text)
            state[kind] = text if kind != "Q" else "\n".join(line for line in text.splitlines() if not line.startswith("exit "))
        return state

    def esp_files(self):
        """Each of archix's files on the ESP and loader.conf by name, an entry's boot counter left out, with the sha256 of each file under that name (an entry under two names, as a cut between writing one and removing the other leaves it, has two)."""
        text = out("cd /efi 2>/dev/null && find loader/entries loader/loader.conf EFI/archix -type f ! -name '*.tmp' -exec sha256sum {} + 2>/dev/null")
        found = {}
        for line in text.splitlines():
            digest, _, path = line.partition("  ")
            found.setdefault(esp_key(path), set()).add(digest)
        return found

    def esp_detail(self, key):
        """Each name of the ESP file key with its size, whether it reads, and its first bytes."""
        text = out(f"cd /efi && for f in {shlex.quote(key)} {shlex.quote(key.removesuffix('.conf'))}+*.conf; do [ -f \"$f\" ] || continue; if cat \"$f\" > /dev/null 2>&1; then r=reads; else r=unreadable; fi; echo \"$f size=$(stat -c %s \"$f\") $r: $(head -c 200 \"$f\" 2>/dev/null | tr -c '[:print:]' '.')\"; done")
        return text.strip()[:600]

    def peek(self):
        """The unfinished mark and db.lck on @ itself, which a try boot doesn't run on, from a read-only mount of the top level; a root not laid out for rollbacks has no mark."""
        device = out("findmnt -no SOURCE /.archix 2>/dev/null").strip().split("[")[0]
        if not device:
            return {"mark": None, "lock": common.ok("[ -e /var/lib/pacman/db.lck ]")}
        if not common.ok(f"mkdir -p {PEEK} && mount -o ro,subvolid=5 {device} {PEEK}"):
            return None
        try:
            text = out(f"cat {PEEK}/@/{UNFINISHED} 2>/dev/null").strip()
            return {"mark": json.loads(text) if text else None, "lock": common.ok(f"[ -e {PEEK}/@/{LOCK} ]")}
        except ValueError as e:
            return {"mark": f"unreadable: {e}", "lock": common.ok(f"[ -e {PEEK}/@/{LOCK} ]")}
        finally:
            common.m.execute(f"umount {PEEK}")

    def judge(self, label, spec, locked):
        """Follows the boots of the replayed disk until one settles and checks it: a fallback only for a root marked unfinished, to its slot; no mark on the booted root, and on @ only in a fallback; db.lck only where locked says pacman held it (or in a try of a pacman snapshot, taken under it); the package database one of the uncut systems', with their pacman -Dk and -Qkk but for mtime; no torn file of archix's on the ESP; the menu and records whole."""
        found, verdict = [], {"booted": "NO", "boots": 0, "took": 0, "entry": "-", "try": "-", "mark": "?", "lock": "?", "system": "?", "Dk": "-", "Qkk": "-"}
        try:
            boots, took = cut.settle(timeout=SETTLE)
        except Exception as e:
            return [f"no boot settled with nobody at the machine: {str(e)[:400]}"], verdict
        tried = (trying() or {}).get("snapshot")
        verdict.update(booted="yes", boots=boots, took=round(took), entry=common.selected() or "-", **{"try": tried if tried is not None else "-"})
        held = common.ok(f"[ -e {cut.LOCK} ]")
        verdict["lock"] = "present" if held else "absent"
        if tried is not None:
            lock = cut.kind(tried) == "pacman"
        else:
            lock = held if locked else False
        laid_out = common.ok("mountpoint -q /.archix")
        found += cut.problems(packages=False, lock=lock, mark=False, mark_on_root=(tried is not None) if laid_out else None)
        seen = self.peek()
        if seen is None:
            found.append("couldn't mount the top level read-only to see @'s mark")
        else:
            verdict["mark"] = "present" if seen["mark"] else "absent"
        if tried is None and seen and seen["mark"]:
            found.append("the current system booted with @ marked unfinished; its health check: " + " | ".join(out("journalctl -b -u archix-health-check.service -u archix-unblessed.service -u systemd-bless-boot.service --no-pager -o cat 2>&1 | tail -12; systemctl list-dependencies --plain --no-pager boot-complete.target 2>&1 | head -12; systemctl is-enabled archix-health-check.service 2>&1; ls -la /etc/systemd/system/archix-health-check.service /etc/systemd/system/boot-complete.target.requires 2>&1").split("\n"))[:1500])
        if tried is not None and not spec.fallback:
            found.append(f"the boot fell back to snapshot {tried}, a try boot")
        elif tried is not None and seen and isinstance(seen["mark"], dict) and str(seen["mark"].get("slot")) != str(tried):
            found.append(f"the boot fell back to snapshot {tried}, not the slot the mark names, {seen['mark'].get('slot')}")
        state = self.pacman(label)
        system = spec.identify(self, state, tried)
        verdict["system"] = system or "none"
        if system is None:
            found.append(f"the package database is none of the uncut systems': {package_diff(state, self.baselines)}")
        else:
            base = self.baselines[system]
            for kind in ("Dk", "Qkk"):
                added, removed = diff(state[kind], base[kind], kind)
                verdict[kind] = f"+{sum(added.values())}/-{sum(removed.values())}"
                if added or removed:
                    found.append(f"pacman -{kind} differs from the uncut {system} system's: added {sorted(added)[:8]}, gone {sorted(removed)[:8]} (pacman/{file_name(label)}.{kind}.txt)")
        found += spec.extra()
        found += config_repository()
        marked = seen.get("mark") if seen else None
        files = self.esp_files()
        if isinstance(marked, dict) and marked.get("slot") is not None:
            slot = f"loader/entries/archix-snapshot-{marked['slot']}.conf"
            if not files.get(slot, set()) & self.esp.get(slot, set()):
                found.append(f"@ is marked unfinished, but the slot it names, snapshot {marked['slot']}, has no whole entry on the ESP to fall back to")
        # A name holding bytes archix never wrote there is torn, whatever its other names hold: a directory entry reached the disk before its data.
        for key, digests in files.items():
            if key in self.esp and digests - self.esp[key] - {EMPTY}:
                found.append(f"/efi/{key} holds bytes archix never wrote there under a final name, as a directory entry that reached the disk before its data leaves: {self.esp_detail(key)}")
                continue
        # Else an entry is lost only if no name holds a version archix wrote (an empty name beside a whole one is FAT's unlink, or fsck truncating a cross-link); a brand-new one (the new slot's) may be lost before the mark, as the system before still boots and the next refresh writes it.
        for key, digests in files.items():
            if key in self.esp and digests - self.esp[key] - {EMPTY}:
                continue
            if key not in self.esp_before and not marked and not digests & self.esp.get(key, set()):
                self.note(f"REPLAY-NOTE test={self.name} point={label} the new entry /efi/{key} was lost before @ was marked unfinished; archix status: {' '.join(out('archix status 2>&1 | grep -i -E "entry|snapshot" | head -4').split())[:300]}")
                continue
            if key in self.esp and not digests & self.esp[key]:
                found.append(f"/efi/{key} has no name holding a version archix wrote there: {self.esp_detail(key)}")
        return found, verdict

    def check(self, label, what, spec, locked=False):
        """Judges the booted point as a step of its own, recording its verdict; a failed one is listed at the end rather than stopping the points after it."""
        began = time.monotonic()
        found, verdict, failed = [], {}, None
        try:
            with common.t.step(f"{label}: {what}"):
                found, verdict = self.judge(label, spec, locked)
                common.t.check(not found, f"consistent at {label}: {found}")
        except Exception as e:
            failed = f"{label}: {e}"
            self.failed.append(failed)
        line = f"REPLAY test={self.name} point={label} " + " ".join(f"{k}={v}" for k, v in verdict.items()) + f" checked={time.monotonic() - began:.0f}s consistent={'yes' if not failed else 'NO'} at={what!r}"
        if failed and not found:
            line += f" error={failed[-400:]!r}"
        if found:
            line += f" problems={found!r}"
        self.results.append(line)
        self.note(line)
        common.m.stop()
        return line

    def overlay(self, name, backing, fmt="qcow2"):
        """A throwaway qcow2 overlay of backing, as a drive, the VM stopped first."""
        self.m.stop()
        path = self.work / f"{name}.qcow2"
        path.unlink(missing_ok=True)
        qemu_img("create", "-q", "-f", "qcow2", "-b", backing, "-F", fmt, path)
        return PLAIN.format(disk=path)

    def control(self, label, drive, vars_from, spec):
        """Judges an uncut system as a point, so the checks are seen passing where nothing was cut."""
        use(self.m, drive, vars_from, f"the uncut {label}")
        self.check(label, "the uncut system", spec)

    def harvest(self):
        """Every version of archix's ESP files the run wrote: the log's ESP writes alone, applied to a copy of the ESP, read at each of archix's own marks, where what it wrote is flushed and whole."""
        start, end = self.parts["esp"]
        esp = self.work / "esp.img"
        with open(self.image, "rb") as src, open(esp, "wb") as dst:
            dst.truncate((end - start) * writelog.SECTOR)
            src.seek(start * writelog.SECTOR)
            for at in range(0, (end - start) * writelog.SECTOR, 1 << 20):
                chunk = src.read(1 << 20)
                if chunk.strip(b"\0"):
                    dst.seek(at)
                    dst.write(chunk)
        before = sum(len(v) for v in self.esp.values())
        log = os.open(self.log, os.O_RDONLY)
        disk = os.open(esp, os.O_RDWR)
        try:
            for e in self.entries:
                if e.where == "esp" and e.count and not e.flags & (writelog.MARK | writelog.DISCARD):
                    os.pwrite(disk, os.pread(log, e.count * writelog.SECTOR, e.data), (e.sector - start) * writelog.SECTOR)
                elif e.mark and e.mark.split(" ", 1)[-1] not in OURS:
                    reader = fat.Fat(str(esp))
                    files = dict(reader.files(ESP_DIRS))
                    loader = reader.file("loader/loader.conf")
                    reader.close()
                    if loader:
                        files["loader/loader.conf"] = loader
                    for path, (data, whole) in files.items():
                        # Nothing archix writes there is empty or all NULs, which is what a directory entry written before its data names.
                        if whole and not path.endswith(".tmp") and data.strip(b"\0"):
                            self.esp.setdefault(esp_key(path), set()).add(hashlib.sha256(data).hexdigest())
        finally:
            os.close(log)
            os.close(disk)
            esp.unlink()
        self.note(f"REPLAY-ESP test={self.name} {sum(len(v) for v in self.esp.values()) - before} more versions of archix's ESP files from the log's marks, {len(self.esp)} files")

    def replay(self, spec):
        """Every point of the log, and each flush's ESP subsets over the flush before it, in the order the copy moves forward, then the copy against the system the run left."""
        self.harvest()
        rng = random.Random(f"{SEED}-{self.name}")
        ranges = [window_range(self.entries, start, end) for start, end in WINDOWS]
        for (start, end), (first, last) in zip(WINDOWS, ranges):
            self.note(f"REPLAY-WINDOW test={self.name} {start}..{end} is entries {first} through {last}")
        tasks = []
        for p in self.points:
            if ranges and not any(first <= p.upto <= last for first, last in ranges):
                continue
            if ESP_ONLY and not p.window:
                continue
            if wanted(p.number, None):
                tasks.append((p.upto, 1, p, None))
            for k, picks in enumerate(writelog.subsets(p, self.entries, SUBSETS, rng), 1):
                if wanted(p.number, k):
                    tasks.append((p.floor, 0, p, (k, picks)))
        tasks.sort(key=lambda task: (task[0], task[1]))
        subsets = sum(1 for task in tasks if task[3])
        self.note(f"REPLAY-PLAN test={self.name} seed={SEED} points={len(self.points)} checked={len(tasks) - subsets} subsets={subsets} every={EVERY} only={','.join(ONLY) or '-'} window={os.environ.get('REPLAY_WINDOW') or '-'} esp-only={'yes' if ESP_ONLY else 'no'}")
        order = [e.mark.split(" ", 1)[-1] for e in self.entries if e.mark and e.mark.split(" ", 1)[-1] in ("menu-synced", "unfinished-marked", "unfinished-cleared")]
        self.note(f"REPLAY-ORDER test={self.name} {' '.join(order) or 'no unfinished mark in this run'}")
        if "unfinished-marked" in order and "menu-synced" not in order[: order.index("unfinished-marked")]:
            self.failed.append("the unfinished mark is written before the menu is synced, so a new slot entry could be lost while @ is marked")
        window = held(self.entries, spec.held)
        self.note(f"REPLAY-LOCK test={self.name} db.lck may be left after entry {window[0]} through entry {window[1]}, where pacman held it")
        engine = writelog.Replay(str(self.image), str(self.log), self.entries)
        overlay = self.work / "point.qcow2"
        seen = {}
        if JOBS > 1:
            try:
                self.parallel(engine, tasks, spec, window)
            except BaseException:
                engine.close()
                raise
            tasks = []
        try:
            for position, _, p, subset in tasks:
                engine.to(position)
                label, what = f"P{p.number}", f"{p.kind} at entry {p.upto} after mark {p.after or 'none'} ({p.writes} writes since the last point; {','.join(p.names)})"
                if subset:
                    k, picks = subset
                    digest = engine.apply(picks)
                    if digest is None:
                        engine.revert()
                        continue
                    label, what = f"P{p.number}/{k}", f"{len(picks)} of the {len(p.window)} ESP writes before the flush at entry {p.upto} (mark {p.after or 'none'}) over the flush before it, entry {p.floor}"
                    if digest in seen.setdefault(p.number, {}):
                        self.note(f"REPLAY test={self.name} point={label} same={seen[p.number][digest]} (the same sectors as that subset)")
                        engine.revert()
                        continue
                    seen[p.number][digest] = label
                qemu_img("create", "-q", "-f", "qcow2", "-b", self.image, "-F", "raw", overlay)
                use(self.m, PLAIN.format(disk=overlay), self.pristine_vars, f"replayed {label}")
                self.check(label, what, spec, window[0] < position <= window[1])
                overlay.unlink(missing_ok=True)
                if subset:
                    engine.revert()
            engine.to(len(self.entries) - 1)
        finally:
            engine.close()
        if not ONLY and EVERY == 1 and not WINDOWS and not ESP_ONLY:
            same = subprocess.run(["qemu-img", "compare", "-q", "-f", "raw", "-F", "qcow2", str(self.image), str(self.ran)])
            self.note(f"REPLAY-END test={self.name} the whole log replayed {'matches' if same.returncode == 0 else 'DOES NOT match'} the disk the run left")
            if same.returncode != 0:
                self.failed.append("the whole log, replayed, doesn't give the disk the run left, so the log or its replay is incomplete")

    def parallel(self, engine, tasks, spec, window):
        """The tasks in batches of up to JOBS, each point booted from an overlay of the copy as the batch's first task left it, with the log's writes since then, or its subset, written into the overlay; the copy moves on only between batches."""
        local_m, local_t = Local(self.m), Local(self.t)
        common.bind(local_m, local_t)
        workers = []
        for k in range(JOBS):
            work = self.work / f"job-{k + 1}"
            work.mkdir(exist_ok=True)
            timeline = Timeline(str(self.work / "timeline.log"))
            workers.append((Machine(timeline, work=str(work), disks=[]), Test(timeline), work))
        lines = {}
        seen = {}
        try:
            at = 0
            while at < len(tasks):
                jobs = 1 if io_pressure() > IO_PRESSURE else min(JOBS, memory_room())
                base = tasks[at][0]
                engine.to(base)
                batch = []
                while at < len(tasks) and len(batch) < jobs:
                    index, (position, _, p, subset) = at, tasks[at]
                    at += 1
                    picks = list(subset[1]) if subset else []
                    label, what = f"P{p.number}", f"{p.kind} at entry {p.upto} after mark {p.after or 'none'} ({p.writes} writes since the last point; {','.join(p.names)})"
                    if subset:
                        digest = self.digest(engine, base, position, picks)
                        label, what = f"P{p.number}/{subset[0]}", f"{len(picks)} of the {len(p.window)} ESP writes before the flush at entry {p.upto} (mark {p.after or 'none'}) over the flush before it, entry {p.floor}"
                        if digest is None:
                            continue
                        if digest in seen.setdefault(p.number, {}):
                            self.note(f"REPLAY test={self.name} point={label} same={seen[p.number][digest]} (the same sectors as that subset)")
                            continue
                        seen[p.number][digest] = label
                    batch.append((index, position, label, what, list(range(base + 1, position + 1)) + picks))
                threads = []
                for (index, position, label, what, writes), (machine, test, work) in zip(batch, workers):
                    disk = self.point_overlay(work, writes)

                    def job(index=index, position=position, label=label, what=what, machine=machine, test=test, disk=disk):
                        local_m.set(machine)
                        local_t.set(test)
                        use(machine, PLAIN.format(disk=disk), self.pristine_vars, f"replayed {label}")
                        lines[index] = self.check(label, what, spec, window[0] < position <= window[1])
                        disk.unlink(missing_ok=True)

                    threads.append(threading.Thread(target=job))
                    threads[-1].start()
                for thread in threads:
                    thread.join()
                if len(batch) > 1:
                    self.note(f"REPLAY-BATCH test={self.name} {len(batch)} points at once over entry {base}, I/O pressure {io_pressure():.1f}, room for {memory_room()} VMs")
        finally:
            for machine, _, _ in workers:
                machine.stop()
            common.bind(self.m, self.t)
        # The summary lists the points in the order the sequential replay checks them.
        self.results = [line for line in self.results if line not in lines.values()] + [lines[i] for i in sorted(lines)]

    def point_overlay(self, work, writes):
        """A qcow2 overlay of the copy with the log's entries writes written into it, in order."""
        disk = work / "point.qcow2"
        disk.unlink(missing_ok=True)
        qemu_img("create", "-q", "-f", "qcow2", "-b", self.image, "-F", "raw", disk)
        data = work / "data"
        data.mkdir(exist_ok=True)
        commands = []
        log = os.open(self.log, os.O_RDONLY)
        try:
            for n, i in enumerate(writes):
                e = self.entries[i]
                if not e.count or e.flags & (writelog.MARK | writelog.DISCARD):
                    continue
                piece = data / str(n)
                piece.write_bytes(os.pread(log, e.count * writelog.SECTOR, e.data))
                commands += ["-c", f"write -q -s {piece} {e.sector * writelog.SECTOR} {e.count * writelog.SECTOR}"]
        finally:
            os.close(log)
        if commands:
            subprocess.run(["qemu-io", "-f", "qcow2", *commands, str(disk)], check=True, capture_output=True)
        for piece in data.iterdir():
            piece.unlink()
        return disk

    def digest(self, engine, base, floor, picks):
        """A digest of the sectors a subset's picks leave over the state at floor, or None if they leave them as the floor has them."""
        log = os.open(self.log, os.O_RDONLY)
        try:
            def sectors(indices):
                found = {}
                for i in indices:
                    e = self.entries[i]
                    if e.count and not e.flags & (writelog.MARK | writelog.DISCARD):
                        data = os.pread(log, e.count * writelog.SECTOR, e.data)
                        for k in range(e.count):
                            found[e.sector + k] = data[k * writelog.SECTOR : (k + 1) * writelog.SECTOR]
                return found

            floor_state = sectors(range(base + 1, floor + 1))
            after = sectors(picks)
        finally:
            os.close(log)
        changed = any((floor_state[n] if n in floor_state else os.pread(engine.disk, writelog.SECTOR, n * writelog.SECTOR)) != data for n, data in after.items())
        if not changed:
            return None
        digest = hashlib.sha256()
        for n in sorted(after):
            digest.update(struct.pack("<Q", n) + after[n])
        return digest.hexdigest()[:16]

    def summary(self):
        """Prints every verdict and the totals, keeps the pacman outputs beside the logs, and fails unless every point and subset passed."""
        with tarfile.open(self.work / "pacman.tar.gz", "w:gz") as tar:
            tar.add(self.outputs, arcname="pacman")
        booted = [line for line in self.results if " booted=yes " in line]
        tried = [line for line in booted if " try=- " not in line]
        bad = [line for line in self.results if "consistent=NO" in line]
        self.note(f"REPLAY-SUMMARY test={self.name} checked={len(self.results)} subsets={sum(1 for line in self.results if re.search(r' point=P[0-9]+/', line))} booted={len(booted)} fell-back={len(tried)} unbootable={len(self.results) - len(booted)} inconsistent={len(bad)} pacman-differs={sum(1 for line in self.results if re.search(r' (Dk|Qkk)=\+([1-9]|0/-[1-9])', line))}")
        print("POINTS:", flush=True)
        for line in self.results:
            print("  " + line, flush=True)
        self.lines.close()
        with self.t.step("every replayed point"):
            self.t.require(not self.failed, f"{len(self.failed)} replayed point(s) failed:\n" + "\n".join(self.failed))


def held(entries, marks):
    """The entries between the first mark's and the first flush after the second's, (-1, -1) without marks."""
    if not marks:
        return -1, -1
    named = {}
    for e in entries:
        if e.mark:
            named.setdefault(e.mark.split(" ", 1)[-1], e.index)
    if marks[0] not in named or marks[1] not in named:
        raise ValueError(f"the log lacks the marks {marks}: it has {sorted(named)}")
    end = next((e.index for e in entries[named[marks[1]]:] if e.flags & (writelog.FLUSH | writelog.FUA)), len(entries) - 1)
    return named[marks[0]], end


def config_repository():
    """Root's config repository, if there is one, whole: no empty loose object, which a store whose objects weren't flushed leaves, and a clean git fsck."""
    repo = "/.archix/config.git"
    if not common.ok(f"[ -d {repo}/objects ]"):
        return []
    found = []
    empty = out(f"cd {repo}/objects && find . -regextype posix-extended -regex './[0-9a-f]{{2}}/[0-9a-f]{{38,62}}' -type f -empty").split()
    if empty:
        found.append(f"root's config repository holds {len(empty)} empty loose object(s): {empty[:6]}")
    status, said = common.m.execute(f"git --git-dir={repo} fsck --no-dangling 2>&1", 300)
    if status:
        found.append(f"git fsck of root's config repository exits {status}: {said[-400:]}")
    return found


def esp_key(path):
    """An ESP file's path, an entry's boot counter left out."""
    return re.sub(r"\+\d+(-\d+)?(\.conf)$", r"\2", path)


def window_range(entries, start, end):
    """The entries from mark start's (NAME, or NAME#K for its Kth in the log) through the first flush after mark end's (its Kth after start)."""
    def find(spec, after=-1):
        name, _, nth = spec.partition("#")
        hits = [e.index for e in entries if e.mark and e.mark.split(" ", 1)[-1] == name and e.index > after]
        k = int(nth or 1)
        if len(hits) < k:
            raise ValueError(f"the log has {len(hits)} mark(s) {name} after entry {after}, not a #{k}: {sorted({e.mark.split(' ', 1)[-1] for e in entries if e.mark})}")
        return hits[k - 1]

    first = find(start)
    last = find(end, first)
    flush = next((e.index for e in entries[last:] if e.flags & (writelog.FLUSH | writelog.FUA)), len(entries) - 1)
    return first, flush


class Local:
    """Stands for one object per thread, the default one where a thread set none: boot.common's machine and test while points boot side by side."""

    def __init__(self, default):
        object.__setattr__(self, "_local", threading.local())
        object.__setattr__(self, "_default", default)

    def set(self, value):
        object.__getattribute__(self, "_local").value = value

    def _target(self):
        return getattr(object.__getattribute__(self, "_local"), "value", object.__getattribute__(self, "_default"))

    def __getattr__(self, name):
        return getattr(self._target(), name)

    def __setattr__(self, name, value):
        setattr(self._target(), name, value)


def io_pressure():
    """The host's share of time all tasks stalled on I/O over the last minute, from /proc/pressure/io, 0 where it can't be read."""
    try:
        for line in open("/proc/pressure/io"):
            if line.startswith("full"):
                return float(line.split("avg60=")[1].split()[0])
    except (OSError, IndexError, ValueError):
        pass
    return 0.0


def memory_room():
    """How many VMs of VM_MEM fit in the host's available memory beyond MEMORY_SPARE_MIB, at least one."""
    try:
        for line in open("/proc/meminfo"):
            if line.startswith("MemAvailable:"):
                return max(1, (int(line.split()[1]) // 1024 - MEMORY_SPARE_MIB) // int(os.environ.get("VM_MEM") or 3072))
    except (OSError, ValueError):
        pass
    return JOBS


def file_name(label):
    """A point's label as a file name: P12/3 is P12-3."""
    return label.replace("/", "-")


def wanted(number, subset):
    """Whether REPLAY_ONLY (P or P/S, comma-separated) and REPLAY_EVERY select point number or its subset."""
    if ONLY:
        return (str(number) if subset is None else f"{number}/{subset}") in ONLY
    return (number - 1) % EVERY == 0


def lines(text, kind):
    """pacman's lines as a multiset, but mtime mismatches and, for -Qkk, the per-package totals that count them and the exit status they set."""
    kept = Counter()
    for line in text.splitlines():
        if "(Modification time mismatch)" in line:
            continue
        if kind == "Qkk" and (SUMMARY.match(line) or line.startswith("exit ")):
            continue
        kept[line] += 1
    return kept


def diff(text, base, kind):
    a, b = lines(text, kind), lines(base, kind)
    return a - b, b - a


def package_diff(state, baselines):
    now = set(state["Q"].splitlines())
    return "; ".join(f"against {name}: +{sorted(now - set(b['Q'].splitlines()))[:6]} -{sorted(set(b['Q'].splitlines()) - now)[:6]}" for name, b in baselines.items())


class Spec:
    """What a test allows: fallback, whether a try boot is right for a root marked unfinished; identify(scenario, state, tried), which baseline the booted system is, or None; extra(), the test's own problems with the booted system; held, the driver's marks around the command that runs pacman, which holds db.lck from the first until the flush after the second."""

    def __init__(self, identify, fallback, extra=lambda: [], held=None):
        self.identify, self.fallback, self.extra, self.held = identify, fallback, extra, held


def by_packages(scenario, state, tried):
    """The baseline whose package list the booted system has: "try" in a try boot, else the uncut system's before or after the run."""
    if tried is not None:
        return "try" if "try" in scenario.baselines else None
    return next((name for name, base in scenario.baselines.items() if name != "try" and base["Q"] == state["Q"]), None)


def transaction(m, t, name, download, command, after=lambda: None):
    """A pacman transaction's scenario: a two-minute unblessed timeout and a reboot on a failed health check, download run first, a boot that counts as good; then command and the boot after it logged, with baselines before, after and of the slot in try mode, and every point replayed."""
    s = Scenario(m, t, name)
    with t.step("0: the transaction downloaded, a boot that counts as good, the marks in place"):
        common.boot()
        edit_config("'s|  boot = { loader = \"systemd-boot\" },|  boot = { loader = \"systemd-boot\", unblessed_timeout = 2, health_check = { reboot_on_failure = true } },|'")
        switch("switch to a two-minute unblessed timeout and a reboot on a failed health check")
        download()
        install(m)
        # A boot that counts as good arms the slot, so the transaction's snapshot becomes it.
        m.reboot()
        cut.settle()
        t.require(trying() is None and state().get("armed"), f"the boot before the transaction should be the current system's, blessed and armed: {trying()}, {state()}")
    s.freeze()

    def body():
        cut.settle()
        mark(m, "pacman-start")
        m.succeed(f"{command} > /root/transaction.log 2>&1", timeout=2400, what="the transaction")
        mark(m, "pacman-done")
        t.log(out("tail -c 2500 /root/transaction.log"))
        idle(m)
        m.reboot()
        cut.settle()
        t.require(trying() is None, f"the boot after the transaction should be the current system's: {trying()}")
        after()
        m.wait_until_succeeds(f"grep -q '\"armed\": true' {common.STATE}", timeout=300, what="the good boot arming the slot")
        mark(m, "end")

    with t.step("1: the transaction and the boot after it, logged"):
        s.record(body)
    spec = Spec(by_packages, fallback=True, held=("pacman-start", "pacman-done"))
    with t.step("2: the uncut systems' baselines: before the transaction, after it, and the slot in try mode"):
        s.boot(s.overlay("before", s.base), s.pristine_vars, "the system before the transaction")
        s.capture("before")
        s.boot(s.overlay("after", s.ran), s.end_vars, "the system after the transaction")
        s.capture("after")
        slot = state().get("slot")
        t.require(slot is not None, f"the system after the transaction should have a fallback slot: {state()}")
        m.select_entry(f"archix-snapshot-{slot}.conf")
        m.reboot()
        cut.settle()
        t.require(str((trying() or {}).get("snapshot")) == str(slot), f"the slot's entry should start snapshot {slot} in try mode: {trying()}")
        s.capture("try")
    s.control("before", s.overlay("before", s.base), s.pristine_vars, spec)
    s.control("after", s.overlay("after", s.ran), s.end_vars, spec)
    s.replay(spec)
    s.summary()
