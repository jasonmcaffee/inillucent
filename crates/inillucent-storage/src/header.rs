//! The 100-byte database header.
//!
//! Invariant: every field is decoded from a named offset and validated before
//! anything above trusts it. The header is the first hundred bytes of a file
//! anyone can hand us, so a page size that is not a power of two, a freelist
//! head past the end of the file, or a text encoding of 47 all have to be
//! refused here rather than turning into an out-of-range read three layers up.
//!
//! One field deserves its own note. The header carries a page count, and the
//! file has a length; they disagree whenever a writer crashed between growing
//! the file and updating the header. SQLite resolves this with the
//! version-valid-for number: the header's page count may be believed only when
//! that number equals the change counter, meaning the header was written by
//! the same transaction that last changed the file. Otherwise the file's
//! length wins. `effective_page_count` is that rule, and reading it wrong is
//! how a reader ends up scanning pages that belong to a rolled-back write.

use inillucent_base::bytes;
use inillucent_base::error::corrupt;
use inillucent_base::ids::PageId;
use inillucent_base::page::{self, PageSize};
use inillucent_base::DbResult;
use inillucent_value::TextEncoding;

/// The bytes every SQLite database file starts with.
pub const MAGIC: &[u8; 16] = b"SQLite format 3\0";

/// The size of the header, in bytes.
pub const HEADER_SIZE: usize = 100;

/// The payload fractions the file format fixes. SQLite refuses a file whose
/// header disagrees, because the B-tree's local-payload arithmetic is derived
/// from them and a different value would produce a different tree shape.
pub const MAX_EMBEDDED_FRACTION: u8 = 64;
/// The minimum embedded payload fraction, which the format fixes at 32.
pub const MIN_EMBEDDED_FRACTION: u8 = 32;
/// The leaf payload fraction, which the format fixes at 32.
pub const LEAF_FRACTION: u8 = 32;

/// Byte offsets of every header field, named so a reader can check them
/// against the file-format document without counting.
pub mod offsets {
    /// The 16-byte magic string.
    pub const MAGIC: usize = 0;
    /// The page size, as a big-endian `u16` where 1 means 65536.
    pub const PAGE_SIZE: usize = 16;
    /// The write format version.
    pub const WRITE_VERSION: usize = 18;
    /// The read format version.
    pub const READ_VERSION: usize = 19;
    /// Bytes reserved at the end of every page.
    pub const RESERVED_BYTES: usize = 20;
    /// The maximum embedded payload fraction.
    pub const MAX_FRACTION: usize = 21;
    /// The minimum embedded payload fraction.
    pub const MIN_FRACTION: usize = 22;
    /// The leaf payload fraction.
    pub const LEAF_FRACTION: usize = 23;
    /// The file change counter.
    pub const CHANGE_COUNTER: usize = 24;
    /// The database size in pages, as the header understands it.
    pub const DATABASE_SIZE: usize = 28;
    /// The first freelist trunk page, or zero.
    pub const FREELIST_HEAD: usize = 32;
    /// The number of pages on the freelist.
    pub const FREELIST_COUNT: usize = 36;
    /// The schema cookie.
    pub const SCHEMA_COOKIE: usize = 40;
    /// The schema format number, 1 through 4.
    pub const SCHEMA_FORMAT: usize = 44;
    /// The suggested page cache size.
    pub const CACHE_SIZE: usize = 48;
    /// The largest root B-tree page, non-zero under auto or incremental vacuum.
    pub const LARGEST_ROOT: usize = 52;
    /// The text encoding: 1 UTF-8, 2 UTF-16le, 3 UTF-16be.
    pub const TEXT_ENCODING: usize = 56;
    /// The application-defined user version.
    pub const USER_VERSION: usize = 60;
    /// Non-zero when incremental vacuum is enabled.
    pub const INCREMENTAL_VACUUM: usize = 64;
    /// The application id.
    pub const APPLICATION_ID: usize = 68;
    /// The start of the twenty reserved-for-expansion bytes.
    pub const RESERVED_EXPANSION: usize = 72;
    /// The change counter this header's page count is valid for.
    pub const VERSION_VALID_FOR: usize = 92;
    /// The `SQLITE_VERSION_NUMBER` of the library that last wrote the file.
    pub const WRITE_LIBRARY_VERSION: usize = 96;
}

