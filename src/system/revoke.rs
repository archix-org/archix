//! Puts the hashes of boot images archix signed and no longer uses into dbx, so the firmware refuses them, leaving every other dbx entry as it is.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};

use super::authenticode;
use crate::paths::Paths;

/// The owner GUID archix's dbx entries carry, e85b82c6-e8da-40eb-b968-e918df35dec1, which tells them from Microsoft's and anyone else's.
pub const OWNER: [u8; 16] = [
    0xc6, 0x82, 0x5b, 0xe8, 0xda, 0xe8, 0xeb, 0x40, 0xb9, 0x68, 0xe9, 0x18, 0xdf, 0x35, 0xde, 0xc1,
];
const DBX: &str = "/sys/firmware/efi/efivars/dbx-d719b2cb-3d3a-4596-a3bc-dad00e67656f";
/// Firmware keeps dbx in a small NVRAM store that Windows' own dbx updates need room in too, so archix adds at most this many entries.
const MAX_ENTRIES: usize = 128;
/// Variable space left free after a write.
const NVRAM_MARGIN: u64 = 64 << 10;
const KEK_DIRS: &[&str] = &["/var/lib/sbctl/keys/KEK", "/usr/share/secureboot/keys/KEK"];

/// A signed boot image archix recorded: its Authenticode hash in hex and the file it was.
#[derive(Debug, Clone, PartialEq)]
pub struct Signed {
    pub hash: String,
    pub file: String,
}

fn signed_list(paths: &Paths) -> PathBuf {
    paths.state.join("boot").join("signed")
}

fn revoked_list(paths: &Paths) -> PathBuf {
    paths.state.join("boot").join("revoked")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Remembers a boot image archix just signed, so it can be revoked once nothing uses it.
pub fn record_signed(paths: &Paths, image: &Path, file: &str) -> Result<()> {
    let bytes = fs::read(image).with_context(|| format!("reading {}", image.display()))?;
    let hash = hex(&authenticode::hash(&bytes)?);
    let list = signed_list(paths);
    let text = fs::read_to_string(&list).unwrap_or_default();
    if text.lines().any(|line| line.split(' ').next() == Some(hash.as_str())) {
        return Ok(());
    }
    crate::durable::write(&list, format!("{text}{hash} {file}\n").as_bytes())
}

fn signed(paths: &Paths) -> Vec<Signed> {
    fs::read_to_string(signed_list(paths))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.split_once(' '))
        .map(|(hash, file)| Signed {
            hash: hash.to_string(),
            file: file.to_string(),
        })
        .collect()
}

/// Whether archix has ever put an image in dbx, after which every image it builds gets a section of its own so none can come out identical to a revoked one.
pub fn any_revoked(paths: &Paths) -> bool {
    fs::metadata(revoked_list(paths)).is_ok_and(|meta| meta.len() > 0)
}

/// The signed images to revoke: ones no file on the ESP is any more and dbx doesn't list yet.
fn to_revoke(signed: &[Signed], on_esp: &BTreeSet<String>, in_dbx: &BTreeSet<String>) -> Vec<Signed> {
    let mut seen = BTreeSet::new();
    signed
        .iter()
        .filter(|image| !on_esp.contains(&image.hash) && !in_dbx.contains(&image.hash))
        .filter(|image| seen.insert(image.hash.clone()))
        .cloned()
        .collect()
}

/// The Authenticode hash of every EFI binary on the ESP, archix's or not.
fn on_esp(esp: &Path) -> BTreeSet<String> {
    let mut hashes = BTreeSet::new();
    let mut dirs = vec![esp.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                dirs.push(path);
            } else if path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("efi"))
                && let Ok(hash) = fs::read(&path)
                    .map_err(anyhow::Error::from)
                    .and_then(|bytes| authenticode::hash(&bytes))
            {
                hashes.insert(hex(&hash));
            }
        }
    }
    hashes
}

