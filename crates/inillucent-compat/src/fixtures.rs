//! The fixture corpus: databases SQLite created, and databases we then broke.
//!
//! Invariant: every valid fixture is written by the pinned SQLite 3.53.4
//! binary and by nothing else. A fixture inillucent produced would prove that
//! inillucent agrees with itself, which is the one thing a parity corpus must not
//! be able to say. The generator therefore drives the pinned shell, and the
//! checked-in files carry the SHA-256 of what that shell produced, so a
//! regenerated corpus that differs is a visible change rather than a silent
//! one.
//!
//! Malformed fixtures are the opposite: each one is a *named, single* edit to
//! a valid fixture, applied here in Rust, so that a test can say which lie it
//! is testing rather than pointing at an opaque blob. A corpus of randomly
//! damaged files tells you the reader did not crash; a corpus of one-field
//! lies tells you which field the reader checks.

use std::path::{Path, PathBuf};

/// One database the pinned SQLite shell creates.
#[derive(Clone, Debug)]
pub struct ValidFixture {
    /// The file name, without a directory.
    pub name: &'static str,
    /// What this fixture is for.
    pub purpose: &'static str,
    /// Shell dot-commands to run before the SQL, such as `.filectrl`.
    pub dot_commands: &'static [&'static str],
    /// The SQL that builds it.
    pub sql: &'static str,
    /// SQL to run after the body, for a fixture that needs a VACUUM to take.
    pub trailing_sql: &'static str,
}

/// How a valid fixture is damaged to make a malformed one.
#[derive(Clone, Debug)]
pub enum Damage {
    /// Replace the bytes at an absolute file offset.
    Bytes {
        /// Where in the file to write.
        offset: usize,
        /// What to write there.
        value: &'static [u8],
    },
    /// Replace bytes at an offset inside a page, one-based page number.
    InPage {
        /// The page to damage, counting from one.
        page: u32,
        /// The offset within that page.
        offset: usize,
        /// What to write there.
        value: &'static [u8],
    },
    /// Cut the file short.
    Truncate {
        /// How many bytes to keep.
        keep: usize,
    },
}

/// One deliberately broken database.
#[derive(Clone, Debug)]
pub struct MalformedFixture {
    /// The file name, without a directory.
    pub name: &'static str,
    /// The valid fixture this one is derived from.
    pub base: &'static str,
    /// The lie this fixture tells.
    pub lie: &'static str,
    /// Whether opening the database must fail, as opposed to failing later.
    pub fails_at_open: bool,
    /// Whether SQLite itself refuses this file.
    ///
    /// Not every lie is one SQLite catches: it masks the text-encoding field
    /// rather than validating it, so a file with an impossible encoding is one
    /// SQLite reads happily. inillucent has to agree, and a corpus entry that
    /// says so is more useful than one quietly left out.
    pub refused_by_sqlite: bool,
    /// The edit applied.
    pub damage: Damage,
}

