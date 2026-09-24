//! The `Bom`, which is the list the macOS Installer records a receipt from.
//!
//! **Why this is written here rather than called.** `apple-bom` has a
//! `BomBuilder` and it is the obvious thing to use. It does not work, and it
//! cannot have been run by anybody: `build_bom` writes the root record as
//! `CString::new(b".\0")`, and `CString::new` rejects a string that already
//! ends in a NUL, so the first call panics at that line before a byte is
//! produced. Fixing that reveals a second fault — it stores the whole path in
//! each `BomBlockFile`, while the reader in the same crate builds a path by
//! walking `parent_path_id` and joining the names, so every path would come
//! back doubled, and a third where every variable's block index is written one
//! past the block it names, so `Paths` resolves to a paths block rather than to
//! the tree the reader expects. The crate's own documentation says "writing
//! support is still a work in progress" and this is what that means.
//!
//! So the block assembly below is adapted from that builder — `apple-bom`
//! 0.3.0, `src/builder.rs`, by Gregory Szorc, MIT or Apache-2.0 — with the root
//! record's string fixed and each record carrying only its own leaf name. The
//! part that would be expensive to redo, the binary layout of every block type,
//! is still `apple_bom::format`, which is the read path and is exercised.
//!
//! The test at the bottom of this file reads every BOM it writes back with
//! `apple_bom::ParsedBom` and compares the paths, modes, sizes and checksums to
//! what went in. That reader is the one part of this that was already known to
//! work against BOMs Apple produced, so it is the check with something behind
//! it rather than a second copy of the same assumptions.
//!
//! **That reader is not enough on its own, and 0.1.8 is the proof.** It never
//! reads the free list after the block table, never looks at the header's
//! block count, and walks the paths leaves without caring what order they are
//! in. macOS's Bom framework does all three, so a BOM `ParsedBom` accepted took
//! Installer.app down with `abort()` in `_ReadFreeList`. The other tests below
//! check the layout against `python-applications.bom`, the Apple produced BOM
//! in `apple-bom`'s test data, and against `mkbom`: the free list and the
//! header count, one leaf directly under the `Paths` tree, leaf entries in
//! (parent path id, name) order, and a `VIndex` tree with a block size of 128.

use {
    crate::vars::{self, Variable},
    anyhow::{Context, Result},
    apple_bom::{
        format::{
            BomBlock, BomBlockBomInfo, BomBlockFile, BomBlockPathInfoIndex, BomBlockPathRecord,
            BomBlockPathRecordPointer, BomBlockPaths, BomBlockTree, BomBlockTreePointer,
            BomBlockVIndex, BomBlocksEntry, BomBlocksIndex, BomHeader, BomInfoEntry, BomPathsEntry,
        },
        path::BomPathType,
    },
    scroll::IOwrite,
    std::{
        borrow::Cow,
        collections::{BTreeMap, VecDeque},
        ffi::CString,
        io::{Cursor, Write},
    },
};

/// The file offset block data starts at, which leaves room for a header whose
/// contents are not known until the blocks have been written.
const BLOCK_DATA_FILE_OFFSET: u32 = 512;
/// Where the variables index sits, between the header and the block data.
const VARS_INDEX_OFFSET: u32 = 128;
/// The size Apple's tooling gives a paths block, which decides how many entries
/// fit in one before another is started.
const PATHS_BLOCK_SIZE: u32 = 4096;
/// The block size of the tree the `VIndex` variable points at, in Apple's BOMs
/// and in `mkbom`.
const VINDEX_BLOCK_SIZE: u32 = 128;
/// The free list that follows the block table: a count of zero, then the two
/// zeroed pointers Apple's BOMs and `mkbom` both end the index with.
///
/// `apple-bom` stops reading at the end of the block table and never looks for
/// a free list, so a BOM without one reads back cleanly there. macOS does look
/// for it. `BOMStorageOpenInRAM` calls `_ReadFreeList` straight after the block
/// table, and when the file ends there the read of its count fails and Bom
/// calls `abort()`. That is how the 0.1.8 package took Installer.app down on
/// every Mac it was opened on, with a crash in `BOMStreamReadUInt32`, while
/// notarisation, which does not open the BOM, accepted it.
const EMPTY_FREE_LIST: [u8; 20] = [0; 20];
/// The mode bits that say "regular file".
const S_IFREG: u16 = 0o100_000;
/// The mode bits that say "directory".
const S_IFDIR: u16 = 0o040_000;

