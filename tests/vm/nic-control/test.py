# The NIC no-carrier control: NIC_REBOOTS warm reboots and NIC_RESETS system_resets of the boot suites' image, plain Arch with archix removed (NIC_ARM=plain) or laid out by base.py's first switch (deployed), either with -replay, each boot judged as boot() judges one and a NIC without carrier reported before any recovery.
import os
import re
import shlex

from driver import MachineError
from boot.common import CONFIG, HOST, NIC_STATE, bind, ok, out, settled

ARM = os.environ.get("NIC_ARM", "plain")
# plain-replay and deployed-replay add the 20 GetVariable calls archix made each boot before it read each variable once with one read() (4 reads of LoaderEntrySelected and 1 of StubImageIdentifier, 4 calls a read), at about boot-check's and boot-complete's places.
BASE, REPLAY = ARM.removesuffix("-replay"), ARM.endswith("-replay")
EFIVARS = "/sys/firmware/efi/efivars"
REPLAY_UNITS = {
    "smm-replay-early.service": f"[Unit]\nDefaultDependencies=no\nAfter=local-fs.target\nBefore=sysinit.target\n\n[Service]\nType=oneshot\nRemainAfterExit=yes\nExecStart=/bin/sh -c 'cd {EFIVARS} && cat LoaderEntrySelected-* StubImageIdentifier-* LoaderEntrySelected-* > /dev/null'\n\n[Install]\nWantedBy=sysinit.target\n",
    "smm-replay-late.service": f"[Unit]\nAfter=systemd-bless-boot.service\n\n[Service]\nType=oneshot\nRemainAfterExit=yes\nExecStart=/bin/sh -c 'cd {EFIVARS} && cat LoaderEntrySelected-* LoaderEntrySelected-* > /dev/null'\n\n[Install]\nWantedBy=multi-user.target\n",
}
REBOOTS = int(os.environ.get("NIC_REBOOTS") or 30)
RESETS = int(os.environ.get("NIC_RESETS") or 6)
HANG = int(os.environ.get("NIC_HANG") or 300)
TRACE = os.environ.get("NIC_TRACE") == "1"
MACHINE = {"global_timeout": max(int(os.environ.get("VM_TIMEOUT") or 2400), 120 * (REBOOTS + RESETS + 1))}
ARCHIX = "/usr/bin/archix /usr/lib/archix /etc/archix /var/lib/archix /etc/systemd/system/archix-protected.service /etc/systemd/system/multi-user.target.wants/archix-protected.service"
# Every EFI runtime call the kernel makes, in its caller's context: __efi_queue_work's id and the variable's name, its arguments' first field.
KPROBE = "kprobe_event=p,__efi_queue_work,id=%di:u32,name=+0(+0(%si)):u16[40]"
RTS = {1: "GetTime", 5: "GetVariable", 6: "GetNextVariableName", 7: "SetVariable", 8: "QueryVariableInfo", 10: "ResetSystem"}


def plan():
    """Which boots are resets: NIC_RESETS of them spread evenly through the NIC_REBOOTS + NIC_RESETS boots after the first."""
    total = REBOOTS + RESETS
    resets = {round((k + 1) * total / (RESETS + 1)) for k in range(RESETS)} if RESETS else set()
    return ["reset" if n in resets else "reboot" for n in range(1, total + 1)]


def trace_setup(m):
    """NIC_TRACE: puts KPROBE on the arm's kernel command line from the next boot, plain's UKI rebuilt as build-image.sh made it, deployed's through a switch."""
    if BASE == "plain":
        m.succeed(f"sed -i 's|$| {KPROBE}|' /etc/kernel/cmdline && mkinitcpio -k $(ls /usr/lib/modules | head -1) -S autodetect -U /efi/EFI/Linux/arch-linux.efi > /dev/null 2>&1")
    else:
        m.execute("systemctl is-system-running --wait > /dev/null", 900)
        m.succeed(f"sed -i 's|\"console=ttyS0\" }}|\"console=ttyS0\", \"{KPROBE}\" }}|' {CONFIG} && grep -qF kprobe_event {CONFIG}")
        m.succeed(f"{HOST} switch --noconfirm", what="the switch that adds the trace to the command line")


