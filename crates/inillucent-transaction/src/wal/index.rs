//! The WAL index: the shared-memory structure every connection on a log
//! reads, and the one that makes finding a page in the log an O(1) lookup
//! rather than a scan.
//!
//! Invariant: the index is a cache of the log file and never the source of
//! truth. Anything it says can be rebuilt by reading the log from its first
//! byte, and a connection that finds the index unreadable does exactly that
//! rather than reporting an error. That is why the index may live in a file
//! that is deleted at any time, and why its format is a memory layout rather
//! than a durable one.
//!
//! The layout is SQLite's, byte for byte, in the host's own word order. It has
//! to be: two processes coordinate through this file, and one of them may be a
//! real SQLite build. The header appears twice with a barrier between the
//! writes, so a reader that catches a half-published header sees two copies
//! that disagree and rebuilds rather than trusting a mixture. The eight lock
//! slots sit at byte 120, which is where SQLite's `WalCkptInfo.aLock` lands and
//! what `inillucent_vfs::os::ranges::SHM_LOCK_FIRST` already names.
//!
//! Reference: <https://sqlite.org/walformat.html#the_wal_index_file_format>.

use std::sync::Arc;

use inillucent_base::checksum::{WalByteOrder, WalChecksum};
use inillucent_base::error::{corrupt, misuse};
use inillucent_base::page::PageSize;
use inillucent_base::{bytes, DbResult};
use inillucent_vfs::{SharedMemory, ShmLockRequest, ShmRegion};

/// The size of one copy of the index header.
pub const INDEX_HEADER_SIZE: usize = 48;

/// The size of the header area: two header copies and the checkpoint block.
pub const INDEX_PREFIX_SIZE: usize = 136;

/// The size of one shared-memory region.
pub const REGION_SIZE: usize = 32_768;

/// How many page numbers one region's array holds.
pub const HASH_PAGES: u32 = 4_096;

/// How many slots one region's hash table holds.
pub const HASH_SLOTS: u32 = 8_192;

/// How many page numbers the first region holds, which is fewer because the
/// header sits in front of them.
pub const HASH_PAGES_FIRST: u32 = HASH_PAGES - (INDEX_PREFIX_SIZE as u32) / 4;

/// The multiplier of the hash function.
const HASH_MULTIPLIER: u32 = 383;

/// How many reader marks the index carries.
pub const READ_MARK_COUNT: u16 = 5;

/// The value a reader mark holds when nobody is using it.
pub const READ_MARK_UNUSED: u32 = 0xffff_ffff;

/// The index version inillucent writes and is willing to read.
pub const INDEX_VERSION: u32 = 3_007_000;

/// The lock slot the single writer holds.
pub const WRITE_LOCK: u16 = 0;

/// The lock slot a checkpointer holds.
pub const CHECKPOINT_LOCK: u16 = 1;

/// Returns the lock slot of reader mark `index`.
pub fn read_lock(index: u16) -> u16 {
    3u16.saturating_add(index)
}

/// Returns which region holds the entry for a frame.
///
/// The first region is short by the header that sits in front of it, so the
/// arithmetic is not a plain division; getting it wrong puts an entry in a
/// region no reader looks in.
pub fn region_of(frame: u32) -> u32 {
    frame
        .saturating_add(HASH_PAGES)
        .saturating_sub(HASH_PAGES_FIRST)
        .saturating_sub(1)
        / HASH_PAGES
}

/// Returns the frame number one before the first that `region` holds.
pub fn first_frame_of(region: u32) -> u32 {
    if region == 0 {
        return 0;
    }
    HASH_PAGES_FIRST.saturating_add(region.saturating_sub(1).saturating_mul(HASH_PAGES))
}

/// Returns the hash slot a page number starts its probe at.
fn hash_of(page: u32) -> u32 {
    page.wrapping_mul(HASH_MULTIPLIER) & HASH_SLOTS.saturating_sub(1)
}

/// Returns the slot after `slot`, wrapping at the end of the table.
fn next_hash(slot: u32) -> u32 {
    slot.saturating_add(1) & HASH_SLOTS.saturating_sub(1)
}

