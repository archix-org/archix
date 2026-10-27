#!/usr/bin/env python3
"""The rescue suite's console: copies QEMU's serial socket into serial.log and types each `expect<TAB>send` step of the script the guest last announced (`VMTEST: rescue script: <base64>`) once `expect` shows; `<enter>` sends an empty line, and a step `!text` fails the run if text shows before the script's last step does."""

import base64
import json
import os
import re
import socket
import sys
import time

serial_sock, log_path, qmp_sock = sys.argv[1:4]
step_timeout = int(os.environ.get("RESCUE_STEP_TIMEOUT", "900"))

log = open(log_path, "ab", buffering=0)


def note(line):
    log.write(f"\r\nVMTEST: rescue: {line}\r\n".encode())


def quit_qemu():
    try:
        q = socket.socket(socket.AF_UNIX)
        q.connect(qmp_sock)
        q.recv(4096)
        for command in ("qmp_capabilities", "quit"):
            q.sendall(json.dumps({"execute": command}).encode() + b"\n")
            time.sleep(0.5)
    except OSError:
        pass


console = None
for _ in range(600):
    try:
        console = socket.socket(socket.AF_UNIX)
        console.connect(serial_sock)
        break
    except OSError:
        console = None
        time.sleep(0.1)
if console is None:
    sys.exit(f"couldn't connect to {serial_sock}")
console.settimeout(1)

seen = b""
script = []
forbidden = []
watched = b""
# Only a whole line: the console arrives in chunks, and half an announcement decodes to nothing.
announced = re.compile(rb"VMTEST: rescue script: ([A-Za-z0-9+/=]+)\r?\n")


def fail(why):
    note(f"FAIL: {why}")
    log.write(b"\r\nVMTEST: FAIL: the rescue prompt didn't go as scripted\r\n")
    quit_qemu()


waiting_since = None
while True:
    try:
        data = console.recv(65536)
        if not data:
            break
        log.write(data)
        seen += data
        if forbidden:
            watched += data
    except socket.timeout:
        pass
    except OSError:
        break
    for match in announced.finditer(seen):
        try:
            text = base64.b64decode(match.group(1)).decode()
            script = [line.split("\t", 1) for line in text.splitlines() if line]
            assert all(len(step) == 2 for step in script)
        except Exception as e:
            fail(f"the announced script doesn't decode: {e!r}")
            sys.exit(1)
        note(f"got a script of {len(script)} steps")
        seen = seen[match.end():]
        forbidden, watched = [], b""
        waiting_since = time.time()
    while script and script[0][0].startswith("!"):
        forbidden.append(script.pop(0)[0][1:].encode())
        watched = seen
        note(f"watching for {forbidden[-1]!r}, which must not show")
    shown = [text for text in forbidden if text in watched]
    if shown:
        fail(f"{shown[0]!r} showed")
        break
    if not script:
        if forbidden:
            note(f"none of {forbidden!r} showed")
            forbidden, watched = [], b""
        continue
    expect, send = script[0]
    at = seen.find(expect.encode())
    if at >= 0:
        seen = seen[at + len(expect):]
        script.pop(0)
        waiting_since = time.time()
        if send:
            note(f"typing {send!r} after {expect!r}")
            console.sendall((b"" if send == "<enter>" else send.encode()) + b"\r")
        else:
            note(f"saw {expect!r}")
    elif time.time() - waiting_since > step_timeout:
        fail(f"{expect!r} never showed in {step_timeout} s")
        break
