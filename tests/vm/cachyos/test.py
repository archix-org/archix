# CachyOS as its installer leaves it (zlib-ng-compat, ufw enabled, no systemd-ukify, its btrfs subvolumes), booted by Limine, and each archix failure of a real CachyOS install: diff makes switch's refusals (an enabled ufw beside archix's firewall, a root= naming no device); a relative --config; the first switch installs its boot tools and builds the rescue entry; the lock takes CachyOS's builds and archix keeps copies; a switch on a full disk is refused before it changes anything; swapping zlib-ng-compat for zlib, and a power cut in the middle of that swap, never leave libz.so.1 missing; a failed switch keeps the lock it resolved and the AUR trust given; an AUR package whose build dependency only CachyOS's repo has builds with it locked from there; keep across an initramfs change is refused on Limine; a revert with CachyOS's mirror unreachable and pacman's cache empty reinstalls CachyOS's builds from archix's copies; the move to systemd-boot keeps Limine first until a boot through systemd-boot counts as good, then hands over, with entries named CachyOS, after which keep works.
import base64
import time

from boot.common import CONFIG, HOST, LAID_OUT_FIRST, bind, boot, edit_config, entries, entry, ok, out, selected, snapshots
from suite import add_packages, current_generation, drop_packages, newest_snapshot, switch

MACHINE = {"mem": "4096", "cpus": "4", "global_timeout": 14400}
LOADER_INFO = "/sys/firmware/efi/efivars/LoaderInfo-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f"


def arch_of(name):
    return out(f"pacman -Qi {name} 2>/dev/null | sed -n 's/^Architecture *: //p'").strip()


def through_systemd_boot():
    """Whether systemd-boot started this boot: Limine sets the Boot Loader Interface's variables too, under its own name."""
    return "systemd-boot" in out(f"tail -c +5 {LOADER_INFO} 2>/dev/null | tr -d '\\000'")


def boot_order():
    """The firmware's boot order as labels, first first, and BootNext's label or None."""
    said = out("efibootmgr")
    labels = {}
    order, after = [], None
    for line in said.splitlines():
        if line.startswith("BootOrder:"):
            order = line.split(":", 1)[1].strip().split(",")
        elif line.startswith("BootNext:"):
            after = line.split(":", 1)[1].strip()
        elif line.startswith("Boot") and line[4:8].isalnum() and len(line) > 9 and line[8] in "* ":
            labels[line[4:8]] = line[9:].split("\t")[0].strip()
    return [labels.get(n, n) for n in order], labels.get(after) if after else None


def migration_done():
    return ok("grep -q '\"done\"' /var/lib/archix/boot/limine-migration.json")


def packages():
    return out("pacman -Q")


SUBVOLUMES = {"/": "/@", "/home": "/@home", "/root": "/@root", "/srv": "/@srv", "/var/cache": "/@cache", "/var/tmp": "/@tmp", "/var/log": "/@log"}
LIBZ = "/usr/lib/libz.so.1"
STOCK = "/root/init.lua.stock"


def libz_works():
    """libz.so.1 is there and what needs it starts: pacman, sudo and btrfs."""
    return ok(f"[ -e {LIBZ} ] && pacman -Q > /dev/null && sudo -n true && btrfs --version > /dev/null")


def refused(m, t, what, says):
    """`archix diff` exits 2 naming the refusal, and `archix switch` refuses with the same words, changing nothing."""
    before = (packages(), out("ls -a /.archix 2>&1"), out(f"sha256sum {CONFIG.rsplit('/', 1)[0]}/archix.lock"))
    status, said = m.execute(f"{HOST} diff > /root/diff.log 2>&1; echo $?; cat /root/diff.log", 1800)
    code = said.split("\n", 1)[0].strip()
    t.log(f"diff with {what}: {said[-2000:]}")
    t.check(code == "2" and "`archix switch` would refuse this" in said and says in said, f"diff with {what} should exit 2 naming the refusal ({says!r}), not {code}: {said[-1500:]}")
    status, said = m.execute(f"{HOST} switch --noconfirm > /root/refused.log 2>&1; echo $?; cat /root/refused.log", 1800)
    code = said.split("\n", 1)[0].strip()
    t.check(code != "0" and says in said and "nothing was changed" in said, f"switch with {what} should be refused with the same words ({says!r}): {said[-1500:]}")
    t.check(packages() == before[0], f"the refused switch with {what} changed the packages")
    t.check(out("ls -a /.archix 2>&1") == before[1], f"the refused switch with {what} changed the layout")
    kept = out(f"sha256sum {CONFIG.rsplit('/', 1)[0]}/archix.lock")
    if kept != before[2]:
        m.succeed(f"cp {CONFIG.rsplit('/', 1)[0]}/archix.lock /root/kept.lock && {HOST} lock")
        t.check(out(f"sha256sum {CONFIG.rsplit('/', 1)[0]}/archix.lock").split()[0] == kept.split()[0], f"the lock the refused switch with {what} kept should be the one the config resolves to: {out('diff /root/kept.lock ' + CONFIG.rsplit('/', 1)[0] + '/archix.lock | head -20')}")
    m.succeed(f"cp {STOCK} {CONFIG}")