/// Reads a native-order 32-bit word.
fn read_word(source: &[u8], offset: usize) -> DbResult<u32> {
    let mut raw = [0u8; 4];
    raw.copy_from_slice(bytes::window(source, offset, 4)?);
    Ok(u32::from_ne_bytes(raw))
}

/// Writes a native-order 32-bit word.
fn write_word(target: &mut [u8], offset: usize, value: u32) -> DbResult<()> {
    let slot = bytes::window_mut(target, offset, 4)?;
    slot.copy_from_slice(&value.to_ne_bytes());
    Ok(())
}

/// Reads a native-order 16-bit word.
fn read_half(source: &[u8], offset: usize) -> DbResult<u16> {
    let mut raw = [0u8; 2];
    raw.copy_from_slice(bytes::window(source, offset, 2)?);
    Ok(u16::from_ne_bytes(raw))
}

/// Writes a native-order 16-bit word.
fn write_half(target: &mut [u8], offset: usize, value: u16) -> DbResult<()> {
    let slot = bytes::window_mut(target, offset, 2)?;
    slot.copy_from_slice(&value.to_ne_bytes());
    Ok(())
}

/// The index header: what every connection has to agree on about the log.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IndexHeader {
    /// The index format version.
    pub version: u32,
    /// A counter the writer moves on every transaction, which is what makes
    /// two headers with the same frame count still distinguishable.
    pub change: u32,
    /// Whether the header has ever been written.
    pub initialised: bool,
    /// Whether the log's checksums are read big-endian.
    pub big_endian_checksum: bool,
    /// The page size, encoded the way the database header encodes it.
    pub page_size_encoded: u16,
    /// The last frame of the log that has been committed.
    pub max_frame: u32,
    /// How many pages the database has as of that frame.
    pub page_count: u32,
    /// The running checksum through the end of the last frame.
    pub frame_checksum: WalChecksum,
    /// The log's salt values.
    pub salt: [u8; 8],
    /// The checksum over the first forty bytes.
    pub checksum: WalChecksum,
}

impl IndexHeader {
    /// Returns the page size the header declares.
    pub fn page_size(&self) -> DbResult<PageSize> {
        PageSize::from_encoded(self.page_size_encoded)
    }

    /// Encodes the header, computing the checksum over what it wrote.
    pub fn encode(&self) -> DbResult<[u8; INDEX_HEADER_SIZE]> {
        let mut raw = [0u8; INDEX_HEADER_SIZE];
        write_word(&mut raw, 0, self.version)?;
        write_word(&mut raw, 4, 0)?;
        write_word(&mut raw, 8, self.change)?;
        bytes::write_u8(&mut raw, 12, u8::from(self.initialised))?;
        bytes::write_u8(&mut raw, 13, u8::from(self.big_endian_checksum))?;
        write_half(&mut raw, 14, self.page_size_encoded)?;
        write_word(&mut raw, 16, self.max_frame)?;
        write_word(&mut raw, 20, self.page_count)?;
        write_word(&mut raw, 24, self.frame_checksum.s0)?;
        write_word(&mut raw, 28, self.frame_checksum.s1)?;
        let salt = bytes::window_mut(&mut raw, 32, 8)?;
        salt.copy_from_slice(&self.salt);
        let checksum = header_checksum(&raw)?;
        write_word(&mut raw, 40, checksum.s0)?;
        write_word(&mut raw, 44, checksum.s1)?;
        Ok(raw)
    }

