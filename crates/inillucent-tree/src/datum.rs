//! The borrowed value: what one cell of one row is, while a page is pinned.
//!
//! Invariant: a `Datum` never outlives the bytes it borrows. That is a lifetime
//! rather than a convention, so a value read out of a leaf cannot be kept past
//! the point where the leaf may be rewritten. The engine's owned value lives at
//! the API boundary and is a different type; inside the executor everything
//! borrows, which is the whole reason a scan can hand a `sum` the page's own
//! integers instead of a copy of them.
//!
//! The five storage classes are SQLite's, and the ordering rules
//! (NULL < numeric < text < blob, integers and reals compared numerically) are
//! SQLite's too, because the SQL dialect is the one thing the rearchitecture
//! keeps.

use std::cmp::Ordering;

use inillucent_base::error::corrupt;
use inillucent_base::DbResult;

/// A test-only count of how many times [`Datum::tagged_span`] ran.
///
/// **Compiled out of every non-test build**, so it costs nothing anywhere this
/// crate ships. It exists because a per-column re-walk of a delta row is a
/// defect a timing test cannot pin reliably - per `tests/inillucent-testing-tdd.md`
/// §1.7, a duration on a shared box reads differently between runs while a
/// count of decode steps does not. `crates/inillucent-tree/src/leaf.rs`'s
/// `locate_stops_reading_a_delta_row_at_the_first_mismatched_column` is what
/// reads it.
#[cfg(test)]
pub(crate) mod probe {
    use std::cell::Cell;

    thread_local! {
        static TAGGED_SPAN_CALLS: Cell<usize> = const { Cell::new(0) };
    }

    /// Zeroes this thread's `tagged_span` call count.
    pub(crate) fn reset_tagged_span_calls() {
        TAGGED_SPAN_CALLS.with(|calls| calls.set(0));
    }

    /// Returns this thread's `tagged_span` call count since the last reset.
    pub(crate) fn tagged_span_calls() -> usize {
        TAGGED_SPAN_CALLS.with(|calls| calls.get())
    }

    /// Records one call. Only [`super::Datum::tagged_span`] does this.
    pub(crate) fn record_tagged_span_call() {
        TAGGED_SPAN_CALLS.with(|calls| calls.set(calls.get().saturating_add(1)));
    }
}