def swap_zlib(m, t, take, give):
    m.succeed(f"sed -i 's|\"{give}\", |\"{take}\", |' {CONFIG}")
    t.require(ok(f"grep -q '\"{take}\", ' {CONFIG}") and not ok(f"grep -q '\"{give}\"' {CONFIG}"), f"the config should list {take}, not {give}")


AUR = "/srv/archix-vmtest-aur"
published = {}


def publish_aur(m, name, makedepends=(), build="true", package=""):
    """Publishes name 1.0 to the guest's own AUR at AUR, a bare git repo per package and the metadata dump, as tests/common's LocalAur does."""
    deps = " ".join(f"'{dep}'" for dep in makedepends)
    pkgbuild = (f"pkgname={name}\npkgver=1.0\npkgrel=1\npkgdesc='archix VM test package {name}'\narch=('any')\nlicense=('MIT')\nmakedepends=({deps})\n"
                f"build() {{\n  {build}\n}}\npackage() {{\n  install -d \"$pkgdir/usr/share/{name}\"\n  echo 1.0 > \"$pkgdir/usr/share/{name}/version\"\n  {package}\n}}\n")
    srcinfo = f"pkgbase = {name}\n\tpkgdesc = archix VM test package {name}\n\tpkgver = 1.0\n\tpkgrel = 1\n\tarch = any\n\tlicense = MIT\n"
    srcinfo += "".join(f"\tmakedepends = {dep}\n" for dep in makedepends) + f"\npkgname = {name}\n"
    published[name] = '{"Name": "%s", "PackageBase": "%s", "Version": "1.0-1", "Provides": [], "Popularity": 0.0, "Maintainer": "archix-tests"}' % (name, name)
    dump = "[" + ", ".join(published.values()) + "]"
    encode = lambda text: base64.b64encode(text.encode()).decode()
    git = "git -c user.name=test -c user.email=test@example.com"
    m.succeed(f"set -e; mkdir -p {AUR}/work/{name}; cd {AUR}/work/{name}; [ -d {AUR}/{name}.git ] || {{ git init -q --bare -b master {AUR}/{name}.git; git init -q -b master; git remote add origin {AUR}/{name}.git; }}; "
              f"echo {encode(pkgbuild)} | base64 -d > PKGBUILD; echo {encode(srcinfo)} | base64 -d > .SRCINFO; {git} add .; {git} commit -q -m '{name} 1.0'; git push -q origin master; "
              f"echo {encode(dump)} | base64 -d | gzip > {AUR}/packages-meta-ext-v1.json.gz", what=f"publishing {name} to the guest's AUR")


def use_aur():
    edit_config(f"'s|^  hostname = \"archix-vm\",|  hostname = \"archix-vm\",\\n  aur = {{ url = \"file://{AUR}\" }},|'")


def zlib_owner():
    return out(f"pacman -Qqo {LIBZ} 2>&1").strip()