/// dbx's entries, without the variable's four attribute bytes.
fn dbx() -> Result<Vec<([u8; 16], [u8; 32])>> {
    let bytes = fs::read(DBX).with_context(|| format!("reading {DBX}; the machine has to have booted through UEFI"))?;
    Ok(authenticode::sha256_entries(bytes.get(4..).unwrap_or_default()))
}

/// Free variable space as the firmware reports it through efivarfs, if it does.
fn nvram_free() -> Option<u64> {
    let dir = std::ffi::CString::new("/sys/firmware/efi/efivars").ok()?;
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(dir.as_ptr(), &mut stat) } != 0 || stat.f_blocks == 0 {
        return None;
    }
    Some(stat.f_bavail as u64 * stat.f_bsize as u64)
}

/// sbctl's KEK key and certificate, when the key is a plain file archix can sign with.
fn kek() -> Result<(PathBuf, PathBuf)> {
    for dir in KEK_DIRS {
        let (key, cert) = (Path::new(dir).join("KEK.key"), Path::new(dir).join("KEK.pem"));
        if key.is_file() && cert.is_file() {
            let text = fs::read_to_string(&key).unwrap_or_default();
            if !text.contains("PRIVATE KEY-----") {
                bail!(
                    "sbctl's KEK at {} isn't a key file (it may live in a TPM or a YubiKey), so archix can't sign a dbx update with it",
                    key.display()
                );
            }
            return Ok((key, cert));
        }
    }
    bail!(
        "no sbctl KEK found in {}; dbx updates have to be signed with your KEK",
        KEK_DIRS.join(" or ")
    )
}

