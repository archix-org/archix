#!/usr/bin/env python3
"""Boots the test VM with QEMU, as the calling user, for one job: run inside a plain container with /dev/kvm; see tests/testvm/run.sh."""
import argparse
import os
import subprocess
import sys
import time

p = argparse.ArgumentParser()
p.add_argument("--image", required=True)
p.add_argument("--ctl", required=True)
p.add_argument("--work", required=True)
p.add_argument("--mem", default="8G")
p.add_argument("--cpus", default="4")
p.add_argument("--share", action="append", default=[], help="TAG=PATH[:ro]")
p.add_argument("--disk", action="append", default=[], help="SERIAL=FILE[:SIZE][:overlay]")
p.add_argument("--proxy", type=int, default=0, help="the host's 127.0.0.1 port the guest reaches as 10.0.2.100")
p.add_argument("--offline", action="store_true")
a = p.parse_args()

overlay = os.path.join(a.work, "root.qcow2")
subprocess.run(["qemu-img", "create", "-q", "-f", "qcow2", "-F", "raw", "-b", os.path.join(a.image, "root.img"), overlay], check=True)
cmd = ["qemu-system-x86_64", "-enable-kvm", "-cpu", "host", "-m", a.mem, "-smp", a.cpus, "-no-reboot", "-display", "none",
       # The job's output on a console of its own, the kernel's and systemd's in a file shown only when the job didn't finish.
       "-serial", f"file:{os.path.join(a.work, 'console.log')}", "-monitor", "none",
       "-device", "virtio-serial-pci", "-chardev", "stdio,id=job,signal=off", "-device", "virtconsole,chardev=job",
       "-kernel", os.path.join(a.image, "vmlinuz"), "-initrd", os.path.join(a.image, "initramfs.img"),
       "-append", "root=LABEL=archix-testvm rw console=ttyS0 quiet loglevel=1 systemd.show_status=no rd.udev.log_level=3 mitigations=off",
       "-drive", f"file={overlay},if=virtio,format=qcow2,cache=unsafe"]
for n, disk in enumerate(a.disk):
    serial, rest = disk.split("=", 1)
    parts = rest.split(":")
    path = parts[0]
    size = next((x for x in parts[1:] if x and x[-1] in "GM"), "100G")
    if "overlay" in parts:
        top = os.path.join(a.work, f"{serial}.qcow2")
        subprocess.run(["qemu-img", "create", "-q", "-f", "qcow2", "-F", "qcow2", "-b", path, top], check=True)
        path = top
    elif not os.path.exists(path):
        subprocess.run(["qemu-img", "create", "-q", "-f", "qcow2", path, size], check=True)
    cmd += ["-drive", f"file={path},if=none,id=d{n},format=qcow2,cache=writeback,discard=unmap",
            "-device", f"virtio-blk-pci,drive=d{n},serial={serial}"]
for n, share in enumerate([f"ctl={a.ctl}"] + a.share):
    tag, rest = share.split("=", 1)
    path, _, mode = rest.partition(":")
    ro = ",readonly=on" if mode == "ro" else ""
    # A read-only share has nothing to map; a writable one keeps the guest's owners and modes in xattrs, its files the host user's.
    model = "none" if ro else "mapped-xattr"
    cmd += ["-fsdev", f"local,id=f{n},path={path},security_model={model}{ro}",
            "-device", f"virtio-9p-pci,fsdev=f{n},mount_tag={tag}"]
net = "user,id=n0"
if a.offline:
    net += ",restrict=on"
if a.proxy:
    net += f",guestfwd=tcp:10.0.2.100:{a.proxy}-tcp:127.0.0.1:{a.proxy}"
cmd += ["-netdev", net, "-device", "virtio-net-pci,netdev=n0"]
qemu = subprocess.Popen(cmd, stdin=subprocess.DEVNULL)
deadline = time.monotonic() + int(os.environ.get("ARCHIX_TESTVM_TIMEOUT") or 6 * 3600)
console = os.path.join(a.work, "console.log")
while qemu.poll() is None:
    try:
        with open(console, errors="replace") as f:
            stuck = "emergency shell" in f.read()
    except FileNotFoundError:
        stuck = False
    # A boot that can't find its root waits in the initramfs's shell for a keyboard it hasn't got.
    if stuck or time.monotonic() > deadline:
        qemu.kill()
    time.sleep(5)
status = qemu.returncode
try:
    with open(os.path.join(a.ctl, "exit")) as f:
        sys.exit(int(f.read().strip() or 1))
except FileNotFoundError:
    with open(os.path.join(a.work, "console.log"), errors="replace") as f:
        sys.stderr.write("".join(f.readlines()[-40:]))
    print(f"testvm: the VM stopped without finishing its job (QEMU exit {status})", file=sys.stderr)
    sys.exit(125)
