# A system laid out as deployments by the last build of that model, upgraded to this tree's archix-git as its users would: pacman -U (makepkg -si, yay -S) and the declared archix update + archix switch; each must refuse with nothing on the ESP or under /archix changed, and the old system must still boot its old archix.
import re

from boot.common import HOST, bind, boot, ok, out

MACHINE = {"mem": "4096", "cpus": "4"}
NEXT = "/srv/archix-next"
# The refusal the new build gives on a deployments system, each part required.
REFUSAL = ("deployments model", "no longer supports")
PARTS = ("esp", "archix", "state", "subvolumes")
# Manifests of the ESP (random-seed left out, which every boot rewrites), of the real root's /archix and /var/lib/archix under the deployment binds with each file's sha256, and of the btrfs subvolumes; each part's digest is printed.
FINGERPRINT = r"""set -euo pipefail
mkdir -p /run/fp
mountpoint -q /run/fp || { mount --bind / /run/fp && mount -o remount,bind,ro /run/fp; }
cd /efi
{ find . -path ./loader/random-seed -prune -o -printf '%p %y %s\n' | sort; find . -type f ! -path ./loader/random-seed -print0 | sort -z | xargs -0 sha256sum; } > /run/fp-NAME.esp
cd /run/fp
{ find archix -printf '%p %y %m %s %l\n' | sort; find archix -type f -print0 | sort -z | xargs -0 sha256sum; } > /run/fp-NAME.archix
{ find var/lib/archix -printf '%p %y %m %s %l\n' | sort; find var/lib/archix -type f -print0 | sort -z | xargs -0 sha256sum; } > /run/fp-NAME.state
if [ "$(stat -f -c %T /)" = btrfs ]; then btrfs subvolume list -u / | awk '{ print $2, $NF, $(NF-2) }'; fi > /run/fp-NAME.subvolumes
for part in esp archix state subvolumes; do echo "$part $(sha256sum < /run/fp-NAME.$part | cut -c1-64)"; done
"""


def fingerprint(m, name):
    """Each part's digest by name; the manifests stay in /run until the next boot, for a diff."""
    text = m.succeed(FINGERPRINT.replace("NAME", name), 900, what=f"fingerprinting the ESP and /archix ({name})")
    return dict(line.split() for line in text.splitlines() if line.strip())


def differs(t, a, b, parts, what):
    changed = [part for part in parts if a[part] != b[part]]
    t.check(not changed, f"{what} changed {', '.join(changed)} (the diff is above)")


def unchanged(m, t, before, after, what):
    a, b = fingerprint(m, before), fingerprint(m, after)
    for part in PARTS:
        if a[part] != b[part]:
            t.log(f"{part}: " + out(f"diff /run/fp-{before}.{part} /run/fp-{after}.{part} | head -60"))
    differs(t, a, b, PARTS, what)
    return a


def current():
    return out("archix generations | awk '$1 == \"*\" { print $2 }'").strip()


def deployment_entry(started):
    found = re.match(r"archix-(\d+)(\+[0-9-]+)?\.conf$", started)
    return found.group(1) if found else None


