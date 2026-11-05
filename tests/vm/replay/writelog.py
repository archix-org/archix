#!/usr/bin/env python3
# Usage: writelog.py check (a self-test) | writelog.py points LOG DISK; reads a dm-log-writes log, as QEMU's blklogwrites driver writes it, into the points a power cut can stop it at, and replays it onto a raw copy of the disk it started from.
import hashlib
import json
import os
import random
import struct
import subprocess
import sys
from dataclasses import dataclass, field

MAGIC = 0x6A736677736872
FLUSH, FUA, DISCARD, MARK = 1, 2, 4, 8
SECTOR = 512
# The marks' records, in the GPT gap before the first partition, which nothing else writes.
MARK_FIRST, MARK_END = 1024, 2048
MARK_TAG = b"ARCHIX-REPLAY-MARK "


@dataclass
class Entry:
    index: int
    sector: int
    count: int
    flags: int
    data: int
    where: str = ""
    mark: str = ""


@dataclass
class Point:
    """A state a power cut can leave: the log applied through entry `upto`; a flush point's ESP writes since the last flush can be lost in any subset."""

    number: int
    upto: int
    kind: str
    after: str
    writes: int
    names: list = field(default_factory=list)
    window: list = field(default_factory=list)
    floor: int = -1

    def label(self):
        return f"P{self.number} ({self.kind} at entry {self.upto}, {self.writes} writes after mark {self.after or 'none'})"


def layout(disk):
    """The ESP's and the root's sector ranges from the disk's partition table, [start, end)."""
    table = json.loads(subprocess.run(["sfdisk", "-J", disk], check=True, capture_output=True, text=True).stdout)["partitiontable"]["partitions"]
    if table[0]["start"] < MARK_END:
        raise ValueError(f"{disk}'s first partition starts at sector {table[0]['start']}, inside the marks' area")
    return {"esp": (table[0]["start"], table[0]["start"] + table[0]["size"]), "root": (table[1]["start"], table[1]["start"] + table[1]["size"])}


