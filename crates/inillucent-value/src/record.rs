//! The SQLite record format: serial types, lazy decode, and key comparison.
//!
//! Invariant: nothing in a record is trusted until it has been checked against
//! the record's own length. A record arrives as bytes from a database page, so
//! every varint may be nine bytes of `0xff`, every serial type may claim a
//! payload longer than the file, and the header size may point past the end.
//! `RecordRef::parse` establishes, once, that the header fits, that every
//! field's payload fits, and that the payloads use every byte the record has;
//! after that the accessors index into slices that were proved to exist.
//!
//! Decode is lazy on purpose. Reading one column out of a fifty-column row
//! should cost one field's worth of work, not fifty, so `parse` walks the
//! header - which it must, to validate it - and records the offsets, and the
//! payload of a field is touched only when someone asks for that field.
//!
//! The format itself: a varint holding the byte length of the header
//! *including that varint*, then one varint per column giving its serial type,
//! then the payloads back to back in column order.

use std::cmp::Ordering;

extern crate alloc;

use inillucent_base::bytes;
use inillucent_base::error::{corrupt, too_big};
use inillucent_base::limits::{Limit, Limits};
use inillucent_base::varint;
use inillucent_base::{DbError, DbResult};

use crate::collation::{self, Collation};
use crate::compare;
use crate::encoding::TextEncoding;
use crate::value::{BlobValue, Bytes, StorageClass, TextValue, Value};

/// The largest serial type inillucent will decode.
///
/// A serial type is a varint, so the format allows values up to `2^64 - 1`.
/// Anything past this cannot describe a payload that fits in a database, and
/// treating it as corrupt here means the length arithmetic below can never
/// overflow.
pub const MAX_SERIAL_TYPE: u64 = u64::MAX / 2;

/// A record field's serial type.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct SerialType(pub u64);

impl SerialType {
    /// NULL.
    pub const NULL: SerialType = SerialType(0);
    /// The integer zero, which occupies no payload bytes.
    pub const INTEGER_ZERO: SerialType = SerialType(8);
    /// The integer one, which occupies no payload bytes.
    pub const INTEGER_ONE: SerialType = SerialType(9);

    /// Returns the number of payload bytes this serial type occupies.
    ///
    /// Reserved types 10 and 11 have no length because they may not appear in
    /// a record at all; asking is a corruption report rather than a number.
    pub fn payload_len(self) -> DbResult<u64> {
        Ok(match self.0 {
            0 | 8 | 9 => 0,
            1 => 1,
            2 => 2,
            3 => 3,
            4 => 4,
            5 => 6,
            6 | 7 => 8,
            10 | 11 => {
                return Err(corrupt(format!(
                    "serial type {} is reserved for internal use",
                    self.0
                )))
            }
            other => {
                if other > MAX_SERIAL_TYPE {
                    return Err(corrupt(format!("serial type {other} is impossibly large")));
                }
                (other - 12) / 2
            }
        })
    }

    /// Returns the storage class a value of this serial type has.
    pub fn storage_class(self) -> DbResult<StorageClass> {
        Ok(match self.0 {
            0 => StorageClass::Null,
            1..=6 | 8 | 9 => StorageClass::Integer,
            7 => StorageClass::Real,
            10 | 11 => {
                return Err(corrupt(format!(
                    "serial type {} is reserved for internal use",
                    self.0
                )))
            }
            other if other % 2 == 0 => StorageClass::Blob,
            _ => StorageClass::Text,
        })
    }

    /// Reports whether this serial type may appear in a record at all.
    pub fn is_valid(self) -> bool {
        !matches!(self.0, 10 | 11) && self.0 <= MAX_SERIAL_TYPE
    }

    /// Returns the serial type a value is stored with.
    ///
    /// `file_format` is the schema format number from the database header. The
    /// zero and one shortcuts, which store an integer in no payload bytes at
    /// all, arrived with format 4; writing them into an older database would
    /// produce a file that older SQLite cannot read.
    pub fn for_value(value: &Value<'_>, file_format: u32) -> SerialType {
        match value {
            Value::Null => SerialType(0),
            Value::Integer(integer) => SerialType::for_integer(*integer, file_format),
            Value::Real(_) => SerialType(7),
            Value::Text(text) => {
                SerialType((text.len() as u64).saturating_mul(2).saturating_add(13))
            }
            Value::Blob(blob) => {
                SerialType((blob.len() as u64).saturating_mul(2).saturating_add(12))
            }
        }
    }

    /// Returns the narrowest serial type that holds an integer.
    pub fn for_integer(value: i64, file_format: u32) -> SerialType {
        let magnitude = if value < 0 { !value } else { value } as u64;
        if magnitude <= 127 {
            if (value == 0 || value == 1) && file_format >= 4 {
                return SerialType(8u64.saturating_add(value as u64));
            }
            return SerialType(1);
        }
        if magnitude <= 32_767 {
            return SerialType(2);
        }
        if magnitude <= 8_388_607 {
            return SerialType(3);
        }
        if magnitude <= 2_147_483_647 {
            return SerialType(4);
        }
        if magnitude <= 140_737_488_355_327 {
            return SerialType(5);
        }
        SerialType(6)
    }
}

