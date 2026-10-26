#!/usr/bin/env python
# Usage: qmp.py QMP-SOCKET COMMAND...; runs each HMP command line through QMP's human-monitor-command and prints its output under a "(qemu) COMMAND" line.
import json
import socket
import sys

sock = socket.socket(socket.AF_UNIX)
sock.settimeout(10)
sock.connect(sys.argv[1])
reader = sock.makefile("r")


def call(execute, **arguments):
    sock.sendall(json.dumps({"execute": execute, "arguments": arguments}).encode() + b"\n")
    while True:
        reply = json.loads(reader.readline())
        if "return" in reply or "error" in reply:
            return reply


json.loads(reader.readline())
call("qmp_capabilities")
for line in sys.argv[2:]:
    reply = call("human-monitor-command", **{"command-line": line})
    print(f"(qemu) {line}")
    print(json.dumps(reply["error"]) + "\n" if "error" in reply else reply["return"], end="", flush=True)