/// One value, borrowing whatever holds its payload.
#[derive(Clone, Copy, Debug)]
pub enum Datum<'p> {
    /// SQL NULL.
    Null,
    /// A signed 64-bit integer.
    Int(i64),
    /// An IEEE-754 binary64.
    Real(f64),
    /// UTF-8 text. The database encoding is UTF-8 only.
    Text(&'p [u8]),
    /// Uninterpreted bytes.
    Blob(&'p [u8]),
}

/// The tag byte a tagged value carries in the page heap.
pub mod tag {
    /// SQL NULL, no payload.
    pub const NULL: u8 = 0;
    /// Eight little-endian bytes.
    pub const INT: u8 = 1;
    /// Eight little-endian bytes of IEEE-754 binary64.
    pub const REAL: u8 = 2;
    /// A `u32` length then that many UTF-8 bytes.
    pub const TEXT: u8 = 3;
    /// A `u32` length then that many bytes.
    pub const BLOB: u8 = 4;
    /// A reference to the run of pages holding the value: the same sixteen
    /// bytes the sorted region's heap holds. Whether it reads back as text or
    /// as a blob comes from the column's spec, exactly as it does there.
    ///
    /// **Only a delta row carries one.** The sorted region says a value is out
    /// of line in its class array, where the two bits cost nothing; a delta row
    /// has no class array, so the tag has to say it. A key column is never
    /// spilled, so a tag byte in a key position is always one of the four above.
    pub const EXTENT: u8 = 5;
}

/// How many bytes a tagged extent reference occupies.
pub const EXTENT_TAG_BYTES: usize = 17;

impl<'p> Datum<'p> {
    /// Returns the rank the class sorts at.
    ///
    /// Integer and real share a rank because SQLite compares them numerically
    /// rather than by class.
    pub fn sort_rank(&self) -> u8 {
        match self {
            Datum::Null => 0,
            Datum::Int(_) | Datum::Real(_) => 1,
            Datum::Text(_) => 2,
            Datum::Blob(_) => 3,
        }
    }

    /// Reports whether this is SQL NULL.
    pub fn is_null(&self) -> bool {
        matches!(self, Datum::Null)
    }

    /// Returns the integer this holds, if it holds one.
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Datum::Int(number) => Some(*number),
            _ => None,
        }
    }

    /// Returns the value as a double, for the numeric comparison rules.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Datum::Int(number) => Some(*number as f64),
            Datum::Real(number) => Some(*number),
            _ => None,
        }
    }

    /// Returns the borrowed bytes of a text or blob value.
    pub fn as_bytes(&self) -> Option<&'p [u8]> {
        match self {
            Datum::Text(bytes) | Datum::Blob(bytes) => Some(bytes),
            _ => None,
        }
    }

    /// Compares two values by SQLite's rules, under the BINARY collation.
    ///
    /// Classes are ordered NULL, numeric, text, blob. Two numerics compare
    /// numerically even when one is an integer and the other a real, which is
    /// why the integer path is tried first (exact) before falling back to
    /// doubles (which lose precision above 2^53).
    ///
    /// @param other - the value to compare against
    pub fn compare(&self, other: &Datum<'_>) -> Ordering {
        let (left, right) = (self.sort_rank(), other.sort_rank());
        if left != right {
            return left.cmp(&right);
        }
        match (self, other) {
            (Datum::Null, Datum::Null) => Ordering::Equal,
            (Datum::Int(a), Datum::Int(b)) => a.cmp(b),
            (Datum::Text(a), Datum::Text(b)) | (Datum::Blob(a), Datum::Blob(b)) => a.cmp(b),
            (Datum::Int(a), Datum::Real(b)) => compare_int_real(*a, *b),
            (Datum::Real(a), Datum::Int(b)) => compare_int_real(*b, *a).reverse(),
            _ => match (self.as_f64(), other.as_f64()) {
                (Some(a), Some(b)) => a.partial_cmp(&b).unwrap_or(Ordering::Equal),
                _ => Ordering::Equal,
            },
        }
    }

    /// Writes this value to a buffer as a tagged value.
    ///
    /// The tagged form is what the delta area, the `Any` mini-column and the
    /// exception heap all store. It is self-describing, so a reader that has
    /// lost track of the column's physical type can still decode it.
    ///
    /// @param out - the buffer the encoding is appended to
    pub fn encode_tagged(&self, out: &mut Vec<u8>) {
        match self {
            Datum::Null => out.push(tag::NULL),
            Datum::Int(number) => {
                out.push(tag::INT);
                out.extend_from_slice(&number.to_le_bytes());
            }
            Datum::Real(number) => {
                out.push(tag::REAL);
                out.extend_from_slice(&number.to_bits().to_le_bytes());
            }
            Datum::Text(bytes) => {
                out.push(tag::TEXT);
                out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
                out.extend_from_slice(bytes);
            }
            Datum::Blob(bytes) => {
                out.push(tag::BLOB);
                out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
                out.extend_from_slice(bytes);
            }
        }
    }

    /// Returns the number of bytes [`Datum::encode_tagged`] would write.
    pub fn tagged_len(&self) -> usize {
        match self {
            Datum::Null => 1,
            Datum::Int(_) | Datum::Real(_) => 9,
            Datum::Text(bytes) | Datum::Blob(bytes) => 5usize.saturating_add(bytes.len()),
        }
    }

    /// Decodes one tagged value from the front of a buffer.
    ///
    /// Returns the value and the number of bytes it consumed. Every length is
    /// checked against what is actually there, because these bytes come off a
    /// page and a page can be corrupt.
    ///
    /// @param bytes - the buffer to read from
    pub fn decode_tagged(bytes: &'p [u8]) -> DbResult<(Datum<'p>, usize)> {
        let (&kind, rest) = bytes
            .split_first()
            .ok_or_else(|| corrupt("tagged value has no tag byte"))?;
        match kind {
            tag::NULL => Ok((Datum::Null, 1)),
            tag::INT => {
                let raw = read_eight(rest, "tagged integer")?;
                Ok((Datum::Int(i64::from_le_bytes(raw)), 9))
            }
            tag::REAL => {
                let raw = read_eight(rest, "tagged real")?;
                Ok((Datum::Real(f64::from_bits(u64::from_le_bytes(raw))), 9))
            }
            tag::TEXT | tag::BLOB => {
                let length_bytes = rest
                    .get(..4)
                    .ok_or_else(|| corrupt("tagged string has no length"))?;
                let mut raw = [0u8; 4];
                raw.copy_from_slice(length_bytes);
                let length = u32::from_le_bytes(raw) as usize;
                let payload = rest
                    .get(4..4usize.saturating_add(length))
                    .ok_or_else(|| corrupt("tagged string runs past the buffer"))?;
                let value = if kind == tag::TEXT {
                    Datum::Text(payload)
                } else {
                    Datum::Blob(payload)
                };
                Ok((value, 5usize.saturating_add(length)))
            }
            tag::EXTENT => Err(corrupt(concat!(
                "this delta value is stored out of line; read the leaf's ",
                "extents through the tree first"
            ))),
            other => Err(corrupt(format!("tag byte {other} is not a value"))),
        }
    }

    /// Returns how many bytes the tagged value at the front of a buffer occupies.
    ///
    /// **The one reader that walks past a value without wanting it.** Decoding
    /// refuses an out-of-line value, because a `Datum` borrows bytes and the
    /// bytes of that one are on other pages - but a reader looking for column
    /// four still has to step over columns one to three whatever they hold.
    ///
    /// @param bytes - the buffer to measure from
    pub fn tagged_span(bytes: &[u8]) -> DbResult<usize> {
        #[cfg(test)]
        probe::record_tagged_span_call();
        let (&kind, rest) = bytes
            .split_first()
            .ok_or_else(|| corrupt("tagged value has no tag byte"))?;
        match kind {
            tag::NULL => Ok(1),
            tag::INT | tag::REAL => Ok(9),
            tag::EXTENT => {
                if rest.len() < EXTENT_TAG_BYTES - 1 {
                    return Err(corrupt("tagged extent runs past the buffer"));
                }
                Ok(EXTENT_TAG_BYTES)
            }
            tag::TEXT | tag::BLOB => {
                let length_bytes = rest
                    .get(..4)
                    .ok_or_else(|| corrupt("tagged string has no length"))?;
                let mut raw = [0u8; 4];
                raw.copy_from_slice(length_bytes);
                let length = u32::from_le_bytes(raw) as usize;
                if rest.len() < 4usize.saturating_add(length) {
                    return Err(corrupt("tagged string runs past the buffer"));
                }
                Ok(5usize.saturating_add(length))
            }
            other => Err(corrupt(format!("tag byte {other} is not a value"))),
        }
    }

    /// Returns the tag byte at the front of a buffer.
    ///
    /// @param bytes - the buffer to read from
    pub fn tag_of(bytes: &[u8]) -> DbResult<u8> {
        bytes
            .first()
            .copied()
            .ok_or_else(|| corrupt("tagged value has no tag byte"))
    }
}