/// How a file's pages are laid out for vacuum.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VacuumMode {
    /// No pointer map; freed pages go on the freelist and stay there.
    None,
    /// A pointer map exists and the file is truncated on commit.
    Auto,
    /// A pointer map exists and pages are released by `PRAGMA incremental_vacuum`.
    Incremental,
}

/// The decoded 100-byte header.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DatabaseHeader {
    /// The page size in bytes.
    pub page_size: PageSize,
    /// The write format version: 1 for a rollback journal, 2 for WAL.
    pub write_version: u8,
    /// The read format version.
    pub read_version: u8,
    /// Bytes reserved at the end of every page.
    pub reserved_bytes: u8,
    /// The file change counter.
    pub change_counter: u32,
    /// The page count the header claims.
    pub database_size: u32,
    /// The first freelist trunk page, or zero.
    pub freelist_head: u32,
    /// How many pages are on the freelist.
    pub freelist_count: u32,
    /// The schema cookie, which invalidates prepared statements when it moves.
    pub schema_cookie: u32,
    /// The schema format number.
    pub schema_format: u32,
    /// The suggested page cache size, which may be negative in the file.
    pub cache_size: i32,
    /// The largest root B-tree page, non-zero only under a vacuum mode.
    pub largest_root: u32,
    /// The database's text encoding.
    pub text_encoding: TextEncoding,
    /// The application-defined user version.
    pub user_version: i32,
    /// The vacuum mode the file is in.
    pub vacuum_mode: VacuumMode,
    /// The application id.
    pub application_id: i32,
    /// Whether the twenty reserved-for-expansion bytes are all zero.
    ///
    /// The format says they must be, and SQLite does not check; a file with
    /// something there is still readable, and this is how a diagnostic can say
    /// so without the reader refusing it.
    pub reserved_expansion_is_zero: bool,
    /// The change counter the page count is valid for.
    pub version_valid_for: u32,
    /// The `SQLITE_VERSION_NUMBER` that last wrote the file.
    pub write_library_version: u32,
}

impl DatabaseHeader {
    /// Returns a header that describes nothing but a page size.
    ///
    /// It exists for the one moment where a page size is known and a header is
    /// not: a database whose page one is only readable through a write-ahead
    /// log, where the log's own header says how big a page is and the real
    /// header arrives with the first snapshot. Nothing should read any other
    /// field of it, and a zero page count is what makes that true - every page
    /// is out of range until the snapshot says otherwise.
    pub fn provisional(page_size: PageSize) -> DatabaseHeader {
        DatabaseHeader {
            page_size,
            write_version: 2,
            read_version: 2,
            reserved_bytes: 0,
            change_counter: 0,
            database_size: 0,
            freelist_head: 0,
            freelist_count: 0,
            schema_cookie: 0,
            schema_format: 4,
            cache_size: 0,
            largest_root: 0,
            text_encoding: TextEncoding::Utf8,
            user_version: 0,
            vacuum_mode: VacuumMode::None,
            application_id: 0,
            reserved_expansion_is_zero: true,
            version_valid_for: 0,
            write_library_version: 0,
        }
    }