/// Adds the hashes of boot images archix signed and no longer uses to dbx, signed with sbctl's KEK and appended, so every other entry stays as it is.
pub fn revoke(paths: &Paths, dry_run: bool, automatic: bool, confirm: &dyn Fn(&str) -> Result<bool>) -> Result<()> {
    let esp = fs::read_to_string(paths.state.join("boot").join("esp"))
        .context("archix hasn't written boot entries on this machine")?;
    let esp = PathBuf::from(esp.trim());
    let current = match dbx() {
        Ok(entries) => entries,
        Err(e) if dry_run => {
            println!("note: {e:#}; listing as if dbx were empty");
            Vec::new()
        }
        Err(e) => return Err(e),
    };
    let in_dbx: BTreeSet<String> = current.iter().map(|(_, hash)| hex(hash)).collect();
    let ours = current.iter().filter(|(owner, _)| *owner == OWNER).count();
    let images = to_revoke(&signed(paths), &on_esp(&esp), &in_dbx);
    if images.is_empty() {
        if !automatic {
            println!("nothing to revoke: every boot image archix signed is still on the ESP or already in dbx");
        }
        return Ok(());
    }
    println!(
        "{} archix signed and no longer uses:",
        count(images.len(), "boot image", "boot images")
    );
    for image in &images {
        println!("  {}  {}", &image.hash[..16], image.file);
    }
    let size = 28 + 48 * images.len() as u64;
    println!(
        "dbx has {} entries, {ours} of them archix's; this adds {} ({size} bytes)",
        current.len(),
        images.len()
    );
    if let Some(warning) = room_for(ours, images.len())? {
        eprintln!("warning: {warning}");
    }
    if dry_run {
        return Ok(());
    }
    let free = nvram_free().context(
        "the firmware doesn't report how much variable space is free, so archix won't write dbx; nothing was changed",
    )?;
    if free < size + NVRAM_MARGIN {
        bail!(
            "the firmware has {free} bytes of variable space free, and archix keeps {NVRAM_MARGIN} free besides what it writes; nothing was changed. Firmware usually reclaims the space old copies of variables hold at a reboot, so this may pass after one. Until it does, {}",
            PAST_THE_CEILING
        );
    }
    let (key, cert) = kek()?;
    for tool in ["sign-efi-sig-list", "efi-updatevar"] {
        if !on_path(tool) {
            bail!("{tool} isn't installed; install efitools (pacman -S efitools); nothing was changed");
        }
    }
    if !confirm(&format!("add {} to dbx?", count(images.len(), "hash", "hashes")))? {
        bail!("aborted; nothing was changed");
    }
    let work = paths.state.join("revoke-work");
    let _ = fs::remove_dir_all(&work);
    fs::create_dir_all(&work)?;
    let hashes: Vec<[u8; 32]> = images
        .iter()
        .map(|image| {
            let bytes: Vec<u8> = (0..32)
                .map(|i| u8::from_str_radix(&image.hash[i * 2..i * 2 + 2], 16).unwrap_or(0))
                .collect();
            bytes.try_into().expect("thirty-two bytes")
        })
        .collect();
    let (esl, auth) = (work.join("revoke.esl"), work.join("revoke.auth"));
    fs::write(&esl, authenticode::sha256_list(&hashes, &OWNER))?;
    let backup = paths.state.join("boot").join(format!(
        "dbx-before-{}",
        SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
    ));
    fs::copy(DBX, &backup).with_context(|| format!("backing dbx up to {}", backup.display()))?;
    run(
        Command::new("sign-efi-sig-list")
            .arg("-a")
            .arg("-k")
            .arg(&key)
            .arg("-c")
            .arg(&cert)
            .arg("dbx")
            .arg(&esl)
            .arg(&auth),
        "sign-efi-sig-list",
    )?;
    // efivarfs marks existing variables immutable, and efi-updatevar doesn't lift that itself.
    let was_immutable = set_immutable(Path::new(DBX), false)?;
    let written = run(
        Command::new("efi-updatevar").arg("-a").arg("-f").arg(&auth).arg("dbx"),
        "efi-updatevar",
    );
    if was_immutable {
        let _ = set_immutable(Path::new(DBX), true);
    }
    if let Err(e) = written {
        let size = fs::metadata(&backup).map_or(0, |meta| meta.len().saturating_sub(4));
        if format!("{e:#}").contains("No space left") {
            bail!(
                "the firmware has no room for dbx to grow from {size} bytes, which can be the most one variable may hold, a limit Linux doesn't show; nothing was changed. From here on {PAST_THE_CEILING}"
            );
        }
        return Err(e);
    }
    let _ = fs::remove_dir_all(&work);
    let now: BTreeSet<String> = dbx()?.iter().map(|(_, hash)| hex(hash)).collect();
    let missing: Vec<&Signed> = images.iter().filter(|image| !now.contains(&image.hash)).collect();
    if !missing.is_empty() {
        bail!(
            "dbx doesn't list {} after the update; the dbx from before is saved in {}",
            missing.len(),
            backup.display()
        );
    }
    let mut revoked = fs::read_to_string(revoked_list(paths)).unwrap_or_default();
    for image in &images {
        revoked.push_str(&format!("{} {}\n", image.hash, image.file));
    }
    crate::durable::write(&revoked_list(paths), revoked.as_bytes())?;
    println!(
        "revoked {}; the firmware refuses them from now on (dbx from before: {})",
        count(images.len(), "boot image", "boot images"),
        backup.display()
    );
    Ok(())
}

/// From this many of its own entries in dbx on, archix warns that it nears its cap.
const WARN_ENTRIES: usize = MAX_ENTRIES * 3 / 4;

/// What a revocation archix can't write leaves, and the one way past it.
const PAST_THE_CEILING: &str = "the boot images archix stops using stay bootable for anyone who kept a copy, and only a new Secure Boot db key retires them all at once, as the firmware then trusts nothing the old key signed (https://archix.dev/docs/guides/secure-boot/#the-dbx-ceiling)";