def run(m, t):
    bind(m, t)
    with t.step("0: the image is CachyOS as its installer leaves it, booted by Limine"):
        m.start()
        boot()
        t.check("cachyos" in out("uname -r"), f"the image should boot linux-cachyos: {out('uname -r')}")
        t.check(not through_systemd_boot(), "the image should boot through Limine, not systemd-boot")
        t.check(ok("pacman -Q zlib-ng-compat") and not ok("pacman -Qq | grep -qx zlib") and zlib_owner() == "zlib-ng-compat", f"zlib-ng-compat should provide {LIBZ}: {zlib_owner()}")
        t.check(not ok("pacman -Q systemd-ukify") and not ok("command -v ukify"), "systemd-ukify shouldn't be installed")
        t.check(ok("systemctl -q is-enabled ufw.service") and ok("systemctl -q is-active ufw.service"), f"ufw should be enabled and running: {out('systemctl status ufw.service --no-pager 2>&1 | head -5')}")
        listed = out("btrfs subvolume list /")
        for point, subvolume in SUBVOLUMES.items():
            t.check(f"path {subvolume.lstrip('/')}\n" in listed + "\n", f"subvolume {subvolume} should exist: {listed}")
            t.check(out(f"findmnt -no FSROOT {point}").strip() == subvolume, f"{point} should be {subvolume}: {out(f'findmnt {point}')}")
        t.check(ok("grep -qx 'PRETTY_NAME=\"CachyOS\"' /etc/os-release"), f"os-release should name CachyOS: {out('cat /etc/os-release')}")
        m.succeed("efibootmgr --create --disk /dev/vda --part 1 --label Limine --loader '\\EFI\\limine\\BOOTX64.EFI' > /dev/null", what="giving Limine a firmware entry, as CachyOS's installer does")
        m.succeed(f"cp {CONFIG} {STOCK}")

    with t.step("0b: before the first switch, diff makes the refusals switch makes"):
        status, said = m.execute(f"{HOST} diff 2>&1", 1800)
        t.log(said[-2000:])
        t.check(status == 0, f"diff of the stock config should find nothing to refuse, exit {status}: {said[-1500:]}")
        edit_config("'s|^  boot = {|  firewall = { enable = true, allowed_tcp_ports = { 22 } },\\n  boot = {|'")
        refused(m, t, "archix's firewall beside the enabled ufw", "ufw.service would run beside the archix firewall (it's enabled on this system")
        edit_config("'s|\"root=PARTUUID=[^\"]*\"|\"root=UUID=00000000-0000-0000-0000-000000000000\"|'")
        t.require(ok(f"grep -q root=UUID=00000000 {CONFIG}"), "the config should have the placeholder root=")
        refused(m, t, "a placeholder root=", "kernel_params' root=UUID=00000000-0000-0000-0000-000000000000 names no device on this machine")

    with t.step("1: the first switch, given a relative --config, installs the boot tools it lacks, sets rollbacks up and leaves Limine's menu alone"):
        m.succeed(f"useradd -m admin && cp -a {CONFIG.rsplit('/', 1)[0]} /home/admin/archix-config && chown -R admin:admin /home/admin/archix-config")
        limine_conf = out("sha256sum /efi/limine.conf /efi/vmlinuz-linux-cachyos /efi/initramfs-linux-cachyos.img")
        status, said = m.execute("set -o pipefail; cd /home/admin && timeout 1700 archix --config archix-config/ --host archix-vm switch --noconfirm 2>&1 | tee /root/first-switch.log | tail -c 4000", 1800)
        t.log(said[-2500:])
        t.require(status == 0, f"the first switch with --config archix-config/ exited {status}: {said[-2500:]}")
        t.check(LAID_OUT_FIRST in said and "the root is set up for rollbacks" in out("cat /root/first-switch.log"), f"the first switch lays the root out and stops for the reboot: {said[-800:]}")
        t.check(ok("grep -E '^install .*systemd-ukify.* before the layout' /root/first-switch.log") and ok("pacman -Q systemd-ukify"), f"the first switch installs systemd-ukify itself: {out('grep ^install /root/first-switch.log')}")
        t.check(not [e for e in entries() if e != "archix-rescue.conf"], f"on Limine archix writes no entries of its own: {entries()}")
        t.check(out("sha256sum /efi/limine.conf /efi/vmlinuz-linux-cachyos /efi/initramfs-linux-cachyos.img") == limine_conf, "Limine's menu, kernel and initramfs on the ESP stay as they were")
        m.succeed(f"cp /home/admin/archix-config/archix.lock {CONFIG.rsplit('/', 1)[0]}/archix.lock")
        m.reboot()
        boot()
        t.check(not through_systemd_boot(), "Limine still boots the machine")
        said = switch(m, t, "the switch after the layout's reboot")
        t.log(said[-1500:])
        m.reboot()
        boot()
        t.check(not through_systemd_boot(), "Limine still boots the machine")
        t.check(out("findmnt -no FSROOT /var/lib").strip() == "/@varlib", f"/var/lib should be @varlib after the boot: {out('findmnt /var/lib')}")
        t.check(out("findmnt -no FSROOT /.archix").strip() == "/@archix", f"@archix should be mounted after the reboot: {out('findmnt /.archix')}")
        t.check(out("findmnt -no FSROOT /var/lib/pacman").strip() == "/@/var/rollback/pacman", f"pacman's database should be bound from the root: {out('findmnt /var/lib/pacman')}")
        for point, subvolume in SUBVOLUMES.items():
            t.check(out(f"findmnt -no FSROOT {point}").strip() == subvolume, f"{point} should still be {subvolume}: {out(f'findmnt {point}')}")
        t.check(ok("pacman -Q systemd-ukify ufw zlib-ng-compat sudo"), f"the boot tools and CachyOS's own packages stay: {out('pacman -Q systemd-ukify ufw zlib-ng-compat sudo 2>&1')}")

    with t.step("2: CachyOS's v3 builds, locked from its repos, with archix's own copies"):
        for name in ("coreutils", "htop", "jq"):
            t.check(arch_of(name) == "x86_64_v3", f"{name} should be CachyOS's x86_64_v3 build, not {arch_of(name)}")
        for name, repo in (("coreutils", "cachyos-core-v3"), ("htop", "cachyos-extra-v3")):
            t.check(ok(f"grep -q '^{name} = .*repo = \"{repo}\"' /etc/archix/archix.lock"), f"the lock should take {name} from {repo}: {out(f'grep ^{name} /etc/archix/archix.lock')}")
        # The first switch kept its copies before @archix was mounted; the boot after moves them there.
        t.check(ok("ls /.archix/packages/htop-*.pkg.tar.zst") and not ok("ls -d /var/cache/archix/pkg"), f"archix's copy of CachyOS's htop should be in @archix: {out('ls /.archix/packages /var/cache/archix/pkg 2>&1 | head -20')}")

    with t.step("3: a switch on a full disk is refused before it changes anything"):
        add_packages(m, "tree")
        before = (packages(), snapshots(), out("cat /etc/archix/archix.lock"))
        m.succeed("fallocate -l $(( $(df --output=avail -B1 / | tail -1) - 20 * 1024 * 1024 )) /var/tmp/archix-fill")
        status, said = m.execute(f"{HOST} switch --noconfirm > /run/full.log 2>&1; echo $?; tail -c 1500 /run/full.log", 1800)
        m.succeed("rm -f /var/tmp/archix-fill")
        t.log(said)
        t.check(said.split("\n", 1)[0].strip() != "0", "a switch with 20 MB free should fail")
        t.check("nothing was changed" in said, f"the refusal should say nothing was changed: {said[-600:]}")
        t.check((packages(), snapshots()) == before[:2], f"the refused switch changed the packages or the snapshots: {snapshots()} vs {before[1]}")
        drop_packages(m, "tree")

    with t.step("3b: a switch swapping zlib-ng-compat for zlib keeps libz.so.1 throughout"):
        swap_zlib(m, t, "zlib", "zlib-ng-compat")
        said = switch(m, t, "the switch swapping zlib-ng-compat for zlib")
        t.log(said[-1500:])
        t.check(ok("pacman -Qq | grep -qx zlib") and not ok("pacman -Q zlib-ng-compat") and zlib_owner() == "zlib", f"zlib should own {LIBZ} now: {zlib_owner()}")
        t.check(libz_works(), f"libz.so.1 should be there and pacman, sudo and btrfs start: {out(f'ls -l {LIBZ}* 2>&1; sudo -n true 2>&1')}")
        t.check(not ok("[ -e /var/lib/pacman/archix-displaced.json ]"), "nothing displaced is left pending")
        swap_zlib(m, t, "zlib-ng-compat", "zlib")
        said = switch(m, t, "the switch swapping zlib back for zlib-ng-compat")
        t.log(said[-3000:])
        t.check(zlib_owner() == "zlib-ng-compat" and libz_works(), f"zlib-ng-compat should own {LIBZ} again, and it work: {zlib_owner()}; {out('ls -l /usr/lib/libz* 2>&1; journalctl -b -n 30 --no-pager -o cat 2>&1 | tail -30')}")

    with t.step("3c: a power cut in the middle of the swap leaves libz.so.1, and the system boots and switches again"):
        swap_zlib(m, t, "zlib", "zlib-ng-compat")
        # The watcher stops archix and pacman once zlib-ng-compat has left pacman's database, before the switch is done.
        m.succeed("systemd-run --unit=archix-vmtest-cut-watch --collect sh -c 'while ls -d /var/lib/pacman/local/zlib-ng-compat-[0-9]* > /dev/null 2>&1; do sleep 0.02; done; pkill -STOP -x pacman; pkill -STOP -x archix; touch /run/archix-vmtest-stopped'")
        m.succeed(f"systemd-run --unit=archix-vmtest-cut-switch --collect sh -c '{HOST} switch --noconfirm > /root/cut-switch.log 2>&1'")
        m.wait_until_succeeds("[ -e /run/archix-vmtest-stopped ]", timeout=2400, what="the switch should reach the swap")
        t.log("stopped at: " + out("ps -eo pid,stat,args | grep -E '[p]acman|[a]rchix --host'; tail -5 /root/cut-switch.log; ls /var/lib/pacman/archix-displaced.json 2>&1"))
        t.check(ok(f"[ -e {LIBZ} ]"), f"in the middle of the swap {LIBZ} should be there: {out(f'ls -l /usr/lib/libz* 2>&1')}")
        m.succeed("sync -f /usr/lib && sync -f /var/lib/pacman && sync -f /.archix", what="writing the root as it is mid-swap")
        m.crash()
        boot()
        t.log(out("archix status 2>&1 | head -30"))
        t.check(libz_works(), f"after the cut libz.so.1 should be there and pacman, sudo and btrfs start: {out(f'ls -l /usr/lib/libz* 2>&1; pacman -Q zlib zlib-ng-compat 2>&1')}")
        said = switch(m, t, "the switch after the cut")
        t.log(said[-1500:])
        t.check(zlib_owner() == "zlib" and not ok("pacman -Q zlib-ng-compat") and libz_works(), f"the switch after the cut finishes the swap: {zlib_owner()}")
        t.check(ok("pacman -Qk zlib > /dev/null") and not ok("[ -e /var/lib/pacman/archix-displaced.json ]"), f"zlib's files are all there and nothing is left pending: {out('pacman -Qk zlib 2>&1 | tail -3')}")
        m.reboot()
        boot()
        t.check(libz_works(), "the system boots after the swap")

    with t.step("3d: a switch that fails after resolving keeps the lock it resolved, so the rerun doesn't ask about the AUR recipe again"):
        publish_aur(m, "archix-vmtest-trusted")
        # Published now, so the dump archix fetches here already lists the package step 3e builds.
        publish_aur(m, "archix-vmtest-cursor", makedepends=("python-clickgen",), build='python -c "import clickgen" && echo clickgen > "$srcdir/marker"',
                    package='install -Dm644 "$srcdir/marker" "$pkgdir/usr/share/archix-vmtest-cursor/marker"')
        m.succeed(f"cp {CONFIG} /root/init.lua.before-aur && mkfifo /etc/archix-vmtest-pipe")
        use_aur()
        edit_config("'s|  files = {|  files = {\\n    [\"/etc/archix-vmtest-pipe\"] = { text = \"x\" },|'")
        add_packages(m, "archix-vmtest-trusted")
        status, said = m.execute(f"printf 'y\\n' | {HOST} switch > /root/aur.log 2>&1; echo $?; cat /root/aur.log", 1800)
        t.log(said[-2000:])
        t.check(said.split("\n", 1)[0].strip() != "0" and "trust archix-vmtest-trusted at" in said and "is not a file or symlink" in said, f"the switch asks about the recipe, then is refused for the pipe: {said[-1500:]}")
        t.check(ok("grep -q '^archix-vmtest-trusted = ' /etc/archix/archix.lock") and not ok("pacman -Q archix-vmtest-trusted"), f"the lock it resolved, with the AUR package, stays: {out('grep ^archix-vmtest /etc/archix/archix.lock')}")
        status, said = m.execute(f"{HOST} switch < /dev/null > /root/aur-again.log 2>&1; echo $?; cat /root/aur-again.log", 1800)
        t.log(said[-2000:])
        t.check(said.split("\n", 1)[0].strip() != "0" and "is not a file or symlink" in said, f"the rerun is refused for the pipe again: {said[-1500:]}")
        t.check("resolving" not in said and "trust archix-vmtest-trusted at" not in said, f"the rerun neither resolves again nor asks about the recipe: {said[-1500:]}")
        m.succeed(f"rm /etc/archix-vmtest-pipe && cp /root/init.lua.before-aur {CONFIG}")

    with t.step("3e: an AUR package whose build dependency only CachyOS's repo has builds, with that dependency locked from the repo"):
        m.succeed(f"cp {CONFIG} /root/init.lua.before-build")
        use_aur()
        add_packages(m, "archix-vmtest-cursor", "devtools")
        status, said = m.execute(f"set -o pipefail; timeout 3500 {HOST} switch --noconfirm 2>&1 | tee /root/build.log | tail -c 4000", 3600)
        t.log(said[-2500:])
        t.require(status == 0, f"the switch building archix-vmtest-cursor exited {status}: {said[-2500:]}")
        locked = out("grep '^python-clickgen = ' /etc/archix/archix.lock")
        t.check('repo = "cachyos"' in locked and "build_dep = true" in locked and "build_only = true" in locked, f"python-clickgen should be locked from CachyOS's repo as a build dependency: {locked!r}")
        t.check(out("cat /usr/share/archix-vmtest-cursor/marker 2>&1").strip() == "clickgen", f"the build ran with clickgen in the chroot: {out('cat /usr/share/archix-vmtest-cursor/marker 2>&1')}")
        t.check(not ok("pacman -Q python-clickgen"), "the build dependency isn't installed on the system")
        t.check(ok("grep -q '^\\[cachyos\\]' /var/lib/archix/chroot/root/etc/pacman.conf"), f"the build chroot gets CachyOS's repo: {out('cat /var/lib/archix/chroot/root/etc/pacman.conf 2>&1 | tail -30')}")
        m.succeed(f"cp /root/init.lua.before-build {CONFIG}")
        switch(m, t, "the switch dropping the AUR package and devtools")

    with t.step("4: on Limine, keeping a snapshot from before an initramfs change is refused"):
        older = current_generation()
        m.succeed("mkdir -p /etc/archix/files && printf 'MODULES+=(vfat)\\n' > /etc/archix/files/archix-vmtest.conf")
        edit_config("'s|  files = {|  files = {\\n    [\"/etc/mkinitcpio.conf.d/archix-vmtest.conf\"] = { source = \"files/archix-vmtest.conf\" },|'")
        drop_packages(m, "htop", "jq")
        add_packages(m, "tree")
        switch(m, t, "the switch changing the initramfs")
        kept = newest_snapshot()
        t.check(not ok("pacman -Q htop") and ok("pacman -Q tree"), "the switch applied live")
        status, said = m.execute(f"{HOST} rollback {kept} --noconfirm 2>&1", 600)
        t.log(said)
        t.check(status != 0 and "initramfs" in said and "systemd-boot" in said, f"keep across an initramfs change should be refused on Limine, pointing at systemd-boot: {said[-600:]}")
        t.check(not ok("[ -e /run/archix/keep-pending ]"), "nothing is pending after the refusal")

    with t.step(f"5: a revert to generation {older} with CachyOS's mirror unreachable reinstalls its builds from archix's copies"):
        m.succeed("cp /etc/hosts /root/hosts && echo '127.0.0.1 mirror.cachyos.org' >> /etc/hosts && rm -rf /var/cache/pacman/pkg/*")
        t.check(not ok("curl -sf -m 10 -o /dev/null https://mirror.cachyos.org/"), "mirror.cachyos.org should be unreachable now")
        said = m.succeed(f"set -o pipefail; {HOST} revert {older} --noconfirm 2>&1 | tail -30", 2400, what=f"archix revert {older} offline")
        t.log(said)
        for name in ("htop", "jq"):
            t.check(arch_of(name) == "x86_64_v3", f"the revert should reinstall CachyOS's {name}, not {arch_of(name)!r}")
        m.succeed("cp /root/hosts /etc/hosts")

    with t.step("6: the move to systemd-boot keeps Limine first until a boot through systemd-boot counts as good"):
        edit_config("'s|loader = \"limine\"|loader = \"systemd-boot\"|'")
        said = switch(m, t, "the switch to systemd-boot")
        t.log(said[-1500:])
        order, after = boot_order()
        t.log(f"boot order {order}, next {after}: {out('efibootmgr')}")
        t.check(order[:1] == ["Limine"], f"Limine should stay first in the boot order: {order}")
        t.check(after is not None and after != "Limine", f"the firmware should try systemd-boot once at the next boot: next {after}")
        t.check(entry("archix-current.conf") is not None, f"the switch should write systemd-boot's menu: {entries()}")
        info = out("archix boot-info")
        t.check("loader: limine" in info and "protection: none" in info and "problem:" not in info, f"before the reboot boot-info names Limine and finds the firmware as archix set it: {info}")
        m.reboot()
        started = boot()
        t.check(through_systemd_boot() and started == "archix-current.conf", f"systemd-boot should start the current entry: {started}")
        info = out("archix boot-info")
        t.check("loader: systemd-boot" in info and "protection: in effect" in info, f"boot-info should say systemd-boot started the boot, protected: {info}")
        # Under Limine archix can't tell systemd-boot counts boots, so that entry had no tries; the switch from systemd-boot gives it some, and the good boot through them finishes the move.
        for attempt in range(3):
            if migration_done():
                break
            switch(m, t, f"a switch from systemd-boot retrying the move ({attempt + 1})")
            m.reboot()
            boot()
            m.wait_until_succeeds("[ -f /efi/loader/entries/archix-current.conf ]", timeout=300, what="the boot through systemd-boot should count as good")
            for _ in range(60):
                if migration_done():
                    break
                time.sleep(5)
        order, _ = boot_order()
        t.check(migration_done(), f"the move should be done: {out('cat /var/lib/archix/boot/limine-migration.json')}")
        t.check(order[:1] != ["Limine"] and "Limine" in order, f"systemd-boot first, Limine kept after it: {order}")
        t.check(ok("grep -aq systemd-boot /efi/EFI/BOOT/BOOTX64.EFI") and ok("[ -f /efi/EFI/BOOT/BOOTX64.EFI.before-archix ]"), "the removable-media path should be systemd-boot's, Limine's kept beside it")
        m.reboot()
        started = boot()
        t.check(through_systemd_boot() and started == "archix-current.conf", f"systemd-boot boots by default now: {started}")

    with t.step("6b: the entries and their images are named after CachyOS, not Arch Linux"):
        t.check(ok("grep -qx 'PRETTY_NAME=\"CachyOS\"' /etc/os-release"), f"os-release should still name CachyOS: {out('cat /etc/os-release')}")
        titles = out("grep -h '^title' /efi/loader/entries/archix-*.conf")
        t.log(titles)
        named = [line for line in titles.splitlines() if "rescue" not in line]
        t.check(named and all("CachyOS" in line for line in named) and "Arch Linux" not in titles, f"archix's entries should be titled CachyOS: {titles}")
        image = out("sed -nE 's#^(efi|uki) /#/efi/#p' /efi/loader/entries/$(ls /efi/loader/entries | grep '^archix-current' | head -1)").strip()
        inspected = out(f"ukify inspect {image} 2>&1")
        t.check('PRETTY_NAME="CachyOS (' in inspected and "Arch Linux" not in inspected, f"the current image's os-release should name CachyOS: {inspected[:1500]}")
        rescue = out("sed -nE 's#^(efi|uki) /#/efi/#p' /efi/loader/entries/archix-rescue.conf 2>/dev/null").strip()
        t.check(rescue and ok(f"[ -s {rescue} ]"), f"on systemd-boot the rescue entry and its image are built, with ukify the first switch installed: {entries()} {rescue!r}")

    with t.step(f"7: on systemd-boot, keeping snapshot {kept} works"):
        m.succeed(f"set -o pipefail; {HOST} rollback {kept} --noconfirm 2>&1 | tail -20", 1800, what=f"keeping snapshot {kept}")
        m.reboot()
        started = boot()
        t.check(started == "archix-current.conf", f"the kept root boots from the current entry: {started}")
        t.check(ok("pacman -Q htop") and not ok("[ -e /etc/mkinitcpio.conf.d/archix-vmtest.conf ]"), f"the root is snapshot {kept}, from before the initramfs change: {out('pacman -Q htop tree 2>&1')}")
        t.log(f"selected {selected()}")