/// Where one field lives inside a record's payload area.
///
/// Public so that a caller reading several columns of one row can keep the
/// spans between reads. Parsing a record is a walk of its whole header, and a
/// machine that parsed it once per column read was doing that walk - and one
/// heap allocation - for every column of every row.
#[derive(Clone, Copy, Debug)]
pub struct FieldSpan {
    /// The field's serial type.
    serial: SerialType,
    /// The byte offset of the field's payload within the whole record.
    offset: usize,
    /// The payload length in bytes.
    len: usize,
}

/// A parsed, validated record that has not decoded any of its fields yet.
#[derive(Clone, Debug)]
pub struct RecordRef<'a> {
    bytes: &'a [u8],
    /// Where each field is, owned when this parsed the record and borrowed when
    /// a caller kept the spans from an earlier parse of the same row.
    fields: alloc::borrow::Cow<'a, [FieldSpan]>,
    header_len: usize,
    encoding: TextEncoding,
}

impl<'a> RecordRef<'a> {
    /// Validates a record's structure and records where every field lives.
    ///
    /// Every check that can be made once is made here, so the accessors do not
    /// have to re-derive anything from attacker-controlled bytes.
    pub fn parse(bytes: &'a [u8], encoding: TextEncoding) -> DbResult<RecordRef<'a>> {
        RecordRef::parse_with_limits(bytes, encoding, &Limits::default())
    }

    /// Validates a record, enforcing a connection's length limit as well.
    pub fn parse_with_limits(
        bytes: &'a [u8],
        encoding: TextEncoding,
        limits: &Limits,
    ) -> DbResult<RecordRef<'a>> {
        let header = varint::decode(bytes)
            .map_err(|_| corrupt("a record's header size varint is truncated"))?;
        let header_len = usize::try_from(header.value)
            .map_err(|_| corrupt("a record's header size does not fit in memory"))?;
        if header_len < header.len || header_len > bytes.len() {
            return Err(corrupt(format!(
                "a record claims a {header_len}-byte header inside {} bytes",
                bytes.len()
            )));
        }
        let mut fields: Vec<FieldSpan> = Vec::new();
        let header_len =
            RecordRef::parse_fields(bytes, header_len, header.len, limits, &mut fields)?;
        Ok(RecordRef {
            bytes,
            fields: alloc::borrow::Cow::Owned(fields),
            header_len,
            encoding,
        })
    }

