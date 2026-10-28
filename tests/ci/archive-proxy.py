#!/usr/bin/env python3
"""Caching proxy for archive.archlinux.org's immutable /repos/<date>/ and /packages/ files, and /packages/ listings pinned at their first fetch; it keeps only files it has verified."""

import argparse
import base64
import email.utils
import gzip
import hashlib
import http.server
import io
import lzma
import os
import re
import shutil
import subprocess
import sys
import tarfile
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

CACHEABLE = re.compile(r"^/(repos/\d{4}/\d{2}/\d{2}/[^/]+/os/[^/]+|packages/[^/]/[^/]+)/[^/]+$")
LISTING = re.compile(r"^/packages/[^/]/[^/]+/$")
INDEX = "index.html"
PACKAGE_FILE = re.compile(r"^packages/[^/]/([^/]+)/([^/]+)$")
REPO_FILE = re.compile(r"^(repos/\d{4}/\d{2}/\d{2}/([^/]+)/os/[^/]+)/[^/]+$")
DB = re.compile(r"\.(db|files)(\.tar\.(gz|xz|zst))?$")
ZSTD = b"\x28\xb5\x2f\xfd"
CHUNK = 1 << 20


def cacheable(path):
    parts = path[1:].split("/")
    return bool(CACHEABLE.match(path)) and not any(part in ("", "..") or part.startswith(".") for part in parts)


def listed(text, name):
    hrefs = {urllib.parse.unquote(href) for href in re.findall(r'href="([^"]+)"', text)}
    return {href for href in hrefs if href.startswith(f"{name}-") and ".pkg.tar" in href}


def magic(path):
    with open(path, "rb") as f:
        return f.read(6)


def decompressed(path):
    head = magic(path)
    if head[:2] == b"\x1f\x8b":
        return gzip.open(path, "rb")
    if head == b"\xfd7zXZ\x00":
        return lzma.open(path, "rb")
    if head[:4] == ZSTD:
        return io.BytesIO(subprocess.run(["zstd", "-dcq", str(path)], capture_output=True, check=True).stdout)
    raise ValueError("not gzip, xz or zstd")


def stream_complete(path):
    if magic(path)[:4] == ZSTD:
        return subprocess.run(["zstd", "-tq", str(path)], stderr=subprocess.DEVNULL).returncode == 0
    try:
        with decompressed(path) as f:
            while f.read(CHUNK):
                pass
        return True
    except (ValueError, EOFError, OSError, lzma.LZMAError):
        return False


def db_entries(path):
    entries = {}
    with decompressed(path) as raw, tarfile.open(fileobj=raw, mode="r|") as tar:
        for member in tar:
            data = tar.extractfile(member).read() if member.isfile() else b""
            if not member.name.endswith("/desc"):
                continue
            fields, key = {}, None
            for line in data.decode().splitlines():
                if line.startswith("%") and line.endswith("%"):
                    key = line[1:-1]
                elif line and key and key not in fields:
                    fields[key] = line
            if "FILENAME" in fields:
                sig = base64.b64decode(fields["PGPSIG"]) if "PGPSIG" in fields else None
                entries[fields["FILENAME"]] = (int(fields["CSIZE"]), fields.get("SHA256SUM"), sig)
    if not entries:
        raise ValueError("no packages in it")
    return entries


def one_signature_packet(data):
    if len(data) < 2 or not data[0] & 0x80:
        return False
    if data[0] & 0x40:
        tag, first = data[0] & 0x3F, data[1]
        if first < 192:
            header, length = 2, first
        elif first < 224 and len(data) > 2:
            header, length = 3, ((first - 192) << 8) + data[2] + 192
        elif first == 255 and len(data) > 5:
            header, length = 6, int.from_bytes(data[2:6], "big")
        else:
            return False
    else:
        tag, kind = (data[0] >> 2) & 0x0F, data[0] & 3
        if kind == 3:
            return False
        header = 1 + (1 << kind)
        length = int.from_bytes(data[1:header], "big")
    return tag == 2 and header + length == len(data)


def sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as f:
        while block := f.read(CHUNK):
            digest.update(block)
    return digest.hexdigest()


def repo_db_rel(rel):
    match = REPO_FILE.match(rel)
    return f"{match.group(1)}/{match.group(2)}.db" if match else None