/// One file to record in the BOM. Directories are derived from these paths.
pub struct BomFile {
    /// The path relative to the install root, with no leading `./`.
    pub path: String,
    /// The permission bits, without the file type bits.
    pub mode: u32,
    /// The file's size in bytes.
    pub size: u32,
    /// The CRC-32 of the file's contents, which is what the BOM records.
    pub crc32: u32,
}

/// A path record and the name record that goes with it, before block assembly.
type Record<'a> = (u32, BomBlockPathRecord<'a>, BomBlockFile<'a>);

/// Wraps a leaf name as the NUL terminated string a `BomBlockFile` holds.
///
/// @param name - the leaf name, with no separators in it
fn c_name(name: &str) -> Result<CString> {
    CString::new(name.as_bytes().to_vec()).with_context(|| format!("{name} cannot be a C string"))
}

/// Builds the record for one directory.
///
/// @param mode - the directory's mode, including the type bits
/// @param mtime - the modification time every record is given
fn directory_record<'a>(mode: u16, mtime: u32) -> BomBlockPathRecord<'a> {
    BomBlockPathRecord {
        path_type: BomPathType::Directory.into(),
        a: 1,
        architecture: 15,
        mode,
        user: 0,
        group: 0,
        mtime,
        size: 0,
        b: 1,
        checksum_or_type: 0,
        link_name_length: 0,
        link_name: None,
    }
}

/// Builds the record for one file.
///
/// @param file - the file being recorded
/// @param mtime - the modification time every record is given
fn file_record<'a>(file: &BomFile, mtime: u32) -> BomBlockPathRecord<'a> {
    BomBlockPathRecord {
        path_type: BomPathType::File.into(),
        a: 1,
        architecture: 15,
        mode: S_IFREG | (file.mode as u16 & 0o7777),
        user: 0,
        group: 0,
        mtime,
        size: file.size,
        b: 1,
        checksum_or_type: file.crc32,
        link_name_length: 0,
        link_name: None,
    }
}

/// One directory or file in the payload, with its children sorted by name.
#[derive(Default)]
struct Node<'f> {
    /// The file this node records, or None for a directory.
    file: Option<&'f BomFile>,
    /// The entries inside a directory, in byte order of their names.
    children: BTreeMap<String, Node<'f>>,
}

/// Builds the directory tree the file paths describe.
///
/// @param files - the files to record
fn build_tree(files: &[BomFile]) -> Node<'_> {
    let mut root = Node::default();
    for file in files {
        let mut node = &mut root;
        for part in file.path.split('/') {
            node = node.children.entry(part.to_string()).or_default();
        }
        node.file = Some(file);
    }
    root
}

/// Turns the file list into the path records the BOM is built from.
///
/// Every directory on the way to a file gets a record of its own, and each
/// record carries only its own leaf name — the reader rebuilds the full path by
/// following `parent_path_id` upwards.
///
/// **The order is a B-tree's order, not the order the files arrived in.** The
/// `Paths` tree is keyed by (parent path id, name), and every BOM Apple writes
/// has its leaf entries sorted by that key. Walking the tree breadth first, with
/// each directory's children in name order and ids handed out as records are
/// emitted, produces exactly that order. `mkbom` does the same. The 0.1.8
/// package walked depth first in file order, which put `(3, include)` after
/// `(4, inillucent)`.
///
/// @param files - the files to record
/// @param directory_mode - the mode every derived directory is given
/// @param mtime - the modification time every record is given
fn build_records(files: &[BomFile], directory_mode: u16, mtime: u32) -> Result<Vec<Record<'_>>> {
    let tree = build_tree(files);
    let mut records: Vec<Record> = Vec::with_capacity(files.len() + 1);

    // The root is always path id 1 and is the only record with no parent.
    records.push((
        1,
        BomBlockPathRecord {
            path_type: BomPathType::Directory.into(),
            a: 1,
            architecture: 1,
            ..Default::default()
        },
        BomBlockFile {
            parent_path_id: 0,
            name: Cow::from(c_name(".")?),
        },
    ));

    let mut queue: VecDeque<(u32, &Node)> = VecDeque::from([(1, &tree)]);
    while let Some((parent_id, directory)) = queue.pop_front() {
        for (name, node) in &directory.children {
            let path_id = records.len() as u32 + 1;
            records.push((
                path_id,
                match node.file {
                    Some(file) => file_record(file, mtime),
                    None => directory_record(directory_mode, mtime),
                },
                BomBlockFile {
                    parent_path_id: parent_id,
                    name: Cow::from(c_name(name)?),
                },
            ));
            queue.push_back((path_id, node));
        }
    }

    Ok(records)
}