    /// Decodes and validates a header.
    pub fn decode(raw: &[u8]) -> DbResult<DatabaseHeader> {
        let magic = bytes::window(raw, offsets::MAGIC, MAGIC.len())?;
        if magic != MAGIC {
            return Err(corrupt("the file does not start with the SQLite magic"));
        }
        let page_size = PageSize::from_encoded(bytes::read_u16(raw, offsets::PAGE_SIZE)?)?;
        let write_version = bytes::read_u8(raw, offsets::WRITE_VERSION)?;
        let read_version = bytes::read_u8(raw, offsets::READ_VERSION)?;
        if !matches!(write_version, 1 | 2) || !matches!(read_version, 1 | 2) {
            return Err(corrupt(format!(
                "unrecognised format versions: read {read_version}, write {write_version}"
            )));
        }
        let reserved_bytes = bytes::read_u8(raw, offsets::RESERVED_BYTES)?;
        // `usable` refuses a reserved count that leaves too little of the page.
        let _usable = page_size.usable(reserved_bytes)?;

        let max_fraction = bytes::read_u8(raw, offsets::MAX_FRACTION)?;
        let min_fraction = bytes::read_u8(raw, offsets::MIN_FRACTION)?;
        let leaf_fraction = bytes::read_u8(raw, offsets::LEAF_FRACTION)?;
        if max_fraction != MAX_EMBEDDED_FRACTION
            || min_fraction != MIN_EMBEDDED_FRACTION
            || leaf_fraction != LEAF_FRACTION
        {
            return Err(corrupt(format!(
                "payload fractions {max_fraction}/{min_fraction}/{leaf_fraction} are not the \
                 fixed 64/32/32"
            )));
        }

        let schema_format = bytes::read_u32(raw, offsets::SCHEMA_FORMAT)?;
        if !matches!(schema_format, 0..=4) {
            return Err(corrupt(format!(
                "schema format {schema_format} is not one of 1 through 4"
            )));
        }
        // The encoding field is masked rather than validated, because that is
        // what SQLite does with it; `TextEncoding::from_header_code` records
        // the reasoning and the differential case that found it.
        let text_encoding =
            TextEncoding::from_header_code(bytes::read_u32(raw, offsets::TEXT_ENCODING)?);

        // The twenty reserved-for-expansion bytes are documented as "must be
        // zero", and SQLite does not check them: 3.53.4 opens and reads a file
        // with rubbish there without complaint. Refusing it would mean
        // refusing a database SQLite is happy with, so they are read and
        // reported rather than enforced.
        let expansion = bytes::window(raw, offsets::RESERVED_EXPANSION, 20)?;
        let reserved_expansion_is_zero = expansion.iter().all(|byte| *byte == 0);

        let largest_root = bytes::read_u32(raw, offsets::LARGEST_ROOT)?;
        let incremental = bytes::read_u32(raw, offsets::INCREMENTAL_VACUUM)?;
        let vacuum_mode = match (largest_root, incremental) {
            (0, 0) => VacuumMode::None,
            (0, _) => {
                return Err(corrupt(
                    "incremental vacuum is set but no pointer map root is recorded",
                ))
            }
            (_, 0) => VacuumMode::Auto,
            _ => VacuumMode::Incremental,
        };

        Ok(DatabaseHeader {
            page_size,
            write_version,
            read_version,
            reserved_bytes,
            change_counter: bytes::read_u32(raw, offsets::CHANGE_COUNTER)?,
            database_size: bytes::read_u32(raw, offsets::DATABASE_SIZE)?,
            freelist_head: bytes::read_u32(raw, offsets::FREELIST_HEAD)?,
            freelist_count: bytes::read_u32(raw, offsets::FREELIST_COUNT)?,
            schema_cookie: bytes::read_u32(raw, offsets::SCHEMA_COOKIE)?,
            schema_format,
            cache_size: bytes::read_u32(raw, offsets::CACHE_SIZE)? as i32,
            largest_root,
            text_encoding,
            user_version: bytes::read_u32(raw, offsets::USER_VERSION)? as i32,
            vacuum_mode,
            application_id: bytes::read_u32(raw, offsets::APPLICATION_ID)? as i32,
            reserved_expansion_is_zero,
            version_valid_for: bytes::read_u32(raw, offsets::VERSION_VALID_FOR)?,
            write_library_version: bytes::read_u32(raw, offsets::WRITE_LIBRARY_VERSION)?,
        })
    }