    /// Rebuilds a record from spans a previous parse produced.
    ///
    /// No parsing and no allocation: the spans describe this record's fields
    /// and were validated when they were made. The caller promises the bytes
    /// are the ones the spans came from, which is why this is only reachable
    /// through a cursor that clears its cache whenever it moves.
    /// @param bytes - the record, as the spans were parsed from
    /// @param fields - the spans
    /// @param header_len - where the payload area begins
    /// @param encoding - how text fields are decoded
    pub fn with_fields(
        bytes: &'a [u8],
        fields: &'a [FieldSpan],
        header_len: usize,
        encoding: TextEncoding,
    ) -> RecordRef<'a> {
        RecordRef {
            bytes,
            fields: alloc::borrow::Cow::Borrowed(fields),
            header_len,
            encoding,
        }
    }

    /// Parses a record's header into a caller's buffer, returning the header
    /// length.
    ///
    /// The buffer is cleared and refilled, so a caller that keeps one across
    /// rows allocates once rather than once per row.
    /// @param bytes - the record
    /// @param encoding - unused here, kept so the two entry points read alike
    /// @param limits - the run-time limits the fields are checked against
    /// @param into - the buffer to fill
    pub fn parse_into(
        bytes: &'a [u8],
        limits: &Limits,
        into: &mut Vec<FieldSpan>,
    ) -> DbResult<usize> {
        let header = varint::decode(bytes)
            .map_err(|_| corrupt("a record's header size varint is truncated"))?;
        let header_len = usize::try_from(header.value)
            .map_err(|_| corrupt("a record's header size does not fit in memory"))?;
        if header_len < header.len || header_len > bytes.len() {
            return Err(corrupt(format!(
                "a record claims a {header_len}-byte header inside {} bytes",
                bytes.len()
            )));
        }
        RecordRef::parse_fields(bytes, header_len, header.len, limits, into)
    }

    /// Walks a record's header, filling in where every field lives.
    fn parse_fields(
        bytes: &[u8],
        header_len: usize,
        header_varint: usize,
        limits: &Limits,
        fields: &mut Vec<FieldSpan>,
    ) -> DbResult<usize> {
        let payload_area = bytes.len().saturating_sub(header_len);
        fields.clear();
        let mut cursor = header_varint;
        let mut payload_used: usize = 0;
        while cursor < header_len {
            let window = bytes
                .get(cursor..header_len)
                .ok_or_else(|| corrupt("a record's header runs past its own end"))?;
            let decoded = varint::decode(window)
                .map_err(|_| corrupt("a record's serial type varint is truncated"))?;
            if decoded.len > header_len.saturating_sub(cursor) {
                return Err(corrupt("a record's serial type crosses its header end"));
            }
            let serial = SerialType(decoded.value);
            let len = usize::try_from(serial.payload_len()?)
                .map_err(|_| corrupt("a record field is longer than memory"))?;
            let offset = header_len
                .checked_add(payload_used)
                .ok_or_else(|| corrupt("a record's payload offsets overflow"))?;
            payload_used = payload_used
                .checked_add(len)
                .ok_or_else(|| corrupt("a record's payload lengths overflow"))?;
            if payload_used > payload_area {
                return Err(corrupt(format!(
                    "a record's fields claim {payload_used} payload bytes out of {payload_area}"
                )));
            }
            if !limits.permits_length(len as u64) {
                return Err(too_big(format!(
                    "a record field of {len} bytes exceeds the length limit of {}",
                    limits.get(Limit::Length)
                )));
            }
            fields.push(FieldSpan {
                serial,
                offset,
                len,
            });
            cursor = cursor.saturating_add(decoded.len);
        }
        if cursor != header_len {
            return Err(corrupt("a record's header does not end on a serial type"));
        }
        if payload_used != payload_area {
            return Err(corrupt(format!(
                "a record's fields use {payload_used} of {payload_area} payload bytes"
            )));
        }
        Ok(header_len)
    }

    /// Returns the number of fields the record holds.
    pub fn field_count(&self) -> usize {
        self.fields.len()
    }

    /// Returns the record's header length in bytes.
    pub fn header_len(&self) -> usize {
        self.header_len
    }

    /// Returns the whole record's bytes.
    pub fn raw(&self) -> &'a [u8] {
        self.bytes
    }

    /// Returns the text encoding fields are decoded with.
    pub fn encoding(&self) -> TextEncoding {
        self.encoding
    }

    /// Returns a field's serial type.
    pub fn serial_type(&self, index: usize) -> Option<SerialType> {
        self.fields.get(index).map(|field| field.serial)
    }

    /// Returns a field's storage class without touching its payload.
    pub fn storage_class(&self, index: usize) -> DbResult<StorageClass> {
        match self.fields.get(index) {
            Some(field) => field.serial.storage_class(),
            None => Ok(StorageClass::Null),
        }
    }

    /// Returns a field's raw payload bytes.
    pub fn payload(&self, index: usize) -> DbResult<&'a [u8]> {
        let Some(field) = self.fields.get(index) else {
            return Ok(&[]);
        };
        self.bytes
            .get(field.offset..field.offset.saturating_add(field.len))
            .ok_or_else(|| corrupt("a validated record field is out of range"))
    }

    /// Decodes one field.
    ///
    /// A field past the end of the record is NULL rather than an error, which
    /// is what makes a row written before an `ALTER TABLE ADD COLUMN` readable
    /// by a schema that has the new column.
    pub fn value(&self, index: usize) -> DbResult<Value<'a>> {
        let Some(field) = self.fields.get(index) else {
            return Ok(Value::Null);
        };
        let payload = self.payload(index)?;
        decode_field(field.serial, payload, self.encoding)
    }

    /// Decodes every field, in column order.
    pub fn values(&self) -> DbResult<Vec<Value<'a>>> {
        (0..self.fields.len())
            .map(|index| self.value(index))
            .collect()
    }
}

/// Returns how many bytes a record's header occupies.
///
/// Only the first varint is read, so a prefix of the record is enough - which
/// is the point: a caller that has not read the record yet uses this to find
/// out how much of it to read.
pub fn header_length(prefix: &[u8]) -> DbResult<u64> {
    let header = varint::decode(prefix)
        .map_err(|_| corrupt("a record's header size varint is truncated"))?;
    if header.value < header.len as u64 {
        return Err(corrupt(
            "a record whose header is shorter than the size that declares it",
        ));
    }
    Ok(header.value)
}

/// Returns where one field of a record starts and how long it is.
///
/// `header` must hold at least the record's header. Nothing else is read: the
/// header says how long every field is, so finding where the fifth value
/// begins is a walk of four varints rather than a read of the four values. It
/// is what lets a blob handle open a hundred-megabyte value without reading
/// any of it.
pub fn field_extent(header: &[u8], index: usize) -> DbResult<(u64, u64)> {
    let header_len = header_length(header)?;
    let header_len_usize =
        usize::try_from(header_len).map_err(|_| corrupt("a record header longer than memory"))?;
    if header_len_usize > header.len() {
        return Err(corrupt("a record header shorter than the size it declares"));
    }
    let leading = varint::decode(header)
        .map_err(|_| corrupt("a record's header size varint is truncated"))?;
    let mut cursor = leading.len;
    let mut start = header_len;
    let mut field = 0usize;
    while cursor < header_len_usize {
        let window = header
            .get(cursor..header_len_usize)
            .ok_or_else(|| corrupt("a record's header runs past its own end"))?;
        let decoded = varint::decode(window)
            .map_err(|_| corrupt("a record's serial type varint is truncated"))?;
        let len = SerialType(decoded.value).payload_len()?;
        if field == index {
            return Ok((start, len));
        }
        start = start.saturating_add(len);
        cursor = cursor.saturating_add(decoded.len);
        field = field.saturating_add(1);
    }
    Err(corrupt(format!(
        "a record of {field} fields has no field {index}"
    )))
}

