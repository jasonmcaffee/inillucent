//! The XAR container a flat package is delivered in.
//!
//! A `.pkg` is a XAR archive: a 28 byte header, a zlib compressed XML table of
//! contents, and a heap holding each file's compressed bytes. `apple-xar` reads
//! that format and does not write it, so this module is the writer.
//!
//! **Every archive this module writes is read back by `apple-xar` in the tests**
//! rather than by a second copy of the logic here. That matters more than usual
//! for a format writer: an offset that is wrong in both the writer and its own
//! reader is a self-consistent archive that no other program can open, and the
//! failure appears at the far end of a release, on somebody else's machine.
//!
//! The one piece of the layout that is not obvious: the first bytes of the heap
//! are the digest of the compressed table of contents, and the table of
//! contents itself says where that digest is. There is no circularity, because
//! the digest is computed after the table of contents is compressed and the
//! table of contents only records the offset and the size, never the value.

use {
    anyhow::Result,
    sha1::{Digest, Sha1},
    std::io::Write,
};

/// The bytes of the XAR magic, big endian.
const XAR_MAGIC: u32 = 0x7861_7221;
/// The size of the header, which is also the offset the table of contents starts at.
const HEADER_SIZE: u16 = 28;
/// The only format version Apple's tools write or read.
const XAR_VERSION: u16 = 1;
/// The checksum algorithm id for SHA-1 in the header.
///
/// **SHA-1 because that is what Apple writes, and the reader is Apple's.** The format allows
/// SHA-256, and this wrote SHA-256 first because a newer digest is the better choice everywhere
/// else. Apple's notary service answered `The contents of the package could not be extracted` and
/// `has no signed executables or bundles. No tickets can be generated`, and the real product
/// archive `node-v26.9.0.pkg` reads back `checksum_alg=1` with `<checksum style="sha1">` and
/// `sha1` on every file. Nothing here is relying on the digest for security: the signature over the archive is
/// CMS, and every Mach-O inside carries its own. This digest only says the table of contents was not
/// truncated.
const CHECKSUM_SHA1: u32 = 1;
/// The size of a SHA-1 digest, which is how much heap the checksum occupies.
const DIGEST_SIZE: u64 = 20;

/// One file or directory to place in the archive.
pub struct XarEntry {
    /// The path inside the archive, with `/` separators and no leading slash.
    pub path: String,
    /// True when the entry is a directory rather than a file.
    pub is_directory: bool,
    /// The permission bits, written to the table of contents as octal.
    pub mode: u32,
    /// The file's bytes. Empty for a directory.
    pub data: Vec<u8>,
    /// Whether to deflate the bytes into the heap.
    ///
    /// False for a payload that is already compressed. Apple stores `Payload` as
    /// `application/octet-stream` with its archived and extracted lengths equal, and compressing a
    /// gzip stream a second time buys nothing while making the archive differ from every one Apple's
    /// own tools produce.
    pub compress: bool,
}

/// What the layout pass worked out about one entry.
struct Placed {
    /// The entry's id in the table of contents, 1-based.
    id: u64,
    /// The compressed bytes, empty for a directory.
    archived: Vec<u8>,
    /// Where the compressed bytes start, relative to the heap.
    offset: u64,
    /// The digest of the bytes before compression.
    extracted_digest: String,
    /// The digest of the bytes after compression.
    archived_digest: String,
}

/// Compresses one file's bytes the way Apple's `xar` does.
///
/// The encoding is declared as `application/x-gzip` in the table of contents
/// and the bytes are a zlib stream, which is what Apple writes despite what the
/// name says.
///
/// @param data - the file's bytes
fn compress(data: &[u8]) -> Result<Vec<u8>> {
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(data)?;
    Ok(encoder.finish()?)
}

