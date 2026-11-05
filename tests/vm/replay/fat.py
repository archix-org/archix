# A read-only FAT32 reader over an image file, for the write replay to see the ESP's files at a point without mounting it: long names, cluster chains, and a chain that runs into a free cluster read as torn.
import struct

END = 0x0FFFFFF8


class Fat:
    """The FAT32 filesystem at byte offset of the file at path."""

    def __init__(self, path, offset=0):
        self.f = open(path, "rb")
        self.offset = offset
        boot = self.read(0, 512)
        self.bps, self.spc, reserved, fats = struct.unpack_from("<HBHB", boot, 11)
        fat16 = struct.unpack_from("<H", boot, 22)[0]
        size, self.root = struct.unpack_from("<II", boot, 36)[0], struct.unpack_from("<I", boot, 44)[0]
        if fat16 or not self.bps or not self.spc:
            raise ValueError("not a FAT32 boot sector")
        self.fat = reserved * self.bps
        self.data = (reserved + fats * size) * self.bps
        self.cluster = self.bps * self.spc

    def read(self, at, size):
        self.f.seek(self.offset + at)
        return self.f.read(size)

    def chain(self, first):
        """The clusters from first, and whether the chain ended properly."""
        found, c = [], first
        while 2 <= c < END and len(found) < 1 << 20:
            if c in found:
                return found, False
            found.append(c)
            c = struct.unpack("<I", self.read(self.fat + c * 4, 4))[0] & 0x0FFFFFFF
        return found, c >= END

    def contents(self, first, size=None):
        clusters, whole = self.chain(first) if first else ([], True)
        data = b"".join(self.read(self.data + (c - 2) * self.cluster, self.cluster) for c in clusters)
        if size is not None:
            whole = whole and len(data) >= size
            data = data[:size]
        return data, whole

    def entries(self, first):
        """A directory's (name, attributes, first cluster, size)."""
        data, _ = self.contents(first)
        long = {}
        for i in range(0, len(data), 32):
            e = data[i : i + 32]
            if e[0] == 0:
                break
            if e[0] == 0xE5:
                long = {}
                continue
            attr = e[11]
            if attr == 0x0F:
                part = e[1:11] + e[14:26] + e[28:32]
                long[e[0] & 0x3F] = part.decode("utf-16-le", "replace").split("\0")[0].rstrip("￿")
                continue
            if attr & 0x08:
                long = {}
                continue
            name = "".join(long[k] for k in sorted(long)) if long else short(e)
            long = {}
            if name in (".", ".."):
                continue
            first_cluster = struct.unpack_from("<H", e, 20)[0] << 16 | struct.unpack_from("<H", e, 26)[0]
            yield name, attr, first_cluster, struct.unpack_from("<I", e, 28)[0]

    def files(self, under):
        """Every file below the directories under (like "loader/entries"), as {path: (bytes, whole)}."""
        found = {}
        for top in under:
            cluster, parts = self.root, top.split("/")
            for part in parts:
                match = next((e for e in self.entries(cluster) if e[0].lower() == part.lower() and e[1] & 0x10), None)
                if match is None:
                    cluster = None
                    break
                cluster = match[2]
            if cluster is not None:
                self.walk(cluster, top, found)
        return found

    def walk(self, cluster, prefix, found):
        for name, attr, first, size in self.entries(cluster):
            path = f"{prefix}/{name}"
            if attr & 0x10:
                self.walk(first, path, found)
            else:
                found[path] = self.contents(first, size)

    def file(self, path):
        """One file's (bytes, whole), or None."""
        top, _, name = path.rpartition("/")
        for e in self.entries(self.lookup(top)) if self.lookup(top) else []:
            if e[0].lower() == name.lower() and not e[1] & 0x10:
                return self.contents(e[2], e[3])
        return None

    def lookup(self, path):
        cluster = self.root
        for part in [p for p in path.split("/") if p]:
            match = next((e for e in self.entries(cluster) if e[0].lower() == part.lower() and e[1] & 0x10), None)
            if match is None:
                return None
            cluster = match[2]
        return cluster

    def close(self):
        self.f.close()


def short(e):
    base, ext = e[0:8].decode("ascii", "replace").rstrip(), e[8:11].decode("ascii", "replace").rstrip()
    return f"{base}.{ext}" if ext else base
