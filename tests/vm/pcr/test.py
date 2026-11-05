# Signed PCR 11 policies on a swtpm TPM, in place: a LUKS2 volume enrolled with systemd's PCR public key unlocks with no passphrase under every image archix signs with that key, a snapshot's older one included (btrfs), and after `archix secure-boot rotate-pcr-key` only under images built with the new key.
from boot import common
from boot.common import HOST, bind, edit_config, entries, entry, ok, out, trying
from suite import BTRFS, lay_out, newest_snapshot, switch

MACHINE = {"tpm": True, "disks": ["file=/work/disk.img,format=raw,if=virtio", "file=/work/data.img,format=raw,if=virtio"]}
VOLUME = "/dev/vdb"
PASS = "archix-vmtest-passphrase"
PUBLIC = "/etc/systemd/tpm2-pcr-public-key.pem"
PRIVATE = "/etc/systemd/tpm2-pcr-private-key.pem"


def image_of(name):
    file = entry(name)
    return out(f"sed -nE 's#^(efi|uki) /#/efi/#p' /efi/loader/entries/{file}").strip() if file else ""


def image_key(image):
    """The public key an image's signed policy names, from its .pcrpkey section."""
    return out(f"objcopy -O binary --only-section=.pcrpkey {image} /run/pcrpkey 2>/dev/null && sha256sum < /run/pcrpkey | cut -c1-16").strip()


def key_sum():
    return out(f"sha256sum < {PUBLIC} | cut -c1-16").strip()


def unlocked():
    ok("for i in $(seq 60); do systemctl is-active -q systemd-cryptsetup@data.service && exit 0; systemctl is-failed -q systemd-cryptsetup@data.service && exit 1; sleep 1; done; exit 1", 90)
    return ok("systemctl is-active -q systemd-cryptsetup@data.service && [ -b /dev/mapper/data ]")


def assert_unlocked(t, what, marker):
    # headless=true: systemd-cryptsetup never asks, so an open volume was opened by the TPM.
    t.require(unlocked(), f"{what}: the volume should unlock through the TPM with no passphrase: {out('journalctl -b -u systemd-cryptsetup@data.service --no-pager | tail -8')}")
    t.check(out("mkdir -p /run/data && mount -o ro /dev/mapper/data /run/data && cat /run/data/marker; umount /run/data").strip() == marker, f"{what}: the volume's marker isn't the one written")


def assert_locked(t, what):
    t.check(not unlocked(), f"{what}: the volume should not unlock through the TPM")
    t.check(ok(f"printf %s {PASS} | cryptsetup open {VOLUME} data-pass - && cryptsetup close data-pass"), f"{what}: the passphrase should still open the volume")


def boot_entry(m, t, name, allowed=()):
    m.select_entry(name)
    m.reboot()
    started = boot(allowed)
    t.check(started == name, f"{name} should start, not {started}")

# systemd 262 can't set its NvPCRs up on swtpm 0.10 ("Failed to initialize NvPCR index"), so these fail at every boot here; the config turns the health check's failed-units rule off for them.
NVPCR = ("systemd-pcrproduct.service", "systemd-tpm2-setup-early.service", "systemd-tpm2-setup.service")


def boot(allowed=()):
    return common.boot(tuple(allowed) + NVPCR)


