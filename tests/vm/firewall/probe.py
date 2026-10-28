#!/usr/bin/env python
# Usage: probe.py CONTROL-SOCKET PORT KEY REACH-SOCKET; answers each "probe TAG" line the VM writes with whether sshd answered on the forwarded PORT and what an ssh login read there, and each "reach TAG" with what l2.py found on the VM's second NIC.
import os
import socket
import subprocess
import sys
import time

control, port, key, reach = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4]


def banner():
    try:
        with socket.create_connection(("127.0.0.1", port), timeout=10) as conn:
            conn.settimeout(10)
            return conn.recv(64).decode(errors="replace").strip() or "none"
    except OSError as e:
        return f"none ({e.__class__.__name__})"


def login():
    ssh = ["ssh", "-i", key, "-p", str(port), "-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=no",
           "-o", "UserKnownHostsFile=/dev/null", "-o", "ConnectTimeout=10", "-o", "LogLevel=ERROR",
           "root@127.0.0.1", "cat /run/archix/deployment"]
    try:
        done = subprocess.run(ssh, capture_output=True, text=True, timeout=30)
    except subprocess.TimeoutExpired:
        return "failed (timeout)"
    if done.returncode == 0:
        return done.stdout.strip()
    return "failed (" + (done.stderr.strip().splitlines() or ["exit %d" % done.returncode])[-1] + ")"


def reached():
    try:
        with socket.socket(socket.AF_UNIX) as conn:
            conn.settimeout(120)
            conn.connect(reach)
            conn.sendall(b"reach\n")
            return conn.makefile("r").readline().strip() or "failed (no answer)"
    except OSError as e:
        return f"failed ({e.__class__.__name__})"


for _ in range(600):
    if os.path.exists(control):
        break
    time.sleep(0.5)
conn = socket.socket(socket.AF_UNIX)
conn.connect(control)
lines = conn.makefile("r", encoding="utf-8", errors="replace")
for line in lines:
    words = line.split()
    if len(words) != 2 or words[0] not in ("probe", "reach"):
        continue
    if words[0] == "reach":
        reply = f"{words[1]} {reached()}"
        print(reply, flush=True)
    else:
        seen = banner()
        reply = f"{words[1]} banner={'ssh' if seen.startswith('SSH-') else 'none'} login={login()}"
        print(reply, f"(banner: {seen})", flush=True)
    conn.sendall((reply + "\n").encode())