/// The SQL every general-purpose fixture shares.
///
/// It exercises all five storage classes, both empty payloads, the integers
/// that need each serial-type width, the two zeroes, and text that is not
/// ASCII - so a single scan of one fixture touches most of the record codec.
const CORE_SQL: &str = "\
CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT, score REAL, tag BLOB, note);
INSERT INTO people VALUES (1, 'alpha', 1.5, x'00ff', 'first');
INSERT INTO people VALUES (2, 'Bravo', -0.0, x'', NULL);
INSERT INTO people VALUES (3, '', 0.0, x'0102030405', 'third');
INSERT INTO people VALUES (7, 'delta echo', 3.141592653589793, NULL, -9223372036854775808);
INSERT INTO people VALUES (9007199254740993, 'big rowid', 1e308, x'7f', 9223372036854775807);
INSERT INTO people VALUES (-5, 'negative rowid', -1e308, x'80', 0);
INSERT INTO people VALUES (100, 'h\u{e9}llo \u{2603} \u{1F600}', 0.1, x'ff00ff', 1);
CREATE TABLE widths (id INTEGER PRIMARY KEY, v);
INSERT INTO widths VALUES (1, 0);
INSERT INTO widths VALUES (2, 1);
INSERT INTO widths VALUES (3, -1);
INSERT INTO widths VALUES (4, 127);
INSERT INTO widths VALUES (5, -128);
INSERT INTO widths VALUES (6, 32767);
INSERT INTO widths VALUES (7, -32768);
INSERT INTO widths VALUES (8, 8388607);
INSERT INTO widths VALUES (9, -8388608);
INSERT INTO widths VALUES (10, 2147483647);
INSERT INTO widths VALUES (11, -2147483648);
INSERT INTO widths VALUES (12, 140737488355327);
INSERT INTO widths VALUES (13, -140737488355328);
INSERT INTO widths VALUES (14, 9223372036854775807);
INSERT INTO widths VALUES (15, -9223372036854775808);
INSERT INTO widths VALUES (16, 1.0);
INSERT INTO widths VALUES (17, -0.0);
INSERT INTO widths VALUES (18, '');
INSERT INTO widths VALUES (19, x'');
INSERT INTO widths VALUES (20, NULL);
CREATE INDEX people_by_name ON people (name);
CREATE INDEX people_by_score_desc ON people (score DESC, name);
CREATE INDEX people_nocase ON people (name COLLATE NOCASE);
";