/// Decodes one field's payload under its serial type.
pub fn decode_field(
    serial: SerialType,
    payload: &[u8],
    encoding: TextEncoding,
) -> DbResult<Value<'_>> {
    Ok(match serial.0 {
        0 => Value::Null,
        1 => Value::Integer(i64::from(bytes::read_u8(payload, 0)? as i8)),
        2 => Value::Integer(i64::from(bytes::read_u16(payload, 0)? as i16)),
        3 => {
            let raw = bytes::read_u24(payload, 0)?;
            // A 24-bit two's complement value, sign extended.
            let signed = if raw & 0x0080_0000 != 0 {
                (raw | 0xff00_0000) as i32
            } else {
                raw as i32
            };
            Value::Integer(i64::from(signed))
        }
        4 => Value::Integer(i64::from(bytes::read_u32(payload, 0)? as i32)),
        5 => {
            let raw = bytes::read_u48(payload, 0)?;
            // A 48-bit two's complement value, sign extended.
            let signed = if raw & 0x0000_8000_0000_0000 != 0 {
                (raw | 0xffff_0000_0000_0000) as i64
            } else {
                raw as i64
            };
            Value::Integer(signed)
        }
        6 => Value::Integer(bytes::read_i64(payload, 0)?),
        7 => Value::Real(bytes::read_f64(payload, 0)?),
        8 => Value::Integer(0),
        9 => Value::Integer(1),
        10 | 11 => {
            return Err(corrupt(format!(
                "serial type {} is reserved for internal use",
                serial.0
            )))
        }
        other if other % 2 == 0 => Value::Blob(BlobValue::borrowed(payload)),
        _ => Value::Text(TextValue::new(Bytes::Borrowed(payload), encoding)),
    })
}

