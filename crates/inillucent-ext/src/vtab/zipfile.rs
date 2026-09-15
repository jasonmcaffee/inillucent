//! `zipfile`: a ZIP archive as a table, read and written.
//!
//! Invariant: **what this reads is what PKWARE's format says, and what it
//! writes is what every unzipper accepts.** An archive is an interchange
//! format - the whole reason to have one is that something else opens it - so
//! the reader has to accept a member any compressor produced and the writer has
//! to produce one any reader takes. Both halves are checked by a round trip
//! against the reference's own archives.
//!
//! ```text
//! SELECT name, sz, method FROM zipfile('release.zip');
//! CREATE VIRTUAL TABLE z USING zipfile('out.zip');
//! INSERT INTO z(name, mode, mtime, data) VALUES ('a.txt', 33188, 0, 'hello');
//! ```
//!
//! The columns are the reference's, in its order: `name`, `mode`, `mtime`,
//! `sz`, `rawdata`, `data`, `method`, and the hidden `z` that names the file
//! when the table is used as a function.
//!
//! Two members of the format are deliberately narrow, and each is narrow the
//! way the reference is: **stored and deflated** are the two methods, because
//! they are the two anything writes; and the archive is **rewritten whole** on
//! commit rather than appended to, because a zip's directory is at the end and
//! an append that crashed halfway would leave a file with two of them.

use std::sync::{Arc, Mutex};

use inillucent_base::checksum::crc32;
use inillucent_base::deflate::{deflate, inflate};
use inillucent_base::{error::misuse, DbResult};
use inillucent_value::Value;

use super::{
    Change, ConstraintOp, Context, Declaration, DeclaredColumn, FilterPlan, IndexQuery, Module,
    ModuleArguments, ShadowTable, VirtualCursor, VirtualTable,
};

/// The signature at the head of a central-directory entry.
const CENTRAL_SIGNATURE: u32 = 0x0201_4b50;
/// The signature at the head of a local file header.
const LOCAL_SIGNATURE: u32 = 0x0403_4b50;
/// The signature at the head of the end-of-central-directory record.
const END_SIGNATURE: u32 = 0x0605_4b50;
/// How long the end-of-central-directory record is without its comment.
const END_LENGTH: usize = 22;
/// The method number for a member stored without compression.
const STORED: i64 = 0;
/// The method number for a deflated member.
const DEFLATED: i64 = 8;

/// One member of an archive.
#[derive(Clone, Debug)]
struct Member {
    /// The path inside the archive.
    name: String,
    /// The POSIX mode, from the external attributes.
    mode: i64,
    /// Seconds since the epoch, from the DOS date and time.
    mtime: i64,
    /// How many bytes the content is.
    size: i64,
    /// The bytes as they are stored, compressed or not.
    raw: Vec<u8>,
    /// Which compression method the raw bytes are in.
    method: i64,
}

impl Member {
    /// Returns the member's content, decompressing it when it is deflated.
    fn content(&self) -> DbResult<Vec<u8>> {
        match self.method {
            DEFLATED => inflate(&self.raw),
            _ => Ok(self.raw.clone()),
        }
    }
}

/// The `zipfile` module.
pub struct ZipFileModule;

impl Module for ZipFileModule {
    /// Returns the module's name.
    fn name(&self) -> &str {
        "zipfile"
    }

    /// It reads an archive named as an argument, so the name is the table.
    fn eponymous(&self) -> bool {
        true
    }

    /// And a `CREATE VIRTUAL TABLE` names the archive it writes.
    fn constructible(&self) -> bool {
        true
    }

    /// No shadow tables: the rows are the archive's.
    fn shadow_tables(&self, _arguments: &ModuleArguments) -> DbResult<Vec<ShadowTable>> {
        Ok(Vec::new())
    }