/// Returns every valid fixture in the corpus.
pub fn valid_fixtures() -> Vec<ValidFixture> {
    vec![
        ValidFixture {
            name: "basic-p4096-utf8.db",
            purpose: "the default page size and encoding, with rowid tables and three indexes",
            dot_commands: &[],
            sql: CORE_SQL,
            trailing_sql: "",
        },
        ValidFixture {
            name: "basic-p512-utf8.db",
            purpose: "the smallest legal page size, which forces a deeper tree and more overflow",
            dot_commands: &[],
            sql: CORE_SQL,
            trailing_sql: "",
        },
        ValidFixture {
            name: "basic-p1024-utf8.db",
            purpose: "a mid page size, for comparison against the 512 and 4096 forms",
            dot_commands: &[],
            sql: CORE_SQL,
            trailing_sql: "",
        },
        ValidFixture {
            name: "basic-p65536-utf8.db",
            purpose: "the largest page size, whose header field is encoded as 1",
            dot_commands: &[],
            sql: CORE_SQL,
            trailing_sql: "",
        },
        ValidFixture {
            name: "basic-p1024-utf16le.db",
            purpose: "a UTF-16 little-endian database, where text serial types double in length",
            dot_commands: &[],
            sql: CORE_SQL,
            trailing_sql: "",
        },
        ValidFixture {
            name: "basic-p1024-utf16be.db",
            purpose: "a UTF-16 big-endian database",
            dot_commands: &[],
            sql: CORE_SQL,
            trailing_sql: "",
        },
        ValidFixture {
            name: "reserved-p4096-utf8.db",
            purpose: "32 reserved bytes per page, so usable size is not the page size",
            dot_commands: &[".filectrl reserve_bytes 32"],
            sql: CORE_SQL,
            // Reserved bytes take effect only for pages written afterwards, so
            // the whole file is rewritten to make every page carry the tail.
            trailing_sql: "VACUUM;",
        },
        ValidFixture {
            name: "overflow-p512-utf8.db",
            purpose: "payloads on both sides of the local threshold and across many overflow pages",
            dot_commands: &[],
            sql: "\
CREATE TABLE payloads (id INTEGER PRIMARY KEY, body TEXT, raw BLOB);
INSERT INTO payloads VALUES (1, hex(zeroblob(50)), zeroblob(50));
INSERT INTO payloads VALUES (2, hex(zeroblob(200)), zeroblob(200));
INSERT INTO payloads VALUES (3, hex(zeroblob(230)), zeroblob(230));
INSERT INTO payloads VALUES (4, hex(zeroblob(239)), zeroblob(239));
INSERT INTO payloads VALUES (5, hex(zeroblob(240)), zeroblob(240));
INSERT INTO payloads VALUES (6, hex(zeroblob(1000)), zeroblob(1000));
INSERT INTO payloads VALUES (7, hex(zeroblob(20000)), zeroblob(20000));
INSERT INTO payloads VALUES (8, hex(zeroblob(100000)), zeroblob(100000));
CREATE TABLE long_keys (k TEXT PRIMARY KEY, v);
INSERT INTO long_keys VALUES (hex(zeroblob(400)), 1);
INSERT INTO long_keys VALUES (hex(zeroblob(2000)), 2);
INSERT INTO long_keys VALUES (hex(zeroblob(9000)), 3);
",
            trailing_sql: "",
        },
        ValidFixture {
            name: "withoutrowid-p1024-utf8.db",
            purpose: "a WITHOUT ROWID table, which is an index B-tree used as a table",
            dot_commands: &[],
            sql: "\
CREATE TABLE kv (k TEXT PRIMARY KEY, v INTEGER, extra TEXT) WITHOUT ROWID;
INSERT INTO kv VALUES ('alpha', 1, 'a');
INSERT INTO kv VALUES ('bravo', 2, 'b');
INSERT INTO kv VALUES ('charlie', 3, NULL);
INSERT INTO kv VALUES ('', 4, 'empty key');
INSERT INTO kv VALUES ('zulu', 5, 'z');
CREATE TABLE pair (a INTEGER, b TEXT, c, PRIMARY KEY (a, b)) WITHOUT ROWID;
INSERT INTO pair VALUES (1, 'x', 'first');
INSERT INTO pair VALUES (1, 'y', 'second');
INSERT INTO pair VALUES (2, 'x', 'third');
CREATE INDEX kv_by_v ON kv (v);
",
            trailing_sql: "",
        },
        ValidFixture {
            name: "deep-p512-utf8.db",
            purpose: "enough rows at 512 bytes a page to build interior levels in table and index",
            dot_commands: &[],
            sql: "\
CREATE TABLE many (id INTEGER PRIMARY KEY, label TEXT, n INTEGER);
WITH RECURSIVE seq(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM seq WHERE i < 2000)
  INSERT INTO many SELECT i, 'row-' || printf('%06d', i), i * 7 FROM seq;
CREATE INDEX many_by_label ON many (label);
CREATE INDEX many_by_n ON many (n DESC);
",
            trailing_sql: "",
        },
        ValidFixture {
            name: "freelist-p1024-utf8.db",
            purpose: "a file with a populated freelist, from rows inserted and then deleted",
            dot_commands: &[],
            sql: "\
CREATE TABLE churn (id INTEGER PRIMARY KEY, body TEXT);
WITH RECURSIVE seq(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM seq WHERE i < 800)
  INSERT INTO churn SELECT i, hex(zeroblob(200)) FROM seq;
DELETE FROM churn WHERE id % 3 <> 0;
CREATE TABLE survivor (id INTEGER PRIMARY KEY, v);
INSERT INTO survivor VALUES (1, 'kept');
",
            trailing_sql: "",
        },
        ValidFixture {
            name: "autovacuum-p1024-utf8.db",
            purpose: "auto_vacuum FULL, which adds pointer-map pages and a largest-root field",
            dot_commands: &[],
            sql: "\
PRAGMA auto_vacuum = FULL;
CREATE TABLE av (id INTEGER PRIMARY KEY, body TEXT);
WITH RECURSIVE seq(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM seq WHERE i < 600)
  INSERT INTO av SELECT i, hex(zeroblob(150)) FROM seq;
DELETE FROM av WHERE id % 2 = 0;
CREATE INDEX av_by_body ON av (body);
",
            trailing_sql: "",
        },
        ValidFixture {
            name: "incrvacuum-p1024-utf8.db",
            purpose: "auto_vacuum INCREMENTAL, which sets the incremental-vacuum header field",
            dot_commands: &[],
            sql: "\
PRAGMA auto_vacuum = INCREMENTAL;
CREATE TABLE iv (id INTEGER PRIMARY KEY, body TEXT);
WITH RECURSIVE seq(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM seq WHERE i < 400)
  INSERT INTO iv SELECT i, hex(zeroblob(150)) FROM seq;
DELETE FROM iv WHERE id % 2 = 0;
PRAGMA incremental_vacuum(10);
",
            trailing_sql: "",
        },
        ValidFixture {
            name: "empty-p4096-utf8.db",
            purpose: "a database with a schema and no rows at all, and one with no schema either",
            dot_commands: &[],
            sql: "\
CREATE TABLE hollow (id INTEGER PRIMARY KEY, v TEXT);
CREATE INDEX hollow_by_v ON hollow (v);
",
            trailing_sql: "",
        },
        ValidFixture {
            name: "collations-p1024-utf8.db",
            purpose: "indexes under each built-in collation, over text that separates them",
            dot_commands: &[],
            sql: "\
CREATE TABLE words (id INTEGER PRIMARY KEY, w TEXT);
INSERT INTO words (w) VALUES ('abc'), ('ABC'), ('Abc'), ('abc '), ('abc  '), (' abc'), ('abd'), (''), ('  '), ('zz'), ('ZZ');
CREATE INDEX words_binary ON words (w);
CREATE INDEX words_nocase ON words (w COLLATE NOCASE);
CREATE INDEX words_rtrim ON words (w COLLATE RTRIM);
",
            trailing_sql: "",
        },
    ]
}

