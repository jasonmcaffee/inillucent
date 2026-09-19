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
        collections::HashMap,
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

/// Turns the file list into the path records the BOM is built from.
///
/// Every directory on the way to a file gets a record of its own, emitted
/// before the file, and each record carries only its own leaf name — the
/// reader rebuilds the full path by following `parent_path_id` upwards.
///
/// @param files - the files to record
/// @param directory_mode - the mode every derived directory is given
/// @param mtime - the modification time every record is given
fn build_records(files: &[BomFile], directory_mode: u16, mtime: u32) -> Result<Vec<Record<'_>>> {
    let mut by_path: HashMap<String, u32> = HashMap::with_capacity(files.len() + 1);
    let mut records: Vec<Record> = Vec::with_capacity(files.len() + 1);

    // The root is always path id 1 and is the only record with no parent.
    by_path.insert(".".to_string(), 1);
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

    for file in files {
        let parts: Vec<&str> = file.path.split('/').collect();
        let mut parent_id = 1;
        let mut walked = ".".to_string();

        for (index, part) in parts.iter().enumerate() {
            walked = format!("{walked}/{part}");
            if let Some(existing) = by_path.get(&walked) {
                parent_id = *existing;
                continue;
            }
            let path_id = by_path.len() as u32 + 1;
            let last = index + 1 == parts.len();
            records.push((
                path_id,
                if last {
                    file_record(file, mtime)
                } else {
                    directory_record(directory_mode, mtime)
                },
                BomBlockFile {
                    parent_path_id: parent_id,
                    name: Cow::from(c_name(part)?),
                },
            ));
            by_path.insert(walked.clone(), path_id);
            parent_id = path_id;
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
/// @param blocks - the block list being assembled
/// @param variables - the variables being assembled
/// @param entries - one entry per path record
fn push_paths_blocks(
    blocks: &mut Vec<BomBlock>,
    variables: &mut Vec<Variable>,
    entries: Vec<BomPathsEntry>,
) -> Result<()> {
    let chunks = chunk_paths(entries);

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

    // The first paths block is a pointer to the first block that holds records.
    blocks.push(BomBlock::Paths(BomBlockPaths {
        is_path_info: 0,
        count: 1,
        paths: vec![BomPathsEntry {
            block_index: blocks.len() as u32 + 1,
            file_index: chunks
                .first()
                .and_then(|chunk| chunk.paths.first())
                .map(|entry| entry.file_index)
                .unwrap_or(0),
        }],
        ..Default::default()
    }));

    for (index, chunk) in chunks.iter().enumerate() {
        blocks.push(BomBlock::Paths(BomBlockPaths {
            is_path_info: chunk.is_path_info,
            count: chunk.count,
            next_paths_block_index: if index + 1 == chunks.len() {
                0
            } else {
                blocks.len() as u32 + 2
            },
            previous_paths_block_index: if index == 0 { 0 } else { blocks.len() as u32 },
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
fn push_empty_tree(
    blocks: &mut Vec<BomBlock>,
    variables: &mut Vec<Variable>,
    name: Option<&str>,
) -> Result<()> {
    blocks.push(BomBlock::Tree(BomBlockTree {
        block_paths_index: blocks.len() as u32 + 1,
        block_size: PATHS_BLOCK_SIZE,
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
    push_empty_tree(blocks, variables, Some("HLIndex"))?;

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
    push_empty_tree(blocks, variables, None)?;

    push_empty_tree(blocks, variables, Some("Size64"))
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
        blocks_index.blocks.push(BomBlocksEntry {
            file_offset: BLOCK_DATA_FILE_OFFSET + start as u32,
            length: (end - start) as u32,
        });
    }

    let blocks_data = blocks_writer.into_inner();
    let vars_data = vars::to_vec(variables)?;
    let blocks_index_data = blocks_index
        .to_vec()
        .map_err(|e| anyhow::anyhow!("writing the blocks index: {e}"))?;

    let blocks_index_offset =
        BLOCK_DATA_FILE_OFFSET + blocks_data.len() as u32 + (64 - blocks_data.len() % 64) as u32;

    let header = BomHeader {
        magic: *b"BOMStore",
        version: 1,
        number_of_blocks: blocks.len() as u32,
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
}