def traced(t, n):
    """NIC_TRACE: logs this boot's EFI runtime calls by caller and variable, and each vCPU's SMI count (KVM's MSR_SMI_COUNT)."""
    smis = out("modprobe msr 2>/dev/null; for c in /dev/cpu/[0-9]*/msr; do od -An -tu8 -j52 -N8 $c 2>/dev/null || echo -; done").split()
    calls = []
    for line in out("grep -v '^#' /sys/kernel/tracing/trace").splitlines():
        found = re.match(r"\s*(.+?)-(\d+)\s+\[\d+\].*?\s([0-9.]+): \S+: \(.*?\) id=(\d+) name=\{(.*)\}", line)
        if not found:
            continue
        comm, _, at, rts, name = found.groups()
        kind = RTS.get(int(rts), f"rts{rts}")
        units = [int(u, 0) for u in name.split(",") if u]
        text = "".join(map(chr, units[:units.index(0)] if 0 in units else units)) if kind in ("GetVariable", "SetVariable") else ""
        calls.append((comm, kind, text))
        t.log(f"EFI-CALL arm={ARM} boot={n} at={at} comm={comm} {kind} {text}")
    tally = {}
    for call in calls:
        tally[call] = tally.get(call, 0) + 1
    summary = "; ".join(f"{comm} {kind} {name} x{tally[(comm, kind, name)]}" for comm, kind, name in sorted(tally))
    t.log(f"EFI-TRACE arm={ARM} boot={n} smi={','.join(smis)} calls={len(calls)} sets={sum(kind == 'SetVariable' for _, kind, _ in calls)}: {summary}")
    if n == 1:
        # What each kind of efivarfs access costs in SMIs: 100 reads, 10 writes and a delete of a scratch variable, 10 statfs (QueryVariableInfo).
        smi = "od -An -tu8 -j52 -N8 /dev/cpu/0/msr"
        var = "/sys/firmware/efi/efivars/ArchixSmmProbe-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f"
        cost = out(f"""a=$({smi}); for i in $(seq 100); do cat /sys/firmware/efi/efivars/LoaderEntrySelected-* > /dev/null; done; b=$({smi})
            for i in $(seq 10); do printf '\\x07\\x00\\x00\\x00\\x01' > {var}; done; c=$({smi}); chattr -i {var}; rm {var}; d=$({smi})
            for i in $(seq 10); do stat -f /sys/firmware/efi/efivars > /dev/null; done; e=$({smi})
            echo reads=$((b - a)) writes=$((c - b)) delete=$((d - c)) statfs=$((e - d))""")
        t.log(f"EFI-SMI-COST arm={ARM} {cost.strip()}")


def run(m, t):
    bind(m, t)
    boots, good, hangs = [], [], []

    def judged(n, how):
        boots.append(how)
        # A boot that never reaches the shell is another fault than the NIC's: it's reported, counted and reset, and the arm goes on.
        for _ in range(3):
            try:
                m.wait_for_shell(HANG)
                break
            except MachineError as e:
                if not m.alive():
                    raise
                hangs.append(n)
                # The driver placed it (tests/vm/driver/hang.py); the reset clears its verdict, as the arm goes on.
                t.log(f"NIC-CONTROL arm={ARM} boot={n} how={how} HANG phase={m.hang}; {m.hang_cpus}")
                t.log(f"hang {n}: {e}")
                m.crash()
        m.execute("systemctl is-system-running --wait > /dev/null", 900)
        settled()
        if BASE == "deployed":
            t.require(ok("[ -e /run/archix/deployment ]"), "the deployed arm booted without a deployment")
        if REPLAY and n:
            t.require(ok("systemctl is-active -q smm-replay-early.service smm-replay-late.service"), f"the replay didn't run: {out('systemctl status --no-pager smm-replay-early.service smm-replay-late.service 2>&1 | tail -20')}")
        nic = out(NIC_STATE).strip()
        took = re.search(r"Startup finished in .*", out("systemd-analyze 2>&1 | head -1"))
        t.log(f"NIC-CONTROL arm={ARM} boot={n} how={how} ok: {nic}; {took.group(0) if took else 'no systemd-analyze'}")
        if TRACE:
            traced(t, n)
        good.append(how)

    try:
        with t.step(f"{ARM} 0: the first, cold boot"):
            m.start()
            if BASE == "plain":
                t.require(not ok("[ -e /archix ]"), "the plain arm's image should not be laid out")
                m.succeed(f"systemctl disable --now archix-protected.service && rm -rf {ARCHIX} && systemctl daemon-reload")
                t.require(out("ls /usr/bin /usr/lib/systemd/system /etc/systemd/system /usr/lib | grep archix | grep -vc archix-backdoor || true").strip() == "0", f"archix left in the plain arm: {out('ls -d /usr/bin/archix* /usr/lib/systemd/system/archix* /etc/systemd/system/archix* /usr/lib/archix* 2>&1')}")
                # base.py sets the same for the deployed arm, so neither waits on the menu.
                m.succeed("bootctl set-timeout 0")
            else:
                t.require(ok("[ -e /run/archix/deployment ]"), "the deployed arm should boot a deployment")
            if REPLAY:
                for name, text in REPLAY_UNITS.items():
                    m.succeed(f"printf %s {shlex.quote(text)} > /etc/systemd/system/{name}")
                m.succeed(f"systemctl daemon-reload && systemctl enable {' '.join(REPLAY_UNITS)}")
            if TRACE:
                trace_setup(m)
            judged(0, "cold")
            t.log(f"the plan after the cold boot: {' '.join(plan())}")
        for n, how in enumerate(plan(), 1):
            with t.step(f"{ARM} {n}: {'a warm reboot' if how == 'reboot' else 'QMP system_reset'}"):
                if how == "reboot":
                    m.reboot()
                else:
                    m.crash()
                judged(n, how)
    finally:
        t.log(f"NIC-CONTROL arm={ARM} boots={len(boots)} good={len(good)} hangs={len(hangs)}{' at ' + ','.join(map(str, hangs)) if hangs else ''} reboots={good.count('reboot')} resets={good.count('reset')} planned=1+{REBOOTS}+{RESETS}")
