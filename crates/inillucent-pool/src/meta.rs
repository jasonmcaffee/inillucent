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

/// The format version this build writes.
///
/// **The compatibility rule, which `docs/relational-architecture.md` states in
/// full.** A point release reads every file an earlier point release of the
/// same minor version wrote, so this number does not move for a bug fix. A
/// change to the layout of a page, a record or this header raises it; a build
/// that meets a file with a higher number says so rather than reading it as
/// damage.
///
/// **Two, since task-2074, for two changes to the page.** A leaf's delta area
/// has a directory in key order, and a page's checksum covers its LSN - which
/// format 1's did not, so a flipped bit in a page's LSN was a page that read as
/// valid (task-2066 section 4.2, item 17). A build of format 1 cannot read
/// either, and the number is what makes it say so by name instead of reporting
/// a checksum failure on the first page it reads.
///
/// This build still reads format 1, which is [`OLDEST_FORMAT_VERSION`]: a leaf
/// says which layout it is in, and a page's checksum is accepted under either
/// rule. A format 1 file becomes format 2 the first time this build writes its
/// meta record, which a checkpoint does.
pub const FORMAT_VERSION: u32 = 2;

/// The oldest format version this build reads.
///
/// Every published release before task-2074 wrote format 1, and
/// `tests/interop/` holds a file from each of them that
/// `crates/inillucent-compat/tests/release_format.rs` reads with this build.
pub const OLDEST_FORMAT_VERSION: u32 = 1;

/// Reports whether this build reads a file of this format version.
///
/// @param found - the version a file's header carries
pub fn reads_format(found: u32) -> bool {
    (OLDEST_FORMAT_VERSION..=FORMAT_VERSION).contains(&found)
}

/// Returns the refusal a file of another format version reports.
///
/// **A newer file is not a damaged one (task-1979, E3).** Both answered
/// `corrupt`, which sends a reader looking for a torn page in a file that is
/// perfectly well formed and only newer than the build reading it. The newer
/// case carries `unsupported`, so the command line exits 3 and a driver reports
/// the status `unsupported` - the same answer every other "this build has not
/// got that" gives - and the message says what to do about it.
///
/// A *lower* number would mean a format this build has dropped, and there is
/// none: this build reads every format from version 1, the first. So a number
/// below [`OLDEST_FORMAT_VERSION`] is zero, and a zero here is a file whose
/// header was zeroed rather than a file from the past.
///
/// @param found - the version the file's header carries
pub(crate) fn wrong_format(found: u32) -> inillucent_base::DbError {
    if found > FORMAT_VERSION {
        return inillucent_base::error::refusal(format!(
            "this database is format version {found} and this build reads version \
             {FORMAT_VERSION}; upgrade inillucent to open it"
        ))
        .with_unsupported(format!("a database of format version {found}"));
    }
    corrupt(format!(
        "format version {found} is not one of {OLDEST_FORMAT_VERSION} to {FORMAT_VERSION}, and \
         there is no earlier format: the header has been overwritten"
    ))
}

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
    /// Where the reserved region begins, and where the checksum's tail starts.
    ///
    /// **This offset may not move.** The checksum is taken over `0..CHECKSUM`
    /// and `RESERVED..`, so changing it would make every file written by an
    /// earlier build fail its own checksum. Fields are added *inside* the
    /// region instead, at the three offsets below: a file written before they
    /// existed carries zeros there, which is what each of them means by
    /// "unset", and its checksum still verifies because the region was always
    /// covered.
    pub const RESERVED: usize = 92;
    /// The application's `PRAGMA user_version`, 4 bytes.
    pub const USER_VERSION: usize = 92;
    /// The application's `PRAGMA application_id`, 4 bytes.
    pub const APPLICATION_ID: usize = 96;
    /// The schema cookie `PRAGMA schema_version` reports, 4 bytes.
    pub const SCHEMA_COOKIE: usize = 100;
    /// Whether the database is in write-ahead-log mode, 4 bytes.
    ///
    /// **Only WAL is persisted, which is SQLite's rule.** Its header carries a
    /// read/write version of 2 for a WAL database and 1 for every other mode,
    /// so a reopen comes back in WAL and comes back at the connection default
    /// for anything else - `delete`, `truncate` and `persist` differ only in
    /// what happens to a file that is not there after a clean commit, and
    /// `memory` and `off` are a caller's decision about durability rather than
    /// a property of the file.
    ///
    /// Zero means "not WAL", which is what a file written before this field
    /// existed says, and is the right answer for one: those files were written
    /// by a build whose only mode was the log, and a rollback open of one is
    /// safe because recovery replays the log first either way.
    ///
    /// Whether the database is in write-ahead-log mode - see above.
    pub const WAL: usize = 104;
    /// The highest LSN any page in the file has ever been stamped with, 8 bytes.
    ///
    /// **A page's LSN has to be a position in the stream currently beside the
    /// file, and without this it is not always.** Recovery applies
    /// a record to a page only when the page's stamp is below the record's, so
    /// a page carrying a stamp from a stream that no longer exists swallows
    /// every later write to it: the record is skipped, the file stays
    /// structurally intact, and `PRAGMA integrity_check` answers `ok` about a
    /// row that is gone. Measured on the parked copy of Nikaya's mail database,
    /// where 24 log segments were moved aside to recover the file and the new
    /// stream re-used positions the pages already carried - page 3 is stamped
    /// 21,939,058,496 beside a log that ends at 21,075,008,440.
    ///
    /// The number is the highest stamp the pool has written, folded in at every
    /// checkpoint so it never goes backwards, and an open resumes the log above
    /// it. Zero means unset, which is what a file written before this field
    /// existed says; such a file resumes where it always did, and the refusal
    /// in `inillucent-wal`'s replay is what stands in front of it instead.
    ///
    /// The last field named here; the region continues at 116 for whatever
    /// comes next.
    pub const HIGH_WATER_LSN: usize = 108;
}

