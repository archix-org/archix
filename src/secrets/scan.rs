//! The switch's last check before its commit: no decrypted value may be in a file archix writes, a UKI or initramfs (compressed streams opened), or anywhere in the new deployment's /etc.

use std::fs;
use std::io::Read;
use std::path::Path;

use anyhow::{Result, bail};

use super::memory::{SecretBuf, contains};

/// Values shorter than this are too likely to occur by chance to be looked for.
pub const MIN_SCANNED: usize = 8;
/// Values this long found anywhere are a hard failure; shorter ones only in managed files and boot images.
pub const MIN_CERTAIN: usize = 16;
/// The most of one compressed stream inside a boot image that's read, as bytes that only look like a stream's start can unpack to far more.
const MAX_UNPACKED: u64 = 1 << 30;
/// The most places in one file that are tried as a stream's start.
const MAX_STARTS: usize = 4096;

/// A value to look for, by the secret's name.
pub struct Needle<'a> {
    pub name: &'a str,
    pub value: &'a SecretBuf,
}

/// Where a value turned up.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub secret: String,
    pub path: String,
    /// Whether the value is shorter than MIN_CERTAIN.
    pub short: bool,
}

/// Reads `reader` in chunks, up to MAX_UNPACKED, marking in `found` each of `needles` it holds, across chunk boundaries too.
fn search(mut reader: impl Read, needles: &[&[u8]], found: &mut [bool]) {
    let overlap = needles.iter().map(|n| n.len()).max().unwrap_or(1).saturating_sub(1);
    let mut buffer = vec![0u8; (1 << 20) + overlap];
    let (mut kept, mut total) = (0, 0u64);
    while total < MAX_UNPACKED && found.iter().any(|f| !f) {
        let n = match reader.read(&mut buffer[kept..]) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        total += n as u64;
        let window = &buffer[..kept + n];
        for (needle, seen) in needles.iter().zip(found.iter_mut()) {
            *seen = *seen || contains(window, needle);
        }
        kept = overlap.min(window.len());
        let end = window.len();
        buffer.copy_within(end - kept..end, 0);
    }
}

fn external(program: &str, input: &[u8], needles: &[&[u8]], found: &mut [bool]) {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let Ok(mut child) = Command::new(program)
        .args(["-dc", "-q"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return;
    };
    let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        return;
    };
    let data = input.to_vec();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&data);
    });
    search(stdout, needles, found);
    let _ = child.kill();
    let _ = writer.join();
    let _ = child.wait();
}

/// Which of `needles` are in a compressed stream (zstd, gzip, xz, lz4) that starts somewhere in `bytes`, as initramfs images and the UKIs holding them are made of; each stream is searched as it unpacks, never held whole.
pub fn in_streams(bytes: &[u8], needles: &[&[u8]]) -> Vec<bool> {
    const ZSTD: &[u8] = &[0x28, 0xb5, 0x2f, 0xfd];
    const GZIP: &[u8] = &[0x1f, 0x8b, 0x08];
    const XZ: &[u8] = &[0xfd, b'7', b'z', b'X', b'Z', 0x00];
    const LZ4: &[u8] = &[0x02, 0x21, 0x4c, 0x18];
    let mut found = vec![false; needles.len()];
    let mut starts = 0;
    for at in 0..bytes.len().saturating_sub(6) {
        if starts >= MAX_STARTS || found.iter().all(|f| *f) {
            break;
        }
        let rest = &bytes[at..];
        if rest.starts_with(ZSTD) {
            if let Ok(decoder) = zstd::stream::read::Decoder::new(rest) {
                search(decoder, needles, &mut found);
            }
        } else if rest.starts_with(GZIP) {
            search(flate2::read::MultiGzDecoder::new(rest), needles, &mut found);
        } else if rest.starts_with(XZ) {
            external("xz", rest, needles, &mut found);
        } else if rest.starts_with(LZ4) {
            external("lz4", rest, needles, &mut found);
        } else {
            continue;
        }
        starts += 1;
    }
    found
}

/// The values in `bytes` at `path`; `deep` opens compressed streams in it too.
pub fn blob(bytes: &[u8], path: &str, needles: &[Needle], deep: bool) -> Vec<Hit> {
    let looked: Vec<&Needle> = needles
        .iter()
        .filter(|needle| needle.value.len() >= MIN_SCANNED)
        .collect();
    let values: Vec<&[u8]> = looked.iter().map(|needle| needle.value.as_slice()).collect();
    let mut found: Vec<bool> = values.iter().map(|value| contains(bytes, value)).collect();
    if deep && found.iter().any(|f| !f) {
        for (seen, inside) in found.iter_mut().zip(in_streams(bytes, &values)) {
            *seen |= inside;
        }
    }
    looked
        .iter()
        .zip(found)
        .filter(|(_, seen)| *seen)
        .map(|(needle, _)| Hit {
            secret: needle.name.to_string(),
            path: path.to_string(),
            short: needle.value.len() < MIN_CERTAIN,
        })
        .collect()
}

