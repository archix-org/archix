//! The Authenticode hash of a PE image, the value UEFI's db and dbx list for a binary: SHA-256 over the image without its checksum, certificate table entry and certificates.

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

/// The SHA-256 Authenticode hash of a PE32 or PE32+ image.
pub fn hash(image: &[u8]) -> Result<[u8; 32]> {
    let u16_at = |at: usize| image.get(at..at + 2).map(|b| u16::from_le_bytes([b[0], b[1]]) as usize);
    let u32_at = |at: usize| {
        image
            .get(at..at + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
    };
    let pe = u32_at(0x3c)
        .filter(|_| image.starts_with(b"MZ"))
        .context("not a PE image")?;
    if image.get(pe..pe + 4) != Some(b"PE\0\0".as_slice()) {
        bail!("not a PE image");
    }
    let coff = pe + 4;
    let sections = u16_at(coff + 2).context("truncated PE header")?;
    let optional = coff + 20;
    let optional_size = u16_at(coff + 16).context("truncated PE header")?;
    let directories = match u16_at(optional) {
        Some(0x20b) => optional + 112,
        Some(0x10b) => optional + 96,
        _ => bail!("not a PE32 or PE32+ image"),
    };
    let checksum = optional + 64;
    let certificate_entry = directories + 4 * 8;
    let headers = u32_at(optional + 60).context("truncated PE header")?;
    let certificates_size = u32_at(certificate_entry + 4).context("truncated PE header")?;
    if headers > image.len() || certificate_entry + 8 > headers {
        bail!("malformed PE headers");
    }
    let mut sha = Sha256::new();
    sha.update(&image[..checksum]);
    sha.update(&image[checksum + 4..certificate_entry]);
    sha.update(&image[certificate_entry + 8..headers]);
    let table = optional + optional_size;
    let mut raw: Vec<(usize, usize)> = (0..sections)
        .map(|i| {
            let header = table + i * 40;
            Ok((
                u32_at(header + 20).context("truncated section table")?,
                u32_at(header + 16).context("truncated section table")?,
            ))
        })
        .collect::<Result<_>>()?;
    raw.sort_unstable();
    let mut hashed = headers;
    for (start, size) in raw.into_iter().filter(|(_, size)| *size > 0) {
        let data = image
            .get(start..start + size)
            .context("a section runs past the end of the image")?;
        sha.update(data);
        hashed += size;
    }
    let end = image.len().saturating_sub(certificates_size);
    if end > hashed {
        sha.update(&image[hashed..end]);
    }
    Ok(sha.finalize().into())
}

/// EFI_CERT_SHA256_GUID as it's stored.
const SHA256_TYPE: [u8; 16] = [
    0x26, 0x16, 0xc4, 0xc1, 0x4c, 0x50, 0x92, 0x40, 0xac, 0xa9, 0x41, 0xf9, 0x36, 0x93, 0x43, 0x28,
];

/// An EFI_SIGNATURE_LIST of SHA-256 hashes, each owned by `owner`, as dbx stores them.
pub fn sha256_list(hashes: &[[u8; 32]], owner: &[u8; 16]) -> Vec<u8> {
    let mut list = SHA256_TYPE.to_vec();
    list.extend(((28 + hashes.len() * 48) as u32).to_le_bytes());
    list.extend(0u32.to_le_bytes());
    list.extend(48u32.to_le_bytes());
    for hash in hashes {
        list.extend(owner);
        list.extend(hash);
    }
    list
}

/// The SHA-256 hashes in a variable's signature lists, such as dbx's contents, with each one's owner.
pub fn sha256_entries(variable: &[u8]) -> Vec<([u8; 16], [u8; 32])> {
    let mut entries = Vec::new();
    let mut at = 0;
    while at + 28 <= variable.len() {
        let u32_at = |at: usize| u32::from_le_bytes(variable[at..at + 4].try_into().expect("four bytes")) as usize;
        let (list_size, header_size, entry_size) = (u32_at(at + 16), u32_at(at + 20), u32_at(at + 24));
        if list_size < 28 || at + list_size > variable.len() || entry_size == 0 {
            break;
        }
        if variable[at..at + 16] == SHA256_TYPE && entry_size == 48 {
            let mut entry = at + 28 + header_size;
            while entry + 48 <= at + list_size {
                let owner = variable[entry..entry + 16].try_into().expect("sixteen bytes");
                let hash = variable[entry + 16..entry + 48].try_into().expect("thirty-two bytes");
                entries.push((owner, hash));
                entry += 48;
            }
        }
        at += list_size;
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A PE32+ image: headers up to 0x200, two sections listed out of file order, trailing data, then a certificate table.
    fn image() -> Vec<u8> {
        let mut image = vec![0u8; 0x400];
        image[0..2].copy_from_slice(b"MZ");
        image[0x3c..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        image[0x80..0x84].copy_from_slice(b"PE\0\0");
        let coff = 0x84;
        image[coff + 2..coff + 4].copy_from_slice(&2u16.to_le_bytes());
        image[coff + 16..coff + 18].copy_from_slice(&240u16.to_le_bytes());
        let opt = coff + 20;
        image[opt..opt + 2].copy_from_slice(&0x20bu16.to_le_bytes());
        image[opt + 60..opt + 64].copy_from_slice(&0x200u32.to_le_bytes());
        image[opt + 64..opt + 68].copy_from_slice(&0xdeadbeefu32.to_le_bytes());
        let sections = opt + 240;
        for (i, (raw, size)) in [(0x300u32, 0x100u32), (0x200, 0x100)].iter().enumerate() {
            let header = sections + i * 40;
            image[header + 16..header + 20].copy_from_slice(&size.to_le_bytes());
            image[header + 20..header + 24].copy_from_slice(&raw.to_le_bytes());
        }
        for (i, byte) in image.iter_mut().enumerate().skip(0x200) {
            *byte = (i % 251) as u8;
        }
        image.extend_from_slice(b"trailing");
        let certificates = image.len() as u32;
        image.extend_from_slice(&[7u8; 24]);
        let directory = opt + 112 + 4 * 8;
        image[directory..directory + 4].copy_from_slice(&certificates.to_le_bytes());
        image[directory + 4..directory + 8].copy_from_slice(&24u32.to_le_bytes());
        image
    }

    #[test]
    fn the_hash_skips_the_checksum_the_certificate_entry_and_the_certificates() {
        let image = image();
        let opt = 0x84 + 20;
        let directory = opt + 112 + 4 * 8;
        let mut expected = Sha256::new();
        expected.update(&image[..opt + 64]);
        expected.update(&image[opt + 68..directory]);
        expected.update(&image[directory + 8..0x200]);
        expected.update(&image[0x200..0x300]);
        expected.update(&image[0x300..0x400]);
        expected.update(b"trailing");
        assert_eq!(hash(&image).unwrap(), <[u8; 32]>::from(expected.finalize()));
        let mut resigned = image.clone();
        resigned[opt + 64] ^= 0xff;
        let end = resigned.len();
        resigned[end - 1] ^= 0xff;
        assert_eq!(
            hash(&resigned).unwrap(),
            hash(&image).unwrap(),
            "a new checksum or signature doesn't change it"
        );
        let mut changed = image.clone();
        changed[0x250] ^= 0xff;
        assert_ne!(hash(&changed).unwrap(), hash(&image).unwrap());
        assert!(hash(b"not a PE").is_err());
    }

    #[test]
    fn a_signature_list_holds_each_hash_with_its_owner() {
        let owner = [9u8; 16];
        let list = sha256_list(&[[1u8; 32], [2u8; 32]], &owner);
        let sha256_type = [
            0x26, 0x16, 0xc4, 0xc1, 0x4c, 0x50, 0x92, 0x40, 0xac, 0xa9, 0x41, 0xf9, 0x36, 0x93, 0x43, 0x28,
        ];
        assert_eq!(&list[..16], &sha256_type);
        assert_eq!(u32::from_le_bytes(list[16..20].try_into().unwrap()), 28 + 2 * 48);
        assert_eq!(u32::from_le_bytes(list[20..24].try_into().unwrap()), 0);
        assert_eq!(u32::from_le_bytes(list[24..28].try_into().unwrap()), 48);
        assert_eq!(&list[28..44], &owner);
        assert_eq!(&list[44..76], &[1u8; 32]);
        assert_eq!(&list[92..124], &[2u8; 32]);
        assert_eq!(list.len(), 124);
        let mut variable = sha256_list(&[[3u8; 32]], &[5u8; 16]);
        variable.extend(&list);
        assert_eq!(
            sha256_entries(&variable),
            [([5u8; 16], [3u8; 32]), (owner, [1u8; 32]), (owner, [2u8; 32])]
        );
    }
}
