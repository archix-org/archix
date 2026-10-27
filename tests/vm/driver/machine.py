import base64
import json
import os
import re
import shlex
import shutil
import socket
import subprocess
import threading
import time
import uuid
from pathlib import Path

OVMF_CODE = "/usr/share/edk2/x64/OVMF_CODE.secboot.4m.fd"
OVMF_VARS = "/usr/share/edk2/x64/OVMF_VARS.4m.fd"


class MachineError(Exception):
    pass


class Timeline:
    """Every command, boot and step with its time since the start, printed and kept in a file."""

    def __init__(self, path=None):
        self.start = time.monotonic()
        self.step = None
        self.file = open(path, "a", buffering=1) if path else None

    def now(self):
        return time.monotonic() - self.start

    def log(self, msg):
        where = f" [{self.step}]" if self.step else ""
        line = f"+{self.now():7.1f}s{where} {msg}"
        print(line, flush=True)
        if self.file:
            self.file.write(line + "\n")

    def where(self):
        return f"step '{self.step}': " if self.step else ""


def short(text, limit=600):
    text = text.strip()
    return text if len(text) <= limit else "..." + text[-limit:]


class Machine:
    """One QEMU+OVMF VM as tests/vm/boot.sh starts it, driven through the guest's archix-backdoor root shell."""

    def __init__(self, timeline, work="/work", disks=None, mem=None, cpus=None, tpm=False, boot_timeout=900, global_timeout=None):
        self.timeline = timeline
        self.work = Path(work)
        self.disks = disks if disks is not None else [f"file={self.work}/disk.img,format=raw,if=virtio"]
        self.mem = mem or os.environ.get("VM_MEM", "3072")
        self.cpus = cpus or os.environ.get("VM_CPUS", "2")
        self.tpm = tpm
        self.boot_timeout = boot_timeout
        self.global_timeout = global_timeout or int(os.environ.get("VM_TIMEOUT", "2400"))
        self.serial = self.work / "serial.log"
        self.qmp_path = self.work / "qmp.sock"
        self.shell_path = self.work / "shell.sock"
        self.process = None
        self.shell = None
        self.swtpm = None
        self.lock = threading.Condition()
        self.ready = []
        self.seen = 0
        self.replies = {}
        self.boot_id = None
        self.connected = False
        self.timed_out = False
        self.console_mark = 0
        self.boot_started = None
        self.last_stderr = ""

    def log(self, msg):
        self.timeline.log(msg)

    def fault(self, msg):
        return MachineError(self.timeline.where() + msg)

    def qemu_command(self):
        vars_fd = self.work / "vars.fd"
        if not vars_fd.exists():
            shutil.copy(OVMF_VARS, vars_fd)
        cmd = ["qemu-system-x86_64", "-enable-kvm", "-cpu", "host", "-m", str(self.mem), "-smp", str(self.cpus),
               "-machine", "q35,smm=on", "-global", "driver=cfi.pflash01,property=secure,value=on",
               "-drive", f"if=pflash,format=raw,unit=0,readonly=on,file={OVMF_CODE}",
               "-drive", f"if=pflash,format=raw,unit=1,file={vars_fd}"]
        for disk in self.disks:
            cmd += ["-drive", disk]
        if self.tpm:
            cmd += ["-chardev", f"socket,id=chrtpm,path={self.work}/swtpm/sock", "-tpmdev", "emulator,id=tpm0,chardev=chrtpm", "-device", "tpm-tis,tpmdev=tpm0"]
        cmd += ["-netdev", "user,id=net0", "-device", "virtio-net-pci,netdev=net0,romfile=",
                "-device", "virtio-serial-pci", "-chardev", f"socket,id=shell,path={self.shell_path}",
                "-device", "virtserialport,chardev=shell,name=archix.shell",
                "-display", "none", "-serial", f"file:{self.serial}",
                "-qmp", f"unix:{self.qmp_path},server=on,wait=off", "-monitor", "none"]
        return cmd

    def start(self):
        """Starts QEMU and returns at once; the first command waits for the guest's shell."""
        if self.process:
            return
        for path in (self.qmp_path, self.shell_path):
            path.unlink(missing_ok=True)
        if self.tpm:
            (self.work / "swtpm").mkdir(exist_ok=True)
            self.swtpm = subprocess.Popen(["swtpm", "socket", "--tpm2", "--tpmstate", f"dir={self.work}/swtpm", "--ctrl", f"type=unixio,path={self.work}/swtpm/sock"])
            self.until(lambda: (self.work / "swtpm/sock").exists(), 10, "swtpm's socket")
        listener = socket.socket(socket.AF_UNIX)
        listener.bind(str(self.shell_path))
        listener.listen(1)
        listener.settimeout(1)
        self.log("starting the VM")
        self.process = subprocess.Popen(self.qemu_command(), stdout=open(self.work / "qemu.log", "ab"), stderr=subprocess.STDOUT)
        while True:
            try:
                self.shell, _ = listener.accept()
                break
            except socket.timeout:
                if self.process.poll() is not None:
                    raise self.fault(f"QEMU exited with {self.process.returncode} before it connected the shell port: {short((self.work / 'qemu.log').read_text(errors='replace'))}")
        listener.close()
        threading.Thread(target=self.read_shell, daemon=True).start()
        self.killer = threading.Timer(self.global_timeout, self.time_out)
        self.killer.daemon = True
        self.killer.start()
        self.boot_started = time.monotonic()

    def time_out(self):
        self.timed_out = True
        self.log(f"the VM ran past VM_TIMEOUT={self.global_timeout} s; killing it")
        self.kill()

    def read_shell(self):
        buffer = b""
        while True:
            try:
                chunk = self.shell.recv(65536)
            except OSError:
                chunk = b""
            if not chunk:
                break
            buffer += chunk
            *lines, buffer = buffer.split(b"\n")
            with self.lock:
                for line in lines:
                    words = line.decode(errors="replace").split(" ")
                    if words[0] == "READY" and len(words) == 2:
                        self.ready.append(words[1])
                    elif len(words) == 4:
                        self.replies[words[0]] = words[1:]
                self.lock.notify_all()
        with self.lock:
            self.lock.notify_all()

    def alive(self):
        return self.process is not None and self.process.poll() is None

    def until(self, ready, timeout, what):
        deadline = time.monotonic() + timeout
        while not ready():
            if time.monotonic() > deadline:
                raise self.fault(f"timed out after {timeout} s waiting for {what}")
            time.sleep(0.1)

    def gone(self, what):
        if self.timed_out:
            return self.fault(f"the VM ran past VM_TIMEOUT={self.global_timeout} s while {what}")
        return self.fault(f"QEMU exited ({self.process.returncode if self.process else 'not started'}) while {what}")

    def wait_for_shell(self, timeout=None):
        """Waits for this boot's shell: the first READY since start, reboot or crash from a boot other than the last one."""
        if self.connected:
            return
        self.start()
        timeout = timeout or self.boot_timeout
        deadline = time.monotonic() + timeout
        with self.lock:
            while True:
                fresh = [b for b in self.ready[self.seen:] if b != self.boot_id]
                if fresh:
                    self.boot_id = fresh[-1]
                    self.seen = len(self.ready)
                    break
                if not self.alive():
                    raise self.gone("waiting for the guest's shell")
                left = deadline - time.monotonic()
                if left <= 0:
                    raise self.fault(f"the guest's shell didn't come up within {timeout} s of the boot; console: {short(self.console_since(self.console_mark), 1500)}")
                self.lock.wait(min(left, 1))
        self.connected = True
        self.log(f"booted ({time.monotonic() - self.boot_started:.1f} s), boot id {self.boot_id[:8]}")

    def send(self, cmd, timeout):
        token = uuid.uuid4().hex[:12]
        line = f"{token} {int(timeout)} {base64.b64encode(cmd.encode()).decode()}\n"
        self.shell.sendall(line.encode())
        return token

    def execute(self, cmd, timeout=900, quiet=False):
        """Runs cmd with bash as root in the guest: (status, stdout); the stderr goes to the timeline when it fails."""
        self.wait_for_shell()
        began = time.monotonic()
        token = self.send(cmd, timeout)
        deadline = began + timeout + 30
        with self.lock:
            while token not in self.replies:
                if not self.alive():
                    raise self.gone(f"running `{cmd}`")
                left = deadline - time.monotonic()
                if left <= 0:
                    raise self.fault(f"`{cmd}` gave no answer within {timeout + 30} s")
                self.lock.wait(min(left, 1))
            status, out, err = self.replies.pop(token)
        status = int(status)
        out = base64.b64decode(out).decode(errors="replace")
        err = base64.b64decode(err).decode(errors="replace")
        took = time.monotonic() - began
        if not quiet or status != 0:
            self.log(f"$ {cmd} -> {status} ({took:.1f} s)" + (f"; stderr: {short(err, 400)}" if status and err.strip() else ""))
        if status == 124 and took >= timeout:
            raise self.fault(f"`{cmd}` timed out after {timeout} s; stderr: {short(err)}")
        self.last_stderr = err
        return status, out

    def succeed(self, cmd, timeout=900, what=None):
        status, out = self.execute(cmd, timeout)
        if status != 0:
            raise self.fault(f"{what or f'`{cmd}`'} failed (exit {status}): {short(self.last_stderr or out)}")
        return out

    def fail(self, cmd, timeout=900, what=None):
        status, out = self.execute(cmd, timeout)
        if status == 0:
            raise self.fault(f"{what or f'`{cmd}`'} should have failed: {short(out)}")
        return out

    def wait_until_succeeds(self, cmd, timeout=900, interval=1.0, what=None):
        deadline = time.monotonic() + timeout
        while True:
            status, out = self.execute(cmd, timeout=max(1, min(timeout, 60)), quiet=True)
            if status == 0:
                self.log(f"$ {cmd} -> 0")
                return out
            if time.monotonic() > deadline:
                raise self.fault(f"{what or f'`{cmd}` did not succeed'} within {timeout} s (last exit {status}): {short(self.last_stderr or out)}")
            time.sleep(interval)

    def wait_for_unit(self, unit, timeout=900):
        """Waits for unit to be active; fails at once if it failed, or is inactive with no jobs left."""
        deadline = time.monotonic() + timeout
        while True:
            state = self.execute(f"systemctl show -P ActiveState {shlex.quote(unit)}", quiet=True)[1].strip()
            if state == "active":
                self.log(f"{unit} is active")
                return
            if state == "failed":
                raise self.fault(f"{unit} failed: {short(self.execute(f'journalctl -b -u {shlex.quote(unit)} --no-pager | tail -20', quiet=True)[1])}")
            if state == "inactive" and "No jobs" in self.execute("systemctl list-jobs", quiet=True)[1]:
                raise self.fault(f"{unit} is inactive and no job will start it")
            if time.monotonic() > deadline:
                raise self.fault(f"{unit} wasn't active within {timeout} s (it's {state})")
            time.sleep(1)

    def console_since(self, offset):
        try:
            with open(self.serial, "rb") as f:
                f.seek(offset)
                return f.read().decode(errors="replace").replace("\r", "")
        except FileNotFoundError:
            return ""

    def wait_for_console_text(self, regex, timeout=900):
        """Waits for regex on the serial console since this boot began; returns the match."""
        pattern = re.compile(regex)
        deadline = time.monotonic() + timeout
        while True:
            found = pattern.search(self.console_since(self.console_mark))
            if found:
                self.log(f"console: {found.group(0)!r}")
                return found.group(0)
            if not self.alive():
                raise self.gone(f"waiting for /{regex}/ on the console")
            if time.monotonic() > deadline:
                raise self.fault(f"/{regex}/ didn't appear on the console within {timeout} s")
            time.sleep(0.5)

    def new_boot(self, how):
        self.log(how)
        self.connected = False
        self.console_mark = self.serial.stat().st_size if self.serial.exists() else 0
        self.boot_started = time.monotonic()

    def reboot(self):
        """A clean reboot; the next command waits for the next boot's shell."""
        self.wait_for_shell()
        self.send("systemctl reboot", 60)
        self.new_boot("rebooting")

    def crash(self):
        """A power cut: QEMU's system_reset, with whatever the guest hadn't written lost."""
        self.qmp("system_reset")
        self.new_boot("power cut (system_reset)")

    def select_entry(self, name):
        """Makes systemd-boot start entry name at the next boot only."""
        self.succeed(f"bootctl set-oneshot {shlex.quote(name)}", what=f"bootctl set-oneshot {name}")

    def shutdown(self, timeout=300):
        self.wait_for_shell()
        self.send("systemctl poweroff", 60)
        self.log("powering off")
        try:
            self.process.wait(timeout)
        except subprocess.TimeoutExpired:
            raise self.fault(f"the VM didn't power off within {timeout} s")
        self.connected = False
        self.stop()

    def qmp(self, execute, **arguments):
        sock = socket.socket(socket.AF_UNIX)
        sock.settimeout(30)
        sock.connect(str(self.qmp_path))
        reader = sock.makefile("r")

        def call(name, **args):
            sock.sendall(json.dumps({"execute": name, "arguments": args}).encode() + b"\n")
            while True:
                reply = json.loads(reader.readline())
                if "return" in reply or "error" in reply:
                    return reply

        try:
            json.loads(reader.readline())
            call("qmp_capabilities")
            reply = call(execute, **arguments)
        finally:
            sock.close()
        if "error" in reply:
            raise self.fault(f"QMP {execute}: {reply['error']}")
        return reply["return"]

    def hmp(self, line):
        """A QEMU monitor command, such as "info network" or "set_link net0 off"."""
        return self.qmp("human-monitor-command", **{"command-line": line})

    def kill(self):
        if self.alive():
            self.process.kill()
            self.process.wait()

    def stop(self):
        if getattr(self, "killer", None):
            self.killer.cancel()
        self.kill()
        if self.shell:
            self.shell.close()
        if self.swtpm and self.swtpm.poll() is None:
            self.swtpm.kill()