/// Every hit under the tree at `dir`, shown as under `shown`; `deep` opens compressed streams, as for /boot.
pub fn tree(dir: &Path, shown: &str, needles: &[Needle], deep: bool) -> Vec<Hit> {
    let mut hits = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return hits;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = format!("{shown}/{}", entry.file_name().to_string_lossy());
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            hits.extend(tree(&path, &name, needles, deep));
        } else if meta.is_file() {
            if let Ok(bytes) = fs::read(&path) {
                hits.extend(blob(&bytes, &name, needles, deep));
            }
        } else if meta.file_type().is_symlink()
            && let Ok(target) = fs::read_link(&path)
        {
            hits.extend(blob(target.as_os_str().as_encoded_bytes(), &name, needles, false));
        }
    }
    hits
}

/// Fails on every hit in a place where any is certain (`strict`: managed files, boot images) or long enough to be no accident; returns warnings for the short ones elsewhere.
pub fn judge(hits: &[Hit], strict: bool) -> Result<Vec<String>> {
    let (fatal, warned): (Vec<&Hit>, Vec<&Hit>) = hits.iter().partition(|hit| strict || !hit.short);
    if !fatal.is_empty() {
        let shown: Vec<String> = fatal
            .iter()
            .map(|hit| format!("secret {} in {}", hit.secret, hit.path))
            .collect();
        bail!(
            "a secret's value is in plain text where archix would write it to disk: {}; nothing was changed. Take it out of the file or hook that puts it there (secrets reach files only as links into {})",
            shown.join(", "),
            super::RUN_DIR
        );
    }
    Ok(warned
        .iter()
        .map(|hit| {
            format!(
                "secret {}'s value, under {} bytes, also occurs in {}; that may be chance, but check that nothing copied it there, and prefer a longer secret",
                hit.secret, MIN_CERTAIN, hit.path
            )
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn a_value_is_found_raw_and_inside_compressed_streams_and_short_ones_only_warn_outside_boot_images() {
        let long = SecretBuf::from_slice(b"0123456789abcdef-long").unwrap();
        let short = SecretBuf::from_slice(b"pin-1234").unwrap();
        let tiny = SecretBuf::from_slice(b"1234").unwrap();
        let needles = [
            Needle {
                name: "long",
                value: &long,
            },
            Needle {
                name: "short",
                value: &short,
            },
            Needle {
                name: "tiny",
                value: &tiny,
            },
        ];
        let mut cpio = vec![b'x'; 300];
        cpio.extend_from_slice(b"conf=0123456789abcdef-long pin-1234 1234");
        let packed = zstd::encode_all(&cpio[..], 3).unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&cpio).unwrap();
        let mut uki = b"MZ....pe header....".to_vec();
        uki.extend(gz.finish().unwrap());
        uki.extend(b"padding");
        assert!(
            !contains(&uki, b"0123456789abcdef-long"),
            "the test's values are only inside the stream"
        );
        assert_eq!(
            in_streams(&[b"x".as_slice(), &packed].concat(), &[b"pin-1234", b"absent"]),
            [true, false],
            "zstd opens too"
        );
        let mut long_stream = vec![b'y'; 3 << 20];
        long_stream.extend_from_slice(b"0123456789abcdef-long");
        let packed_long = zstd::encode_all(&long_stream[..], 3).unwrap();
        assert_eq!(
            in_streams(&packed_long, &[b"0123456789abcdef-long"]),
            [true],
            "found past the first chunk"
        );
        let found = blob(&uki, "/boot/archix/x.efi", &needles, true);
        assert_eq!(
            found.iter().map(|h| h.secret.as_str()).collect::<Vec<_>>(),
            ["long", "short"],
            "under 8 bytes isn't looked for"
        );
        assert!(blob(&uki, "/boot/archix/x.efi", &needles, false).is_empty());
        let error = judge(&found[1..], true).unwrap_err().to_string();
        assert!(
            error.contains("secret short in /boot/archix/x.efi") && !error.contains("pin-1234"),
            "{error}"
        );
        let warned = judge(&found[1..], false).unwrap();
        assert!(
            warned.len() == 1 && warned[0].contains("secret short") && !warned[0].contains("pin-1234"),
            "{warned:?}"
        );
        assert!(judge(&found, false).is_err(), "a long value is no accident anywhere");
    }
}