    /// Decodes a header and reports whether it checks out.
    ///
    /// A header that does not is not an error to report upwards: it is the
    /// ordinary state of an index nobody has built yet, and of one a writer
    /// was half-way through publishing. The caller rebuilds.
    pub fn decode(raw: &[u8]) -> DbResult<Option<IndexHeader>> {
        if raw.len() < INDEX_HEADER_SIZE {
            return Ok(None);
        }
        let mut salt = [0u8; 8];
        salt.copy_from_slice(bytes::window(raw, 32, 8)?);
        let header = IndexHeader {
            version: read_word(raw, 0)?,
            change: read_word(raw, 8)?,
            initialised: bytes::read_u8(raw, 12)? != 0,
            big_endian_checksum: bytes::read_u8(raw, 13)? != 0,
            page_size_encoded: read_half(raw, 14)?,
            max_frame: read_word(raw, 16)?,
            page_count: read_word(raw, 20)?,
            frame_checksum: WalChecksum::new(read_word(raw, 24)?, read_word(raw, 28)?),
            salt,
            checksum: WalChecksum::new(read_word(raw, 40)?, read_word(raw, 44)?),
        };
        if !header.initialised || header.version != INDEX_VERSION {
            return Ok(None);
        }
        if header_checksum(raw)? != header.checksum {
            return Ok(None);
        }
        Ok(Some(header))
    }
}

/// Computes the checksum over a header's first forty bytes.
///
/// The words are read in the host's own order because this structure never
/// leaves the machine: it lives in a shared-memory file that is recreated from
/// the log whenever it is missing.
fn header_checksum(raw: &[u8]) -> DbResult<WalChecksum> {
    let order = if cfg!(target_endian = "big") {
        WalByteOrder::Big
    } else {
        WalByteOrder::Little
    };
    WalChecksum::default().extended(bytes::window(raw, 0, 40)?, order)
}

/// The mapped WAL index of one database.
#[derive(Debug)]
pub struct WalIndex {
    shm: Arc<dyn SharedMemory>,
    regions: Vec<Option<Arc<dyn ShmRegion>>>,
}

impl WalIndex {
    /// Wraps a shared-memory file.
    pub fn new(shm: Arc<dyn SharedMemory>) -> WalIndex {
        WalIndex {
            shm,
            regions: Vec::new(),
        }
    }

    /// Returns region `index`, mapping it when it has not been mapped yet.
    ///
    /// `extend` says whether the file may grow to hold it. A reader passes
    /// false, so that asking about a frame nobody has written cannot create
    /// the region it would have lived in.
    fn region(&mut self, index: u32, extend: bool) -> DbResult<Option<Arc<dyn ShmRegion>>> {
        let slot = index as usize;
        if let Some(Some(existing)) = self.regions.get(slot) {
            return Ok(Some(Arc::clone(existing)));
        }
        let Some(mapped) = self.shm.map(index, REGION_SIZE, extend)? else {
            return Ok(None);
        };
        if self.regions.len() <= slot {
            self.regions.resize_with(slot.saturating_add(1), || None);
        }
        if let Some(entry) = self.regions.get_mut(slot) {
            *entry = Some(Arc::clone(&mapped));
        }
        Ok(Some(mapped))
    }

    /// Orders this connection's reads and writes against every other one's.
    pub fn barrier(&self) {
        self.shm.barrier();
    }

    /// Takes or releases index lock slots.
    pub fn lock(&self, offset: u16, count: u16, exclusive: bool, acquire: bool) -> DbResult<()> {
        self.shm
            .lock(ShmLockRequest {
                offset,
                count,
                acquire,
                exclusive,
            })
            .map_err(|error| error.into_db_error())
    }

    /// Reads the published header, or `None` when there is not a valid one.
    ///
    /// Both copies are read with a barrier between them and must agree. A
    /// writer publishes the second copy first and the first copy last, so a
    /// reader that catches it mid-publish sees a disagreement rather than a
    /// header that describes neither state.
    pub fn header(&mut self) -> DbResult<Option<IndexHeader>> {
        let Some(region) = self.region(0, false)? else {
            return Ok(None);
        };
        let mut first = [0u8; INDEX_HEADER_SIZE];
        let mut second = [0u8; INDEX_HEADER_SIZE];
        region.read(0, &mut first)?;
        self.shm.barrier();
        region.read(INDEX_HEADER_SIZE, &mut second)?;
        if first != second {
            return Ok(None);
        }
        IndexHeader::decode(&first)
    }

    /// Publishes a header, second copy first.
    pub fn write_header(&mut self, header: &IndexHeader) -> DbResult<()> {
        let mut published = *header;
        published.initialised = true;
        published.version = INDEX_VERSION;
        let raw = published.encode()?;
        let Some(region) = self.region(0, true)? else {
            return Err(misuse("the WAL index cannot be created"));
        };
        region.write(INDEX_HEADER_SIZE, &raw)?;
        self.shm.barrier();
        region.write(0, &raw)?;
        Ok(())
    }