/// A value that owns its payload.
///
/// The executor never produces one of these: everything inside a pipeline
/// borrows a pinned page or the statement arena. They exist for the two places
/// that genuinely have to outlive a page - a leaf being rewritten under the
/// rows it used to hold, and a sort or hash table whose input pages have moved
/// on - and nowhere else. Keeping them a distinct type rather than a lifetime
/// escape hatch is what stops "just make it owned" from spreading into the scan.
// `PartialEq` and not `Eq`, because a `Real` holds an `f64` and NaN is not
// equal to itself. The derive compares *representations* - `Int(1)` is not
// `Real(1.0)` - which is what a test asserting on a materialised row wants;
// SQL's own numeric comparison is [`Datum::compare`] and is a different
// question with a different answer.
#[derive(Clone, Debug, PartialEq)]
pub enum OwnedDatum {
    /// SQL NULL.
    Null,
    /// A signed 64-bit integer.
    Int(i64),
    /// An IEEE-754 binary64.
    Real(f64),
    /// UTF-8 text.
    Text(Vec<u8>),
    /// Uninterpreted bytes.
    Blob(Vec<u8>),
}

impl OwnedDatum {
    /// Copies a borrowed value into owned storage.
    ///
    /// @param value - the value to copy
    pub fn from_datum(value: &Datum<'_>) -> OwnedDatum {
        match value {
            Datum::Null => OwnedDatum::Null,
            Datum::Int(number) => OwnedDatum::Int(*number),
            Datum::Real(number) => OwnedDatum::Real(*number),
            Datum::Text(bytes) => OwnedDatum::Text(bytes.to_vec()),
            Datum::Blob(bytes) => OwnedDatum::Blob(bytes.to_vec()),
        }
    }