def run(m, t):
    bind(m, t)
    with t.step("old: the last deployments build lays the booted system out"):
        m.start()
        boot()
        old_version = out(f"cat {NEXT}/old.version").strip()
        new_version = out(f"cat {NEXT}/new.version").strip()
        t.log(f"old archix-git {old_version} ({out(f'cat {NEXT}/old.commit').strip()}), new {new_version} ({out(f'cat {NEXT}/new.commit').strip()})")
        t.require(out("pacman -Q archix-git").split()[1:] == [f"{old_version}-1"], f"the image should run the old archix-git: {out('pacman -Q archix-git')}")
        status, _ = m.execute(f"set -o pipefail; {HOST} switch --noconfirm 2>&1 | tee /root/first-switch.log > /dev/console", 1800)
        t.require(status == 0, f"the old build's first switch exited {status}: {out('tail -c 2000 /root/first-switch.log')}")
        t.check(ok("[ -d /archix/deploy/2 ]") and current() == "2", f"the first switch should lay out deployment 1 and build 2: {out('ls /archix/deploy; archix generations')}")
        m.succeed("bootctl set-timeout 0")
        m.reboot()

    with t.step("old: deployment 2 boots and counts as good"):
        started = boot()
        t.require(deployment_entry(started) == "2", f"systemd-boot should start deployment 2's entry, not {started}")
        t.require(out("cat /run/archix/deployment").strip() == "2", f"the initramfs should mount deployment 2: {out('cat /run/archix/deployment')}")
        m.wait_until_succeeds("[ -f /efi/loader/entries/archix-2.conf ]", timeout=300, what="the boot of deployment 2 should be blessed")
        m.wait_until_succeeds("systemctl is-system-running --wait > /dev/null; [ -z \"$(systemctl list-jobs --no-legend)\" ]", timeout=300, what="the boot's jobs should finish")
        unchanged(m, t, "settled-1", "settled-2", "the settled old system, left alone,")

    with t.step("pacman -U, as makepkg -si and yay -S install archix-git: refused, nothing changed"):
        first = fingerprint(m, "before-pacman")
        esp = out("cat /run/fp-before-pacman.esp")
        status, said = m.execute(f"pacman -U --noconfirm {NEXT}/new.pkg.tar.zst 2>&1", 600)
        t.log(f"pacman -U said: {said[-1500:]}")
        t.check(status != 0, "pacman -U of the new archix-git should be refused on a deployments system")
        t.check("archix" in said and "deployment" in said, f"the refusal should name archix and the deployment: {said[-800:]}")
        t.check(out("pacman -Q archix-git").split()[1:] == [f"{old_version}-1"], f"archix-git should still be the old one: {out('pacman -Q archix-git')}")
        unchanged(m, t, "before-pacman", "after-pacman", "pacman -U")

    with t.step("archix update and archix switch to the new archix-git: refused, nothing changed"):
        package = f"/srv/archix-repo/archix-git-{new_version}-1-x86_64.pkg.tar.zst"
        m.succeed(f"cp {NEXT}/new.pkg.tar.zst {package} && repo-add -q /srv/archix-repo/archix-self.db.tar.gz {package}")
        m.succeed(f"set -o pipefail; {HOST} update --to 2026-10-10 --noconfirm 2>&1 | tail -20 > /dev/console", 900, what="the old build locking the new archix-git")
        t.require(ok(f"grep -q '^archix-git = .*version = \"{new_version}-1\"' /etc/archix/archix.lock"), f"the lock should hold the new archix-git: {out('grep ^archix-git /etc/archix/archix.lock')}")
        fingerprint(m, "before-switch")
        status, said = m.execute(f"set -o pipefail; {HOST} switch --noconfirm 2>&1 | tee /root/upgrade.log", 2400)
        t.log(f"the switch to the new archix-git exited {status}: {said[-3000:]}")
        t.check(status != 0, "the switch to the new archix-git should be refused")
        t.check(all(part in said for part in REFUSAL), f"the refusal should say {' and '.join(repr(p) for p in REFUSAL)}: {said[-1200:]}")
        t.check(current() == "2", f"deployment 2 should still be current: {out('archix generations')}")
        unchanged(m, t, "before-switch", "after-switch", "the refused switch")
        m.reboot()

    with t.step("the old system still boots its old archix"):
        started = boot()
        t.check(deployment_entry(started) == "2" and out("cat /run/archix/deployment").strip() == "2", f"deployment 2 should boot again: entry {started}, deployment {out('cat /run/archix/deployment')}")
        t.check(out("pacman -Q archix-git").split()[1:] == [f"{old_version}-1"], f"the booted system should run the old archix-git: {out('pacman -Q archix-git')}")
        t.check(ok(f"cmp -s /usr/lib/archix/archix-static <(bsdtar -xOf {NEXT}/old.pkg.tar.zst usr/lib/archix/archix-static)"), "the booted deployment's static archix should be the old build's")
        t.check(ok("archix generations > /dev/null && archix status > /dev/null"), f"the old archix should read its state: {out('archix status 2>&1 | tail -5')}")
        m.wait_until_succeeds("[ -f /efi/loader/entries/archix-2.conf ]", timeout=300, what="the boot of deployment 2 should be blessed")
        again = fingerprint(m, "rebooted")
        if again["esp"] != first["esp"]:
            t.log("the ESP before the attempts, then after the reboot: " + esp[-3000:] + "\n---\n" + out("cat /run/fp-rebooted.esp")[-3000:])
        differs(t, first, again, ("esp", "subvolumes"), "the attempts and the reboot after them")
        m.shutdown()