    /// Writes the header back into a hundred-byte buffer.
    ///
    /// Encoding exists so that a decoded header can be proved to round-trip,
    /// and so that fixtures can be built. Phase 3 never writes one to a file.
    pub fn encode(&self, raw: &mut [u8]) -> DbResult<()> {
        let magic = bytes::window_mut(raw, offsets::MAGIC, MAGIC.len())?;
        magic.copy_from_slice(MAGIC);
        bytes::write_u16(raw, offsets::PAGE_SIZE, self.page_size.to_encoded())?;
        bytes::write_u8(raw, offsets::WRITE_VERSION, self.write_version)?;
        bytes::write_u8(raw, offsets::READ_VERSION, self.read_version)?;
        bytes::write_u8(raw, offsets::RESERVED_BYTES, self.reserved_bytes)?;
        bytes::write_u8(raw, offsets::MAX_FRACTION, MAX_EMBEDDED_FRACTION)?;
        bytes::write_u8(raw, offsets::MIN_FRACTION, MIN_EMBEDDED_FRACTION)?;
        bytes::write_u8(raw, offsets::LEAF_FRACTION, LEAF_FRACTION)?;
        bytes::write_u32(raw, offsets::CHANGE_COUNTER, self.change_counter)?;
        bytes::write_u32(raw, offsets::DATABASE_SIZE, self.database_size)?;
        bytes::write_u32(raw, offsets::FREELIST_HEAD, self.freelist_head)?;
        bytes::write_u32(raw, offsets::FREELIST_COUNT, self.freelist_count)?;
        bytes::write_u32(raw, offsets::SCHEMA_COOKIE, self.schema_cookie)?;
        bytes::write_u32(raw, offsets::SCHEMA_FORMAT, self.schema_format)?;
        bytes::write_u32(raw, offsets::CACHE_SIZE, self.cache_size as u32)?;
        bytes::write_u32(raw, offsets::LARGEST_ROOT, self.largest_root)?;
        bytes::write_u32(
            raw,
            offsets::TEXT_ENCODING,
            self.text_encoding.header_code(),
        )?;
        bytes::write_u32(raw, offsets::USER_VERSION, self.user_version as u32)?;
        bytes::write_u32(
            raw,
            offsets::INCREMENTAL_VACUUM,
            u32::from(self.vacuum_mode == VacuumMode::Incremental),
        )?;
        bytes::write_u32(raw, offsets::APPLICATION_ID, self.application_id as u32)?;
        let expansion = bytes::window_mut(raw, offsets::RESERVED_EXPANSION, 20)?;
        expansion.fill(0);
        bytes::write_u32(raw, offsets::VERSION_VALID_FOR, self.version_valid_for)?;
        bytes::write_u32(
            raw,
            offsets::WRITE_LIBRARY_VERSION,
            self.write_library_version,
        )?;
        Ok(())
    }

    /// Returns the usable bytes per page, after the reserved tail.
    pub fn usable_size(&self) -> DbResult<u32> {
        self.page_size.usable(self.reserved_bytes)
    }

    /// Returns the page count a reader should use.
    ///
    /// The header's count is authoritative only when the version-valid-for
    /// number matches the change counter, meaning it was written by the
    /// transaction that last changed the file. When it does not - a crash
    /// between growing the file and updating the header, or a file another
    /// tool truncated - the file's own length is what a reader may trust.
    pub fn effective_page_count(&self, file_bytes: u64) -> u32 {
        let from_file = page::page_count(self.page_size, file_bytes);
        if self.database_size != 0 && self.version_valid_for == self.change_counter {
            self.database_size
        } else {
            from_file
        }
    }

    /// Reports whether the header's page count may be believed.
    pub fn page_count_is_authoritative(&self) -> bool {
        self.database_size != 0 && self.version_valid_for == self.change_counter
    }

    /// Reports whether the file is in WAL mode.
    pub fn is_wal(&self) -> bool {
        self.write_version == 2 || self.read_version == 2
    }

    /// Checks the fields that can only be judged against the file's size.
    ///
    /// The structural checks live in `decode`, which has only the hundred
    /// bytes; these need the page count, which needs the file.
    pub fn validate_against_file(&self, file_bytes: u64) -> DbResult<()> {
        let pages = self.effective_page_count(file_bytes);
        if pages == 0 {
            return Err(corrupt(
                "a database with a header must have at least one page",
            ));
        }
        if self.freelist_head > pages {
            return Err(corrupt(format!(
                "the freelist head is page {} in a {pages}-page database",
                self.freelist_head
            )));
        }
        if self.freelist_count > pages {
            return Err(corrupt(format!(
                "the freelist claims {} pages in a {pages}-page database",
                self.freelist_count
            )));
        }
        if self.largest_root > pages {
            return Err(corrupt(format!(
                "the largest root is page {} in a {pages}-page database",
                self.largest_root
            )));
        }
        if self.freelist_head == 0 && self.freelist_count != 0 {
            return Err(corrupt(
                "the freelist has no head but claims a non-zero length",
            ));
        }
        Ok(())
    }