def read(path, parts):
    """Every entry of the log at path, each placed in parts' ranges ("esp", "root", "mark" or "other")."""
    entries = []
    with open(path, "rb") as f:
        magic, version, total, size = struct.unpack("<QQQI", f.read(28))
        if magic != MAGIC or version != 1 or size != SECTOR:
            raise ValueError(f"{path} isn't a version 1 write log with {SECTOR}-byte sectors (magic {magic:#x}, version {version}, sector size {size})")
        end = os.fstat(f.fileno()).st_size
        pos = SECTOR
        while pos + SECTOR <= end:
            f.seek(pos)
            sector, count, flags, length = struct.unpack("<QQQQ", f.read(32))
            if not (sector or count or flags or length):
                break
            pos += SECTOR
            entry = Entry(len(entries), sector, count, flags, pos)
            if flags & MARK:
                f.seek(pos)
                entry.mark, entry.where = f.read(length).decode(errors="replace"), "mark"
                pos += -(-length // SECTOR) * SECTOR
            elif not flags & DISCARD and count:
                pos += count * SECTOR
            if count and not flags & MARK:
                entry.where = next((name for name, (a, b) in parts.items() if a <= sector < b), "other")
                if MARK_FIRST <= sector < MARK_END:
                    f.seek(entry.data)
                    record = f.read(SECTOR)
                    if record.startswith(MARK_TAG):
                        entry.mark, entry.where = record[len(MARK_TAG):].split(b"\n")[0].decode(errors="replace"), "mark"
            entries.append(entry)
    if total and total != len(entries):
        raise ValueError(f"{path}'s superblock counts {total} entries, but {len(entries)} are there")
    return entries


def points(entries):
    """Every flush (or FUA write) and every mark, but where nothing outside the marks' area was written since the last point, which then takes its name too."""
    found, writes, after, window, floor, last = [], 0, "", [], -1, None
    for e in entries:
        if e.mark:
            after = e.mark.split(" ", 1)[-1]
        elif e.count and e.where != "mark":
            writes += 1
            if e.where == "esp" and not e.flags & FUA:
                window.append(e.index)
        flush = bool(e.flags & (FLUSH | FUA))
        if not (flush or e.mark):
            continue
        if writes == 0 and last is not None:
            last.names.append(e.mark.split(" ", 1)[-1] if e.mark else "flush")
            if flush:
                if "flush" not in last.kind:
                    last.kind, last.window, last.floor = f"{last.kind}+flush", window, floor
                window, floor = [], e.index
            continue
        last = Point(len(found) + 1, e.index, "flush" if flush else "mark", after, writes, [e.mark.split(" ", 1)[-1] if e.mark else "flush"])
        if flush:
            last.window, last.floor = window, floor
            window, floor = [], e.index
        found.append(last)
        writes = 0
    if writes and entries:
        found.append(Point(len(found) + 1, entries[-1].index, "end", after, writes, ["end"]))
    return found


def subsets(point, entries, limit, rng):
    """Up to limit sets of the ESP writes point's window could have kept, none of them all or nothing unless the window wrote the root too, as index tuples."""
    window = point.window
    if not window or limit <= 0:
        return []
    rooted = any(entries[i].where == "root" for i in range(point.floor + 1, point.upto))
    whole = tuple(window)
    if 2 ** len(window) - 1 <= limit + 1:
        picks = [tuple(i for k, i in enumerate(window) if mask >> k & 1) for mask in range(1, 2 ** len(window))]
    else:
        picks, seen, tries = [], set(), 0
        while len(picks) < limit + 1 and tries < limit * 50:
            tries += 1
            pick = tuple(i for i in window if rng.random() < 0.5)
            if pick and pick not in seen:
                seen.add(pick)
                picks.append(pick)
    return [p for p in picks if p != whole or rooted][:limit]


class Replay:
    """The disk image at path, as the log has been applied to it so far, moving forward only; a subset goes on top and comes off again."""

    def __init__(self, path, log, entries):
        self.disk = os.open(path, os.O_RDWR)
        self.log = os.open(log, os.O_RDONLY)
        self.entries = entries
        self.done = -1
        self.undo = []

    def write(self, e, keep=False):
        # What a discarded sector reads back is undefined; it keeps its data, as QEMU's qcow2 does below a cluster.
        if not e.count or e.flags & (MARK | DISCARD):
            return
        offset, size = e.sector * SECTOR, e.count * SECTOR
        if keep:
            self.undo.append((offset, os.pread(self.disk, size, offset)))
        os.pwrite(self.disk, os.pread(self.log, size, e.data), offset)

    def to(self, upto):
        if self.undo:
            raise RuntimeError("a subset is still applied")
        for e in self.entries[self.done + 1 : upto + 1]:
            self.write(e)
        self.done = max(self.done, upto)

    def apply(self, picks):
        """Puts the writes picks name on top of the disk as it is; returns a digest of the sectors they leave, or None if they leave them as they were."""
        digest, changed = hashlib.sha256(), False
        for i in picks:
            self.write(self.entries[i], keep=True)
        for offset, old in sorted(self.undo):
            now = os.pread(self.disk, len(old), offset)
            changed |= now != old
            digest.update(struct.pack("<Q", offset) + now)
        return digest.hexdigest()[:16] if changed else None

    def revert(self):
        for offset, old in reversed(self.undo):
            os.pwrite(self.disk, old, offset)
        self.undo = []

    def close(self):
        os.close(self.disk)
        os.close(self.log)


def selftest():
    """Writes a log by hand in blklogwrites' layout and checks the points, the subsets and the replay against it."""
    import tempfile

    with tempfile.TemporaryDirectory() as d:
        disk, log = os.path.join(d, "disk"), os.path.join(d, "log")
        with open(disk, "wb") as f:
            f.truncate(8192 * SECTOR)
        parts = {"esp": (2048, 4096), "root": (4096, 8192)}
        body = []

        def entry(sector, count, flags, data=b""):
            body.append(struct.pack("<QQQQ", sector, count, flags, 0).ljust(SECTOR, b"\0") + data)

        entry(4096, 1, 0, b"r" * SECTOR)
        entry(1024, 1, 0, (MARK_TAG + b"0-boot step-a\n").ljust(SECTOR, b"\0"))
        entry(2048, 2, 0, b"e" * 2 * SECTOR)
        entry(2050, 1, 0, b"f" * SECTOR)
        entry(4097, 1, 0, b"s" * SECTOR)
        entry(0, 0, FLUSH)
        entry(0, 0, FLUSH)
        entry(2051, 1, DISCARD)
        entry(1025, 1, 0, (MARK_TAG + b"1-boot step-b\n").ljust(SECTOR, b"\0"))
        entry(4098, 1, 0, b"t" * SECTOR)
        with open(log, "wb") as f:
            f.write(struct.pack("<QQQI", MAGIC, 1, len(body), SECTOR).ljust(SECTOR, b"\0") + b"".join(body))
        entries = read(log, parts)
        assert [e.where for e in entries] == ["root", "mark", "esp", "esp", "root", "", "", "esp", "mark", "root"], [e.where for e in entries]
        found = points(entries)
        assert [(p.kind, p.upto, p.after, p.writes) for p in found] == [("mark", 1, "step-a", 1), ("flush", 5, "step-a", 3), ("mark", 8, "step-b", 1), ("end", 9, "step-b", 1)], found
        assert found[1].names == ["flush", "flush"] and found[1].window == [2, 3] and found[1].floor == -1, found[1]
        picks = subsets(found[1], entries, 16, random.Random(1))
        assert sorted(picks) == [(2,), (2, 3), (3,)], picks
        r = Replay(disk, log, entries)
        r.to(1)
        before = open(disk, "rb").read()
        digest = r.apply((3,))
        assert open(disk, "rb").read()[2050 * SECTOR : 2051 * SECTOR] == b"f" * SECTOR and len(digest) == 16
        r.revert()
        assert open(disk, "rb").read() == before
        r.to(9)
        r.close()
        image = open(disk, "rb").read()
        assert image[4096 * SECTOR : 4099 * SECTOR] == b"r" * SECTOR + b"s" * SECTOR + b"t" * SECTOR
        assert image[2048 * SECTOR : 2051 * SECTOR] == b"e" * 2 * SECTOR + b"f" * SECTOR
    print("writelog: self-test passed")


def main():
    if sys.argv[1:] == ["check"]:
        selftest()
        return 0
    if len(sys.argv) == 4 and sys.argv[1] == "points":
        entries = read(sys.argv[2], layout(sys.argv[3]))
        for p in points(entries):
            print(f"{p.label()} names={','.join(p.names)} esp-window={len(p.window)}")
        return 0
    print("usage: writelog.py check | writelog.py points LOG DISK", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main())
