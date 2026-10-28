# The /etc merge: the watcher carries live /etc changes into the pending deployment, which boots with them, and a reset inside the merge's window leaves a note and the old /etc's last change.
from boot.common import HOST, bind, boot, current, ok, out


def run(m, t):
    bind(m, t)
    with t.step("etc-merge: the watcher carries live /etc changes into the pending deployment"):
        _, booted, _ = boot()
        m.succeed(f"{HOST} install zsh --noconfirm", what="archix install zsh")
        pending = current()
        t.require(pending != booted, "installing zsh should leave a deployment pending")
        t.check(ok(f"grep -qx /bin/zsh /archix/deploy/{pending}/etc/shells"), "zsh's scriptlet should add it to the staged /etc/shells")
        merge_active = "systemctl is-active -q archix-etc-merge.service"
        t.check(ok(f"for _ in $(seq 50); do {merge_active} && break; sleep 0.2; done; {merge_active}"),
                f"the commit should start archix-etc-merge.service: {out('systemctl status archix-etc-merge.service --no-pager | tail -5')}")
        m.succeed("echo 'root:etc-merge-vmtest' | chpasswd", what="chpasswd")
        shadow = out("grep '^root:' /etc/shadow").strip()
        m.succeed("mkdir -p /etc/NetworkManager/system-connections && (umask 077 && printf '[connection]\\nid=vmtest\\ntype=wifi\\n\\n[wifi-security]\\npsk=vmtest-secret\\n' > /etc/NetworkManager/system-connections/.vmtest.nmconnection.T3MP)")
        m.succeed("mv /etc/NetworkManager/system-connections/.vmtest.nmconnection.T3MP /etc/NetworkManager/system-connections/vmtest.nmconnection")
        m.succeed("mkdir -p /etc/vmtest-dir/sub && echo 'made after the switch' > /etc/vmtest-dir/sub/file && echo /bin/vmtest-shell >> /etc/shells")
        m.execute(f"for _ in $(seq 50); do grep -qx /bin/vmtest-shell /archix/deploy/{pending}/etc/shells && break; sleep 0.2; done")
        # The copy lands before the merge records it for status, which a status read 0.1 s later missed on btrfs.
        counted = f"grep -q '/etc changes carried into the pending deployment {pending}' <<< \"$s\" && grep -q '/etc/shells: changed on the running system' <<< \"$s\""
        status = out(f"for _ in $(seq 50); do s=$({HOST} status 2>&1); {counted} && break; sleep 0.2; done; echo \"$s\"")
        if not t.check(f"/etc changes carried into the pending deployment {pending}" in status and "/etc/shells: changed on the running system" in status,
                       "status should count the carried changes and the conflict"):
            t.log("status: " + status)
            t.log("journal: " + out("journalctl -b -u archix-etc-merge.service --no-pager | tail -30"))
            t.log("state: " + out("ls -la /var/lib/archix/etc-merge 2>&1"))
        m.reboot()

    with t.step("etc-merge-2: the pending deployment boots with the carried changes; a reset in the merge's window"):
        _, booted, _ = boot()
        t.check(booted == pending, f"the restart should boot deployment {pending}")
        t.check(ok(f"grep -qxF '{shadow}' /etc/shadow"), f"the password changed after the switch didn't reach deployment {pending}")
        nm = "/etc/NetworkManager/system-connections/vmtest.nmconnection"
        mode = out(f"stat -c %a {nm} 2>&1").strip()
        t.check(ok(f"grep -qx psk=vmtest-secret {nm}") and mode == "600", f"the connection should arrive whole and 0600: {mode}")
        t.check(out("cat /etc/vmtest-dir/sub/file 2>/dev/null").strip() == "made after the switch", f"the new directory didn't reach deployment {pending}")
        t.check(ok("grep -qx /bin/vmtest-shell /etc/shells"), "the live edit to /etc/shells should win")
        t.check(ok("grep -qx /bin/zsh /etc/shells.pacnew"), f"deployment {pending}'s /etc/shells should be /etc/shells.pacnew: {out('ls /etc/shells* 2>&1')}")
        status = out(f"{HOST} status 2>&1")
        t.check("were carried from deployment" in status, f"status should report the merge after the boot: {status}")
        t.check(not ok("grep -q \"didn't shut down cleanly\" /var/lib/archix/notes 2>/dev/null"), "a clean reboot shouldn't leave the power-cut note")
        m.succeed(f"{HOST} install bc --noconfirm", what="archix install bc")
        pending = current()
        m.execute("for _ in $(seq 50); do systemctl is-active -q archix-etc-merge.service && break; sleep 0.2; done")
        m.succeed("echo 'written just before the power went' > /etc/vmtest-power-cut && sync -f /etc")
        m.crash()

    with t.step("etc-merge-3: a reset inside the window leaves a note and the old /etc's last change"):
        _, booted, _ = boot()
        t.check(booted == pending, f"after the reset deployment {pending} should boot")
        t.check(out("cat /etc/vmtest-power-cut 2>/dev/null").strip() == "written just before the power went", "the boot should carry what the old /etc held on disk")
        t.check(ok(f"grep -q \"didn't shut down cleanly before deployment {pending} first booted\" /var/lib/archix/notes"), f"the login note should name the window: {out('cat /var/lib/archix/notes')}")
        t.check("didn't shut down cleanly" in out(f"{HOST} status 2>&1"), "status should name the window")
