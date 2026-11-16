# CachyOS as its installer leaves it (zlib-ng-compat, ufw enabled, no systemd-ukify, its btrfs subvolumes), booted by Limine, and each archix failure of a real CachyOS install: diff makes switch's refusals (an enabled ufw beside archix's firewall, a root= naming no device); a relative --config; the first switch leaves Limine's menu, the ESP and the subvolumes alone; the lock takes CachyOS's builds; a switch on a full disk is refused before it changes anything; swapping zlib-ng-compat for zlib, and a power cut in the middle of that swap, never leave libz.so.1 missing; a failed switch keeps the lock it resolved and the AUR trust given; an AUR package whose build dependency only CachyOS's repo has builds with it locked from there.
import base64

from boot.common import CONFIG, HOST, bind, boot, edit_config, not_stub, ok, out
from suite import add_packages, drop_packages, switch

MACHINE = {"mem": "4096", "cpus": "4", "global_timeout": 14400}
LOADER_INFO = "/sys/firmware/efi/efivars/LoaderInfo-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f"


def arch_of(name):
    return out(f"pacman -Qi {name} 2>/dev/null | sed -n 's/^Architecture *: //p'").strip()


def through_systemd_boot():
    """Whether systemd-boot started this boot: Limine sets the Boot Loader Interface's variables too, under its own name."""
    return "systemd-boot" in out(f"tail -c +5 {LOADER_INFO} 2>/dev/null | tr -d '\\000'")


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
    before = (packages(), out(f"sha256sum {CONFIG.rsplit('/', 1)[0]}/archix.lock"))
    status, said = m.execute(f"{HOST} diff > /root/diff.log 2>&1; echo $?; cat /root/diff.log", 1800)
    code = said.split("\n", 1)[0].strip()
    t.log(f"diff with {what}: {said[-2000:]}")
    t.check(code == "2" and "switch would refuse" in said and says in said, f"diff with {what} should exit 2 naming the refusal ({says!r}), not {code}: {said[-1500:]}")
    status, said = m.execute(f"{HOST} switch --noconfirm > /root/refused.log 2>&1; echo $?; cat /root/refused.log", 1800)
    code = said.split("\n", 1)[0].strip()
    not_stub(int(code) if code.isdigit() else status, said, f"switch with {what}")
    t.check(code != "0" and says in said and "nothing was changed" in said, f"switch with {what} should be refused with the same words ({says!r}): {said[-1500:]}")
    t.check(packages() == before[0], f"the refused switch with {what} changed the packages")
    kept = out(f"sha256sum {CONFIG.rsplit('/', 1)[0]}/archix.lock")
    if kept != before[1]:
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
        t.check(status in (0, 1), f"diff of the stock config should find nothing to refuse, exit {status}: {said[-1500:]}")
        edit_config("'s|^  file_systems = {|  firewall = { enable = true, allowed_tcp_ports = { 22 } },\\n  file_systems = {|'")
        refused(m, t, "archix's firewall beside the enabled ufw", "ufw.service would run beside the archix firewall (it's enabled on this system")
        edit_config("'s|\"root=PARTUUID=[^\"]*\"|\"root=UUID=00000000-0000-0000-0000-000000000000\"|'")
        t.require(ok(f"grep -q root=UUID=00000000 {CONFIG}"), "the config should have the placeholder root=")
        refused(m, t, "a placeholder root=", "kernel_params' root=UUID=00000000-0000-0000-0000-000000000000 names no device on this machine")

    with t.step("1: the first switch, given a relative --config, leaves Limine's menu, the ESP and the subvolumes alone"):
        m.succeed(f"useradd -m admin && cp -a {CONFIG.rsplit('/', 1)[0]} /home/admin/archix-config && chown -R admin:admin /home/admin/archix-config")
        esp = out("find /efi -type f -exec sha256sum {} + | sort")
        status, said = m.execute("set -o pipefail; cd /home/admin && timeout 1700 archix --config archix-config/ --host archix-vm switch --noconfirm 2>&1 | tee /root/first-switch.log | tail -c 4000", 1800)
        t.log(said[-2500:])
        not_stub(status, said, "the first switch")
        t.require(status == 0, f"the first switch with --config archix-config/ exited {status}: {said[-2500:]}")
        t.check(out("find /efi -type f -exec sha256sum {} + | sort") == esp, f"the switch should leave the ESP as it was: {out('find /efi -type f')}")
        m.succeed(f"cp /home/admin/archix-config/archix.lock {CONFIG.rsplit('/', 1)[0]}/archix.lock")
        m.reboot()
        boot()
        t.check(not through_systemd_boot(), "Limine still boots the machine")
        for point, subvolume in SUBVOLUMES.items():
            t.check(out(f"findmnt -no FSROOT {point}").strip() == subvolume, f"{point} should still be {subvolume}: {out(f'findmnt {point}')}")
        t.check(ok("pacman -Q ufw zlib-ng-compat sudo"), f"CachyOS's own packages stay: {out('pacman -Q ufw zlib-ng-compat sudo 2>&1')}")

    with t.step("2: CachyOS's v3 builds, locked from its repos"):
        for name in ("coreutils", "htop", "jq"):
            t.check(arch_of(name) == "x86_64_v3", f"{name} should be CachyOS's x86_64_v3 build, not {arch_of(name)}")
        for name, repo in (("coreutils", "cachyos-core-v3"), ("htop", "cachyos-extra-v3")):
            t.check(ok(f"grep -q '^{name} = .*repo = \"{repo}\"' /etc/archix/archix.lock"), f"the lock should take {name} from {repo}: {out(f'grep ^{name} /etc/archix/archix.lock')}")

    with t.step("3: a switch on a full disk is refused before it changes anything"):
        add_packages(m, "tree")
        before = packages()
        m.succeed("fallocate -l $(( $(df --output=avail -B1 / | tail -1) - 20 * 1024 * 1024 )) /var/tmp/archix-fill")
        status, said = m.execute(f"{HOST} switch --noconfirm > /run/full.log 2>&1; echo $?; tail -c 1500 /run/full.log", 1800)
        m.succeed("rm -f /var/tmp/archix-fill")
        t.log(said)
        t.check(said.split("\n", 1)[0].strip() != "0", "a switch with 20 MB free should fail")
        t.check("nothing was changed" in said, f"the refusal should say nothing was changed: {said[-600:]}")
        t.check(packages() == before, "the refused switch changed the packages")
        drop_packages(m, "tree")

    with t.step("3b: a switch swapping zlib-ng-compat for zlib keeps libz.so.1 throughout"):
        swap_zlib(m, t, "zlib", "zlib-ng-compat")
        said = switch(m, t, "the switch swapping zlib-ng-compat for zlib")
        t.log(said[-1500:])
        t.check(ok("pacman -Qq | grep -qx zlib") and not ok("pacman -Q zlib-ng-compat") and zlib_owner() == "zlib", f"zlib should own {LIBZ} now: {zlib_owner()}")
        t.check(libz_works(), f"libz.so.1 should be there and pacman, sudo and btrfs start: {out(f'ls -l {LIBZ}* 2>&1; sudo -n true 2>&1')}")
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
        t.log("stopped at: " + out("ps -eo pid,stat,args | grep -E '[p]acman|[a]rchix --host'; tail -5 /root/cut-switch.log"))
        t.check(ok(f"[ -e {LIBZ} ]"), f"in the middle of the swap {LIBZ} should be there: {out(f'ls -l /usr/lib/libz* 2>&1')}")
        m.succeed("sync -f /usr/lib && sync -f /var/lib/pacman", what="writing the root as it is mid-swap")
        m.crash()
        boot()
        t.log(out(f"{HOST} diff 2>&1 | head -30"))
        t.check(libz_works(), f"after the cut libz.so.1 should be there and pacman, sudo and btrfs start: {out(f'ls -l /usr/lib/libz* 2>&1; pacman -Q zlib zlib-ng-compat 2>&1')}")
        said = switch(m, t, "the switch after the cut")
        t.log(said[-1500:])
        t.check(zlib_owner() == "zlib" and not ok("pacman -Q zlib-ng-compat") and libz_works(), f"the switch after the cut finishes the swap: {zlib_owner()}")
        t.check(ok("pacman -Qk zlib > /dev/null") and not ok("[ -e /var/lib/pacman/db.lck ]"), f"zlib's files are all there and pacman's lock is gone: {out('pacman -Qk zlib 2>&1 | tail -3')}")
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
