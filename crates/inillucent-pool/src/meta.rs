//! The meta page and its shadow: the two pages that say what the file is.
//!
//! Invariant: a reader takes whichever of page 0 and page 1 has a valid
//! checksum and the higher generation, and refuses the file when neither does.
//! That is the only double-write in the design, and it is what makes a
//! checkpoint atomic without a journal: the previous meta page is still intact
//! and still describes a consistent file until the new one is durable.
//!
//! The layout is the TDD's, byte for byte, and the offsets are named rather
//! than written as literals at their use sites so that a field added later
//! cannot silently overlap one that is already there.

use inillucent_base::checksum::crc32;
use inillucent_base::error::{corrupt, misuse};
use inillucent_base::DbResult;

use crate::PageId;

/// The eight bytes that begin every inillucent data file.
pub const MAGIC: [u8; 8] = *b"RDB2\0\0\0\0";

/// The format version this build writes and is the only one it reads.
pub const FORMAT_VERSION: u32 = 1;

/// The page the meta record lives on.
pub const META_PAGE: PageId = PageId(0);

/// The page the meta record's shadow lives on.
pub const SHADOW_PAGE: PageId = PageId(1);

/// The first page that can hold tree or free-map data.
pub const FIRST_DATA_PAGE: PageId = PageId(2);

/// Byte offsets inside the meta page.
mod at {
    /// The magic, 8 bytes.
    pub const MAGIC: usize = 0;
    /// The format version, 4 bytes.
    pub const FORMAT: usize = 8;
    /// The page size in bytes, 4 bytes.
    pub const PAGE_SIZE: usize = 12;
    /// How many pages the file holds, 8 bytes.
    pub const PAGE_COUNT: usize = 16;
    /// The catalog tree's root page id, 8 bytes.
    pub const CATALOG_ROOT: usize = 24;
    /// The first free-map page's id, 8 bytes.
    pub const FREE_MAP: usize = 32;
    /// The checkpoint LSN, 8 bytes.
    pub const CHECKPOINT_LSN: usize = 40;
    /// The commit timestamp watermark at the checkpoint, 8 bytes.
    pub const CTS_WATERMARK: usize = 48;
    /// The WAL segment sequence at the checkpoint, 8 bytes.
    pub const WAL_SEQUENCE: usize = 56;
    /// The database generation, 8 bytes.
    pub const GENERATION: usize = 64;
    /// The database uuid, 16 bytes.
    pub const UUID: usize = 72;
    /// The checksum, 4 bytes.
    pub const CHECKSUM: usize = 88;
    /// Where the reserved region begins.
    pub const RESERVED: usize = 92;
}

/// The smallest a meta page can be and still hold every field.
pub const META_BYTES: usize = at::RESERVED;

/// What the meta page says about the database.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Meta {
    /// The page size in bytes.
    pub page_size: u32,
    /// How many pages the file holds, including the two meta pages.
    pub page_count: u64,
    /// The catalog tree's root page.
    pub catalog_root: PageId,
    /// The first page of the free-map chain.
    pub free_map: PageId,
    /// Every page write with an LSN at or below this is durable in the file.
    pub checkpoint_lsn: u64,
    /// The commit timestamp watermark at the checkpoint.
    pub cts_watermark: u64,
    /// The WAL segment sequence at the checkpoint.
    pub wal_sequence: u64,
    /// Incremented on every checkpoint; the newer of the two meta pages wins.
    pub generation: u64,
    /// The database's identity, so a WAL segment cannot be attached to the
    /// wrong file.
    pub uuid: u128,
}

impl Meta {
    /// Returns the meta record for a fresh, empty database.
    ///
    /// @param page_size - the page size in bytes
    /// @param uuid - the database's identity
    pub fn fresh(page_size: u32, uuid: u128) -> Meta {
        Meta {
            page_size,
            page_count: FIRST_DATA_PAGE.0,
            catalog_root: PageId::NONE,
            free_map: PageId::NONE,
            checkpoint_lsn: 0,
            cts_watermark: 0,
            wal_sequence: 0,
            generation: 1,
            uuid,
        }
    }