/// The smallest a meta page can be and still hold every field.
pub const META_BYTES: usize = at::RESERVED;

/// How many bytes at the front of a meta page the record itself occupies.
///
/// Everything after it is the zero padding [`Meta::encode`] writes over the
/// rest of the page, so two meta pages whose first `META_RECORD_BYTES` bytes
/// agree describe the same database. That is what lets a connection ask
/// "has another process folded since I last looked" by reading 116 bytes
/// instead of a whole page - see `Database::disk_record_is_as_last_read`,
/// where it was four 32 KiB reads and four crc32 passes over 32 KiB per
/// statement (task-2046).
///
/// **A field added to the reserved region has to move this.** The region
/// begins at [`at::RESERVED`] and the last field in it ends here;
/// `every_field_lives_below_the_record_length` fails when one is added past
/// it, because a check that read 116 bytes of a record 124 bytes long would
/// answer "unchanged" about a change it could not see.
pub const META_RECORD_BYTES: usize = at::HIGH_WATER_LSN + 8;

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
    /// What `PRAGMA user_version` reads and writes.
    ///
    /// **The engine stores none of it and reads none of it**, which is the
    /// point: it is four bytes an application owns, and every migration
    /// framework there is reads them to decide which migrations to run. SQLite
    /// keeps it at offset 60 of its own header for the same reason.
    pub user_version: i32,
    /// What `PRAGMA application_id` reads and writes, with the same contract.
    pub application_id: i32,
    /// What `PRAGMA schema_version` reports: one per schema change.
    pub schema_cookie: i32,
    /// Whether the database is in write-ahead-log mode.
    ///
    /// The one journal mode that outlives the connection that chose it - see
    /// `at::WAL`.
    pub wal: bool,
    /// The highest LSN any page in the file has ever been stamped with.
    ///
    /// The log resumes above it, so a page's stamp is always a position in the
    /// stream beside the file - see `at::HIGH_WATER_LSN`. Zero means unset.
    pub high_water_lsn: u64,
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
            user_version: 0,
            application_id: 0,
            schema_cookie: 0,
            wal: false,
            high_water_lsn: 0,
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
        put(page, at::USER_VERSION, &self.user_version.to_le_bytes())?;
        put(page, at::APPLICATION_ID, &self.application_id.to_le_bytes())?;
        put(page, at::SCHEMA_COOKIE, &self.schema_cookie.to_le_bytes())?;
        put(page, at::WAL, &u32::from(self.wal).to_le_bytes())?;
        // Written only when the page is long enough to hold it, so a page size
        // smaller than the region is still a legal meta page rather than an
        // encode that fails. `META_BYTES` is 92; a field at 108 is inside the
        // reserved region, and the region is only as long as the page.
        if page.len() >= at::HIGH_WATER_LSN.saturating_add(8) {
            put(page, at::HIGH_WATER_LSN, &self.high_water_lsn.to_le_bytes())?;
        }
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
        if !reads_format(format) {
            return Err(wrong_format(format));
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
            // Absent in a file written before these three existed, which reads
            // as zero - the value each of them starts at anyway.
            user_version: i32v(page, at::USER_VERSION),
            application_id: i32v(page, at::APPLICATION_ID),
            schema_cookie: i32v(page, at::SCHEMA_COOKIE),
            wal: i32v(page, at::WAL) != 0,
            high_water_lsn: u64_or_zero(page, at::HIGH_WATER_LSN),
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

/// Returns a 32-bit field from the reserved region, or zero when it is absent.
///
/// Zero rather than an error, because a page written before the field existed
/// is not a corrupt page - it is a page whose application never set a version.
///
/// @param page - the page bytes
/// @param offset - where the field sits
fn i32v(page: &[u8], offset: usize) -> i32 {
    let Some(slice) = page.get(offset..offset.saturating_add(4)) else {
        return 0;
    };
    let mut bytes = [0u8; 4];
    bytes.copy_from_slice(slice);
    i32::from_le_bytes(bytes)
}

/// Returns a 64-bit field from the reserved region, or zero when it is absent.
///
/// The same contract `i32v` has, and for the same reason: a page written before
/// the field existed is not a corrupt page, it is a page whose high water was
/// never recorded. Zero is what "never recorded" means everywhere it is read.
///
/// @param page - the page bytes
/// @param offset - where the field sits
fn u64_or_zero(page: &[u8], offset: usize) -> u64 {
    let Some(slice) = page.get(offset..offset.saturating_add(8)) else {
        return 0;
    };
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(slice);
    u64::from_le_bytes(bytes)
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
            // Non-zero, so the round trip proves the three application fields
            // are written and read rather than defaulted on both sides.
            user_version: 42,
            application_id: -7,
            schema_cookie: 3,
            // And the journal-mode flag, for the same reason.
            wal: true,
            // And the high water, which is the field a reopen resumes above.
            high_water_lsn: 900_000,
        };
        let mut page = vec![0u8; 32_768];
        meta.encode(&mut page).unwrap();
        assert_eq!(Meta::decode(&page).unwrap(), meta);
    }

    /// Every field a record carries is written below [`META_RECORD_BYTES`],
    /// so a comparison that reads only that many bytes sees every change.
    ///
    /// **Written as a sweep over the fields rather than as a length
    /// assertion**, because the number this protects is not the constant but
    /// the claim behind it: a field added to the reserved region past the
    /// constant would leave a change that a staleness check reading
    /// `META_RECORD_BYTES` bytes could not see, and would report a database
    /// another process had folded as unchanged.
    #[test]
    fn every_field_lives_below_the_record_length() {
        let base = Meta {
            page_size: 8_192,
            page_count: 4_096,
            catalog_root: PageId(7),
            free_map: PageId(2),
            checkpoint_lsn: 900_001,
            cts_watermark: 42,
            wal_sequence: 3,
            generation: 11,
            uuid: 0x0123_4567_89ab_cdef_0123_4567_89ab_cdef,
            user_version: 42,
            application_id: -7,
            schema_cookie: 3,
            wal: true,
            high_water_lsn: 900_000,
        };
        let moved: [(&str, Meta); 14] = [
            (
                "page_size",
                Meta {
                    page_size: 4_096,
                    ..base
                },
            ),
            (
                "page_count",
                Meta {
                    page_count: 4_097,
                    ..base
                },
            ),
            (
                "catalog_root",
                Meta {
                    catalog_root: PageId(8),
                    ..base
                },
            ),
            (
                "free_map",
                Meta {
                    free_map: PageId(3),
                    ..base
                },
            ),
            (
                "checkpoint_lsn",
                Meta {
                    checkpoint_lsn: 900_002,
                    ..base
                },
            ),
            (
                "cts_watermark",
                Meta {
                    cts_watermark: 43,
                    ..base
                },
            ),
            (
                "wal_sequence",
                Meta {
                    wal_sequence: 4,
                    ..base
                },
            ),
            (
                "generation",
                Meta {
                    generation: 12,
                    ..base
                },
            ),
            ("uuid", Meta { uuid: 1, ..base }),
            (
                "user_version",
                Meta {
                    user_version: 43,
                    ..base
                },
            ),
            (
                "application_id",
                Meta {
                    application_id: -8,
                    ..base
                },
            ),
            (
                "schema_cookie",
                Meta {
                    schema_cookie: 4,
                    ..base
                },
            ),
            ("wal", Meta { wal: false, ..base }),
            (
                "high_water_lsn",
                Meta {
                    high_water_lsn: 900_001,
                    ..base
                },
            ),
        ];
        let mut original = vec![0u8; 8_192];
        base.encode(&mut original).unwrap();
        for (field, other) in moved {
            let mut page = vec![0u8; 8_192];
            other.encode(&mut page).unwrap();
            assert_ne!(
                original.get(..META_RECORD_BYTES),
                page.get(..META_RECORD_BYTES),
                "moving {field} changed no byte below META_RECORD_BYTES, so a \
                 staleness check reading that many bytes cannot see it"
            );
        }
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
        slot.copy_from_slice(&(FORMAT_VERSION + 1).to_le_bytes());
        let error = Meta::decode(&page).unwrap_err();
        assert!(error.detail().unwrap_or("").contains("format"), "{error:?}");
    }

    /// A meta record format 1 wrote is read, and the next one written says 2.
    ///
    /// Every release before task-2074 wrote format 1, and this build reads
    /// their files; the version moves when this build writes the record, which
    /// is the first checkpoint.
    #[test]
    fn a_format_one_record_is_read_and_rewritten_as_format_two() {
        let mut page = vec![0u8; 8_192];
        Meta::fresh(8_192, 1).encode(&mut page).unwrap();
        put(&mut page, at::FORMAT, &OLDEST_FORMAT_VERSION.to_le_bytes()).unwrap();
        let sum = checksum(&page).unwrap();
        put(&mut page, at::CHECKSUM, &sum.to_le_bytes()).unwrap();
        let read = Meta::decode(&page).expect("a format 1 record reads");
        let mut again = vec![0u8; 8_192];
        read.encode(&mut again).unwrap();
        assert_eq!(u32(&again, at::FORMAT).unwrap(), FORMAT_VERSION);
        assert!(reads_format(1) && reads_format(2));
        assert!(!reads_format(0) && !reads_format(3));
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

    /// A meta page whose reserved region is all zeros - which is every file
    /// written before the field existed - reads a high water of zero.
    ///
    /// The compatibility claim as a test rather than as a comment: zero is what
    /// "never recorded" means, and an open that reads it resumes where it
    /// always did.
    #[test]
    fn a_file_written_before_the_high_water_reads_zero() {
        let mut page = vec![0u8; 8_192];
        let mut meta = Meta::fresh(8_192, 3);
        meta.high_water_lsn = 77;
        meta.encode(&mut page).unwrap();
        assert_eq!(Meta::decode(&page).unwrap().high_water_lsn, 77);
        // Now blank the field the way a page written by the earlier build
        // carries it, and re-checksum: the record still decodes, and the high
        // water reads as unset rather than as damage.
        let slot = page
            .get_mut(at::HIGH_WATER_LSN..at::HIGH_WATER_LSN + 8)
            .expect("the field is inside the page");
        slot.copy_from_slice(&0u64.to_le_bytes());
        let sum = checksum(&page).unwrap();
        let slot = page
            .get_mut(at::CHECKSUM..at::CHECKSUM + 4)
            .expect("the checksum is inside the page");
        slot.copy_from_slice(&sum.to_le_bytes());
        assert_eq!(Meta::decode(&page).unwrap().high_water_lsn, 0);
    }

    /// Flipping any byte of the high water is detected, which is what says the
    /// field is inside the checksum's range rather than beside it.
    #[test]
    fn corrupting_the_high_water_is_detected() {
        let mut meta = Meta::fresh(8_192, 1);
        meta.high_water_lsn = 21_939_058_496;
        let mut page = vec![0u8; 8_192];
        meta.encode(&mut page).unwrap();
        for index in at::HIGH_WATER_LSN..at::HIGH_WATER_LSN + 8 {
            let mut damaged = page.clone();
            let byte = damaged.get_mut(index).expect("the index is in the page");
            *byte ^= 0xFF;
            assert!(
                Meta::decode(&damaged).is_err(),
                "flipping byte {index} of the high water went unnoticed"
            );
        }
    }

    /// A fresh database claims the two meta pages and nothing else.
    #[test]
    fn a_fresh_database_has_two_pages() {
        let meta = Meta::fresh(16_384, 7);
        assert_eq!(meta.page_count, 2);
        assert_eq!(meta.generation, 1);
        assert!(meta.catalog_root.is_none());
        assert!(meta.free_map.is_none());
        assert_eq!(meta.high_water_lsn, 0);
        assert_eq!(FIRST_DATA_PAGE, PageId(2));
        assert_eq!(META_PAGE, PageId(0));
        assert_eq!(SHADOW_PAGE, PageId(1));
    }
}
