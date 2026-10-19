//! The Authenticode hash of a PE image, the value UEFI's db and dbx list for a binary: SHA-256 over the image without its checksum, certificate table entry and certificates.

use anyhow::Result;

/// The SHA-256 Authenticode hash of a PE32 or PE32+ image.
pub fn hash(_image: &[u8]) -> Result<[u8; 32]> {
    anyhow::bail!("not written yet")
}

/// An EFI_SIGNATURE_LIST of SHA-256 hashes, each owned by `owner`, as dbx stores them.
pub fn sha256_list(_hashes: &[[u8; 32]], _owner: &[u8; 16]) -> Vec<u8> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use sha2::{Digest, Sha256};

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
    }
}