    /// Writes the record into a page-sized buffer and checksums it.
    ///
    /// @param page - the buffer, exactly one page long
    pub fn encode(&self, page: &mut [u8]) -> DbResult<()> {
        if page.len() < META_BYTES {
            return Err(misuse(format!(
                "a meta page needs at least {META_BYTES} bytes, not {}",
                page.len()
            )));
        }
        for byte in page.iter_mut() {
            *byte = 0;
        }
        put(page, at::MAGIC, &MAGIC)?;
        put(page, at::FORMAT, &FORMAT_VERSION.to_le_bytes())?;
        put(page, at::PAGE_SIZE, &self.page_size.to_le_bytes())?;
        put(page, at::PAGE_COUNT, &self.page_count.to_le_bytes())?;
        put(page, at::CATALOG_ROOT, &self.catalog_root.0.to_le_bytes())?;
        put(page, at::FREE_MAP, &self.free_map.0.to_le_bytes())?;
        put(page, at::CHECKPOINT_LSN, &self.checkpoint_lsn.to_le_bytes())?;
        put(page, at::CTS_WATERMARK, &self.cts_watermark.to_le_bytes())?;
        put(page, at::WAL_SEQUENCE, &self.wal_sequence.to_le_bytes())?;
        put(page, at::GENERATION, &self.generation.to_le_bytes())?;
        put(page, at::UUID, &self.uuid.to_le_bytes())?;
        let sum = checksum(page)?;
        put(page, at::CHECKSUM, &sum.to_le_bytes())?;
        Ok(())
    }

    /// Reads a meta record out of a page, or says why it is not one.
    ///
    /// @param page - the page bytes
    pub fn decode(page: &[u8]) -> DbResult<Meta> {
        if page.len() < META_BYTES {
            return Err(corrupt("the meta page is shorter than its own fields"));
        }
        let magic = take(page, at::MAGIC, 8)?;
        if magic != MAGIC {
            return Err(corrupt("the file does not begin with the inillucent magic"));
        }
        let format = u32(page, at::FORMAT)?;
        if format != FORMAT_VERSION {
            return Err(corrupt(format!(
                "format version {format} is not {FORMAT_VERSION}"
            )));
        }
        let stored = u32(page, at::CHECKSUM)?;
        let computed = checksum(page)?;
        if stored != computed {
            return Err(corrupt(format!(
                "meta checksum {stored:08x} is not the computed {computed:08x}"
            )));
        }
        Ok(Meta {
            page_size: u32(page, at::PAGE_SIZE)?,
            page_count: u64v(page, at::PAGE_COUNT)?,
            catalog_root: PageId(u64v(page, at::CATALOG_ROOT)?),
            free_map: PageId(u64v(page, at::FREE_MAP)?),
            checkpoint_lsn: u64v(page, at::CHECKPOINT_LSN)?,
            cts_watermark: u64v(page, at::CTS_WATERMARK)?,
            wal_sequence: u64v(page, at::WAL_SEQUENCE)?,
            generation: u64v(page, at::GENERATION)?,
            uuid: u128v(page, at::UUID)?,
        })
    }

    /// Returns whichever of two candidate meta pages a reader should believe.
    ///
    /// Neither valid is a corruption; one valid is that one; both valid is the
    /// higher generation. A tie cannot happen - the generation is bumped before
    /// either copy is written - and if one ever does, the primary wins, because
    /// picking arbitrarily is better than picking differently on two opens.
    ///
    /// @param primary - page 0's bytes
    /// @param shadow - page 1's bytes
    pub fn choose(primary: &[u8], shadow: &[u8]) -> DbResult<Meta> {
        let first = Meta::decode(primary);
        let second = Meta::decode(shadow);
        match (first, second) {
            (Ok(a), Ok(b)) => Ok(if b.generation > a.generation { b } else { a }),
            (Ok(a), Err(_)) => Ok(a),
            (Err(_), Ok(b)) => Ok(b),
            (Err(reason), Err(_)) => Err(reason),
        }
    }
}