    /// Zeroes the header, which is how a connection declares the index
    /// unusable so that the next reader rebuilds it.
    pub fn invalidate_header(&mut self) -> DbResult<()> {
        let Some(region) = self.region(0, true)? else {
            return Ok(());
        };
        let zeroes = [0u8; INDEX_HEADER_SIZE * 2];
        region.write(0, &zeroes)?;
        Ok(())
    }

    /// Returns how many frames a checkpoint has copied into the database.
    pub fn backfill(&mut self) -> DbResult<u32> {
        let Some(region) = self.region(0, false)? else {
            return Ok(0);
        };
        let mut raw = [0u8; 4];
        region.read(INDEX_HEADER_SIZE * 2, &mut raw)?;
        read_word(&raw, 0)
    }

    /// Records how many frames a checkpoint has copied into the database.
    pub fn set_backfill(&mut self, frames: u32) -> DbResult<()> {
        let Some(region) = self.region(0, true)? else {
            return Ok(());
        };
        let mut raw = [0u8; 4];
        write_word(&mut raw, 0, frames)?;
        Ok(region.write(INDEX_HEADER_SIZE * 2, &raw)?)
    }

    /// Records how many frames a checkpoint set out to copy.
    ///
    /// It is written before the copying starts and left behind when the
    /// copying fails, so a later connection can tell "nothing was attempted"
    /// from "something was attempted and may have been half done".
    pub fn set_backfill_attempted(&mut self, frames: u32) -> DbResult<()> {
        let Some(region) = self.region(0, true)? else {
            return Ok(());
        };
        let mut raw = [0u8; 4];
        write_word(&mut raw, 0, frames)?;
        Ok(region.write(INDEX_HEADER_SIZE * 2 + 32, &raw)?)
    }

    /// Returns reader mark `index`.
    pub fn read_mark(&mut self, index: u16) -> DbResult<u32> {
        let offset = read_mark_offset(index)?;
        let Some(region) = self.region(0, false)? else {
            return Ok(READ_MARK_UNUSED);
        };
        let mut raw = [0u8; 4];
        region.read(offset, &mut raw)?;
        read_word(&raw, 0)
    }

    /// Sets reader mark `index`.
    pub fn set_read_mark(&mut self, index: u16, value: u32) -> DbResult<()> {
        let offset = read_mark_offset(index)?;
        let Some(region) = self.region(0, true)? else {
            return Ok(());
        };
        let mut raw = [0u8; 4];
        write_word(&mut raw, 0, value)?;
        Ok(region.write(offset, &raw)?)
    }

    /// Resets the checkpoint block to the state a fresh log has.
    pub fn reset_checkpoint_block(&mut self) -> DbResult<()> {
        self.set_backfill(0)?;
        self.set_backfill_attempted(0)?;
        self.set_read_mark(0, 0)?;
        for index in 1..READ_MARK_COUNT {
            self.set_read_mark(index, READ_MARK_UNUSED)?;
        }
        Ok(())
    }

    /// Adds one frame to the index.
    ///
    /// The first entry of a table zeroes the whole table first, because the
    /// region may be holding what a log that has since been restarted left
    /// there, and a stale hash slot pointing at a page that is no longer in
    /// that frame is the one way this structure can hand back a wrong answer.
    pub fn append(&mut self, frame: u32, page: u32) -> DbResult<()> {
        let table = region_of(frame);
        let zero = first_frame_of(table);
        let index = frame.saturating_sub(zero);
        if index == 0 {
            return Err(corrupt("a WAL index entry before the start of its table"));
        }
        let Some(region) = self.region(table, true)? else {
            return Err(misuse("the WAL index cannot be extended"));
        };
        if index == 1 {
            let first = table == 0;
            let start = if first { INDEX_PREFIX_SIZE } else { 0 };
            let zeroes = vec![0u8; REGION_SIZE.saturating_sub(start)];
            region.write(start, &zeroes)?;
        }
        let mut raw = [0u8; 4];
        write_word(&mut raw, 0, page)?;
        region.write(page_slot_offset(table, index)?, &raw)?;
        let mut slot = hash_of(page);
        let mut probes = HASH_SLOTS;
        loop {
            let offset = hash_slot_offset(slot)?;
            let mut existing = [0u8; 2];
            region.read(offset, &mut existing)?;
            if read_half(&existing, 0)? == 0 {
                let mut value = [0u8; 2];
                write_half(&mut value, 0, u16::try_from(index).unwrap_or(u16::MAX))?;
                region.write(offset, &value)?;
                return Ok(());
            }
            probes = probes.saturating_sub(1);
            if probes == 0 {
                return Err(corrupt("a full WAL index hash table"));
            }
            slot = next_hash(slot);
        }
    }