/// Returns every malformed fixture in the corpus.
///
/// Offsets are chosen against `basic-p4096-utf8.db`, whose header is the first
/// hundred bytes and whose page 2 is the first table's root.
pub fn malformed_fixtures() -> Vec<MalformedFixture> {
    vec![
        MalformedFixture {
            name: "bad-magic.db",
            base: "basic-p4096-utf8.db",
            lie: "the file does not start with the SQLite magic",
            fails_at_open: true,
            refused_by_sqlite: true,
            damage: Damage::Bytes {
                offset: 0,
                value: b"X",
            },
        },
        MalformedFixture {
            name: "bad-page-size.db",
            base: "basic-p4096-utf8.db",
            lie: "a page size that is not a power of two",
            fails_at_open: true,
            refused_by_sqlite: true,
            damage: Damage::Bytes {
                offset: 16,
                value: &[0x03, 0x00],
            },
        },
        MalformedFixture {
            name: "bad-read-version.db",
            base: "basic-p4096-utf8.db",
            lie: "a read format version the reader does not know",
            fails_at_open: true,
            refused_by_sqlite: true,
            damage: Damage::Bytes {
                offset: 19,
                value: &[0x03],
            },
        },
        MalformedFixture {
            name: "bad-payload-fraction.db",
            base: "basic-p4096-utf8.db",
            lie: "a maximum embedded payload fraction other than the fixed 64",
            fails_at_open: true,
            refused_by_sqlite: true,
            damage: Damage::Bytes {
                offset: 21,
                value: &[0x3f],
            },
        },
        MalformedFixture {
            name: "bad-schema-format.db",
            base: "basic-p4096-utf8.db",
            lie: "a schema format number past the four that exist",
            fails_at_open: true,
            refused_by_sqlite: true,
            damage: Damage::Bytes {
                offset: 44,
                value: &[0x00, 0x00, 0x00, 0x05],
            },
        },
        MalformedFixture {
            name: "masked-text-encoding.db",
            base: "basic-p4096-utf8.db",
            lie: "a text encoding of 4, which SQLite masks to UTF-8 rather than refusing",
            fails_at_open: false,
            refused_by_sqlite: false,
            damage: Damage::Bytes {
                offset: 56,
                value: &[0x00, 0x00, 0x00, 0x04],
            },
        },
        MalformedFixture {
            name: "dirty-reserved-expansion.db",
            base: "basic-p4096-utf8.db",
            lie: "reserved expansion bytes that are not zero, which SQLite does not check",
            fails_at_open: false,
            refused_by_sqlite: false,
            damage: Damage::Bytes {
                offset: 80,
                value: &[0x01],
            },
        },
        MalformedFixture {
            name: "freelist-head-out-of-range.db",
            base: "basic-p4096-utf8.db",
            lie: "a freelist head past the end of the file",
            fails_at_open: true,
            refused_by_sqlite: true,
            damage: Damage::Bytes {
                offset: 32,
                value: &[0x00, 0x0f, 0x42, 0x40],
            },
        },
        MalformedFixture {
            name: "bad-page-type.db",
            base: "basic-p4096-utf8.db",
            lie: "a page whose type byte is not one of the four",
            fails_at_open: false,
            refused_by_sqlite: true,
            damage: Damage::InPage {
                page: 2,
                offset: 0,
                value: &[0x03],
            },
        },
        MalformedFixture {
            name: "cell-count-too-large.db",
            base: "basic-p4096-utf8.db",
            lie: "a cell count whose pointer array cannot fit on the page",
            fails_at_open: false,
            refused_by_sqlite: true,
            damage: Damage::InPage {
                page: 2,
                offset: 3,
                value: &[0xff, 0xff],
            },
        },
        MalformedFixture {
            name: "content-start-in-header.db",
            base: "basic-p4096-utf8.db",
            lie: "a cell content area that overlaps the cell pointer array",
            fails_at_open: false,
            refused_by_sqlite: true,
            damage: Damage::InPage {
                page: 2,
                offset: 5,
                value: &[0x00, 0x02],
            },
        },
        MalformedFixture {
            name: "too-many-fragments.db",
            base: "basic-p4096-utf8.db",
            lie: "more fragmented free bytes than the sixty the format allows",
            fails_at_open: false,
            refused_by_sqlite: true,
            damage: Damage::InPage {
                page: 2,
                offset: 7,
                value: &[0xc8],
            },
        },
        MalformedFixture {
            name: "cell-pointer-out-of-range.db",
            base: "basic-p4096-utf8.db",
            lie: "a cell pointer that lands outside the content area",
            fails_at_open: false,
            refused_by_sqlite: true,
            damage: Damage::InPage {
                page: 2,
                offset: 8,
                value: &[0x00, 0x02],
            },
        },
        MalformedFixture {
            name: "freeblock-loop.db",
            base: "basic-p4096-utf8.db",
            lie: "a freeblock chain that points at itself",
            fails_at_open: false,
            refused_by_sqlite: true,
            damage: Damage::InPage {
                page: 2,
                offset: 1,
                value: &[0x03, 0xe8, 0x00, 0x00],
            },
        },
        MalformedFixture {
            name: "bad-schema-page-type.db",
            base: "basic-p4096-utf8.db",
            lie: "a schema root page whose type byte is not a B-tree page",
            fails_at_open: false,
            refused_by_sqlite: true,
            damage: Damage::InPage {
                page: 1,
                offset: 100,
                value: &[0x03],
            },
        },
        MalformedFixture {
            name: "truncated-mid-page.db",
            base: "basic-p4096-utf8.db",
            lie: "a file that stops in the middle of a page",
            fails_at_open: false,
            refused_by_sqlite: true,
            damage: Damage::Truncate { keep: 4096 + 2000 },
        },
        MalformedFixture {
            name: "truncated-header.db",
            base: "basic-p4096-utf8.db",
            lie: "a file too short to hold a header",
            fails_at_open: true,
            refused_by_sqlite: true,
            damage: Damage::Truncate { keep: 40 },
        },
    ]
}

