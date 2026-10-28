#!/usr/bin/env python
# Usage: l2.py QEMU-STREAM-SOCKET REACH-SOCKET; in a container of its own network namespace, joins the VM's second NIC to a tap with LAN and public test addresses on both families, and answers "reach" on REACH-SOCKET with which of the VM's ports answer ssh from each.
import fcntl
import os
import socket
import struct
import subprocess
import sys
import threading
import time

stream, reach = sys.argv[1], sys.argv[2]
TUNSETIFF, IFF_TAP, IFF_NO_PI = 0x400454CA, 0x0002, 0x1000
# The VM's addresses (tests/vm/firewall/config.lua) by what sources = "lan" makes of ours on the same subnet.
TARGETS = [("lan4", "192.168.77.2"), ("lan6", "fd00:77::2"), ("wan4", "198.51.100.2"), ("wan6", "2001:db8::2")]
PORTS = [22, 2201, 2202]

tap = os.open("/dev/net/tun", os.O_RDWR)
fcntl.ioctl(tap, TUNSETIFF, struct.pack("16sH", b"probe0", IFF_TAP | IFF_NO_PI))
for cmd in (["link", "set", "probe0", "up"], ["addr", "add", "192.168.77.1/24", "dev", "probe0"],
            ["addr", "add", "198.51.100.1/24", "dev", "probe0"], ["-6", "addr", "add", "fd00:77::1/64", "dev", "probe0", "nodad"],
            ["-6", "addr", "add", "2001:db8::1/64", "dev", "probe0", "nodad"]):
    subprocess.run(["ip", *cmd], check=True)

for _ in range(600):
    try:
        qemu = socket.socket(socket.AF_UNIX)
        qemu.connect(stream)
        break
    except OSError:
        time.sleep(0.5)
else:
    sys.exit(f"QEMU's stream socket {stream} never took a connection")
print("joined the VM's second NIC to probe0", flush=True)


def exactly(n):
    data = b""
    while len(data) < n:
        chunk = qemu.recv(n - len(data))
        if not chunk:
            raise EOFError
        data += chunk
    return data


def to_tap():
    try:
        while True:
            os.write(tap, exactly(struct.unpack(">I", exactly(4))[0]))
    except (EOFError, OSError):
        os._exit(0)


def to_qemu():
    while True:
        frame = os.read(tap, 65536)
        qemu.sendall(struct.pack(">I", len(frame)) + frame)


threading.Thread(target=to_tap, daemon=True).start()
threading.Thread(target=to_qemu, daemon=True).start()


def answers(addr, port):
    try:
        with socket.create_connection((addr, port), timeout=5) as conn:
            conn.settimeout(5)
            return "open" if conn.recv(64).startswith(b"SSH-") else "closed"
    except OSError:
        return "closed"


if os.path.exists(reach):
    os.unlink(reach)
server = socket.socket(socket.AF_UNIX)
server.bind(reach)
server.listen()
while True:
    conn, _ = server.accept()
    with conn:
        conn.makefile("r").readline()
        result = " ".join(f"{name}/{port}={answers(addr, port)}" for name, addr in TARGETS for port in PORTS)
        print("reach:", result, flush=True)
        conn.sendall((result + "\n").encode())
