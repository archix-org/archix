#!/usr/bin/env python3
"""Checks that archive-proxy.py never caches or keeps a truncated or corrupt file, against a fake upstream."""

import hashlib
import http.client
import http.server
import io
import os
import subprocess
import sys
import tarfile
import tempfile
import threading
import time
import urllib.error
import urllib.request
from pathlib import Path

PROXY = Path(__file__).with_name("archive-proxy.py")
DIR = "repos/2026/10/10/core/os/x86_64"


def package(name):
    return subprocess.run(["zstd", "-q", "-c"], input=os.urandom(300_000) + name.encode(), capture_output=True, check=True).stdout


def repo_db(packages):
    out = io.BytesIO()
    with tarfile.open(fileobj=out, mode="w:gz") as tar:
        for name, data in packages.items():
            desc = f"%FILENAME%\n{name}\n\n%CSIZE%\n{len(data)}\n\n%SHA256SUM%\n{hashlib.sha256(data).hexdigest()}\n\n".encode()
            info = tarfile.TarInfo(name.removesuffix("-x86_64.pkg.tar.zst") + "/desc")
            info.size = len(desc)
            tar.addfile(info, io.BytesIO(desc))
    return out.getvalue()


def main():
    good, truncated, corrupt = (f"{n}-1-1-x86_64.pkg.tar.zst" for n in ("good", "truncated", "corrupt"))
    loose = "loose-1-1-x86_64.pkg.tar.zst"
    files = {name: package(name) for name in (good, truncated, corrupt, loose)}
    db = repo_db({name: files[name] for name in (good, truncated, corrupt)})
    served = {
        f"/{DIR}/core.db": db,
        f"/{DIR}/{good}": files[good],
        f"/{DIR}/{truncated}": files[truncated],
        f"/{DIR}/{corrupt}": bytes([files[corrupt][0] ^ 1]) + files[corrupt][1:],
        f"/packages/l/loose/{loose}": files[loose],
        f"/packages/l/loose/cut-{loose}": files[loose][:-1000],
        "/packages/l/loose/": f'<a href="../">../</a>\n<a href="{loose}">{loose}</a> 21-Oct-2026 10:00 300K\n'.encode(),
        "/packages/e/empty/": b'<a href="../">../</a>\n',
    }
    requested = []

    class Upstream(http.server.BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_GET(self):
            requested.append(self.path)
            body = served.get(self.path)
            if body is None:
                self.send_response(404)
                self.send_header("Content-Length", "0")
                self.end_headers()
                return
            self.send_response(200)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body[: len(body) // 2] if truncated in self.path else body)

    upstream = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Upstream)
    threading.Thread(target=upstream.serve_forever, daemon=True).start()
    failures = []

    def check(ok, what):
        print(f"{'ok  ' if ok else 'FAIL'} {what}")
        if not ok:
            failures.append(what)

    with tempfile.TemporaryDirectory() as tmp:
        cache = Path(tmp, "cache")
        port = 18000 + os.getpid() % 2000
        proxy = subprocess.Popen([sys.executable, str(PROXY), "serve", "--cache", str(cache), "--port", str(port),
                                  "--upstream", f"http://127.0.0.1:{upstream.server_port}", "--log", f"{tmp}/log", "--new-list", f"{tmp}/new"])
        try:
            base = f"http://127.0.0.1:{port}"
            for _ in range(50):
                try:
                    urllib.request.urlopen(f"{base}/archix-proxy", timeout=1).read()
                    break
                except OSError:
                    time.sleep(0.1)

            def get(path):
                try:
                    return urllib.request.urlopen(base + path, timeout=10).read()
                except (urllib.error.URLError, http.client.IncompleteRead, ConnectionError) as e:
                    return e

            check(get(f"/{DIR}/{good}") == files[good], "a good package passes through whole")
            check((cache / DIR / good).is_file(), "a good package is cached")
            check(not isinstance(get(f"/{DIR}/{truncated}"), bytes), "a truncated transfer reaches the client as an error")
            check(not (cache / DIR / truncated).exists(), "a truncated transfer isn't cached")
            check(not isinstance(get(f"/{DIR}/{corrupt}"), bytes), "a package whose sha256 differs from the db's reaches the client as an error")
            check(not (cache / DIR / corrupt).exists(), "a package whose sha256 differs from the db's isn't cached")
            check(get(f"/packages/l/loose/{loose}") == files[loose], "a package in no db passes through whole")
            check((cache / "packages/l/loose" / loose).is_file(), "a package in no db is cached once its zstd stream is whole")
            check(not isinstance(get(f"/packages/l/loose/cut-{loose}"), bytes), "a package in no db with a cut zstd stream reaches the client as an error")
            check(not (cache / f"packages/l/loose/cut-{loose}").exists(), "a package in no db with a cut zstd stream isn't cached")
            listing = served["/packages/l/loose/"]
            check(get("/packages/l/loose/") == listing, "a package's listing passes through")
            index = cache / "packages/l/loose/index.html"
            check(index.is_file() and index.read_bytes() == listing, "a package's listing is cached")
            served["/packages/l/loose/"] = listing + b'<a href="loose-2-1-x86_64.pkg.tar.zst">x</a>\n'
            check(get("/packages/l/loose/") == listing, "a cached listing stays as first fetched, pinned like the files")
            before = len(requested)
            missing = get("/packages/l/loose/loose-0-1-x86_64.pkg.tar.zst")
            check(getattr(missing, "code", None) == 404 and len(requested) == before, "a file the pinned listing lacks is 404 without asking upstream")
            get("/packages/e/empty/")
            check(not (cache / "packages/e/empty/index.html").exists(), "a listing without the package's files isn't cached")
        finally:
            proxy.terminate()
            proxy.wait()

        restored = Path(tmp, "restored")
        (restored / DIR).mkdir(parents=True)
        (restored / DIR / "core.db").write_bytes(db)
        (restored / DIR / good).write_bytes(files[good])
        (restored / DIR / truncated).write_bytes(files[truncated][:-4096])
        (restored / DIR / corrupt).write_bytes(bytes([files[corrupt][0] ^ 1]) + files[corrupt][1:])
        (restored / DIR / f".part-1-1-{good}").write_bytes(files[good][:100])
        report = subprocess.run([sys.executable, str(PROXY), "verify", str(restored)], capture_output=True, text=True, check=True).stdout
        print(report, end="")
        check((restored / DIR / good).is_file() and (restored / DIR / "core.db").is_file(), "verify keeps a good db and package")
        check(not (restored / DIR / truncated).exists(), "verify removes a package cut short, as a power cut leaves it")
        check(not (restored / DIR / corrupt).exists(), "verify removes a package whose sha256 differs from the db's")
        check(not (restored / DIR / f".part-1-1-{good}").exists(), "verify removes a leftover partial download")
    upstream.shutdown()
    if failures:
        sys.exit(f"{len(failures)} archive proxy checks failed")


if __name__ == "__main__":
    main()
