# A year of weekly switches, each with archix-churn's next version and a new kernel command line (so a new boot image), and a reboot: the ESP, the menu, snapshots (on btrfs), generation records and archix's state stay bounded, every entry starts an image there and every image there is started, dbx is untouched while revocation is off and stops at archix's 128-entry cap once on, and every invariant holds every week.
import re

from boot.common import HOST, bind, boot, edit_config, entries, ok, out, settled, snapshots, trying
from suite import BTRFS, images_consistent, lay_out, update

MACHINE = {"global_timeout": 14400}
WEEKS = 52
CARRIER_RACES_ALLOWED = 3
DBX = "/sys/firmware/efi/efivars/dbx-d719b2cb-3d3a-4596-a3bc-dad00e67656f"
KEK = "/var/lib/sbctl/keys/KEK"
OWNER = "c6825be8dae8eb40b968e918df35dec1"
# A dbx append of N made-up SHA-256 entries under archix's owner GUID, signed with sbctl's KEK, as if archix had revoked that many images before.
SEED = r"""set -e
le32() { printf "\\x$(printf %02x $(($1 & 255)))\\x$(printf %02x $((($1 >> 8) & 255)))\\x$(printf %02x $((($1 >> 16) & 255)))\\x$(printf %02x $((($1 >> 24) & 255)))"; }
n=COUNT
{
  printf '\x26\x16\xc4\xc1\x4c\x50\x92\x40\xac\xa9\x41\xf9\x36\x93\x43\x28'
  le32 $((28 + 48 * n)); le32 0; le32 48
  for i in $(seq "$n"); do printf '\xc6\x82\x5b\xe8\xda\xe8\xeb\x40\xb9\x68\xe9\x18\xdf\x35\xde\xc1'; head -c 32 /dev/urandom; done
} > /var/tmp/seed.esl
sign-efi-sig-list -a -k KEK/KEK.key -c KEK/KEK.pem dbx /var/tmp/seed.esl /var/tmp/seed.auth > /dev/null
chattr -i DBX 2> /dev/null || true
efi-updatevar -a -f /var/tmp/seed.auth dbx
""".replace("KEK/", KEK + "/").replace("DBX", DBX)


def ours_in_dbx():
    return int(out(f"[ -e {DBX} ] && od -An -tx1 -v {DBX} | tr -d ' \\n' | grep -o {OWNER} | wc -l || echo 0").strip() or 0)


def records():
    return int(out("ls /var/lib/archix/generations | grep -cE '^[0-9]+$'").strip() or 0)


def current_generation():
    return int(out("archix generations | awk '$1 == \"*\" && $2 ~ /^[0-9]+$/ { print $2; exit }'").strip() or 0)


def invariants(t, week, seen):
    """What must hold every week; seen keeps the largest ESP."""
    started = out("tail -c +5 /sys/firmware/efi/efivars/LoaderEntrySelected-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f | tr -d '\\000'").strip()
    t.check(started == "archix-current.conf" and trying() is None, f"the current entry should boot, not {started} {trying()}")
    listed = entries()
    images_consistent(t)
    rescue = out("sed -nE 's#^(efi|uki) /#/efi/#p' /efi/loader/entries/archix-rescue.conf 2>/dev/null").strip()
    t.check(bool(rescue) and ok(f"[ -f {rescue} ]"), f"the rescue entry and its image should be there: {listed}")
    t.check(out("ls /efi/EFI/archix-rescue | wc -l").strip() == "1", f"EFI/archix-rescue should hold one image: {out('ls /efi/EFI/archix-rescue')}")
    snapshot_entries = [e for e in listed if e.startswith("archix-snapshot-")]
    t.check(len(snapshot_entries) <= 6, f"rollback.entries = 5 and the slot allow at most 6 snapshot entries: {snapshot_entries}")
    if BTRFS:
        have = set(snapshots())
        for name in snapshot_entries:
            number = int(re.match(r"archix-snapshot-(\d+)", name).group(1))
            t.check(number in have, f"{name} names snapshot {number}, which is gone: {sorted(have)}")
        # keep_switch counts the snapshots without an entry; those with one (rollback.entries = 5 and the slot) are never pruned.
        t.check(len(have) <= 16, f"keep_switch = 10 beside the 6 snapshots with entries should bound the snapshots: {sorted(have)}")
        unwritable = out("for s in /.archix/snapshots/*/root; do [ -e $s ] || continue; [ \"$(btrfs property get -ts $s ro)\" = ro=true ] || echo $s; done").strip()
        t.check(not unwritable, f"every snapshot stays read-only: {unwritable}")
    else:
        t.check(not snapshot_entries and not ok("[ -d /.archix/snapshots ]"), f"no snapshots on ext4: {listed}")
    sizes = [int(s) for s in out("stat -c %s /efi/EFI/archix/*.efi /efi/EFI/archix-rescue/* 2>/dev/null").split()]
    # Every EFI binary on the ESP, the image's own UKI and the loaders included, which all but archix's images stay the same.
    binaries = sum(int(s) for s in out("find /efi -type f \\( -iname '*.efi' -o -iname '*.EFI' \\) -exec stat -c %s {} +").split()) // 1048576
    esp_mb = int(out("du -sm /efi | cut -f1").strip())
    biggest = max(sizes) // 1048576 + 1
    t.check(esp_mb <= binaries + 8, f"the ESP holds {esp_mb} MiB, more than its EFI binaries' {binaries} MiB")
    t.check(len(sizes) <= 8, f"archix's images should stay bounded by its entries: {len(sizes)}")
    t.check(len(sizes) <= len(listed), f"{len(sizes)} images for {len(listed)} entries")
    seen["esp"] = max(seen.get("esp", 0), esp_mb)
    t.check(ours_in_dbx() <= 128, f"archix has {ours_in_dbx()} entries in dbx, more than its cap of 128")
    if week <= 14:
        t.check(records() == current_generation(), f"with keep_days = 30, every record of the {current_generation()} generations should stay: {records()}")
    else:
        # A snapshot holds the record of the generation it was taken of, so on btrfs the bounded snapshots bound the records too.
        bound = max(12, len(snapshots()) + 1) if BTRFS else 12
        t.check(records() <= bound, f"with generations.keep = 10 and keep_days = 0, at most {bound} records should stay (10, or one per snapshot and the current one), not {records()}")
    if week < 40:
        t.check(ours_in_dbx() == 0, f"with revoke_unused off, archix should never write dbx: {ours_in_dbx()} entries")