    /// Returns the newest frame holding `page` within `min..=max`.
    ///
    /// Tables are searched newest first and the search stops at the first that
    /// answers, because a page written in a later table has a later frame than
    /// anything an earlier one could hold.
    pub fn find_frame(
        &mut self,
        page: u32,
        min_frame: u32,
        max_frame: u32,
    ) -> DbResult<Option<u32>> {
        if page == 0 || max_frame == 0 {
            return Ok(None);
        }
        let mut table = region_of(max_frame);
        loop {
            if let Some(found) = self.find_in_table(table, page, min_frame, max_frame)? {
                return Ok(Some(found));
            }
            if table == 0 {
                return Ok(None);
            }
            table = table.saturating_sub(1);
        }
    }

    /// Searches one hash table for a page.
    fn find_in_table(
        &mut self,
        table: u32,
        page: u32,
        min_frame: u32,
        max_frame: u32,
    ) -> DbResult<Option<u32>> {
        let zero = first_frame_of(table);
        let Some(region) = self.region(table, false)? else {
            return Ok(None);
        };
        let mut slot = hash_of(page);
        let mut probes = HASH_SLOTS;
        let mut best = None;
        loop {
            let mut raw = [0u8; 2];
            region.read(hash_slot_offset(slot)?, &mut raw)?;
            let index = u32::from(read_half(&raw, 0)?);
            if index == 0 {
                return Ok(best);
            }
            let frame = zero.saturating_add(index);
            if frame <= max_frame && frame >= min_frame {
                let mut stored = [0u8; 4];
                region.read(page_slot_offset(table, index)?, &mut stored)?;
                if read_word(&stored, 0)? == page {
                    best = Some(frame);
                }
            }
            probes = probes.saturating_sub(1);
            if probes == 0 {
                return Err(corrupt("a WAL index hash chain with no end"));
            }
            slot = next_hash(slot);
        }
    }

    /// Drops every entry above `max_frame` from the newest table.
    ///
    /// Only the newest needs it: a table is zeroed when its first entry is
    /// written, so anything above the limit in an older one is already gone.
    pub fn truncate_to(&mut self, max_frame: u32) -> DbResult<()> {
        if max_frame == 0 {
            return Ok(());
        }
        let table = region_of(max_frame);
        let zero = first_frame_of(table);
        let limit = max_frame.saturating_sub(zero);
        let Some(region) = self.region(table, false)? else {
            return Ok(());
        };
        for slot in 0..HASH_SLOTS {
            let offset = hash_slot_offset(slot)?;
            let mut raw = [0u8; 2];
            region.read(offset, &mut raw)?;
            if u32::from(read_half(&raw, 0)?) > limit {
                region.write(offset, &[0u8; 2])?;
            }
        }
        let first = page_slot_offset(table, limit.saturating_add(1))?;
        let end = hash_slot_offset(0)?;
        if end > first {
            let zeroes = vec![0u8; end.saturating_sub(first)];
            region.write(first, &zeroes)?;
        }
        Ok(())
    }