    /// Returns a borrowed view of this value.
    pub fn borrow(&self) -> Datum<'_> {
        match self {
            OwnedDatum::Null => Datum::Null,
            OwnedDatum::Int(number) => Datum::Int(*number),
            OwnedDatum::Real(number) => Datum::Real(*number),
            OwnedDatum::Text(bytes) => Datum::Text(bytes),
            OwnedDatum::Blob(bytes) => Datum::Blob(bytes),
        }
    }
}

/// Copies a borrowed row into owned storage.
///
/// Compares an integer and a double exactly, the way SQLite's
/// `sqlite3IntFloatCompare` does.
///
/// **`a as f64` is not a comparison, it is a lossy conversion, and this is
/// where that mattered (task-1932, H7).** Every integer above 2^53 has
/// several near neighbours that share its double, so widening both sides made
/// `9223372036854775807` compare equal to `9223372036854775808.0`. A seek key
/// that overflowed to that double therefore found the row at `i64::MAX` and
/// returned it, where SQLite returns nothing - and the same widening would
/// equate any two large rowids whose doubles collide.
///
/// The rule here is SQLite's own, in its no-long-double form: a double outside
/// the `i64` range settles the comparison by itself, and inside the range the
/// double is truncated towards zero to an integer, the two integers are
/// compared, and the truncated part breaks a tie. Nothing is widened.
///
/// @param left - the integer operand
/// @param right - the double operand
pub fn compare_int_real(left: i64, right: f64) -> Ordering {
    if right.is_nan() {
        // SQLite sorts NaN with NULL, below every number, so an integer is
        // greater than one. `Datum::compare` never reaches this with a NaN
        // from the tree - a NaN is not storable - but a comparison against a
        // computed one does.
        return Ordering::Greater;
    }
    // `-9223372036854775808.0` is exactly `i64::MIN`, so the bound below it is
    // strict; `9223372036854775808.0` is one past `i64::MAX`, so the bound
    // above it is not.
    if right < -9_223_372_036_854_775_808.0 {
        return Ordering::Greater;
    }
    if right >= 9_223_372_036_854_775_808.0 {
        return Ordering::Less;
    }
    let truncated = right as i64;
    match left.cmp(&truncated) {
        Ordering::Equal => {}
        other => return other,
    }
    // Equal down to the integer part, so the fraction decides. `truncated as
    // f64` is exact here: it is a value the double already held.
    let whole = truncated as f64;
    if whole < right {
        Ordering::Less
    } else if whole > right {
        Ordering::Greater
    } else {
        Ordering::Equal
    }
}

/// @param row - the row to copy
pub fn own_row(row: &[Datum<'_>]) -> Vec<OwnedDatum> {
    row.iter().map(OwnedDatum::from_datum).collect()
}

/// Returns a borrowed view of an owned row.
///
/// @param row - the owned row
pub fn borrow_row(row: &[OwnedDatum]) -> Vec<Datum<'_>> {
    row.iter().map(OwnedDatum::borrow).collect()
}