/// Hex encodes a SHA-1 digest of some bytes.
///
/// @param data - the bytes to digest
fn digest_hex(data: &[u8]) -> String {
    let mut hasher = Sha1::new();
    hasher.update(data);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Escapes the characters XML cannot carry literally in element text.
///
/// @param text - the text to place in an element
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Compresses every file and assigns it a place in the heap.
///
/// The heap begins with the table of contents digest, so the first file starts
/// at that digest's size rather than at zero.
///
/// @param entries - the archive's contents
fn place(entries: &[XarEntry]) -> Result<Vec<Placed>> {
    let mut placed = Vec::with_capacity(entries.len());
    let mut offset: u64 = DIGEST_SIZE;
    for (index, entry) in entries.iter().enumerate() {
        let archived = if entry.is_directory {
            Vec::new()
        } else if entry.compress {
            compress(&entry.data)?
        } else {
            entry.data.clone()
        };
        let length = archived.len() as u64;
        placed.push(Placed {
            id: index as u64 + 1,
            extracted_digest: digest_hex(&entry.data),
            archived_digest: digest_hex(&archived),
            archived,
            offset,
        });
        offset += length;
    }
    Ok(placed)
}

/// Finds the entries that sit directly inside one directory entry.
///
/// @param entries - every entry in the archive
/// @param index - the directory to look inside, or `usize::MAX` for the root
fn children_of(entries: &[XarEntry], index: usize) -> Vec<usize> {
    let prefix = if index == usize::MAX {
        String::new()
    } else {
        format!("{}/", entries[index].path)
    };
    entries
        .iter()
        .enumerate()
        .filter(|(_, candidate)| {
            candidate.path.len() > prefix.len()
                && candidate.path.starts_with(&prefix)
                && !candidate.path[prefix.len()..].contains('/')
        })
        .map(|(position, _)| position)
        .collect()
}

/// Writes the `data` element that tells a reader where a file's bytes are.
///
/// @param xml - the document being built
/// @param entry - the file being described
/// @param spot - where the layout pass put it
fn write_data_element(xml: &mut String, entry: &XarEntry, spot: &Placed) {
    xml.push_str(&format!(
        "<data><length>{}</length><offset>{}</offset><size>{}</size>\
         <encoding style=\"{}\"/>\
         <extracted-checksum style=\"sha1\">{}</extracted-checksum>\
         <archived-checksum style=\"sha1\">{}</archived-checksum></data>",
        spot.archived.len(),
        spot.offset,
        entry.data.len(),
        if entry.compress {
            "application/x-gzip"
        } else {
            "application/octet-stream"
        },
        spot.extracted_digest,
        spot.archived_digest
    ));
}

/// Writes the `file` element for one entry and, recursively, its children.
///
/// The element order matches what Apple's `xar` emits. A reader driven by a
/// schema does not care, but one that walks the document in order does, and a
/// release is not the place to find out which kind the Installer is.
///
/// @param xml - the document being built
/// @param entries - every entry in the archive
/// @param placed - the layout pass's result, indexed alongside `entries`
/// @param index - which entry to write
/// @param timestamp - the ISO 8601 time every entry is stamped with
fn write_file_element(
    xml: &mut String,
    entries: &[XarEntry],
    placed: &[Placed],
    index: usize,
    timestamp: &str,
) {
    let entry = &entries[index];
    let spot = &placed[index];
    let name = entry.path.rsplit('/').next().unwrap_or(entry.path.as_str());

    xml.push_str(&format!("<file id=\"{}\">", spot.id));
    if !entry.is_directory {
        write_data_element(xml, entry, spot);
    }
    xml.push_str(&format!(
        "<ctime>{timestamp}</ctime><mtime>{timestamp}</mtime><atime>{timestamp}</atime>\
         <group>wheel</group><gid>0</gid><user>root</user><uid>0</uid>\
         <mode>{:04o}</mode><deviceno>0</deviceno><inode>{}</inode><type>{}</type>\
         <name>{}</name>",
        entry.mode & 0o7777,
        spot.id,
        if entry.is_directory {
            "directory"
        } else {
            "file"
        },
        escape(name)
    ));

    for child in children_of(entries, index) {
        write_file_element(xml, entries, placed, child, timestamp);
    }
    xml.push_str("</file>");
}

/// Builds the table of contents XML for an archive.
///
/// @param entries - every entry in the archive
/// @param placed - the layout pass's result
/// @param timestamp - the ISO 8601 time the archive is stamped with
fn table_of_contents(entries: &[XarEntry], placed: &[Placed], timestamp: &str) -> String {
    let mut xml = String::new();
    xml.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<xar><toc>");
    xml.push_str(&format!("<creation-time>{timestamp}</creation-time>"));
    xml.push_str(&format!(
        "<checksum style=\"sha1\"><offset>0</offset><size>{DIGEST_SIZE}</size></checksum>"
    ));
    for index in children_of(entries, usize::MAX) {
        write_file_element(&mut xml, entries, placed, index, timestamp);
    }
    xml.push_str("</toc></xar>");
    xml
}

/// Writes a complete XAR archive.
///
/// @param entries - every file and directory, each parent listed before its children
/// @param timestamp - the ISO 8601 time the archive is stamped with
pub fn build(entries: &[XarEntry], timestamp: &str) -> Result<Vec<u8>> {
    let placed = place(entries)?;
    let toc = table_of_contents(entries, &placed, timestamp);
    let toc_compressed = compress(toc.as_bytes())?;

    let mut out = Vec::new();
    out.extend_from_slice(&XAR_MAGIC.to_be_bytes());
    out.extend_from_slice(&HEADER_SIZE.to_be_bytes());
    out.extend_from_slice(&XAR_VERSION.to_be_bytes());
    out.extend_from_slice(&(toc_compressed.len() as u64).to_be_bytes());
    out.extend_from_slice(&(toc.len() as u64).to_be_bytes());
    out.extend_from_slice(&CHECKSUM_SHA1.to_be_bytes());
    out.extend_from_slice(&toc_compressed);

    // The heap: the digest of the compressed table of contents, then every
    // file's compressed bytes in the order the layout pass placed them.
    let mut hasher = Sha1::new();
    hasher.update(&toc_compressed);
    out.extend_from_slice(&hasher.finalize());
    for spot in &placed {
        out.extend_from_slice(&spot.archived);
    }
    Ok(out)
}