    /// Returns the page a frame holds, according to the index.
    ///
    /// This is the reverse of the hash lookup and is what a checkpoint walks:
    /// it visits frames in order and needs to know which page each one is.
    pub fn page_of(&mut self, frame: u32) -> DbResult<Option<u32>> {
        let table = region_of(frame);
        let zero = first_frame_of(table);
        let index = frame.saturating_sub(zero);
        if index == 0 {
            return Ok(None);
        }
        let offset = page_slot_offset(table, index)?;
        let Some(region) = self.region(table, false)? else {
            return Ok(None);
        };
        let mut raw = [0u8; 4];
        region.read(offset, &mut raw)?;
        let page = read_word(&raw, 0)?;
        if page == 0 {
            return Ok(None);
        }
        Ok(Some(page))
    }

    /// Releases the mapping, deleting the file when asked to.
    pub fn unmap(&mut self, delete: bool) -> DbResult<()> {
        self.regions.clear();
        self.shm
            .unmap(delete)
            .map_err(|error| error.into_db_error())
    }
}

/// Returns the byte offset of a reader mark within the first region.
fn read_mark_offset(index: u16) -> DbResult<usize> {
    if index >= READ_MARK_COUNT {
        return Err(misuse(format!("reader mark {index} does not exist")));
    }
    Ok(INDEX_HEADER_SIZE
        .saturating_mul(2)
        .saturating_add(4)
        .saturating_add(usize::from(index).saturating_mul(4)))
}

/// Returns the byte offset of a page-number slot within its region.
fn page_slot_offset(table: u32, index: u32) -> DbResult<usize> {
    let base = if table == 0 { INDEX_PREFIX_SIZE } else { 0 };
    let limit = if table == 0 {
        HASH_PAGES_FIRST
    } else {
        HASH_PAGES
    };
    if index == 0 || index > limit {
        return Err(corrupt(format!(
            "WAL index entry {index} is outside table {table}"
        )));
    }
    let slot = (index as usize).saturating_sub(1).saturating_mul(4);
    Ok(base.saturating_add(slot))
}