class Store:
    def __init__(self, cache, extra):
        self.roots = [Path(cache)] + [Path(e) for e in extra]
        self.lock = threading.Lock()
        self.dbs = {}
        self.index = {}

    def find(self, rel):
        return next((root / rel for root in self.roots if (root / rel).is_file()), None)

    def learn_db(self, rel, path):
        entries = db_entries(path)
        with self.lock:
            self.dbs[rel] = entries
            for name, entry in entries.items():
                self.index.setdefault(name, entry)

    def repo_db(self, rel):
        db_rel = repo_db_rel(rel)
        if db_rel and db_rel not in self.dbs and (path := self.find(db_rel)):
            try:
                self.learn_db(db_rel, path)
            except Exception:
                pass
        return self.dbs.get(db_rel)

    # A package file its package's pinned listing doesn't name, which the proxy answers 404 for without asking upstream.
    def unlisted(self, rel):
        match = PACKAGE_FILE.match(rel)
        if not match or match.group(2) == INDEX or not (index := self.find(f"{rel.rsplit('/', 1)[0]}/{INDEX}")):
            return False
        return urllib.parse.unquote(match.group(2)).removesuffix(".sig") not in listed(index.read_text(errors="replace"), match.group(1))

    # Returns (ok, how it was checked), or None for a file it can't check, which then isn't cached.
    def verify(self, rel, path):
        name = rel.rsplit("/", 1)[1]
        if name == INDEX and (match := PACKAGE_FILE.match(rel)):
            files = listed(path.read_text(errors="replace"), match.group(1))
            return bool(files), f"a listing of {len(files)} {match.group(1)} files" if files else f"no {match.group(1)} files in it"
        if DB.search(name):
            try:
                self.learn_db(rel, path)
                return True, "a complete db"
            except Exception as e:
                return False, f"unreadable db: {e}"
        base = name.removesuffix(".sig")
        entry = (self.repo_db(rel) or {}).get(base) or self.index.get(base)
        if name.endswith(".sig"):
            if entry and entry[2]:
                return path.read_bytes() == entry[2], "the db's signature"
            return one_signature_packet(path.read_bytes()), "one whole signature packet"
        if ".pkg.tar" not in name:
            return None
        if entry:
            size = path.stat().st_size
            if size != entry[0]:
                return False, f"{size} bytes where the db says {entry[0]}"
            if entry[1] and sha256(path) != entry[1]:
                return False, "sha256 differs from the db's"
            return True, "the db's size and sha256"
        return stream_complete(path), "a complete compressed stream"