def week_boot(t, week, races):
    """boot(), carrying on after docs/CI.md's networkd carrier race once the link is back, up to CARRIER_RACES_ALLOWED times: a year of boots meets it often, and it isn't what churn tests."""
    try:
        boot()
    except AssertionError:
        if "networkd-carrier-race" not in (t.verdict or ""):
            raise
        t.verdict = None
        races.append(week)
        t.log(f"week {week}: carrying on after networkd carrier race {len(races)} of at most {CARRIER_RACES_ALLOWED} (weeks {races})")
        t.require(len(races) <= CARRIER_RACES_ALLOWED, f"the networkd carrier race came {len(races)} times in a run, at weeks {races}, more than the {CARRIER_RACES_ALLOWED} allowed")
        # The race can stop boot() before it judged the other units, and wait-online's failure is the race's own.
        settled(allowed=("systemd-networkd-wait-online.service",))


def run(m, t):
    bind(m, t)
    lay_out(m, t)
    seen = {}
    capped = None
    races = []
    for week in range(1, WEEKS + 1):
        with t.step(f"week {week}"):
            t.log(f"week {week}: records {records()}, snapshots {snapshots() if BTRFS else '-'}, ESP {out('du -sm /efi | cut -f1').strip()} MiB, /var/lib/archix {out('du -sk /var/lib/archix | cut -f1').strip()} KiB, dbx {ours_in_dbx()}, efivars free {out('stat -f -c %f /sys/firmware/efi/efivars').strip()}")
            invariants(t, week, seen)
            if week == 14:
                edit_config("'s|keep_days = 30|keep_days = 0|'")
            elif week == 27:
                m.succeed(f"set -o pipefail; {HOST} secure-boot setup 2>&1 | tail -20", 600, what="archix secure-boot setup")
                edit_config("'s|boot = { loader = \"systemd-boot\" }|boot = { loader = \"systemd-boot\", secure_boot = true }|'")
            elif week == 40:
                t.check("Secure Boot: enabled (user)" in out("bootctl status 2>/dev/null"), "Secure Boot should be enforcing")
                said = out("archix secure-boot revoke --dry-run 2>&1")
                found = re.search(r"^(\d+) boot images? archix signed", said, re.M)
                pending = int(found.group(1)) if found else 0
                t.require(pending > 0, f"twelve weeks of signed images should have left some unused but unrevoked: {said}")
                # Two weeks to the cap: OVMF's variable store keeps every superseded copy of dbx, so a longer approach runs into archix's 64 KiB free-space margin first.
                m.succeed(SEED.replace("COUNT", str(128 - pending - 2)), 300, what="seeding dbx")
                t.log(f"{pending} signed images wait for revocation; seeded dbx with {128 - pending - 2} of archix's entries")
                edit_config("'s|secure_boot = true }|secure_boot = true, revoke_unused = true }|'")
            package = f"/srv/churn-pkgs/archix-churn-{week + 1}-1-any.pkg.tar.zst"
            m.succeed(f"cp {package} /srv/churn-repo/ && repo-add -q /srv/churn-repo/churn.db.tar.gz /srv/churn-repo/archix-churn-{week + 1}-1-any.pkg.tar.zst")
            edit_config(f"'s|\"archix.week=[0-9]*\"|\"archix.week={week}\"|'")
            update(m, t, "2026-10-10")
            before = current_generation()
            status, said = m.execute(f"{HOST} switch --noconfirm > /root/switch.log 2>&1; echo $?; tail -c 2000 /root/switch.log", 1800)
            code = said.split("\n", 1)[0].strip()
            t.require(current_generation() > before, f"the week's switch should make a generation (exit {code}): {said[-1500:]}")
            if "more than the 128 it allows itself" in said:
                capped = capped or week
                t.log(f"week {week}: revocation refused at the cap, the switch applied anyway (exit {code})")
            else:
                t.require(code == "0", f"the week's switch failed: {said[-1500:]}")
                if week >= 40:
                    t.check(ok("grep -q '^revoked ' /root/switch.log"), f"with revoke_unused on, the switch should revoke the image it dropped: {said[-1000:]}")
            t.check(week < 42 or capped is not None, "the cap should have been reached by week 42")
            m.reboot()
            week_boot(t, week, races)
    with t.step("after a year"):
        invariants(t, WEEKS + 1, seen)
        kib = int(out("du -sk /var/lib/archix | cut -f1").strip())
        t.log(f"after {WEEKS} weeks: ESP at most {seen['esp']} MiB, {records()} records, /var/lib/archix {kib} KiB, dbx {ours_in_dbx()}, cap reached in week {capped}")
        t.check(kib < 65536, f"/var/lib/archix grew to {kib} KiB")
        t.check(capped is not None and ours_in_dbx() == 128, f"dbx should hold exactly archix's 128 entries at the cap, not {ours_in_dbx()} (capped in week {capped})")
        m.shutdown()