    /// Returns the pointer-map page that covers `page`, if the file has one.
    ///
    /// Under a vacuum mode, page 2 and every `entries_per_map + 1` page after
    /// it is a pointer map covering the pages that follow it.
    pub fn pointer_map_page(&self, page: PageId) -> DbResult<Option<PageId>> {
        if self.vacuum_mode == VacuumMode::None {
            return Ok(None);
        }
        let usable = self.usable_size()? as u64;
        let entries = usable / 5;
        if entries == 0 {
            return Err(corrupt("a page too small to hold a pointer-map entry"));
        }
        let page_number = u64::from(page.get());
        if page_number < 2 {
            return Ok(None);
        }
        // Page 2 is the first map; the pages it covers are 3..=entries+2.
        let group = (page_number - 2) / (entries + 1);
        let map_page = group * (entries + 1) + 2;
        if map_page == page_number {
            return Ok(None);
        }
        Ok(PageId::new(u32::try_from(map_page).map_err(|_| {
            corrupt("a pointer-map page past the range")
        })?))
    }

    /// Reports whether `page` is itself a pointer-map page.
    pub fn is_pointer_map_page(&self, page: PageId) -> DbResult<bool> {
        if self.vacuum_mode == VacuumMode::None {
            return Ok(false);
        }
        let usable = self.usable_size()? as u64;
        let entries = usable / 5;
        if entries == 0 {
            return Err(corrupt("a page too small to hold a pointer-map entry"));
        }
        let page_number = u64::from(page.get());
        if page_number < 2 {
            return Ok(false);
        }
        Ok((page_number - 2) % (entries + 1) == 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a header that decodes, so a test can spoil exactly one field.
    fn sample() -> [u8; HEADER_SIZE] {
        let mut raw = [0u8; HEADER_SIZE];
        let header = DatabaseHeader {
            page_size: PageSize::new(4096).unwrap(),
            write_version: 1,
            read_version: 1,
            reserved_bytes: 0,
            change_counter: 7,
            database_size: 3,
            freelist_head: 0,
            freelist_count: 0,
            schema_cookie: 1,
            schema_format: 4,
            cache_size: -2000,
            largest_root: 0,
            text_encoding: TextEncoding::Utf8,
            user_version: 0,
            vacuum_mode: VacuumMode::None,
            application_id: 0,
            reserved_expansion_is_zero: true,
            version_valid_for: 7,
            write_library_version: 3_053_004,
        };
        header.encode(&mut raw).unwrap();
        raw
    }

    /// Every field must survive an encode/decode round trip, or a field is
    /// being read from the wrong offset.
    #[test]
    fn every_header_field_round_trips() {
        let raw = sample();
        let decoded = DatabaseHeader::decode(&raw).unwrap();
        let mut again = [0u8; HEADER_SIZE];
        decoded.encode(&mut again).unwrap();
        assert_eq!(raw, again);
        assert_eq!(decoded.page_size.bytes(), 4096);
        assert_eq!(decoded.change_counter, 7);
        assert_eq!(decoded.cache_size, -2000);
        assert_eq!(decoded.schema_format, 4);
        assert_eq!(decoded.write_library_version, 3_053_004);
    }

    /// The offsets are the file format and are checked against it directly.
    #[test]
    fn the_field_offsets_match_the_file_format() {
        let raw = sample();
        assert_eq!(&raw[0..16], MAGIC);
        assert_eq!(u16::from_be_bytes([raw[16], raw[17]]), 4096);
        assert_eq!(raw[21], 64);
        assert_eq!(raw[22], 32);
        assert_eq!(raw[23], 32);
        assert_eq!(u32::from_be_bytes([raw[56], raw[57], raw[58], raw[59]]), 1);
        assert!(raw[72..92].iter().all(|byte| *byte == 0));
    }

    /// A page size of 65536 is encoded as 1, which is the one field in the
    /// header that does not mean what it says.
    #[test]
    fn the_largest_page_size_is_encoded_as_one() {
        let mut raw = sample();
        raw[16] = 0x00;
        raw[17] = 0x01;
        let decoded = DatabaseHeader::decode(&raw).unwrap();
        assert_eq!(decoded.page_size.bytes(), 65_536);
        assert_eq!(decoded.page_size.to_encoded(), 1);
    }

    /// Each structural lie in the header must be refused on its own.
    #[test]
    fn a_spoiled_header_field_is_refused() {
        let mut raw = sample();
        raw[0] = b'X';
        assert!(DatabaseHeader::decode(&raw).is_err());

        // A page size that is not a power of two.
        let mut raw = sample();
        raw[16] = 0x03;
        raw[17] = 0x00;
        assert!(DatabaseHeader::decode(&raw).is_err());

        // A page size below the minimum.
        let mut raw = sample();
        raw[16] = 0x01;
        raw[17] = 0x00;
        assert!(DatabaseHeader::decode(&raw).is_err());

        // An unrecognised read version.
        let mut raw = sample();
        raw[19] = 3;
        assert!(DatabaseHeader::decode(&raw).is_err());

        // Payload fractions the format fixes.
        for offset in [21usize, 22, 23] {
            let mut raw = sample();
            raw[offset] = raw[offset].wrapping_add(1);
            assert!(DatabaseHeader::decode(&raw).is_err(), "offset {offset}");
        }

        // A schema format past 4.
        let mut raw = sample();
        raw[47] = 5;
        assert!(DatabaseHeader::decode(&raw).is_err());

        // A text encoding past three is *not* refused: SQLite masks it, and
        // refusing would mean refusing a file SQLite reads. The encoding tests
        // in inillucent-value cover the masking itself.
        let mut raw = sample();
        raw[59] = 4;
        assert_eq!(
            DatabaseHeader::decode(&raw).unwrap().text_encoding,
            TextEncoding::Utf8
        );

        // Reserved expansion bytes that are not zero are *not* refused, and
        // are reported instead: SQLite reads such a file without complaint.
        let mut raw = sample();
        raw[80] = 1;
        let decoded = DatabaseHeader::decode(&raw).unwrap();
        assert!(!decoded.reserved_expansion_is_zero);
        assert!(
            DatabaseHeader::decode(&sample())
                .unwrap()
                .reserved_expansion_is_zero
        );

        // A reserved byte count that leaves too little usable page.
        let mut raw = sample();
        raw[20] = 255;
        raw[16] = 0x02;
        raw[17] = 0x00;
        assert!(DatabaseHeader::decode(&raw).is_err());
    }

    /// The page count is the header's only when the header was written by the
    /// transaction that last changed the file.
    #[test]
    fn the_page_count_follows_the_version_valid_for_rule() {
        let raw = sample();
        let header = DatabaseHeader::decode(&raw).unwrap();
        assert!(header.page_count_is_authoritative());
        // The header says three pages; a longer file does not change that.
        assert_eq!(header.effective_page_count(4096 * 10), 3);

        let mut raw = sample();
        // Break the match: the header's count is now not to be believed.
        raw[95] = 0;
        let header = DatabaseHeader::decode(&raw).unwrap();
        assert!(!header.page_count_is_authoritative());
        assert_eq!(header.effective_page_count(4096 * 10), 10);
        // A trailing partial page is not a page: the file ends, for a
        // reader, at the last whole page it can actually read.
        assert_eq!(header.effective_page_count(4096 * 10 + 1), 10);

        // A zero page count in the header always falls back to the file.
        let mut raw = sample();
        raw[28] = 0;
        raw[29] = 0;
        raw[30] = 0;
        raw[31] = 0;
        let header = DatabaseHeader::decode(&raw).unwrap();
        assert_eq!(header.effective_page_count(4096 * 5), 5);
    }

    /// Page numbers in the header must be inside the file they describe.
    #[test]
    fn out_of_range_page_numbers_are_refused_against_the_file() {
        let mut raw = sample();
        // A freelist head past the end of a three-page database.
        raw[35] = 99;
        let header = DatabaseHeader::decode(&raw).unwrap();
        assert!(header.validate_against_file(4096 * 3).is_err());

        let mut raw = sample();
        // A freelist count with no head.
        raw[39] = 2;
        let header = DatabaseHeader::decode(&raw).unwrap();
        assert!(header.validate_against_file(4096 * 3).is_err());

        let raw = sample();
        let header = DatabaseHeader::decode(&raw).unwrap();
        assert!(header.validate_against_file(4096 * 3).is_ok());

        // A file with no whole page in it has no pages, and a database with
        // no pages cannot be read from at all.
        let mut raw = sample();
        raw[95] = 0;
        let header = DatabaseHeader::decode(&raw).unwrap();
        assert!(!header.page_count_is_authoritative());
        assert!(header.validate_against_file(0).is_err());
    }

    /// The vacuum mode is read from two fields together, and the impossible
    /// combination is refused rather than guessed at.
    #[test]
    fn the_vacuum_mode_is_read_from_both_of_its_fields() {
        let raw = sample();
        assert_eq!(
            DatabaseHeader::decode(&raw).unwrap().vacuum_mode,
            VacuumMode::None
        );

        let mut raw = sample();
        raw[55] = 2; // largest root page 2
        assert_eq!(
            DatabaseHeader::decode(&raw).unwrap().vacuum_mode,
            VacuumMode::Auto
        );

        let mut raw = sample();
        raw[55] = 2;
        raw[67] = 1;
        assert_eq!(
            DatabaseHeader::decode(&raw).unwrap().vacuum_mode,
            VacuumMode::Incremental
        );

        // Incremental vacuum with no pointer map is impossible.
        let mut raw = sample();
        raw[67] = 1;
        assert!(DatabaseHeader::decode(&raw).is_err());
    }

    /// The pointer map covers a fixed run of pages after each map page, and
    /// page 1 and the map pages themselves are not covered.
    #[test]
    fn pointer_map_pages_land_where_the_format_says() {
        let mut raw = sample();
        raw[55] = 2;
        let header = DatabaseHeader::decode(&raw).unwrap();
        let entries = u64::from(header.usable_size().unwrap()) / 5;
        assert!(entries > 100);

        assert!(header.is_pointer_map_page(PageId::new(2).unwrap()).unwrap());
        assert!(!header.is_pointer_map_page(PageId::new(3).unwrap()).unwrap());
        assert!(!header.is_pointer_map_page(PageId::new(1).unwrap()).unwrap());

        let next_map = u32::try_from(entries + 3).unwrap();
        assert!(header
            .is_pointer_map_page(PageId::new(next_map).unwrap())
            .unwrap());
        assert_eq!(
            header.pointer_map_page(PageId::new(3).unwrap()).unwrap(),
            PageId::new(2)
        );
        assert_eq!(
            header
                .pointer_map_page(PageId::new(next_map - 1).unwrap())
                .unwrap(),
            PageId::new(2)
        );
        assert_eq!(
            header
                .pointer_map_page(PageId::new(next_map + 1).unwrap())
                .unwrap(),
            PageId::new(next_map)
        );
        // A map page has no map page of its own.
        assert_eq!(
            header.pointer_map_page(PageId::new(2).unwrap()).unwrap(),
            None
        );
        // Without a vacuum mode there is no pointer map at all.
        let plain = DatabaseHeader::decode(&sample()).unwrap();
        assert_eq!(
            plain.pointer_map_page(PageId::new(3).unwrap()).unwrap(),
            None
        );
    }

    /// Decoding arbitrary bytes must never panic and must almost always fail;
    /// the first hundred bytes of a file are entirely attacker-controlled.
    #[test]
    fn decoding_arbitrary_bytes_never_panics() {
        let mut state = 0xa5a5_5a5a_1234_4321u64;
        for _ in 0..50_000 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let mut raw = sample();
            let offset = (state % HEADER_SIZE as u64) as usize;
            raw[offset] = (state >> 32) as u8;
            let _ = DatabaseHeader::decode(&raw);
            let short = &raw[..(state % 101) as usize];
            let _ = DatabaseHeader::decode(short);
        }
    }
}