/// Appends the four blocks Apple's tooling emits for each path record.
///
/// What they are for is not documented anywhere and nothing here reads them
/// back. They are written because a BOM without them differs from every BOM
/// Apple produces, and a release is a poor place to discover which consumer
/// cares.
///
/// @param blocks - the block list being assembled
fn push_per_record_blocks(blocks: &mut Vec<BomBlock>) {
    let path_record_indices = blocks
        .iter()
        .enumerate()
        .filter_map(|(index, block)| match block {
            BomBlock::PathRecord(_) => Some(index as u32),
            _ => None,
        })
        .collect::<Vec<_>>();

    for block_path_record_index in path_record_indices {
        let block_tree_index = blocks.len() as u32;
        blocks.push(BomBlock::Tree(BomBlockTree {
            block_paths_index: blocks.len() as u32 + 1,
            block_size: 64,
            ..Default::default()
        }));
        blocks.push(BomBlock::Paths(BomBlockPaths {
            is_path_info: 1,
            ..Default::default()
        }));
        blocks.push(BomBlock::PathRecordPointer(BomBlockPathRecordPointer {
            block_path_record_index,
        }));
        blocks.push(BomBlock::TreePointer(BomBlockTreePointer {
            block_tree_index,
        }));
    }
}

/// Splits the path entries across as many paths blocks as they need.
///
/// @param entries - one entry per path record
fn chunk_paths(entries: Vec<BomPathsEntry>) -> Vec<BomBlockPaths> {
    let mut chunks = Vec::new();
    let mut current = BomBlockPaths {
        is_path_info: 1,
        ..Default::default()
    };
    for entry in entries {
        current.count += 1;
        current.paths.push(entry);
        let remaining = PATHS_BLOCK_SIZE - 12 - 8 * current.count as u32;
        if remaining < 16 {
            chunks.push(current.clone());
            current = BomBlockPaths {
                is_path_info: 1,
                ..Default::default()
            };
        }
    }
    if current.count > 0 || chunks.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// Appends the tree and paths blocks that the `Paths` variable points at.
///
/// One leaf is the tree's child directly, as in every BOM Apple writes for a
/// package this size. More than one gets a branch block above them whose entry
/// for each leaf is keyed by that leaf's **last** file, which is how `mkbom`
/// keys them. The 0.1.8 package always wrote a branch, keyed by the first file.
///
/// @param blocks - the block list being assembled
/// @param variables - the variables being assembled
/// @param entries - one entry per path record, in key order
fn push_paths_blocks(
    blocks: &mut Vec<BomBlock>,
    variables: &mut Vec<Variable>,
    entries: Vec<BomPathsEntry>,
) -> Result<()> {
    let chunks = chunk_paths(entries);
    let branch = chunks.len() > 1;

    blocks.push(BomBlock::Tree(BomBlockTree {
        block_paths_index: blocks.len() as u32 + 1,
        block_size: PATHS_BLOCK_SIZE,
        path_count: chunks.iter().map(|chunk| chunk.count as u32).sum(),
        ..Default::default()
    }));
    variables.push(Variable {
        name: "Paths".to_string(),
        block_index: blocks.len() as u32 - 1,
    });

    if branch {
        let first_leaf = blocks.len() as u32 + 1;
        blocks.push(BomBlock::Paths(BomBlockPaths {
            is_path_info: 0,
            count: chunks.len() as u16,
            paths: chunks
                .iter()
                .enumerate()
                .map(|(index, chunk)| BomPathsEntry {
                    block_index: first_leaf + index as u32,
                    file_index: chunk.paths.last().map(|e| e.file_index).unwrap_or(0),
                })
                .collect(),
            ..Default::default()
        }));
    }

    for (index, chunk) in chunks.iter().enumerate() {
        blocks.push(BomBlock::Paths(BomBlockPaths {
            is_path_info: chunk.is_path_info,
            count: chunk.count,
            // Leaves are consecutive blocks, so a neighbour is one index away.
            next_paths_block_index: if index + 1 == chunks.len() {
                0
            } else {
                blocks.len() as u32 + 1
            },
            previous_paths_block_index: if index == 0 {
                0
            } else {
                blocks.len() as u32 - 1
            },
            paths: chunk.paths.clone(),
        }));
    }
    Ok(())
}

/// Appends an empty tree and its paths block, and names a variable for it.
///
/// @param blocks - the block list being assembled
/// @param variables - the variables being assembled
/// @param name - the variable's name, or None to add the tree without naming it
/// @param block_size - the tree's block size, which Apple sets to 128 for the VIndex tree
fn push_empty_tree(
    blocks: &mut Vec<BomBlock>,
    variables: &mut Vec<Variable>,
    name: Option<&str>,
    block_size: u32,
) -> Result<()> {
    blocks.push(BomBlock::Tree(BomBlockTree {
        block_paths_index: blocks.len() as u32 + 1,
        block_size,
        ..Default::default()
    }));
    if let Some(name) = name {
        variables.push(Variable {
            name: name.to_string(),
            block_index: blocks.len() as u32 - 1,
        });
    }
    blocks.push(BomBlock::Paths(BomBlockPaths {
        is_path_info: 1,
        ..Default::default()
    }));
    Ok(())
}

/// Appends the three variables that carry no paths but are always present.
///
/// `HLIndex` and `Size64` are a tree and an empty paths block each. `VIndex` is
/// a block of its own that points at a third such pair. None of them holds
/// anything for a package whose payload has no hard links and no file above
/// four gigabytes, and all three are written because every BOM Apple produces
/// has them.
///
/// @param blocks - the block list being assembled
/// @param variables - the variables being assembled
fn push_empty_variables(blocks: &mut Vec<BomBlock>, variables: &mut Vec<Variable>) -> Result<()> {
    push_empty_tree(blocks, variables, Some("HLIndex"), PATHS_BLOCK_SIZE)?;

    blocks.push(BomBlock::VIndex(BomBlockVIndex {
        a: 1,
        tree_block_index: blocks.len() as u32 + 1,
        b: 0,
        c: 0,
    }));
    variables.push(Variable {
        name: "VIndex".to_string(),
        block_index: blocks.len() as u32 - 1,
    });
    push_empty_tree(blocks, variables, None, VINDEX_BLOCK_SIZE)?;

    push_empty_tree(blocks, variables, Some("Size64"), PATHS_BLOCK_SIZE)
}

/// Serialises the assembled blocks into the BOM's on-disk layout.
///
/// @param blocks - every block, in the order they are indexed
/// @param variables - the named variables
fn serialise(blocks: &[BomBlock], variables: &[Variable]) -> Result<Vec<u8>> {
    let mut blocks_index = BomBlocksIndex::default();
    let mut blocks_writer = Cursor::new(Vec::<u8>::new());

    for block in blocks {
        let start = blocks_writer.position();
        block
            .write(&mut blocks_writer)
            .map_err(|e| anyhow::anyhow!("writing a BOM block: {e}"))?;
        let end = blocks_writer.position();
        blocks_index.count += 1;
        // The null block is the pointer (0, 0), not an empty block at the
        // data offset. Apple's BOMs all start the table that way.
        let entry = match block {
            BomBlock::Empty => BomBlocksEntry {
                file_offset: 0,
                length: 0,
            },
            _ => BomBlocksEntry {
                file_offset: BLOCK_DATA_FILE_OFFSET + start as u32,
                length: (end - start) as u32,
            },
        };
        blocks_index.blocks.push(entry);
    }

    let blocks_data = blocks_writer.into_inner();
    let vars_data = vars::to_vec(variables)?;
    let mut blocks_index_data = blocks_index
        .to_vec()
        .map_err(|e| anyhow::anyhow!("writing the blocks index: {e}"))?;
    blocks_index_data.extend_from_slice(&EMPTY_FREE_LIST);

    let blocks_index_offset =
        BLOCK_DATA_FILE_OFFSET + blocks_data.len() as u32 + (64 - blocks_data.len() % 64) as u32;

    let header = BomHeader {
        magic: *b"BOMStore",
        version: 1,
        // The count of blocks that are not the null block, as Apple writes it.
        number_of_blocks: non_null_block_count(blocks),
        blocks_index_offset,
        blocks_index_length: blocks_index_data.len() as u32,
        vars_index_offset: VARS_INDEX_OFFSET,
        vars_index_length: vars_data.len() as u32,
    };

    let mut writer = Cursor::new(Vec::<u8>::new());
    writer.iowrite_with(header, scroll::BE)?;
    pad_to(&mut writer, VARS_INDEX_OFFSET)?;
    writer.write_all(&vars_data)?;
    pad_to(&mut writer, BLOCK_DATA_FILE_OFFSET)?;
    writer.write_all(&blocks_data)?;
    pad_to(&mut writer, blocks_index_offset)?;
    writer.write_all(&blocks_index_data)?;

    Ok(writer.into_inner())
}

/// Counts the blocks that are not the null block at index 0.
///
/// @param blocks - every block, in the order they are indexed
fn non_null_block_count(blocks: &[BomBlock]) -> u32 {
    blocks
        .iter()
        .filter(|block| !matches!(block, BomBlock::Empty))
        .count() as u32
}

/// Writes NUL bytes until the writer reaches a given offset.
///
/// @param writer - the document being written
/// @param offset - the offset to reach
fn pad_to(writer: &mut Cursor<Vec<u8>>, offset: u32) -> Result<()> {
    while (writer.position() as u32) < offset {
        writer.write_all(b"\0")?;
    }
    Ok(())
}

/// Builds a complete BOM for a set of files.
///
/// @param files - the files the package installs
/// @param directory_mode - the mode every derived directory is given, without type bits
/// @param mtime - the modification time every record is given
pub fn build(files: &[BomFile], directory_mode: u32, mtime: u32) -> Result<Vec<u8>> {
    let records = build_records(files, S_IFDIR | (directory_mode as u16 & 0o7777), mtime)?;

    let mut blocks = vec![BomBlock::Empty];
    blocks.push(BomBlock::BomInfo(BomBlockBomInfo {
        version: 1,
        // One more than the records, for the null path Apple's tooling counts.
        number_of_paths: records.len() as u32 + 1,
        number_of_info_entries: 3,
        // These three entries are what Apple's tooling writes. What they mean
        // is not documented and nothing reads them back.
        entries: vec![
            BomInfoEntry {
                a: 0,
                b: 0,
                c: 8546296,
                d: 0,
            },
            BomInfoEntry {
                a: 16777223,
                b: 0,
                c: 37959280,
                d: 0,
            },
            BomInfoEntry {
                a: 16777228,
                b: 0,
                c: 25620800,
                d: 0,
            },
        ],
    }));

    let mut variables = vec![Variable {
        name: "BomInfo".to_string(),
        block_index: 1,
    }];

    let mut path_entries = Vec::with_capacity(records.len());
    for (path_id, path_record, file) in records {
        let path_record_index = blocks.len() as u32;
        blocks.push(BomBlock::PathRecord(path_record));
        let file_index = blocks.len() as u32;
        blocks.push(BomBlock::File(file));
        let path_info_index = blocks.len() as u32;
        blocks.push(BomBlock::PathInfoIndex(BomBlockPathInfoIndex {
            path_id,
            path_record_index,
        }));
        path_entries.push(BomPathsEntry {
            block_index: path_info_index,
            file_index,
        });
    }

    push_per_record_blocks(&mut blocks);
    push_paths_blocks(&mut blocks, &mut variables, path_entries)?;
    push_empty_variables(&mut blocks, &mut variables)?;

    serialise(&blocks, &variables)
}

#[cfg(test)]
mod tests {
    use super::*;
    use apple_bom::ParsedBom;

    /// The round trip that matters: a BOM this module wrote, read back by the
    /// crate's own reader, has to name the same paths with the same modes,
    /// sizes and checksums. A writer checked against itself agrees with itself
    /// about a mistake.
    #[test]
    fn a_written_bom_reads_back_with_the_same_paths() {
        let files = vec![
            BomFile {
                path: "usr/local/bin/inillucent".to_string(),
                mode: 0o755,
                size: 4096,
                crc32: 0x1234_5678,
            },
            BomFile {
                path: "usr/local/include/inillucent_driver.h".to_string(),
                mode: 0o644,
                size: 100,
                crc32: 0x9abc_def0,
            },
        ];
        let data = build(&files, 0o755, 0).expect("the BOM is written");
        let parsed = ParsedBom::parse(&data).expect("the BOM parses");
        let paths = parsed.paths().expect("the paths resolve");

        let names: Vec<&str> = paths.iter().map(|path| path.path()).collect();
        assert!(names.contains(&"./usr/local/bin/inillucent"), "{names:?}");
        assert!(
            names.contains(&"./usr/local/include/inillucent_driver.h"),
            "{names:?}"
        );
        // The directories on the way there are recorded too, exactly once each.
        assert_eq!(names.iter().filter(|n| **n == "./usr/local").count(), 1);

        let binary = paths
            .iter()
            .find(|path| path.path() == "./usr/local/bin/inillucent")
            .expect("the program is in the BOM");
        assert_eq!(binary.file_mode(), 0o100_755);
        assert_eq!(binary.size(), 4096);
        assert_eq!(binary.crc32(), Some(0x1234_5678));

        let directory = paths
            .iter()
            .find(|path| path.path() == "./usr/local/bin")
            .expect("the directory is in the BOM");
        assert_eq!(directory.file_mode(), 0o40_755);
    }

    /// Reads a big endian u32 at an offset, or None past the end.
    ///
    /// @param data - the BOM
    /// @param offset - where the value starts
    fn be_u32(data: &[u8], offset: usize) -> Option<u32> {
        let bytes = data.get(offset..offset + 4)?;
        Some(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    /// Walks the index region in the order macOS's `BOMStorageOpenInRAM` does:
    /// the block table, then the free list's count and pointers, all inside
    /// `blocks_index_offset + blocks_index_length`, which must be the end of the
    /// file. Returns the header's block count and the table's non null count.
    ///
    /// `ParsedBom` cannot catch a missing free list, because it never reads
    /// one. That gap is what shipped in 0.1.8.
    ///
    /// @param data - the BOM
    fn read_index_like_macos(data: &[u8]) -> Result<(u32, u32), String> {
        let header_blocks = be_u32(data, 12).ok_or("no header")?;
        let index_offset = be_u32(data, 16).ok_or("no header")? as usize;
        let index_length = be_u32(data, 20).ok_or("no header")? as usize;
        if index_offset + index_length != data.len() {
            return Err(format!(
                "index ends at {} but the file is {} bytes",
                index_offset + index_length,
                data.len()
            ));
        }
        let index = &data[index_offset..];
        let table_count = be_u32(index, 0).ok_or("no block table count")? as usize;
        let mut non_null = 0;
        for slot in 0..table_count {
            let address = be_u32(index, 4 + 8 * slot).ok_or("block table is short")?;
            let length = be_u32(index, 8 + 8 * slot).ok_or("block table is short")?;
            if slot == 0 && (address, length) != (0, 0) {
                return Err(format!("block 0 is ({address}, {length}), not null"));
            }
            if (address, length) != (0, 0) {
                non_null += 1;
            }
        }
        let free_list = 4 + 8 * table_count;
        let free_count = be_u32(index, free_list)
            .ok_or("the file ends where the free list count should be")?
            as usize;
        if index.len() < free_list + 4 + 8 * free_count {
            return Err("the free list runs past the end of the file".to_string());
        }
        Ok((header_blocks, non_null))
    }

    /// A written BOM has the free list macOS reads after the block table, and
    /// the header counts blocks the way Apple's BOMs do. The 0.1.8 package had
    /// neither, and Installer.app aborted in `_ReadFreeList` on opening it.
    #[test]
    fn a_written_bom_has_the_free_list_macos_reads() {
        let files = vec![BomFile {
            path: "usr/local/bin/inillucent".to_string(),
            mode: 0o755,
            size: 4096,
            crc32: 0x1234_5678,
        }];
        let data = build(&files, 0o755, 0).expect("the BOM is written");
        let (header_blocks, non_null) = read_index_like_macos(&data).expect("macOS can open it");
        assert_eq!(header_blocks, non_null);
        assert!(ParsedBom::parse(&data).is_ok());

        // The check is not vacuous: the layout 0.1.8 shipped, a table that runs
        // to the end of the file, is refused at the free list.
        let without_free_list = &data[..data.len() - EMPTY_FREE_LIST.len()];
        let mut shipped = without_free_list.to_vec();
        let length = be_u32(&shipped, 20).expect("header") - EMPTY_FREE_LIST.len() as u32;
        shipped[20..24].copy_from_slice(&length.to_be_bytes());
        let refusal = read_index_like_macos(&shipped).expect_err("0.1.8's layout is refused");
        assert!(refusal.contains("free list"), "{refusal}");
    }

    /// A `Paths` tree key: the parent path id and the leaf name.
    type Key = (u32, String);

    /// Reads the `Paths` tree's leaves in order and returns the branch keys,
    /// if there is a branch, and every leaf's (parent path id, name) keys.
    ///
    /// @param parsed - the BOM
    fn paths_tree_keys(parsed: &ParsedBom) -> (Option<Vec<Key>>, Vec<Vec<Key>>) {
        let key = |file_index: u32| {
            let file = parsed.block_as_file(file_index as usize).expect("a file");
            (file.parent_path_id, file.string_file_name())
        };
        let variable = parsed.find_variable("Paths").expect("Paths");
        let tree = parsed
            .block_as_tree(variable.block_index as usize)
            .expect("tree");
        let top = parsed
            .block_as_paths(tree.block_paths_index as usize)
            .expect("paths");
        let (branch, mut leaf_index) = if top.is_path_info == 0 {
            let keys = top.paths.iter().map(|e| key(e.file_index)).collect();
            (Some(keys), top.paths[0].block_index)
        } else {
            (None, tree.block_paths_index)
        };
        let mut leaves = Vec::new();
        while leaf_index != 0 {
            let leaf = parsed.block_as_paths(leaf_index as usize).expect("leaf");
            leaves.push(leaf.paths.iter().map(|e| key(e.file_index)).collect());
            leaf_index = leaf.next_paths_block_index;
        }
        (branch, leaves)
    }

    /// A payload the size of a release's is one leaf, directly under the tree,
    /// with its entries in (parent path id, name) order. That is what Apple's
    /// BOMs look like, and the files are given here out of order on purpose.
    #[test]
    fn a_small_payload_is_one_sorted_leaf() {
        let file = |path: &str| BomFile {
            path: path.to_string(),
            mode: 0o644,
            size: 1,
            crc32: 1,
        };
        let files = vec![
            file("usr/local/lib/libinillucent_driver_capi.dylib"),
            file("usr/local/bin/inillucent-shell"),
            file("usr/local/bin/inillucent"),
            file("usr/local/include/inillucent_driver.h"),
        ];
        let data = build(&files, 0o755, 0).expect("the BOM is written");
        let parsed = ParsedBom::parse(&data).expect("the BOM parses");
        let (branch, leaves) = paths_tree_keys(&parsed);

        assert!(branch.is_none(), "one leaf needs no branch block");
        assert_eq!(leaves.len(), 1);
        let keys = &leaves[0];
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, &sorted);
        assert_eq!(keys[0], (0, ".".to_string()));
        // The root, usr and local, bin include and lib, and the four files.
        assert_eq!(keys.len(), 1 + 2 + 3 + 4);

        let vindex = parsed.find_variable("VIndex").expect("VIndex");
        let vindex = parsed
            .block_as_vindex(vindex.block_index as usize)
            .expect("a VIndex block");
        let vtree = vindex.tree(&parsed).expect("the VIndex tree");
        assert_eq!(vtree.block_size, VINDEX_BLOCK_SIZE);
    }

    /// A payload too big for one leaf is split across leaves that link to each
    /// other, under a branch whose key for each leaf is that leaf's last entry,
    /// and every path still reads back.
    #[test]
    fn a_large_payload_spans_linked_sorted_leaves() {
        let files: Vec<BomFile> = (0..1200)
            .map(|n| BomFile {
                path: format!("usr/local/share/inillucent/f{n:04}"),
                mode: 0o644,
                size: n,
                crc32: n,
            })
            .collect();
        let data = build(&files, 0o755, 0).expect("the BOM is written");
        read_index_like_macos(&data).expect("macOS can open it");
        let parsed = ParsedBom::parse(&data).expect("the BOM parses");
        let (branch, leaves) = paths_tree_keys(&parsed);

        let branch = branch.expect("more than one leaf has a branch block");
        assert!(leaves.len() > 1, "{} leaves", leaves.len());
        let last_keys: Vec<_> = leaves.iter().map(|l| l.last().cloned().unwrap()).collect();
        assert_eq!(branch, last_keys);
        let all: Vec<_> = leaves.concat();
        let mut sorted = all.clone();
        sorted.sort();
        assert_eq!(all, sorted);
        assert_eq!(all.len(), 1 + 4 + 1200);
        assert_eq!(
            parsed.paths().expect("the paths resolve").len(),
            1 + 4 + 1200
        );
    }
}