    /// Connects, remembering the file when one was named.
    fn connect(
        &self,
        arguments: &ModuleArguments,
        _creating: bool,
    ) -> DbResult<Box<dyn VirtualTable>> {
        let named = arguments
            .arguments
            .first()
            .map(|held| unquote(&String::from_utf8_lossy(held)));
        Ok(Box::new(ZipFileTable {
            declaration: Declaration {
                columns: vec![
                    DeclaredColumn::visible("name"),
                    DeclaredColumn::visible("mode"),
                    DeclaredColumn::visible("mtime"),
                    DeclaredColumn::visible("sz"),
                    DeclaredColumn::visible("rawdata"),
                    DeclaredColumn::visible("data"),
                    DeclaredColumn::visible("method"),
                    DeclaredColumn::hidden("z"),
                ],
                without_rowid: false,
            },
            path: named,
            pending: Arc::new(Mutex::new(Vec::new())),
            loaded: Mutex::new(false),
        }))
    }
}

/// Returns an argument with any surrounding quotes removed.
///
/// @param written - the argument as it was written
fn unquote(written: &str) -> String {
    let text = written.trim();
    for quote in ['\'', '"', '`'] {
        if text.len() >= 2 && text.starts_with(quote) && text.ends_with(quote) {
            return text[1..text.len() - 1].to_string();
        }
    }
    text.to_string()
}

/// One connected archive.
struct ZipFileTable {
    declaration: Declaration,
    /// The file the archive lives in, when the table names one.
    path: Option<String>,
    /// The members, shared with the cursors this table opens.
    pending: Arc<Mutex<Vec<Member>>>,
    /// Whether the file has been read into `pending` yet.
    loaded: Mutex<bool>,
}

/// Which hidden column carries the archive's name.
const FILE_COLUMN: i32 = 7;

impl ZipFileTable {
    /// Reads the archive into memory, once.
    ///
    /// An archive that is not there yet is an empty one, which is what makes
    /// `CREATE VIRTUAL TABLE z USING zipfile('new.zip')` followed by an
    /// `INSERT` the way an archive is built.
    fn load(&self) -> DbResult<()> {
        let Ok(mut loaded) = self.loaded.lock() else {
            return Err(misuse("zipfile: the archive's state is poisoned"));
        };
        if *loaded {
            return Ok(());
        }
        *loaded = true;
        let Some(path) = &self.path else {
            return Ok(());
        };
        let Ok(bytes) = std::fs::read(path) else {
            return Ok(());
        };
        let Ok(mut held) = self.pending.lock() else {
            return Err(misuse("zipfile: the archive's members are poisoned"));
        };
        *held = read_archive(&bytes)?;
        Ok(())
    }
}

impl VirtualTable for ZipFileTable {
    /// Returns the seven columns and the hidden file name.
    fn declaration(&self) -> &Declaration {
        &self.declaration
    }

    /// Claims the file name and nothing else.
    fn best_index(&self, query: &mut IndexQuery) -> DbResult<()> {
        for index in 0..query.constraints.len() {
            let Some(constraint) = query.constraints.get(index).copied() else {
                continue;
            };
            if constraint.usable
                && constraint.op == ConstraintOp::Eq
                && constraint.column == FILE_COLUMN
            {
                query.use_constraint(index, true);
            }
        }
        query.index_number = 0;
        query.estimated_cost = 100.0;
        query.estimated_rows = 100;
        Ok(())
    }

    /// Opens a cursor over the members.
    fn open(&self) -> DbResult<Box<dyn VirtualCursor>> {
        Ok(Box::new(ZipFileCursor {
            table: Arc::clone(&self.pending),
            named: self.path.clone(),
            rows: Vec::new(),
            at: 0,
        }))
    }

    /// Applies one insert, update or delete to the member list.
    fn update(&mut self, _context: &mut Context<'_>, change: &Change) -> DbResult<Option<i64>> {
        self.load()?;
        match change {
            Change::Delete(key) => {
                let name = text_of(key).unwrap_or_default();
                if let Ok(mut members) = self.pending.lock() {
                    members.retain(|held| held.name != name);
                }
                Ok(None)
            }
            Change::Insert { values, .. } | Change::Update { values, .. } => {
                let member = member_of(values)?;
                let Ok(mut held) = self.pending.lock() else {
                    return Err(misuse("zipfile: the archive's members are poisoned"));
                };
                held.retain(|other| other.name != member.name);
                held.push(member);
                Ok(None)
            }
        }
    }

