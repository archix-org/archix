//! Puts the hashes of boot images archix signed and no longer uses into dbx, so the firmware refuses them, leaving every other dbx entry as it is.

use std::collections::BTreeSet;

/// The owner GUID archix's dbx entries carry, e85b82c6-e8da-40eb-b968-e918df35dec1, which tells them from Microsoft's and anyone else's.
pub const OWNER: [u8; 16] = [0; 16];

/// A signed boot image archix recorded: its Authenticode hash in hex and the file it was.
#[derive(Debug, Clone, PartialEq)]
pub struct Signed {
    pub hash: String,
    pub file: String,
}

/// The signed images to revoke: ones no file on the ESP is any more and dbx doesn't list yet.
fn to_revoke(_signed: &[Signed], _on_esp: &BTreeSet<String>, _in_dbx: &BTreeSet<String>) -> Vec<Signed> {
    Vec::new()
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
    fn archix_entries_carry_archix_owner_guid() {
        assert_eq!(
            OWNER,
            [
                0xc6, 0x82, 0x5b, 0xe8, 0xda, 0xe8, 0xeb, 0x40, 0xb9, 0x68, 0xe9, 0x18, 0xdf, 0x35, 0xde, 0xc1
            ]
        );
    }
}