/// Applies one damage to a file image.
pub fn apply_damage(image: &mut Vec<u8>, damage: &Damage, page_size: usize) -> Result<(), String> {
    match damage {
        Damage::Bytes { offset, value } => write_at(image, *offset, value),
        Damage::InPage {
            page,
            offset,
            value,
        } => {
            let base = (*page as usize)
                .checked_sub(1)
                .and_then(|index| index.checked_mul(page_size))
                .ok_or_else(|| "a page number of zero".to_string())?;
            write_at(image, base.saturating_add(*offset), value)
        }
        Damage::Truncate { keep } => {
            if *keep > image.len() {
                return Err(format!(
                    "cannot truncate a {}-byte image to {keep} bytes",
                    image.len()
                ));
            }
            image.truncate(*keep);
            Ok(())
        }
    }
}

/// Writes bytes into an image, refusing to write past its end.
fn write_at(image: &mut [u8], offset: usize, value: &[u8]) -> Result<(), String> {
    let end = offset
        .checked_add(value.len())
        .ok_or_else(|| "an offset that overflows".to_string())?;
    let window = image
        .get_mut(offset..end)
        .ok_or_else(|| format!("offset {offset}..{end} is outside the image"))?;
    window.copy_from_slice(value);
    Ok(())
}

/// Returns the directory the corpus lives in.
pub fn corpus_root(workspace: &Path) -> PathBuf {
    workspace.join("compat/fixtures")
}