class Proxy(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    store = upstream = log = new_list = None
    log_lock = threading.Lock()

    def log_message(self, *args):
        pass

    def note(self, outcome, detail=""):
        with self.log_lock:
            print(f"{time.strftime('%H:%M:%S')} {outcome} {self.command} {self.path} {detail}".rstrip(), file=self.log, flush=True)

    def do_GET(self):
        self.serve(head=False)

    def do_HEAD(self):
        self.serve(head=True)

    def serve(self, head):
        url = urllib.parse.urlsplit(self.path)
        if url.path == "/archix-proxy":
            return self.reply(200, b"ok\n", head)
        listing = bool(LISTING.match(url.path)) and cacheable(url.path + INDEX)
        if url.query or not (listing or cacheable(url.path)):
            return self.passthrough(head)
        rel = url.path[1:] + (INDEX if listing else "")
        if cached := self.store.find(rel):
            self.note("HIT")
            return self.send_file(cached, head, "text/html" if listing else "application/octet-stream")
        if self.store.unlisted(rel):
            self.note("UNLISTED", "404: the pinned listing has no such file")
            return self.reply(404, b"", head)
        if head or "Range" in self.headers:
            return self.passthrough(head)
        db_rel = repo_db_rel(rel)
        if db_rel and db_rel != rel and ".pkg.tar" in rel and self.store.repo_db(rel) is None:
            self.fetch(db_rel, None)
        self.fetch(rel, self, url.path if listing else None)

    def reply(self, status, body, head, headers=()):
        self.send_response(status)
        for key, value in headers:
            self.send_header(key, value)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        if not head:
            self.wfile.write(body)

    def send_file(self, path, head, kind):
        size = path.stat().st_size
        start, end = 0, size - 1
        if match := re.fullmatch(r"bytes=(\d+)-(\d*)", self.headers.get("Range", "")):
            start, end = int(match.group(1)), min(int(match.group(2) or end), end)
            if start > end:
                return self.reply(416, b"", head, [("Content-Range", f"bytes */{size}")])
            self.send_response(206)
            self.send_header("Content-Range", f"bytes {start}-{end}/{size}")
        else:
            self.send_response(200)
        self.send_header("Content-Length", str(end - start + 1))
        self.send_header("Last-Modified", email.utils.formatdate(path.stat().st_mtime, usegmt=True))
        self.send_header("Content-Type", kind)
        self.end_headers()
        if head:
            return
        with open(path, "rb") as f:
            f.seek(start)
            left = end - start + 1
            while left > 0 and (block := f.read(min(CHUNK, left))):
                self.wfile.write(block)
                left -= len(block)

    def open_upstream(self, path, method="GET", headers=None):
        request = urllib.request.Request(self.upstream + path, method=method, headers={"User-Agent": "archix-ci-proxy", **(headers or {})})
        for attempt in range(1, 4):
            try:
                return urllib.request.urlopen(request, timeout=60)
            except urllib.error.HTTPError:
                raise
            except OSError:
                if attempt == 3:
                    raise
                time.sleep(5 * attempt)

    def relay_headers(self, response, status):
        self.send_response(status)
        for key in ("Content-Length", "Content-Range", "Content-Type", "Last-Modified"):
            if response.headers.get(key):
                self.send_header(key, response.headers[key])
        if not response.headers.get("Content-Length"):
            self.send_header("Connection", "close")
            self.close_connection = True
        self.end_headers()

    def passthrough(self, head):
        headers = {"Range": self.headers["Range"]} if "Range" in self.headers else {}
        try:
            response = self.open_upstream(self.path, "HEAD" if head else "GET", headers)
        except urllib.error.HTTPError as e:
            self.note("PASS", str(e.code))
            return self.reply(e.code, b"" if head else e.read(), head)
        except OSError as e:
            self.note("PASS", f"upstream: {e}")
            return self.reply(502, b"", head)
        with response:
            self.note("PASS", str(response.status))
            self.relay_headers(response, response.status)
            if not head:
                shutil.copyfileobj(response, self.wfile, CHUNK)

    def fetch(self, rel, client, upstream_path=None):
        try:
            response = self.open_upstream(upstream_path or "/" + rel)
        except (urllib.error.HTTPError, OSError) as e:
            status = e.code if isinstance(e, urllib.error.HTTPError) else 502
            self.note("MISS", f"{rel}: {e}")
            if client:
                self.reply(status, e.read() if status != 502 else b"", False)
            return
        dest = self.store.roots[0] / rel
        dest.parent.mkdir(parents=True, exist_ok=True)
        tmp = dest.parent / f".part-{os.getpid()}-{threading.get_ident()}-{dest.name}"
        expected = response.headers.get("Content-Length")
        received = 0
        last = b""

        def send(block):
            nonlocal client
            if client:
                try:
                    client.wfile.write(block)
                except OSError:
                    client.close_connection = True
                    client = None

        with response, open(tmp, "wb") as out:
            if client:
                client.relay_headers(response, 200)
            while block := response.read(CHUNK):
                out.write(block)
                received += len(block)
                send(last)
                last = block
            out.flush()
            os.fsync(out.fileno())
        if expected is not None and received != int(expected):
            result = (False, f"{received} of {expected} bytes")
        else:
            result = self.store.verify(rel, tmp)
        if result and result[0]:
            if modified := response.headers.get("Last-Modified"):
                stamp = email.utils.parsedate_to_datetime(modified).timestamp()
                os.utime(tmp, (stamp, stamp))
            os.replace(tmp, dest)
            fd = os.open(dest.parent, os.O_RDONLY)
            os.fsync(fd)
            os.close(fd)
            with self.log_lock, open(self.new_list, "a") as new:
                print(rel, file=new)
            self.note("MISS", f"cached, checked against {result[1]}")
        else:
            tmp.unlink(missing_ok=True)
            self.note("BAD" if result else "NOCACHE", f"{rel}: {result[1] if result else 'not a file it can check'}")
        # The last block goes out once the file is cached, and never for a bad one, so a client can't take a bad file for a whole one.
        if result is None or result[0]:
            send(last)
        elif client:
            client.close_connection = True


def verify_dirs(dirs):
    store = Store(dirs[0], dirs[1:])
    files = [(root, path) for root in store.roots if root.is_dir() for path in sorted(root.rglob("*")) if path.is_file()]
    files.sort(key=lambda item: not DB.search(item[1].name))
    good = bad = 0
    for root, path in files:
        rel = path.relative_to(root).as_posix()
        result = None if path.name.startswith(".") else store.verify(rel, path)
        if result and result[0]:
            good += 1
            continue
        bad += 1
        print(f"removed {path}: {result[1] if result else 'not a file it can check'}")
        path.unlink()
    print(f"archive cache: {good} files verified, {bad} removed")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    serve = commands.add_parser("serve", help="serve the archive, caching verified files")
    serve.add_argument("--cache", required=True, help="where new files go")
    serve.add_argument("--extra", action="append", default=[], help="a read-only cache to serve from too")
    serve.add_argument("--bind", action="append", default=[], help="an address to listen on (127.0.0.1 by default)")
    serve.add_argument("--port", type=int, default=8079)
    serve.add_argument("--upstream", default="https://archive.archlinux.org")
    serve.add_argument("--log", default="-")
    serve.add_argument("--new-list", default=os.devnull, help="a file each newly cached path is appended to")
    verify = commands.add_parser("verify", help="check every cached file and remove the bad ones")
    verify.add_argument("dirs", nargs="+")
    args = parser.parse_args()
    if args.command == "verify":
        return verify_dirs(args.dirs)
    Path(args.cache).mkdir(parents=True, exist_ok=True)
    Proxy.store = Store(args.cache, args.extra)
    Proxy.upstream = args.upstream.rstrip("/")
    Proxy.log = sys.stdout if args.log == "-" else open(args.log, "a")
    Proxy.new_list = args.new_list
    binds = args.bind or ["127.0.0.1"]
    servers = [http.server.ThreadingHTTPServer((bind, args.port), Proxy) for bind in binds]
    for server in servers[1:]:
        threading.Thread(target=server.serve_forever, daemon=True).start()
    print(f"serving {Proxy.upstream} on {', '.join(binds)} port {args.port}", file=Proxy.log, flush=True)
    servers[0].serve_forever()


if __name__ == "__main__":
    main()