def run(m, t):
    bind(m, t)
    lay_out(m, t, allowed=NVPCR)
    with t.step("1: the volume enrolled against systemd's PCR key; the switch signs a policy for it"):
        t.require(ok("[ -c /dev/tpmrm0 ]"), "the VM should have a TPM")
        m.succeed(f"ukify genkey --pcr-private-key={PRIVATE} --pcr-public-key={PUBLIC}", what="ukify genkey")
        m.succeed(f"printf %s {PASS} | cryptsetup luksFormat --type luks2 --batch-mode {VOLUME} - && printf %s {PASS} | cryptsetup open {VOLUME} data-setup -"
                  " && mkfs.ext4 -q /dev/mapper/data-setup && mkdir -p /run/data && mount /dev/mapper/data-setup /run/data"
                  " && head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \\n' > /run/data/marker && umount /run/data && cryptsetup close data-setup", what="making the LUKS volume")
        marker = out("printf %s " + PASS + f" | cryptsetup open {VOLUME} data-read - && mkdir -p /run/data && mount -o ro /dev/mapper/data-read /run/data && cat /run/data/marker; umount /run/data; cryptsetup close data-read").strip()
        m.succeed(f"PASSWORD={PASS} systemd-cryptenroll --tpm2-device=auto --tpm2-public-key={PUBLIC} --tpm2-public-key-pcrs=11 {VOLUME}", what="systemd-cryptenroll")
        t.check(out(f"cryptsetup luksDump {VOLUME} | grep -c systemd-tpm2").strip() == "1", "the volume should have one TPM2 token")
        edit_config("'s|    -- \\[\"/etc/crypttab\"\\]|    [\"/etc/crypttab\"]|'")
        switch(m, t, "the switch with the PCR key in place")
        first = image_of("archix-current.conf")
        t.check(image_key(first) == key_sum(), f"the current image {first} should carry the PCR public key")
        t.check(ok(f"ukify inspect {first} | grep -q pcrsig"), "and a signed PCR policy")
        m.reboot()

    with t.step("2: the signed image unlocks the volume through the TPM"):
        boot()
        assert_unlocked(t, "the current image", marker)

    with t.step("3: a switch that changes the image still unlocks it, with nothing re-sealed"):
        edit_config("'s|\"console=ttyS0\" }|\"console=ttyS0\", \"archix.pcrtest\" }|'")
        switch(m, t, "the switch changing the command line")
        older = newest_snapshot() if BTRFS else None
        second = image_of("archix-current.conf")
        t.check(second != first, "a new command line should make a new image")
        m.reboot()
        boot()
        assert_unlocked(t, "a new image measuring differently", marker)

    if BTRFS:
        with t.step(f"4: snapshot {older}'s entry, an older image signed with the same key, unlocks it"):
            t.require(entry(f"archix-snapshot-{older}.conf"), f"snapshot {older} should have an entry: {entries()}")
            t.check(image_of(f"archix-snapshot-{older}.conf") == first, f"snapshot {older}'s entry should boot the first image: {image_of(f'archix-snapshot-{older}.conf')}")
            boot_entry(m, t, f"archix-snapshot-{older}.conf")
            t.check((trying() or {}).get("snapshot") == older, f"a try boot of snapshot {older}: {trying()}")
            assert_unlocked(t, f"snapshot {older}'s image", marker)
            m.reboot()
            boot()

    with t.step("5: rotate-pcr-key re-enrolls the volume and the next switch signs for the new key"):
        old = key_sum()
        m.succeed(f"set -o pipefail; PASSWORD={PASS} {HOST} secure-boot rotate-pcr-key {VOLUME} 2>&1 | tail -10", 600, what="archix secure-boot rotate-pcr-key")
        t.check(key_sum() != old, "the PCR public key should be new")
        t.check(out(f"cryptsetup luksDump {VOLUME} | grep -c systemd-tpm2").strip() == "1", f"re-enrolling should leave one TPM2 token: {out(f'cryptsetup luksDump {VOLUME} | grep -A3 Tokens:')}")
        switch(m, t, "the switch after rotating")
        rotated = image_of("archix-current.conf")
        t.check(rotated not in (first, second) and image_key(rotated) == key_sum(), f"the new image {rotated} should carry the new key")
        m.reboot()
        boot()
        assert_unlocked(t, "the image built with the new key", marker)

    if BTRFS:
        with t.step(f"6: snapshot {older}'s image, signed with the old key, no longer unlocks it"):
            boot_entry(m, t, f"archix-snapshot-{older}.conf", allowed=("systemd-cryptsetup@data.service",))
            assert_locked(t, f"snapshot {older}'s image, built with the old key")