/// Refuses adding `adding` entries to archix's `ours` in dbx past its cap, and warns once they reach three quarters of it.
fn room_for(ours: usize, adding: usize) -> Result<Option<String>> {
    let after = ours + adding;
    if after > MAX_ENTRIES {
        bail!(
            "that would make {after} of archix's entries in dbx, more than the {MAX_ENTRIES} it allows itself, since the firmware's variable store is small and Windows' dbx updates need room too; nothing was changed. From here on {PAST_THE_CEILING}"
        );
    }
    Ok((after >= WARN_ENTRIES).then(|| {
        format!(
            "archix will have {after} of the {MAX_ENTRIES} dbx entries it allows itself; past that it revokes nothing more (https://archix.dev/docs/guides/secure-boot/#the-dbx-ceiling)"
        )
    }))
}

/// Sets or clears a file's immutable flag, returning whether it was set.
fn set_immutable(path: &Path, on: bool) -> Result<bool> {
    use std::os::fd::AsRawFd;
    const IMMUTABLE: libc::c_int = 0x10;
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e).with_context(|| format!("opening {}", path.display())),
    };
    let mut flags: libc::c_int = 0;
    if unsafe { libc::ioctl(file.as_raw_fd(), libc::FS_IOC_GETFLAGS, &mut flags) } != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("reading the flags of {}", path.display()));
    }
    let was = flags & IMMUTABLE != 0;
    let wanted = if on { flags | IMMUTABLE } else { flags & !IMMUTABLE };
    if wanted != flags && unsafe { libc::ioctl(file.as_raw_fd(), libc::FS_IOC_SETFLAGS, &wanted) } != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("changing the flags of {}", path.display()));
    }
    Ok(was)
}

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
}

fn count(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn run(cmd: &mut Command, what: &str) -> Result<()> {
    let out = cmd.output().with_context(|| format!("running {what}"))?;
    if !out.status.success() {
        bail!("{what} failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signed(hash: &str, file: &str) -> Signed {
        Signed {
            hash: hash.into(),
            file: file.into(),
        }
    }

    #[test]
    fn only_images_off_the_esp_and_not_yet_in_dbx_are_revoked() {
        let all = [
            signed("aa", "7.2.8-arch1-1-aaaa.efi"),
            signed("bb", "7.2.9-arch1-1-bbbb.efi"),
            signed("cc", "7.2.9-arch1-1-rescue-cccc.efi"),
            signed("aa", "7.2.8-arch1-1-aaaa.efi"),
            signed("dd", "7.2.9-arch1-1-dddd.efi"),
        ];
        let on_esp = BTreeSet::from(["bb".to_string()]);
        let in_dbx = BTreeSet::from(["cc".to_string()]);
        assert_eq!(
            to_revoke(&all, &on_esp, &in_dbx),
            [
                signed("aa", "7.2.8-arch1-1-aaaa.efi"),
                signed("dd", "7.2.9-arch1-1-dddd.efi")
            ]
        );
    }

    #[test]
    fn archix_warns_from_three_quarters_of_its_dbx_cap_and_refuses_past_it() {
        assert_eq!(room_for(0, 1).unwrap(), None);
        assert_eq!(room_for(94, 1).unwrap(), None);
        let near = room_for(95, 1).unwrap().unwrap();
        assert!(near.contains("96 of the 128"), "{near}");
        assert!(room_for(127, 1).unwrap().is_some());
        let refused = format!("{:#}", room_for(127, 2).unwrap_err());
        assert!(
            refused.contains("that would make 129 of archix's entries in dbx, more than the 128 it allows itself")
                && refused.contains("nothing was changed")
                && refused.contains("new Secure Boot db key"),
            "{refused}"
        );
    }

    #[test]
    fn archix_entries_carry_archix_owner_guid() {
        assert_eq!(
            OWNER,
            [
                0xc6, 0x82, 0x5b, 0xe8, 0xda, 0xe8, 0xeb, 0x40, 0xb9, 0x68, 0xe9, 0x18, 0xdf, 0x35, 0xde, 0xc1
            ]
        );
    }
}
