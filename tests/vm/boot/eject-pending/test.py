# Eject while a switch waits for its reboot: refused without --discard-pending under --noconfirm, then the running deployment becomes plain Arch at once and the pending one goes, /etc edits made since the switch stay, the /etc merge stops; plain Arch boots with the running deployment's packages, a clean pacman -Qkk, and pacman -Syu from the archive snapshot.
from boot.common import HOST, bind, boot, current, diff, has_line, loader, ok, out, qkk_problems

EDITED = "/etc/archix-vmtest-after-switch"


def run(m, t):
    bind(m, t)
    with t.step("ep1: a switch waits for its reboot; eject lays out the running deployment and discards it"):
        _, _, running = boot()
        packages = out("pacman -Q")
        qkk = out("pacman -Qkk 2>&1")
        t.log(f"pacman -Qkk on deployment {running}: {qkk.splitlines()[-1:] if qkk else ''}")
        m.succeed(f"{HOST} install tree --noconfirm", what="a switch installing tree")
        pending = current()
        t.require(pending != running and not ok("command -v tree"), f"the switch should leave deployment {pending} pending beside the running {running}")
        m.succeed(f"echo after > {EDITED}")
        t.log(f"the /etc merge is {out('systemctl is-active archix-etc-merge.service').strip()}")
        refused = m.execute("archix eject --noconfirm 2>&1")
        t.check(refused[0] != 0 and "--discard-pending" in refused[1] and "nothing was changed" in refused[1], f"eject --noconfirm with a switch pending should ask for --discard-pending: {refused[1][-600:]}")
        t.check(current() == pending and ok("[ -d /archix/deploy ]"), "the refused eject should change nothing")
        said = m.succeed("archix eject --noconfirm --discard-pending 2>&1", what="eject with a switch pending")
        t.log(said[-1500:])
        t.check(f"deployment {running}, which runs now" in said and f"deployment {pending}, which hasn't booted, is discarded" in said, f"eject should name the deployment it lays out and the one it discards: {said[-1200:]}")
        t.check(not ok("[ -e /archix ]"), f"eject should delete /archix: {out('ls -A /archix 2>&1')}")
        t.check("archix" not in out("findmnt -no SOURCE /usr"), f"/usr should be the root's own now: {out('findmnt -no SOURCE /usr')}")
        t.check(not ok("command -v tree"), "the pending deployment's tree came along")
        t.check(out(f"cat {EDITED} 2>&1").strip() == "after", "an /etc edit made after the switch should stay")
        t.check(not ok("systemctl is-active -q archix-etc-merge.service") and not ok("[ -e /var/lib/archix/etc-merge ]"), f"the /etc merge should stop and its state go: {out('systemctl status archix-etc-merge.service --no-pager 2>&1 | head -5')}")
        t.check(not ok(f"[ -e /var/lib/archix/pending-hooks/{pending}.json ]"), "the hooks waiting for the discarded deployment should go")
        t.check(has_line(loader(), "default arch-linux.efi"), f"eject should make the plain UKI the default: {loader()}")
        t.check(not ok("ls /efi/loader/entries/archix-* /efi/EFI/Linux/archix-fallback-* > /dev/null 2>&1"), "eject left archix's boot entries")
        m.reboot()

    with t.step("ep2: plain Arch boots with the running deployment's packages; pacman -Qkk is clean and pacman -Syu works"):
        selected, _, mounted = boot(match=False)
        t.check(selected == "arch-linux.efi", f"plain Arch should boot through arch-linux.efi, not {selected}")
        t.check(mounted == "" and not ok("[ -e /archix ]"), "a plain boot mounts no deployment")
        now = out("pacman -Q")
        t.check(now == packages, f"plain Arch has other packages than deployment {running} had: {diff(packages, now)}")
        now = out("pacman -Qkk 2>&1")
        t.check(now == qkk, f"pacman -Qkk differs from the deployment's: {diff(qkk, now)}")
        problems = qkk_problems()
        t.check(problems == [], f"pacman -Qkk should find nothing on plain Arch that stock Arch doesn't: {problems}")
        t.check(out(f"cat {EDITED} 2>&1").strip() == "after", "the /etc edit should outlive the reboot")
        t.check(ok("[ -x /usr/local/bin/archix-vmtest-declared ]") and not ok("findmnt /usr/local"), "plain Arch should keep /usr/local's files in its own /usr/local")
        for unit in ("archix-boot", "archix-deploy-activate", "archix-etc-merge", "archix-etc-merge-boot"):
            t.check(not ok(f"[ -e /etc/systemd/system/{unit}.service ]"), f"eject should remove {unit}.service")
        m.succeed("pacman -Syu --noconfirm", what="plain pacman -Syu after the eject")
        m.succeed("pacman -S --noconfirm tree", what="plain pacman -S after the eject")
        t.check(ok("command -v tree"), "pacman -S tree should install it")
        m.shutdown()
