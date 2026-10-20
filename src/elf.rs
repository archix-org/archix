use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

const SHT_DYNAMIC: u32 = 6;
const DT_NULL: u64 = 0;
const DT_NEEDED: u64 = 1;

/// The shared libraries a 64-bit little-endian ELF file links (DT_NEEDED), reading only its headers; None for anything else.
pub fn needed(path: &Path) -> Option<Vec<String>> {
    let mut file = File::open(path).ok()?;
    let mut header = [0u8; 64];
    file.read_exact(&mut header).ok()?;
    if &header[..4] != b"\x7fELF" || header[4] != 2 || header[5] != 1 {
        return None;
    }
    let u16_at = |bytes: &[u8], at: usize| u16::from_le_bytes([bytes[at], bytes[at + 1]]);
    let u32_at = |bytes: &[u8], at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
    let u64_at = |bytes: &[u8], at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
    let (shoff, shentsize, shnum) = (u64_at(&header, 0x28), u16_at(&header, 0x3a), u16_at(&header, 0x3c));
    if shoff == 0 || shentsize < 64 || shnum == 0 || shnum > 4096 {
        return None;
    }
    let mut sections = vec![0u8; shentsize as usize * shnum as usize];
    file.seek(SeekFrom::Start(shoff)).ok()?;
    file.read_exact(&mut sections).ok()?;
    let section = |index: usize| &sections[index * shentsize as usize..(index + 1) * shentsize as usize];
    let dynamic = (0..shnum as usize).find(|&i| u32_at(section(i), 4) == SHT_DYNAMIC)?;
    let (dyn_offset, dyn_size, link) = (
        u64_at(section(dynamic), 0x18),
        u64_at(section(dynamic), 0x20),
        u32_at(section(dynamic), 0x28) as usize,
    );
    if link >= shnum as usize || dyn_size > 1 << 20 {
        return None;
    }
    let (str_offset, str_size) = (u64_at(section(link), 0x18), u64_at(section(link), 0x20));
    if str_size > 16 << 20 {
        return None;
    }
    let mut entries = vec![0u8; dyn_size as usize];
    file.seek(SeekFrom::Start(dyn_offset)).ok()?;
    file.read_exact(&mut entries).ok()?;
    let mut strings = vec![0u8; str_size as usize];
    file.seek(SeekFrom::Start(str_offset)).ok()?;
    file.read_exact(&mut strings).ok()?;
    let mut libraries = Vec::new();
    for entry in entries.as_chunks::<16>().0 {
        let (tag, value) = (u64_at(entry, 0), u64_at(entry, 8) as usize);
        if tag == DT_NULL {
            break;
        }
        if tag == DT_NEEDED && value < strings.len() {
            let end = strings[value..].iter().position(|b| *b == 0)? + value;
            libraries.push(String::from_utf8_lossy(&strings[value..end]).into_owned());
        }
    }
    Some(libraries)
}

#[cfg(test)]
mod tests {
    #[test]
    fn reads_the_libraries_a_binary_links() {
        let needed = super::needed(std::path::Path::new("/proc/self/exe")).expect("the test binary is ELF");
        assert!(needed.iter().any(|lib| lib.starts_with("libc.so")), "{needed:?}");
        assert_eq!(super::needed(std::path::Path::new("/etc/hostname")), None);
    }
}
