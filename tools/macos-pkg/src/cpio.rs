//! The `Payload` half of a flat package: a cpio archive, gzipped.
//!
//! `pkgbuild` writes the payload as a cpio archive in the POSIX `odc` format
//! and gzips it, and the macOS Installer reads nothing else. The format is six
//! octal fields and a name per entry, which is why this is a module here rather
//! than a dependency: the entire specification is the `write_entry` function
//! below, and a crate would be a larger thing to trust for the same bytes.
//!
//! The one property worth stating, because it is not obvious from the format:
//! **every path in the payload is relative and prefixed with `./`**, including
//! the directories leading to a file. The Installer lays the payload down by
//! walking those entries against the package's install location, so a payload
//! that names `usr/local/bin/inillucent` without also naming `./usr`,
//! `./usr/local` and `./usr/local/bin` installs a file into directories whose
//! ownership and mode nobody set.

use std::io::Write;

/// The mode bits that say "regular file" in a cpio header.
const S_IFREG: u32 = 0o100_000;
/// The mode bits that say "directory" in a cpio header.
const S_IFDIR: u32 = 0o040_000;

/// One entry in the payload, in the order the Installer will read it.
pub struct CpioEntry {
    /// The archive path, without the leading `./` this module adds.
    pub path: String,
    /// True when the entry is a directory rather than a file.
    pub is_directory: bool,
    /// The permission bits, without the file type bits.
    pub mode: u32,
    /// The file's bytes. Empty for a directory.
    pub data: Vec<u8>,
}

/// Writes one fixed-width octal field of a cpio header.
///
/// @param out - the archive being written
/// @param value - the number to write
/// @param width - how many octal digits the field has
fn write_octal(out: &mut Vec<u8>, value: u64, width: usize) {
    let text = format!("{:0width$o}", value, width = width);
    // A value too large for its field would silently shift every field after
    // it, producing an archive that reads as garbage rather than as an error.
    // Truncating from the left keeps the header the right length and is what
    // GNU cpio does; the sizes involved here are three orders of magnitude
    // below the limits, so this is a guard rather than a behaviour.
    let bytes = text.as_bytes();
    out.extend_from_slice(&bytes[bytes.len() - width..]);
}

/// Appends one entry to an in-progress cpio archive.
///
/// @param out - the archive being written
/// @param entry - the file or directory to add
/// @param inode - a number unique within this archive
/// @param mtime - the modification time every entry is given
fn write_entry(out: &mut Vec<u8>, entry: &CpioEntry, inode: u64, mtime: u64) {
    // The payload's own root is the entry named `.`, and everything else hangs
    // off it as `./…`. An empty path is that root.
    let name = if entry.path.is_empty() {
        ".".to_string()
    } else {
        format!("./{}", entry.path)
    };
    let name_bytes = name.as_bytes();
    let type_bits = if entry.is_directory { S_IFDIR } else { S_IFREG };
    let links = if entry.is_directory { 2 } else { 1 };

    out.extend_from_slice(b"070707");
    write_octal(out, 0, 6); // dev
    write_octal(out, inode, 6);
    write_octal(out, u64::from(type_bits | (entry.mode & 0o7777)), 6);
    write_octal(out, 0, 6); // uid: root, because the package installs into /usr/local
    write_octal(out, 0, 6); // gid: wheel
    write_octal(out, links, 6);
    write_octal(out, 0, 6); // rdev
    write_octal(out, mtime, 11);
    write_octal(out, (name_bytes.len() + 1) as u64, 6);
    write_octal(out, entry.data.len() as u64, 11);
    out.extend_from_slice(name_bytes);
    out.push(0);
    out.extend_from_slice(&entry.data);
}

/// Builds the complete cpio archive for a payload.
///
/// The trailer entry is what tells a reader the archive ended; without it
/// `cpio` reports a truncated archive and the Installer reports nothing at all.
///
/// @param entries - every file and directory, parents before children
/// @param mtime - the modification time every entry is given
pub fn build(entries: &[CpioEntry], mtime: u64) -> Vec<u8> {
    let mut out = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        write_entry(&mut out, entry, index as u64 + 1, mtime);
    }
    // The trailer's name is the literal string below and it is not a `./`
    // path, so it is written here rather than through `write_entry`.
    out.extend_from_slice(b"070707");
    write_octal(&mut out, 0, 6);
    write_octal(&mut out, 0, 6);
    write_octal(&mut out, 0, 6);
    write_octal(&mut out, 0, 6);
    write_octal(&mut out, 0, 6);
    write_octal(&mut out, 1, 6);
    write_octal(&mut out, 0, 6);
    write_octal(&mut out, 0, 11);
    write_octal(&mut out, 11, 6);
    write_octal(&mut out, 0, 11);
    out.extend_from_slice(b"TRAILER!!!\0");
    out
}

/// Gzips a built payload.
///
/// @param data - the cpio archive
pub fn gzip(data: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(data)?;
    encoder.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every header is 76 bytes of ASCII octal, so an archive of one empty file
    /// has a size this test can state rather than approximate.
    #[test]
    fn a_header_is_seventy_six_bytes_plus_the_name() {
        let entries = vec![CpioEntry {
            path: "a".to_string(),
            is_directory: false,
            mode: 0o644,
            data: b"hello".to_vec(),
        }];
        let archive = build(&entries, 0);
        // 76 header + "./a\0" (4) + 5 data, then the trailer: 76 + 11.
        assert_eq!(archive.len(), 76 + 4 + 5 + 76 + 11);
        assert!(archive.starts_with(b"070707"));
        assert!(archive.ends_with(b"TRAILER!!!\0"));
    }

    /// The type bits are what tell the Installer to make a directory rather
    /// than an empty file, and they are the field most easily lost.
    #[test]
    fn a_directory_carries_the_directory_type_bits() {
        let entries = vec![CpioEntry {
            path: "usr".to_string(),
            is_directory: true,
            mode: 0o755,
            data: Vec::new(),
        }];
        let archive = build(&entries, 0);
        let mode = std::str::from_utf8(&archive[18..24]).expect("ascii octal");
        assert_eq!(mode, "040755");
    }
}