/// Returns the crc32c over every byte except the checksum field itself.
///
/// @param page - the page bytes
fn checksum(page: &[u8]) -> DbResult<u32> {
    let head = page
        .get(..at::CHECKSUM)
        .ok_or_else(|| corrupt("the meta page is too short to checksum"))?;
    let tail = page
        .get(at::RESERVED..)
        .ok_or_else(|| corrupt("the meta page is too short to checksum"))?;
    Ok(inillucent_base::checksum::crc32_continue(crc32(head), tail))
}

/// Copies bytes into the page at an offset, or says the page was short.
///
/// @param page - the page bytes
/// @param offset - where to write
/// @param bytes - what to write
fn put(page: &mut [u8], offset: usize, bytes: &[u8]) -> DbResult<()> {
    let slot = page
        .get_mut(offset..offset.saturating_add(bytes.len()))
        .ok_or_else(|| misuse(format!("the meta page ends before offset {offset}")))?;
    slot.copy_from_slice(bytes);
    Ok(())
}

/// Returns a run of bytes from the page, or says the page was short.
///
/// @param page - the page bytes
/// @param offset - where to read
/// @param len - how many bytes
fn take(page: &[u8], offset: usize, len: usize) -> DbResult<&[u8]> {
    page.get(offset..offset.saturating_add(len))
        .ok_or_else(|| corrupt(format!("the meta page ends before offset {offset}")))
}

/// Reads a little-endian `u32`.
///
/// @param page - the page bytes
/// @param offset - where to read
fn u32(page: &[u8], offset: usize) -> DbResult<u32> {
    let mut raw = [0u8; 4];
    raw.copy_from_slice(take(page, offset, 4)?);
    Ok(u32::from_le_bytes(raw))
}

/// Reads a little-endian `u64`.
///
/// @param page - the page bytes
/// @param offset - where to read
fn u64v(page: &[u8], offset: usize) -> DbResult<u64> {
    let mut raw = [0u8; 8];
    raw.copy_from_slice(take(page, offset, 8)?);
    Ok(u64::from_le_bytes(raw))
}