    /// Writes the archive out.
    ///
    /// **Rewritten whole, not appended to.** A zip's directory is at the end of
    /// the file, so an append that failed halfway would leave a file with a
    /// directory that does not describe it. Writing beside the target and
    /// renaming is the same publish-by-rename the rest of this engine uses.
    fn sync(&mut self, _context: &mut Context<'_>) -> DbResult<()> {
        let Some(path) = self.path.clone() else {
            return Ok(());
        };
        if !self.loaded.lock().map(|held| *held).unwrap_or(false) {
            return Ok(());
        }
        let Ok(members) = self.pending.lock() else {
            return Err(misuse("zipfile: the archive's members are poisoned"));
        };
        let bytes = write_archive(&members)?;
        let beside = format!("{path}.partial");
        std::fs::write(&beside, &bytes)
            .map_err(|error| misuse(format!("zipfile: cannot write {path}: {error}")))?;
        std::fs::rename(&beside, &path)
            .map_err(|error| misuse(format!("zipfile: cannot publish {path}: {error}")))?;
        Ok(())
    }
}

/// Returns the member a written row describes.
///
/// @param values - one value per declared column
fn member_of(values: &[Value<'static>]) -> DbResult<Member> {
    let name = values
        .first()
        .and_then(text_of)
        .ok_or_else(|| misuse("zipfile: a member needs a name"))?;
    let mode = values
        .get(1)
        .and_then(Value::as_integer)
        .unwrap_or(0o100_644);
    let mtime = values.get(2).and_then(Value::as_integer).unwrap_or(0);
    // `data` is the content and `rawdata` is what is stored. A row that gave
    // neither is a directory entry, which is a member with no bytes.
    let content = values.get(5).and_then(bytes_of).unwrap_or_default();
    let packed = deflate(&content);
    let (raw, method) = if packed.len() < content.len() {
        (packed, DEFLATED)
    } else {
        (content.clone(), STORED)
    };
    Ok(Member {
        name,
        mode,
        mtime,
        size: content.len() as i64,
        raw,
        method,
    })
}

/// A walk over an archive's members.
struct ZipFileCursor {
    /// The table's members, shared so a write is visible to a later read.
    table: Arc<Mutex<Vec<Member>>>,
    /// The file the table names, when it names one.
    named: Option<String>,
    /// The members this scan is over.
    rows: Vec<Member>,
    at: usize,
}

impl VirtualCursor for ZipFileCursor {
    /// Reads the archive the argument names, or the table's own.
    fn filter(&mut self, _context: &mut Context<'_>, plan: &FilterPlan) -> DbResult<()> {
        self.at = 0;
        let argument = plan.arguments.first().and_then(text_of);
        let path = argument.or_else(|| self.named.clone());
        let held = self
            .table
            .lock()
            .map(|members| members.clone())
            .unwrap_or_default();
        let Some(path) = path else {
            self.rows = held;
            return Ok(());
        };
        // The table's own members win when it has any: a `CREATE VIRTUAL TABLE`
        // that has been written to has not been flushed yet, and a read that
        // went to the file would not see the write.
        if !held.is_empty() {
            self.rows = held;
            return Ok(());
        }
        let bytes = std::fs::read(&path)
            .map_err(|_| misuse(format!("zipfile: cannot open file: {path}")))?;
        self.rows = read_archive(&bytes)?;
        Ok(())
    }

    /// Steps to the next member.
    fn next(&mut self, _context: &mut Context<'_>) -> DbResult<()> {
        self.at = self.at.saturating_add(1);
        Ok(())
    }

    /// Returns whether the walk is finished.
    fn eof(&self) -> bool {
        self.at >= self.rows.len()
    }

    /// Returns one column of the current member.
    fn column(&mut self, _context: &mut Context<'_>, index: usize) -> DbResult<Value<'static>> {
        let Some(member) = self.rows.get(self.at) else {
            return Ok(Value::Null);
        };
        Ok(match index {
            0 => Value::owned_text(member.name.as_bytes())?,
            1 => Value::Integer(member.mode),
            2 => Value::Integer(member.mtime),
            3 => Value::Integer(member.size),
            4 => Value::owned_blob(&member.raw)?,
            5 => Value::owned_blob(&member.content()?)?,
            6 => Value::Integer(member.method),
            _ => Value::Null,
        })
    }

    /// Returns the member's position in the archive.
    fn rowid(&self) -> DbResult<i64> {
        Ok(self.at as i64)
    }
}

/// Returns every member an archive holds.
///
/// The central directory is the authority - it is what an unzipper reads - and
/// the local header is used only for where the data starts, because its own
/// sizes may be zero when the member was written by a streaming writer.
///
/// @param bytes - the whole archive
fn read_archive(bytes: &[u8]) -> DbResult<Vec<Member>> {
    let Some(end) = find_end(bytes) else {
        return Err(misuse("zipfile: this is not a zip archive"));
    };
    let count = usize::from(word(bytes, end.saturating_add(10)));
    let mut at = long(bytes, end.saturating_add(16)) as usize;
    let mut members = Vec::with_capacity(count);
    for _ in 0..count {
        if long(bytes, at) != CENTRAL_SIGNATURE {
            break;
        }
        let method = i64::from(word(bytes, at.saturating_add(10)));
        let time = word(bytes, at.saturating_add(12));
        let date = word(bytes, at.saturating_add(14));
        let compressed = long(bytes, at.saturating_add(20)) as usize;
        let size = long(bytes, at.saturating_add(24)) as i64;
        let name_len = usize::from(word(bytes, at.saturating_add(28)));
        let extra_len = usize::from(word(bytes, at.saturating_add(30)));
        let comment_len = usize::from(word(bytes, at.saturating_add(32)));
        let external = long(bytes, at.saturating_add(38));
        let local = long(bytes, at.saturating_add(42)) as usize;
        let name = String::from_utf8_lossy(
            bytes
                .get(at.saturating_add(46)..at.saturating_add(46).saturating_add(name_len))
                .unwrap_or_default(),
        )
        .into_owned();
        // The data begins after the *local* header, whose name and extra fields
        // may differ in length from the directory's.
        let local_name = usize::from(word(bytes, local.saturating_add(26)));
        let local_extra = usize::from(word(bytes, local.saturating_add(28)));
        let from = local
            .saturating_add(30)
            .saturating_add(local_name)
            .saturating_add(local_extra);
        let raw = bytes
            .get(from..from.saturating_add(compressed))
            .unwrap_or_default()
            .to_vec();
        members.push(Member {
            name,
            mode: mode_of(external),
            mtime: epoch_of(date, time),
            size,
            raw,
            method,
        });
        at = at
            .saturating_add(46)
            .saturating_add(name_len)
            .saturating_add(extra_len)
            .saturating_add(comment_len);
    }
    Ok(members)
}

/// Returns the archive bytes a member list becomes.
///
/// @param members - the members, in the order they should appear
fn write_archive(members: &[Member]) -> DbResult<Vec<u8>> {
    let mut out: Vec<u8> = Vec::new();
    let mut directory: Vec<u8> = Vec::new();
    for member in members {
        let offset = out.len() as u32;
        let (date, time) = dos_of(member.mtime);
        let crc = crc32(&member.content()?);
        let name = member.name.as_bytes();
        out.extend_from_slice(&LOCAL_SIGNATURE.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(member.method as u16).to_le_bytes());
        out.extend_from_slice(&time.to_le_bytes());
        out.extend_from_slice(&date.to_le_bytes());
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&(member.raw.len() as u32).to_le_bytes());
        out.extend_from_slice(&(member.size as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name);
        out.extend_from_slice(&member.raw);

        directory.extend_from_slice(&CENTRAL_SIGNATURE.to_le_bytes());
        directory.extend_from_slice(&0x031eu16.to_le_bytes());
        directory.extend_from_slice(&20u16.to_le_bytes());
        directory.extend_from_slice(&0u16.to_le_bytes());
        directory.extend_from_slice(&(member.method as u16).to_le_bytes());
        directory.extend_from_slice(&time.to_le_bytes());
        directory.extend_from_slice(&date.to_le_bytes());
        directory.extend_from_slice(&crc.to_le_bytes());
        directory.extend_from_slice(&(member.raw.len() as u32).to_le_bytes());
        directory.extend_from_slice(&(member.size as u32).to_le_bytes());
        directory.extend_from_slice(&(name.len() as u16).to_le_bytes());
        directory.extend_from_slice(&0u16.to_le_bytes());
        directory.extend_from_slice(&0u16.to_le_bytes());
        directory.extend_from_slice(&0u16.to_le_bytes());
        directory.extend_from_slice(&0u16.to_le_bytes());
        directory.extend_from_slice(&(((member.mode as u32) & 0xffff) << 16).to_le_bytes());
        directory.extend_from_slice(&offset.to_le_bytes());
        directory.extend_from_slice(name);
    }
    let start = out.len() as u32;
    let size = directory.len() as u32;
    out.extend_from_slice(&directory);
    out.extend_from_slice(&END_SIGNATURE.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&(members.len() as u16).to_le_bytes());
    out.extend_from_slice(&(members.len() as u16).to_le_bytes());
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&start.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    Ok(out)
}

/// Returns where the end-of-central-directory record begins.
///
/// Searched backwards, because the record is at the end of the file and may be
/// followed by a comment of up to 64 KiB.
///
/// @param bytes - the whole archive
fn find_end(bytes: &[u8]) -> Option<usize> {
    if bytes.len() < END_LENGTH {
        return None;
    }
    let last = bytes.len().saturating_sub(END_LENGTH);
    let earliest = last.saturating_sub(65_535);
    (earliest..=last)
        .rev()
        .find(|&at| long(bytes, at) == END_SIGNATURE)
}

/// Returns a little-endian sixteen-bit field.
fn word(bytes: &[u8], at: usize) -> u16 {
    let low = bytes.get(at).copied().unwrap_or(0);
    let high = bytes.get(at.saturating_add(1)).copied().unwrap_or(0);
    u16::from_le_bytes([low, high])
}

/// Returns a little-endian thirty-two-bit field.
fn long(bytes: &[u8], at: usize) -> u32 {
    let mut held = [0u8; 4];
    for (step, slot) in held.iter_mut().enumerate() {
        *slot = bytes.get(at.saturating_add(step)).copied().unwrap_or(0);
    }
    u32::from_le_bytes(held)
}

/// Returns the POSIX mode a member's external attributes carry.
///
/// The upper sixteen bits are the Unix mode when the archive was written on a
/// Unix-like system, and zero when it was not; a member with none is reported
/// as an ordinary readable file, which is what the reference does.
///
/// @param external - the external attributes field
fn mode_of(external: u32) -> i64 {
    let mode = i64::from(external >> 16);
    if mode == 0 {
        0o100_644
    } else {
        mode
    }
}

/// Returns seconds since the epoch from a DOS date and time.
///
/// @param date - the packed date: year since 1980, month, day
/// @param time - the packed time: hour, minute, two-second units
fn epoch_of(date: u16, time: u16) -> i64 {
    let year = i64::from(date >> 9).saturating_add(1980);
    let month = i64::from((date >> 5) & 0x0f).max(1);
    let day = i64::from(date & 0x1f).max(1);
    let hour = i64::from(time >> 11);
    let minute = i64::from((time >> 5) & 0x3f);
    let second = i64::from(time & 0x1f).saturating_mul(2);
    let days = days_from_civil(year, month, day);
    days.saturating_mul(86_400)
        .saturating_add(hour.saturating_mul(3_600))
        .saturating_add(minute.saturating_mul(60))
        .saturating_add(second)
}

/// Returns the DOS date and time for a moment.
///
/// @param epoch - seconds since 1970
fn dos_of(epoch: i64) -> (u16, u16) {
    let days = epoch.div_euclid(86_400);
    let rest = epoch.rem_euclid(86_400);
    let civil = inillucent_scalar::datetime::civil_of_unix_day(days);
    let (year, month, day) = (civil.year, civil.month, civil.day);
    let year = year.saturating_sub(1980).clamp(0, 127);
    let date = ((year as u16) << 9) | ((month as u16) << 5) | (day as u16);
    let time = (((rest / 3_600) as u16) << 11)
        | ((((rest % 3_600) / 60) as u16) << 5)
        | (((rest % 60) / 2) as u16);
    (date, time)
}

/// Returns the day number of a civil date, counting from 1970-01-01.
///
/// Howard Hinnant's `days_from_civil`, which is the algorithm the date
/// functions in this workspace already use.
///
/// @param year - the year
/// @param month - the month, one to twelve
/// @param day - the day of the month
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 {
        year.saturating_sub(1)
    } else {
        year
    };
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let shifted = if month > 2 {
        month.saturating_sub(3)
    } else {
        month.saturating_add(9)
    };
    let day_of_year = (153 * shifted + 2) / 5 + day.saturating_sub(1);
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era.saturating_mul(146_097)
        .saturating_add(day_of_era)
        .saturating_sub(719_468)
}

/// Returns a value's text, when it holds any.
fn text_of(value: &Value<'static>) -> Option<String> {
    match value {
        Value::Text(text) => Some(String::from_utf8_lossy(&text.utf8_bytes()).into_owned()),
        Value::Blob(blob) => Some(String::from_utf8_lossy(blob.raw()).into_owned()),
        _ => None,
    }
}

/// Returns a value's bytes, when it holds any.
fn bytes_of(value: &Value<'static>) -> Option<Vec<u8>> {
    match value {
        Value::Text(text) => Some(text.utf8_bytes().into_owned()),
        Value::Blob(blob) => Some(blob.raw().to_vec()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An archive this writes is an archive this reads, member for member.
    #[test]
    fn an_archive_round_trips() {
        let members = vec![
            Member {
                name: "a.txt".to_string(),
                mode: 0o100_644,
                mtime: 1_700_000_000,
                size: 5,
                raw: b"hello".to_vec(),
                method: STORED,
            },
            Member {
                name: "dir/b.txt".to_string(),
                mode: 0o100_644,
                mtime: 1_700_000_000,
                size: 400,
                raw: deflate(&b"repeat ".repeat(50)),
                method: DEFLATED,
            },
        ];
        let bytes = write_archive(&members).expect("it writes");
        let read = read_archive(&bytes).expect("it reads");
        assert_eq!(read.len(), 2);
        assert_eq!(read[0].name, "a.txt");
        assert_eq!(read[0].content().expect("stored"), b"hello");
        assert_eq!(read[1].name, "dir/b.txt");
        assert_eq!(read[1].content().expect("deflated"), b"repeat ".repeat(50));
        assert_eq!(read[1].method, DEFLATED);
    }

    /// The DOS date and time survive a round trip to the nearest two seconds,
    /// which is all the format records.
    #[test]
    fn a_moment_round_trips_through_the_dos_fields() {
        for epoch in [315_532_800i64, 1_000_000_000, 1_700_000_000] {
            let (date, time) = dos_of(epoch);
            let back = epoch_of(date, time);
            assert!(
                (back - epoch).abs() <= 2,
                "{epoch} became {back} through {date:04x}/{time:04x}"
            );
        }
    }

    /// Something that is not an archive is refused rather than read as an
    /// empty one, because an empty answer reads as a working archive.
    #[test]
    fn what_is_not_an_archive_is_refused() {
        assert!(read_archive(b"not a zip at all").is_err());
        assert!(read_archive(&[]).is_err());
    }
}