/// Encodes a row of values into a record.
///
/// The output is byte-for-byte what SQLite writes for the same values, which
/// matters because a record inillucent writes has to be readable by SQLite and
/// has to compare identically inside an index.
pub fn encode_record(
    values: &[Value<'_>],
    encoding: TextEncoding,
    file_format: u32,
) -> DbResult<Vec<u8>> {
    encode_record_with_limits(values, encoding, file_format, &Limits::default())
}

/// Encodes a row of values, enforcing a connection's length limit.
pub fn encode_record_with_limits(
    values: &[Value<'_>],
    encoding: TextEncoding,
    file_format: u32,
    limits: &Limits,
) -> DbResult<Vec<u8>> {
    // Every text payload has to be in the database encoding before its length
    // is known, so the conversion happens once, up front, rather than being
    // repeated for the length and then for the bytes.
    let mut payloads: Vec<Vec<u8>> = Vec::with_capacity(values.len());
    let mut serials: Vec<SerialType> = Vec::with_capacity(values.len());
    for value in values {
        let payload = field_payload(value, encoding)?;
        if !limits.permits_length(payload.len() as u64) {
            return Err(too_big(format!(
                "a value of {} bytes exceeds the length limit of {}",
                payload.len(),
                limits.get(Limit::Length)
            )));
        }
        let serial = match value {
            Value::Text(_) => {
                SerialType((payload.len() as u64).saturating_mul(2).saturating_add(13))
            }
            Value::Blob(_) => {
                SerialType((payload.len() as u64).saturating_mul(2).saturating_add(12))
            }
            other => SerialType::for_value(other, file_format),
        };
        serials.push(serial);
        payloads.push(payload);
    }

    // The header length includes the varint that states it, and that varint's
    // own width depends on the length - so the width is solved for rather than
    // guessed, the same way SQLite widens the header when it has to.
    let body: usize = serials
        .iter()
        .map(|serial| varint::encoded_len(serial.0))
        .sum();
    let mut header_len = body.saturating_add(1);
    while varint::encoded_len(header_len as u64) != header_len.saturating_sub(body) {
        header_len = body.saturating_add(varint::encoded_len(header_len as u64));
    }

    let payload_total: usize = payloads.iter().map(|payload| payload.len()).sum();
    let mut record = vec![0u8; header_len.saturating_add(payload_total)];
    let written = varint::encode(&mut record, header_len as u64)?;
    let mut cursor = written;
    for serial in &serials {
        let window = bytes::window_mut(&mut record, cursor, varint::encoded_len(serial.0))?;
        let advanced = varint::encode(window, serial.0)?;
        cursor = cursor.saturating_add(advanced);
    }
    if cursor != header_len {
        return Err(DbError::primary(inillucent_base::PrimaryCode::Internal)
            .with_detail("record header width solved incorrectly"));
    }
    for payload in &payloads {
        let window = bytes::window_mut(&mut record, cursor, payload.len())?;
        window.copy_from_slice(payload);
        cursor = cursor.saturating_add(payload.len());
    }
    Ok(record)
}

/// Returns one value's payload bytes in the database encoding.
fn field_payload(value: &Value<'_>, encoding: TextEncoding) -> DbResult<Vec<u8>> {
    Ok(match value {
        Value::Null => Vec::new(),
        Value::Integer(integer) => integer_payload(*integer, encoding),
        Value::Real(real) => real.to_bits().to_be_bytes().to_vec(),
        Value::Text(text) => {
            crate::encoding::convert(text.raw(), text.encoding(), encoding).into_owned()
        }
        Value::Blob(blob) => blob.raw().to_vec(),
    })
}

/// Returns an integer's big-endian payload at its narrowest width.
fn integer_payload(value: i64, _encoding: TextEncoding) -> Vec<u8> {
    // Format 4 is what every database SQLite has created since 2006 uses, and
    // it is what the zero/one shortcuts require; the caller decides the format
    // for the serial type, and the payload for those two is empty either way.
    let serial = SerialType::for_integer(value, 4);
    let width = serial.payload_len().unwrap_or(0) as usize;
    let full = value.to_be_bytes();
    full.get(8usize.saturating_sub(width)..)
        .map(<[u8]>::to_vec)
        .unwrap_or_default()
}

/// How one column of an index key is ordered.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KeyColumn {
    /// The collation this column's text is compared with.
    pub collation: Collation,
    /// Whether the column is stored in descending order.
    pub descending: bool,
}

impl Default for KeyColumn {
    /// Ascending, BINARY - the column an index declares nothing about.
    fn default() -> KeyColumn {
        KeyColumn {
            collation: Collation::Binary,
            descending: false,
        }
    }
}

/// The ordering an index's key columns have.
#[derive(Clone, Debug, Default)]
pub struct KeyInfo {
    /// One entry per key column, in key order.
    pub columns: Vec<KeyColumn>,
}

impl KeyInfo {
    /// Builds a key description with every column ascending and BINARY.
    pub fn binary(count: usize) -> KeyInfo {
        KeyInfo {
            columns: vec![KeyColumn::default(); count],
        }
    }

    /// Returns how column `index` is ordered, defaulting for a column past the
    /// declared key, which is the trailing rowid an index appends.
    pub fn column(&self, index: usize) -> KeyColumn {
        self.columns.get(index).copied().unwrap_or_default()
    }
}

/// Compares a probe key against a record, field by field.
///
/// The probe may be shorter than the record, which is how a range scan asks
/// "everything whose first two columns are these". A shorter probe that
/// matches on every field it has compares equal, so the caller decides whether
/// that means the range starts before or after the record.
pub fn compare_values_to_record(
    probe: &[Value<'_>],
    record: &RecordRef<'_>,
    key: &KeyInfo,
) -> DbResult<Ordering> {
    for (index, left) in probe.iter().enumerate() {
        let right = record.value(index)?;
        let column = key.column(index);
        let ordering = compare::compare_values(left, &right, column.collation);
        let ordering = if column.descending {
            ordering.reverse()
        } else {
            ordering
        };
        if ordering != Ordering::Equal {
            return Ok(ordering);
        }
    }
    Ok(Ordering::Equal)
}

/// Compares two records field by field under a key description.
pub fn compare_records(
    left: &RecordRef<'_>,
    right: &RecordRef<'_>,
    key: &KeyInfo,
) -> DbResult<Ordering> {
    let shared = left.field_count().max(right.field_count());
    for index in 0..shared {
        let left_value = left.value(index)?;
        let right_value = right.value(index)?;
        let column = key.column(index);
        let ordering = compare::compare_values(&left_value, &right_value, column.collation);
        let ordering = if column.descending {
            ordering.reverse()
        } else {
            ordering
        };
        if ordering != Ordering::Equal {
            return Ok(ordering);
        }
    }
    Ok(Ordering::Equal)
}

/// Compares two text payloads under a collation, for callers that already
/// have the bytes and do not want to build values first.
pub fn compare_text_payloads(
    left: &[u8],
    right: &[u8],
    encoding: TextEncoding,
    collation: Collation,
) -> Ordering {
    collation::compare_text(left, encoding, right, encoding, collation)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The serial type table is the file format; every entry is spelled out.
    #[test]
    fn the_serial_type_table_matches_the_file_format() {
        let expected: [(u64, u64, StorageClass); 10] = [
            (0, 0, StorageClass::Null),
            (1, 1, StorageClass::Integer),
            (2, 2, StorageClass::Integer),
            (3, 3, StorageClass::Integer),
            (4, 4, StorageClass::Integer),
            (5, 6, StorageClass::Integer),
            (6, 8, StorageClass::Integer),
            (7, 8, StorageClass::Real),
            (8, 0, StorageClass::Integer),
            (9, 0, StorageClass::Integer),
        ];
        for (serial, len, class) in expected {
            let serial = SerialType(serial);
            assert_eq!(serial.payload_len().unwrap(), len);
            assert_eq!(serial.storage_class().unwrap(), class);
        }
        assert_eq!(SerialType(12).payload_len().unwrap(), 0);
        assert_eq!(SerialType(12).storage_class().unwrap(), StorageClass::Blob);
        assert_eq!(SerialType(13).payload_len().unwrap(), 0);
        assert_eq!(SerialType(13).storage_class().unwrap(), StorageClass::Text);
        assert_eq!(SerialType(20).payload_len().unwrap(), 4);
        assert_eq!(SerialType(21).payload_len().unwrap(), 4);
    }

    /// Serial types 10 and 11 are reserved and may never appear in a record.
    #[test]
    fn the_reserved_serial_types_are_refused() {
        for reserved in [10u64, 11] {
            assert!(SerialType(reserved).payload_len().is_err());
            assert!(SerialType(reserved).storage_class().is_err());
            assert!(!SerialType(reserved).is_valid());
        }
    }

    /// An integer takes the narrowest width that holds it, and only zero and
    /// one get the payload-free shortcuts, and only in format 4 or later.
    #[test]
    fn integers_take_the_narrowest_serial_type() {
        let cases: [(i64, u64); 14] = [
            (0, 8),
            (1, 9),
            (2, 1),
            (-1, 1),
            (127, 1),
            (-128, 1),
            (128, 2),
            (32_767, 2),
            (-32_768, 2),
            (32_768, 3),
            (8_388_607, 3),
            (8_388_608, 4),
            (2_147_483_648, 5),
            (140_737_488_355_328, 6),
        ];
        for (value, expected) in cases {
            assert_eq!(
                SerialType::for_integer(value, 4).0,
                expected,
                "{value} took the wrong serial type"
            );
        }
        // Format 1 has no zero/one shortcut, so both are one-byte integers.
        assert_eq!(SerialType::for_integer(0, 1).0, 1);
        assert_eq!(SerialType::for_integer(1, 1).0, 1);
    }

    /// Every value must survive an encode/decode round trip with its class,
    /// its bits, and its bytes unchanged.
    #[test]
    fn every_value_round_trips_through_a_record() {
        let long_text = "x".repeat(500);
        let long_blob: Vec<u8> = (0..=255u8).cycle().take(1000).collect();
        let values: Vec<Value<'_>> = vec![
            Value::Null,
            Value::Integer(0),
            Value::Integer(1),
            Value::Integer(-1),
            Value::Integer(127),
            Value::Integer(-128),
            Value::Integer(32_767),
            Value::Integer(-32_768),
            Value::Integer(8_388_607),
            Value::Integer(-8_388_608),
            Value::Integer(i32::MAX as i64),
            Value::Integer(i32::MIN as i64),
            Value::Integer(140_737_488_355_327),
            Value::Integer(-140_737_488_355_328),
            Value::Integer(i64::MAX),
            Value::Integer(i64::MIN),
            Value::Integer(9_007_199_254_740_993),
            Value::Real(0.0),
            Value::Real(-0.0),
            Value::Real(0.1),
            Value::Real(f64::MIN_POSITIVE),
            Value::Real(f64::MAX),
            Value::Real(f64::INFINITY),
            Value::Real(f64::NEG_INFINITY),
            Value::text_utf8(b""),
            Value::text_utf8(b"hello"),
            Value::text_utf8(long_text.as_bytes()),
            Value::blob(b""),
            Value::blob(b"\x00"),
            Value::blob(&long_blob),
        ];
        for encoding in TextEncoding::all() {
            let encoded = encode_record(&values, encoding, 4).unwrap();
            let parsed = RecordRef::parse(&encoded, encoding).unwrap();
            assert_eq!(parsed.field_count(), values.len());
            for (index, expected) in values.iter().enumerate() {
                let actual = parsed.value(index).unwrap();
                match (expected, &actual) {
                    (Value::Text(left), Value::Text(right)) => {
                        assert_eq!(
                            left.utf8_bytes().as_ref(),
                            right.utf8_bytes().as_ref(),
                            "field {index} in {encoding:?}"
                        );
                    }
                    _ => assert!(
                        expected.identical(&actual),
                        "field {index} in {encoding:?}: {expected:?} became {actual:?}"
                    ),
                }
            }
        }
    }

    /// An empty record is a legal record with no fields.
    #[test]
    fn an_empty_record_is_legal() {
        let encoded = encode_record(&[], TextEncoding::Utf8, 4).unwrap();
        assert_eq!(encoded, vec![1u8]);
        let parsed = RecordRef::parse(&encoded, TextEncoding::Utf8).unwrap();
        assert_eq!(parsed.field_count(), 0);
        assert!(parsed.value(0).unwrap().is_null());
    }

    /// A record with enough fields to need a two-byte header size must solve
    /// the header width rather than assuming one byte.
    #[test]
    fn a_long_record_widens_its_header_size_varint() {
        let values: Vec<Value<'_>> = (0..200).map(Value::Integer).collect();
        let encoded = encode_record(&values, TextEncoding::Utf8, 4).unwrap();
        let parsed = RecordRef::parse(&encoded, TextEncoding::Utf8).unwrap();
        assert_eq!(parsed.field_count(), 200);
        assert!(parsed.header_len() > 127, "{}", parsed.header_len());
        for index in 0..200usize {
            assert_eq!(
                parsed.value(index).unwrap().as_integer(),
                Some(index as i64)
            );
        }
    }

    /// A field past the end of the record is NULL, which is what lets a row
    /// written before ALTER TABLE ADD COLUMN be read afterwards.
    #[test]
    fn a_missing_trailing_field_reads_as_null() {
        let encoded = encode_record(&[Value::Integer(1)], TextEncoding::Utf8, 4).unwrap();
        let parsed = RecordRef::parse(&encoded, TextEncoding::Utf8).unwrap();
        assert_eq!(parsed.field_count(), 1);
        assert!(parsed.value(1).unwrap().is_null());
        assert!(parsed.value(99).unwrap().is_null());
        assert_eq!(parsed.payload(99).unwrap(), b"");
    }

    /// Every structural lie in a record must be refused, not read past.
    #[test]
    fn hostile_records_are_refused_rather_than_read() {
        // A header size larger than the record.
        assert!(RecordRef::parse(&[0x7f, 0x00], TextEncoding::Utf8).is_err());
        // A header size of zero, which cannot even contain its own varint.
        assert!(RecordRef::parse(&[0x00], TextEncoding::Utf8).is_err());
        // A reserved serial type.
        assert!(RecordRef::parse(&[0x02, 0x0a], TextEncoding::Utf8).is_err());
        assert!(RecordRef::parse(&[0x02, 0x0b], TextEncoding::Utf8).is_err());
        // A serial type claiming more payload than the record has.
        assert!(RecordRef::parse(&[0x02, 0x06], TextEncoding::Utf8).is_err());
        // A blob serial type claiming an enormous payload.
        let huge = [0x02u8, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f];
        assert!(RecordRef::parse(&huge, TextEncoding::Utf8).is_err());
        // Payload bytes the header does not account for.
        assert!(RecordRef::parse(&[0x02, 0x00, 0x41], TextEncoding::Utf8).is_err());
        // An empty record with nothing at all in it.
        assert!(RecordRef::parse(&[], TextEncoding::Utf8).is_err());
        // A serial type varint that crosses the header end.
        assert!(RecordRef::parse(&[0x02, 0x81, 0x01], TextEncoding::Utf8).is_err());
    }

    /// A field longer than the length limit is too big rather than corrupt:
    /// the record is well formed, the value is simply not allowed.
    #[test]
    fn an_oversized_field_reports_too_big_rather_than_corruption() {
        let mut limits = Limits::default();
        limits.set(Limit::Length, 8);
        let encoded = encode_record(&[Value::blob(b"0123456789")], TextEncoding::Utf8, 4).unwrap();
        let error = RecordRef::parse_with_limits(&encoded, TextEncoding::Utf8, &limits)
            .expect_err("an oversized field must be refused");
        assert_eq!(error.code(), inillucent_base::PrimaryCode::TooBig);
        let error = encode_record_with_limits(
            &[Value::blob(b"0123456789")],
            TextEncoding::Utf8,
            4,
            &limits,
        )
        .expect_err("an oversized field must not be encodable");
        assert_eq!(error.code(), inillucent_base::PrimaryCode::TooBig);
    }

    /// Parsing arbitrary bytes must terminate, never panic, and never report
    /// a field it cannot then read.
    #[test]
    fn parsing_arbitrary_bytes_never_panics() {
        let mut state = 0xfeed_face_dead_beefu64;
        for _ in 0..inillucent_base::probe::sample_rounds(100_000) {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let len = (state % 40) as usize;
            let bytes: Vec<u8> = (0..len)
                .map(|index| (state.rotate_left(index as u32 * 8)) as u8)
                .collect();
            if let Ok(record) = RecordRef::parse(&bytes, TextEncoding::Utf8) {
                for index in 0..record.field_count().saturating_add(2) {
                    let _ = record.value(index).unwrap();
                    let _ = record.payload(index).unwrap();
                }
            }
        }
    }

    /// A record whose header is well formed but whose payload is one byte
    /// short must be refused; reading it would return a truncated value.
    #[test]
    fn a_record_that_is_one_byte_short_is_refused() {
        let mut encoded = encode_record(&[Value::Integer(1000)], TextEncoding::Utf8, 4).unwrap();
        encoded.pop();
        assert!(RecordRef::parse(&encoded, TextEncoding::Utf8).is_err());
    }

    /// Key comparison walks columns in order, applies each column's collation,
    /// and reverses a descending column.
    #[test]
    fn key_comparison_honours_collation_and_direction() {
        let left = encode_record(
            &[Value::text_utf8(b"ABC"), Value::Integer(1)],
            TextEncoding::Utf8,
            4,
        )
        .unwrap();
        let right = encode_record(
            &[Value::text_utf8(b"abc"), Value::Integer(2)],
            TextEncoding::Utf8,
            4,
        )
        .unwrap();
        let left = RecordRef::parse(&left, TextEncoding::Utf8).unwrap();
        let right = RecordRef::parse(&right, TextEncoding::Utf8).unwrap();

        let binary = KeyInfo::binary(2);
        assert_eq!(
            compare_records(&left, &right, &binary).unwrap(),
            Ordering::Less
        );

        let nocase = KeyInfo {
            columns: vec![
                KeyColumn {
                    collation: Collation::NoCase,
                    descending: false,
                },
                KeyColumn::default(),
            ],
        };
        // The text ties under NOCASE, so the integer column decides.
        assert_eq!(
            compare_records(&left, &right, &nocase).unwrap(),
            Ordering::Less
        );

        let descending = KeyInfo {
            columns: vec![
                KeyColumn {
                    collation: Collation::NoCase,
                    descending: false,
                },
                KeyColumn {
                    collation: Collation::Binary,
                    descending: true,
                },
            ],
        };
        assert_eq!(
            compare_records(&left, &right, &descending).unwrap(),
            Ordering::Greater
        );
    }

    /// A probe shorter than the record compares equal when its fields match,
    /// which is what a prefix range scan depends on.
    #[test]
    fn a_short_probe_matches_a_prefix() {
        let record = encode_record(
            &[Value::Integer(5), Value::text_utf8(b"beta")],
            TextEncoding::Utf8,
            4,
        )
        .unwrap();
        let record = RecordRef::parse(&record, TextEncoding::Utf8).unwrap();
        let key = KeyInfo::binary(2);
        assert_eq!(
            compare_values_to_record(&[Value::Integer(5)], &record, &key).unwrap(),
            Ordering::Equal
        );
        assert_eq!(
            compare_values_to_record(&[Value::Integer(4)], &record, &key).unwrap(),
            Ordering::Less
        );
        assert_eq!(
            compare_values_to_record(
                &[Value::Integer(5), Value::text_utf8(b"alpha")],
                &record,
                &key
            )
            .unwrap(),
            Ordering::Less
        );
    }

    /// Encoding a row and comparing the records must agree with comparing the
    /// values directly, or an index would order differently from a sort.
    #[test]
    fn record_order_agrees_with_value_order() {
        let rows: Vec<Vec<Value<'static>>> = vec![
            vec![Value::Null],
            vec![Value::Integer(-1)],
            vec![Value::Integer(0)],
            vec![Value::Real(0.5)],
            vec![Value::Integer(1)],
            vec![Value::owned_text(b"").unwrap()],
            vec![Value::owned_text(b"a").unwrap()],
            vec![Value::owned_blob(b"").unwrap()],
            vec![Value::owned_blob(b"\x01").unwrap()],
        ];
        let key = KeyInfo::binary(1);
        let encoded: Vec<Vec<u8>> = rows
            .iter()
            .map(|row| encode_record(row, TextEncoding::Utf8, 4).unwrap())
            .collect();
        for (left_index, left_bytes) in encoded.iter().enumerate() {
            for (right_index, right_bytes) in encoded.iter().enumerate() {
                let left = RecordRef::parse(left_bytes, TextEncoding::Utf8).unwrap();
                let right = RecordRef::parse(right_bytes, TextEncoding::Utf8).unwrap();
                let by_record = compare_records(&left, &right, &key).unwrap();
                let by_value = compare::compare_values(
                    rows.get(left_index).and_then(|row| row.first()).unwrap(),
                    rows.get(right_index).and_then(|row| row.first()).unwrap(),
                    Collation::Binary,
                );
                assert_eq!(by_record, by_value, "row {left_index} vs {right_index}");
            }
        }
    }

    /// Text payloads must be stored in the database encoding, so the same
    /// string produces different bytes in a UTF-16 database.
    #[test]
    fn text_is_stored_in_the_database_encoding() {
        let values = [Value::text_utf8(b"ab")];
        let narrow = encode_record(&values, TextEncoding::Utf8, 4).unwrap();
        let wide = encode_record(&values, TextEncoding::Utf16Le, 4).unwrap();
        assert_ne!(narrow, wide);
        let narrow_record = RecordRef::parse(&narrow, TextEncoding::Utf8).unwrap();
        let wide_record = RecordRef::parse(&wide, TextEncoding::Utf16Le).unwrap();
        assert_eq!(narrow_record.serial_type(0).unwrap().0, 2 * 2 + 13);
        assert_eq!(wide_record.serial_type(0).unwrap().0, 4 * 2 + 13);
        assert_eq!(
            wide_record
                .value(0)
                .unwrap()
                .as_text()
                .unwrap()
                .utf8_bytes()
                .as_ref(),
            b"ab"
        );
    }
}
