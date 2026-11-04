# A ptrace attempt from the config owner's processes on the evaluator, in a VM with yama's ptrace_scope at 0 (the host's 1 would refuse every attach that isn't to a child, the control's too): the owner attaching to a process of its own that isn't its child succeeds, the positive control; attaching to the evaluator of a root `archix switch` of the owner's config, which runs as a transient uid with no_new_privs, fails with EPERM; then the runaway config is stopped by its limits and the switch refused with nothing applied.
import time

from boot.common import HOST, bind, boot, out, snapshots

OWNER = "archix-owner"
CONFIG = f"/home/{OWNER}/config"
# PTRACE_ATTACH through syscall(2): exits 0 when attached, else errno.
ATTACH = "perl -e 'my $r = syscall(101, 16, {pid}, 0, 0); exit($r == 0 ? 0 : $! + 0)'"


def run(m, t):
    bind(m, t)
    with t.step("1: the owner can ptrace a process of its own that isn't its child"):
        boot()
        before = out("sysctl -n kernel.yama.ptrace_scope").strip()
        m.succeed("sysctl -q kernel.yama.ptrace_scope=0")
        t.log(f"yama ptrace_scope {before} -> {out('sysctl -n kernel.yama.ptrace_scope').strip()}")
        m.succeed(f"useradd -m {OWNER} && runuser -u {OWNER} -- sh -c 'mkdir {CONFIG} && cd {CONFIG} && git init -q && echo \"while true do end\" > init.lua'")
        m.succeed(f"runuser -u {OWNER} -- setsid -f sleep 300 < /dev/null > /dev/null 2>&1")
        pid = m.succeed(f"pgrep -u {OWNER} -x sleep").split()[0]
        status, said = m.execute(f"runuser -u {OWNER} -- {ATTACH.format(pid=pid)}")
        t.log(f"EVIDENCE ptrace control: {OWNER} attaching to its own sleep {pid}, not its child, exited {status} (0 is attached)")
        t.require(status == 0, f"the positive control: the owner should be able to attach to its own non-child process: exit {status} {said}")
        m.execute(f"pkill -u {OWNER} -x sleep")

    with t.step("2: the owner can't ptrace the evaluator of a root switch of its config"):
        generations = out(f"{HOST} generations 2>&1")
        taken = snapshots()
        m.execute("rm -f /root/ptrace-switch.log")
        m.succeed(f"setsid -f bash -c '{HOST} --config {CONFIG} switch --noconfirm > /root/ptrace-switch.log 2>&1; echo \"exit $?\" >> /root/ptrace-switch.log' < /dev/null > /dev/null 2>&1")
        began = time.monotonic()
        pid = uid = None
        for _ in range(300):
            listed = out("for p in $(pgrep -f evaluate-isolated); do echo $p $(stat -c %u /proc/$p); done").split()
            if len(listed) >= 2:
                pid, uid = listed[0], int(listed[1])
                break
            time.sleep(0.2)
        t.require(pid is not None, f"the evaluator never showed up: {out('cat /root/ptrace-switch.log')}")
        owner_uid = int(out(f"id -u {OWNER}").strip())
        nnp = out(f"grep NoNewPrivs /proc/{pid}/status").strip()
        t.log(f"the evaluator is pid {pid}, uid {uid} (the owner is {owner_uid}), {nnp}")
        t.check(uid != owner_uid and uid >= 61184, f"the evaluator should run as a transient uid, not the owner's: {uid}")
        t.check(nnp.endswith("1"), f"the evaluator should have no_new_privs: {nnp}")
        status, said = m.execute(f"runuser -u {OWNER} -- {ATTACH.format(pid=pid)}")
        t.log(f"EVIDENCE ptrace: {OWNER} attaching to the evaluator {pid} (uid {uid}) exited {status} (1 is EPERM)")
        t.check(status == 1, f"the owner's attach to the evaluator should fail with EPERM: exit {status} {said}")
        m.wait_until_succeeds("grep -q '^exit ' /root/ptrace-switch.log", timeout=300, what="the switch's end")
        said = out("cat /root/ptrace-switch.log")
        took = time.monotonic() - began
        t.log(f"EVIDENCE limits in the VM: `while true do end` under the default limits ended the switch after {took:.0f} s: {said[-600:]}")
        t.check("exit 0" not in said and ("CPU" in said or "longer than" in said), f"the runaway config should be stopped by a limit and the switch refused: {said[-800:]}")
        t.check(out(f"{HOST} generations 2>&1") == generations and snapshots() == taken, f"the refused switch should apply nothing: snapshots {taken} -> {snapshots()}")
        m.succeed(f"sysctl -q kernel.yama.ptrace_scope={before}")