/// Reads a little-endian `u128`.
///
/// @param page - the page bytes
/// @param offset - where to read
fn u128v(page: &[u8], offset: usize) -> DbResult<u128> {
    let mut raw = [0u8; 16];
    raw.copy_from_slice(take(page, offset, 16)?);
    Ok(u128::from_le_bytes(raw))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_meta_record_round_trips() {
        let meta = Meta {
            page_size: 32_768,
            page_count: 4_096,
            catalog_root: PageId(7),
            free_map: PageId(2),
            checkpoint_lsn: 900_001,
            cts_watermark: 42,
            wal_sequence: 3,
            generation: 11,
            uuid: 0x0123_4567_89ab_cdef_0123_4567_89ab_cdef,
        };
        let mut page = vec![0u8; 32_768];
        meta.encode(&mut page).unwrap();
        assert_eq!(Meta::decode(&page).unwrap(), meta);
    }

    /// Corrupting any single byte of any field is detected. This is the
    /// "corrupt every field" table the TDD's coverage tier asks for, written as
    /// a sweep rather than as a list so a field added later is covered without
    /// anybody remembering to add a case.
    #[test]
    fn corrupting_any_byte_is_detected() {
        let meta = Meta::fresh(8_192, 99);
        let mut page = vec![0u8; 8_192];
        meta.encode(&mut page).unwrap();
        for index in 0..META_BYTES {
            let mut damaged = page.clone();
            let byte = damaged
                .get_mut(index)
                .expect("the index is inside the page");
            *byte ^= 0xFF;
            assert!(
                Meta::decode(&damaged).is_err(),
                "flipping byte {index} went unnoticed"
            );
        }
        // And one byte outside the field region, which the checksum also
        // covers because it runs to the end of the page.
        let mut damaged = page.clone();
        let byte = damaged
            .get_mut(META_BYTES + 16)
            .expect("the page is larger than the record");
        *byte ^= 0xFF;
        assert!(Meta::decode(&damaged).is_err());
    }

    /// A page that is not a meta page at all is refused by the magic rather
    /// than by the checksum, so the error says what is actually wrong.
    #[test]
    fn a_page_without_the_magic_is_refused() {
        let page = vec![0u8; 8_192];
        let error = Meta::decode(&page).unwrap_err();
        assert!(error.detail().unwrap_or("").contains("magic"), "{error:?}");
        assert!(Meta::decode(&[]).is_err());
        assert!(Meta::decode(&[0u8; 4]).is_err());
    }

    /// A future format version is refused rather than read as this one.
    #[test]
    fn a_future_format_version_is_refused() {
        let mut page = vec![0u8; 8_192];
        Meta::fresh(8_192, 1).encode(&mut page).unwrap();
        let slot = page
            .get_mut(at::FORMAT..at::FORMAT + 4)
            .expect("the format field is inside the page");
        slot.copy_from_slice(&2u32.to_le_bytes());
        let error = Meta::decode(&page).unwrap_err();
        assert!(error.detail().unwrap_or("").contains("format"), "{error:?}");
    }

    /// The newer generation wins, a damaged copy is ignored, and two damaged
    /// copies are a corruption.
    #[test]
    fn the_newer_valid_copy_wins() {
        let mut older = Meta::fresh(8_192, 5);
        older.generation = 4;
        let mut newer = older;
        newer.generation = 5;
        newer.catalog_root = PageId(9);

        let mut primary = vec![0u8; 8_192];
        let mut shadow = vec![0u8; 8_192];
        older.encode(&mut primary).unwrap();
        newer.encode(&mut shadow).unwrap();
        assert_eq!(Meta::choose(&primary, &shadow).unwrap(), newer);
        assert_eq!(Meta::choose(&shadow, &primary).unwrap(), newer);

        let torn = vec![0u8; 8_192];
        assert_eq!(Meta::choose(&primary, &torn).unwrap(), older);
        assert_eq!(Meta::choose(&torn, &primary).unwrap(), older);
        assert!(Meta::choose(&torn, &torn).is_err());
    }

    /// Two valid copies at the same generation resolve to the primary, so two
    /// opens of one file never disagree.
    #[test]
    fn a_generation_tie_resolves_to_the_primary() {
        let mut a = Meta::fresh(8_192, 1);
        a.catalog_root = PageId(3);
        let mut b = a;
        b.catalog_root = PageId(4);
        let mut primary = vec![0u8; 8_192];
        let mut shadow = vec![0u8; 8_192];
        a.encode(&mut primary).unwrap();
        b.encode(&mut shadow).unwrap();
        assert_eq!(Meta::choose(&primary, &shadow).unwrap(), a);
    }

    /// A buffer too small for the fields is refused rather than indexed into.
    #[test]
    fn a_short_buffer_is_refused() {
        let mut page = vec![0u8; META_BYTES - 1];
        assert!(Meta::fresh(8_192, 1).encode(&mut page).is_err());
    }

    /// A fresh database claims the two meta pages and nothing else.
    #[test]
    fn a_fresh_database_has_two_pages() {
        let meta = Meta::fresh(16_384, 7);
        assert_eq!(meta.page_count, 2);
        assert_eq!(meta.generation, 1);
        assert!(meta.catalog_root.is_none());
        assert!(meta.free_map.is_none());
        assert_eq!(FIRST_DATA_PAGE, PageId(2));
        assert_eq!(META_PAGE, PageId(0));
        assert_eq!(SHADOW_PAGE, PageId(1));
    }
}