/// Reads eight bytes, or says the buffer was short.
///
/// @param bytes - the buffer to read from
/// @param what - what the caller was reading, for the error message
fn read_eight(bytes: &[u8], what: &str) -> DbResult<[u8; 8]> {
    let slice = bytes
        .get(..8)
        .ok_or_else(|| corrupt(format!("{what} is shorter than eight bytes")))?;
    let mut raw = [0u8; 8];
    raw.copy_from_slice(slice);
    Ok(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every class round-trips through the tagged encoding, and the reported
    /// length matches what was written.
    #[test]
    fn tagged_values_round_trip() {
        let cases = [
            Datum::Null,
            Datum::Int(0),
            Datum::Int(i64::MIN),
            Datum::Int(i64::MAX),
            Datum::Real(0.0),
            Datum::Real(-1.5),
            Datum::Real(f64::INFINITY),
            Datum::Text(b""),
            Datum::Text(b"hello"),
            Datum::Blob(&[0, 1, 2, 255]),
        ];
        for case in cases {
            let mut buffer = Vec::new();
            case.encode_tagged(&mut buffer);
            assert_eq!(buffer.len(), case.tagged_len(), "{case:?}");
            let (back, used) = Datum::decode_tagged(&buffer).unwrap();
            assert_eq!(used, buffer.len(), "{case:?}");
            assert_eq!(back.compare(&case), Ordering::Equal, "{case:?}");
        }
    }

    /// A truncation at every length inside every encoding is refused, and none
    /// of them panics. This is the "corrupt every field" shape the TDD asks for,
    /// applied to the smallest codec.
    #[test]
    fn every_truncation_is_refused() {
        let cases = [
            Datum::Int(7),
            Datum::Real(2.5),
            Datum::Text(b"abcdef"),
            Datum::Blob(&[9; 9]),
        ];
        for case in cases {
            let mut buffer = Vec::new();
            case.encode_tagged(&mut buffer);
            for cut in 0..buffer.len() {
                let short = &buffer[..cut];
                assert!(
                    Datum::decode_tagged(short).is_err(),
                    "{case:?} cut at {cut}"
                );
            }
        }
    }

    /// Every tag byte outside the set is refused.
    #[test]
    fn unknown_tags_are_refused() {
        for tag in 5u8..=255 {
            let buffer = [tag, 0, 0, 0, 0, 0, 0, 0, 0];
            assert!(Datum::decode_tagged(&buffer).is_err(), "tag {tag}");
        }
    }

    /// Class ordering is SQLite's, and a mixed integer/real pair compares
    /// numerically rather than by class.
    #[test]
    fn comparison_follows_the_dialect() {
        assert_eq!(Datum::Null.compare(&Datum::Int(-1)), Ordering::Less);
        assert_eq!(Datum::Int(1).compare(&Datum::Text(b"")), Ordering::Less);
        assert_eq!(Datum::Text(b"z").compare(&Datum::Blob(b"")), Ordering::Less);
        assert_eq!(Datum::Int(2).compare(&Datum::Real(2.5)), Ordering::Less);
        assert_eq!(Datum::Real(2.5).compare(&Datum::Int(3)), Ordering::Less);
        assert_eq!(Datum::Int(3).compare(&Datum::Real(3.0)), Ordering::Equal);
        assert_eq!(
            Datum::Text(b"a").compare(&Datum::Text(b"ab")),
            Ordering::Less
        );
    }

    /// Integers compare exactly, including past the point where a double would
    /// lose the difference.
    #[test]
    fn large_integers_compare_exactly() {
        let a = Datum::Int(9_007_199_254_740_993);
        let b = Datum::Int(9_007_199_254_740_992);
        assert_eq!(a.compare(&b), Ordering::Greater);
        assert_eq!(a.as_f64(), b.as_f64(), "the doubles really are equal");
    }
}