/// Returns the page size a fixture's name declares.
///
/// The name carries it because the malformed fixtures are patched at page
/// offsets and the patcher must not have to open the file to know where a page
/// starts - a file whose header is the thing being damaged cannot be asked.
pub fn page_size_from_name(name: &str) -> Option<usize> {
    let marker = name.split("-p").nth(1)?;
    let digits: String = marker.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every fixture name must be unique, or one would overwrite another.
    #[test]
    fn fixture_names_are_unique() {
        let mut names: Vec<&str> = valid_fixtures().iter().map(|item| item.name).collect();
        names.extend(malformed_fixtures().iter().map(|item| item.name));
        let count = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), count, "a fixture name is used twice");
    }

    /// Every malformed fixture must name a valid fixture it derives from.
    #[test]
    fn every_malformed_fixture_has_a_base() {
        let valid: Vec<&str> = valid_fixtures().iter().map(|item| item.name).collect();
        for fixture in malformed_fixtures() {
            assert!(
                valid.contains(&fixture.base),
                "{} derives from an unknown fixture {}",
                fixture.name,
                fixture.base
            );
        }
    }

    /// The page size in a fixture's name is what the patcher uses to find a
    /// page, so it has to be readable from the name alone.
    #[test]
    fn page_sizes_are_readable_from_fixture_names() {
        assert_eq!(page_size_from_name("basic-p4096-utf8.db"), Some(4096));
        assert_eq!(page_size_from_name("basic-p512-utf8.db"), Some(512));
        assert_eq!(page_size_from_name("basic-p65536-utf8.db"), Some(65_536));
        assert_eq!(page_size_from_name("no-page-size.db"), None);
        for fixture in valid_fixtures() {
            assert!(
                page_size_from_name(fixture.name).is_some(),
                "{} does not declare its page size",
                fixture.name
            );
        }
    }

    /// Damage must never write outside the image it is given.
    #[test]
    fn damage_never_writes_past_the_end() {
        let mut image = vec![0u8; 100];
        assert!(apply_damage(
            &mut image,
            &Damage::Bytes {
                offset: 99,
                value: &[1, 2]
            },
            4096
        )
        .is_err());
        assert!(apply_damage(
            &mut image,
            &Damage::InPage {
                page: 5,
                offset: 0,
                value: &[1]
            },
            4096
        )
        .is_err());
        assert!(apply_damage(&mut image, &Damage::Truncate { keep: 200 }, 4096).is_err());
        assert!(apply_damage(
            &mut image,
            &Damage::Bytes {
                offset: 0,
                value: b"X"
            },
            4096
        )
        .is_ok());
        assert_eq!(image.first(), Some(&b'X'));
    }
}