/// Returns the byte offset of a hash slot within its region.
fn hash_slot_offset(slot: u32) -> DbResult<usize> {
    if slot >= HASH_SLOTS {
        return Err(corrupt(format!(
            "WAL index hash slot {slot} is out of range"
        )));
    }
    Ok(((HASH_PAGES as usize).saturating_mul(4)).saturating_add((slot as usize).saturating_mul(2)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use inillucent_vfs::{MemoryVfs, OpenOptions, Vfs};

    /// Opens an index over a memory-backed shared-memory file.
    fn index() -> WalIndex {
        let vfs = MemoryVfs::new();
        let path = inillucent_vfs::DbPath::new(std::path::PathBuf::from("/wal-index.db"));
        let file = vfs.open(&path, OpenOptions::main_db()).unwrap();
        WalIndex::new(file.shared_memory().unwrap().unwrap())
    }

    /// The layout constants are the ones the format specifies. A change here
    /// is a change to a structure another process reads, so they are pinned
    /// rather than derived.
    #[test]
    fn the_layout_is_the_one_the_format_specifies() {
        assert_eq!(INDEX_HEADER_SIZE, 48);
        assert_eq!(INDEX_PREFIX_SIZE, 136);
        assert_eq!(REGION_SIZE, 32_768);
        assert_eq!(HASH_PAGES, 4_096);
        assert_eq!(HASH_SLOTS, 8_192);
        assert_eq!(HASH_PAGES_FIRST, 4_062);
        // The lock slots sit where the VFS already locks.
        assert_eq!(INDEX_HEADER_SIZE * 2 + 4 + 20, 120);
        assert_eq!(read_lock(0), 3);
        assert_eq!(read_lock(4), 7);
    }

    /// Frames map to the regions the format puts them in, and the first
    /// region is short by the header in front of it.
    #[test]
    fn frames_map_to_their_regions() {
        assert_eq!(region_of(1), 0);
        assert_eq!(region_of(4_062), 0);
        assert_eq!(region_of(4_063), 1);
        assert_eq!(region_of(4_062 + 4_096), 1);
        assert_eq!(region_of(4_062 + 4_096 + 1), 2);
        assert_eq!(first_frame_of(0), 0);
        assert_eq!(first_frame_of(1), 4_062);
        assert_eq!(first_frame_of(2), 4_062 + 4_096);
    }

    /// A header round-trips through the shared memory and is rejected when
    /// either copy is damaged.
    #[test]
    fn a_header_round_trips_and_both_copies_must_agree() {
        let mut index = index();
        assert_eq!(index.header().unwrap(), None);
        let header = IndexHeader {
            version: INDEX_VERSION,
            change: 4,
            initialised: true,
            big_endian_checksum: false,
            page_size_encoded: PageSize::new(4096).unwrap().to_encoded(),
            max_frame: 9,
            page_count: 12,
            frame_checksum: WalChecksum::new(3, 4),
            salt: [1, 2, 3, 4, 5, 6, 7, 8],
            checksum: WalChecksum::default(),
        };
        index.write_header(&header).unwrap();
        let read = index.header().unwrap().unwrap();
        assert_eq!(read.max_frame, 9);
        assert_eq!(read.page_count, 12);
        assert_eq!(read.change, 4);
        assert_eq!(read.salt, header.salt);
        assert_eq!(read.page_size().unwrap().bytes(), 4096);

        // Damaging the first copy makes the two disagree, which is what a
        // reader that caught a half-published header sees.
        let region = index.region(0, true).unwrap().unwrap();
        region.write(3, &[0xff]).unwrap();
        assert_eq!(index.header().unwrap(), None);
    }

    /// An entry can be found again, the newest frame for a page wins, and a
    /// frame outside the snapshot is not returned.
    #[test]
    fn entries_are_found_by_page_and_bounded_by_the_snapshot() {
        let mut index = index();
        index.append(1, 5).unwrap();
        index.append(2, 7).unwrap();
        index.append(3, 5).unwrap();
        assert_eq!(index.find_frame(5, 1, 3).unwrap(), Some(3));
        assert_eq!(index.find_frame(5, 1, 2).unwrap(), Some(1));
        assert_eq!(index.find_frame(7, 1, 3).unwrap(), Some(2));
        assert_eq!(index.find_frame(9, 1, 3).unwrap(), None);
        assert_eq!(index.find_frame(5, 2, 3).unwrap(), Some(3));
        assert_eq!(index.find_frame(5, 4, 3).unwrap(), None);
        assert_eq!(index.page_of(2).unwrap(), Some(7));
    }

    /// Truncating drops the entries a rolled-back writer added and leaves the
    /// ones before them.
    #[test]
    fn truncating_drops_the_entries_above_the_limit() {
        let mut index = index();
        for frame in 1..=6u32 {
            index.append(frame, frame.saturating_add(100)).unwrap();
        }
        index.truncate_to(3).unwrap();
        assert_eq!(index.find_frame(101, 1, 6).unwrap(), Some(1));
        assert_eq!(index.find_frame(103, 1, 6).unwrap(), Some(3));
        assert_eq!(index.find_frame(104, 1, 6).unwrap(), None);
        assert_eq!(index.find_frame(106, 1, 6).unwrap(), None);
    }

    /// Reader marks round-trip and reset to the state a fresh log has.
    #[test]
    fn reader_marks_round_trip_and_reset() {
        let mut index = index();
        index.set_read_mark(2, 17).unwrap();
        assert_eq!(index.read_mark(2).unwrap(), 17);
        index.set_backfill(9).unwrap();
        assert_eq!(index.backfill().unwrap(), 9);
        index.reset_checkpoint_block().unwrap();
        assert_eq!(index.backfill().unwrap(), 0);
        assert_eq!(index.read_mark(0).unwrap(), 0);
        assert_eq!(index.read_mark(1).unwrap(), READ_MARK_UNUSED);
        assert!(index.read_mark(READ_MARK_COUNT).is_err());
    }

    /// A table's second region works the same way, which is the case a log of
    /// more than four thousand frames reaches.
    #[test]
    fn the_second_region_holds_the_frames_past_the_first() {
        let mut index = index();
        let frame = HASH_PAGES_FIRST + 1;
        index.append(frame, 4321).unwrap();
        assert_eq!(index.find_frame(4321, 1, frame).unwrap(), Some(frame));
        assert_eq!(region_of(frame), 1);
    }
}
