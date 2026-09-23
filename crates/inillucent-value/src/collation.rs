//! Collating sequences: BINARY, NOCASE, RTRIM, and the registry that names
//! them.
//!
//! Invariant: a collation compares bytes, never characters, and never consults
//! a locale. NOCASE folds `A`-`Z` and nothing else - not `\u{c9}`, not
//! `\u{130}` - because that is what SQLite's `sqlite3UpperToLower` table does,
//! and a collation that folded more would order an existing index differently
//! from the engine that built it.
//!
//! Every built-in prefers UTF-8, so operands reach a collation as UTF-8 bytes.
//! `compare_text` performs that conversion; a UTF-16 database pays for it on
//! the comparison path, exactly as SQLite does.
//!
//! A named collation carries a generation. Replacing a collation bumps it, and
//! anything that recorded the old generation - a prepared statement, an index
//! whose keys were ordered by it - can see that it is out of date rather than
//! silently returning rows in an order the index no longer has.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock, RwLock};

use crate::encoding::{self, TextEncoding};

/// What an application-defined collation does.
pub type Comparator = Arc<dyn Fn(&[u8], &[u8]) -> Ordering + Send + Sync>;

/// One application-defined collation.
struct CustomCollation {
    /// The name it was registered under, for `PRAGMA collation_list`.
    name: String,
    /// What it does.
    body: Comparator,
}

/// Every application-defined collation, indexed by the id inside `Collation`.
///
/// It only grows. See the module comment: an id that appears in a prepared
/// statement or an index key must still resolve years later, and a table that
/// reused ids could give it the wrong comparator.
fn custom_table() -> &'static RwLock<Vec<CustomCollation>> {
    static TABLE: OnceLock<RwLock<Vec<CustomCollation>>> = OnceLock::new();
    TABLE.get_or_init(|| RwLock::new(Vec::new()))
}

/// Registers a comparator and returns the collation that names it.
pub fn register_custom(name: &str, body: Comparator) -> Collation {
    let Ok(mut table) = custom_table().write() else {
        return Collation::Binary;
    };
    table.push(CustomCollation {
        name: name.to_string(),
        body,
    });
    Collation::Custom(table.len().saturating_sub(1) as u32)
}

/// Returns the name an application-defined collation was registered under.
pub fn custom_name(id: u32) -> Option<String> {
    let table = custom_table().read().ok()?;
    table.get(id as usize).map(|entry| entry.name.clone())
}

/// Returns an application-defined collation's comparator.
fn custom_body(id: u32) -> Option<Comparator> {
    let table = custom_table().read().ok()?;
    table.get(id as usize).map(|entry| Arc::clone(&entry.body))
}

/// A built-in collating sequence.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum Collation {
    /// Compare the bytes, then the lengths. The default for every column.
    Binary,
    /// Fold ASCII letters, compare up to a NUL both sides hold, then compare
    /// the lengths.
    NoCase,
    /// Compare the bytes, ignoring trailing spaces on either side.
    RTrim,
    /// Compare text as decimal numbers of unbounded precision.
    ///
    /// `ext/misc/decimal.c`, which the reference shell compiles in - so a
    /// database written against `sqlite3` may name it in a column declaration,
    /// and an engine without it could not open that schema.
    Decimal,
    /// Compare text whose embedded runs of digits compare as numbers.
    ///
    /// `ext/misc/uint.c`. `x2` sorts before `x10` under it and after it under
    /// `BINARY`, which is the whole point of it.
    Uint,
    /// An application-defined comparison, by its id in the process-wide table.
    ///
    /// The id rather than the function is what makes `Collation` stay `Copy`,
    /// which is what lets it travel through the record codec and the b-tree
    /// without either of them knowing this variant exists.
    Custom(u32),
}

impl Collation {
    /// Returns the name the collation is registered under.
    pub fn name(self) -> &'static str {
        match self {
            Collation::Binary => "BINARY",
            Collation::NoCase => "NOCASE",
            Collation::RTrim => "RTRIM",
            Collation::Decimal => "decimal",
            Collation::Uint => "uint",
            // A custom collation's name is not static - it was chosen at run
            // time - so `custom_name` is what answers for one, and this is the
            // honest placeholder for a caller that wanted a `&'static str`.
            Collation::Custom(_) => "CUSTOM",
        }
    }

    /// Returns the name this collation is known by, custom ones included.
    pub fn display_name(self) -> String {
        match self {
            Collation::Custom(id) => custom_name(id).unwrap_or_else(|| "CUSTOM".to_string()),
            other => other.name().to_string(),
        }
    }

    /// Returns the built-in a name selects, matching case-insensitively as
    /// SQLite does.
    pub fn from_name(name: &str) -> Option<Collation> {
        if name.eq_ignore_ascii_case("BINARY") {
            Some(Collation::Binary)
        } else if name.eq_ignore_ascii_case("NOCASE") {
            Some(Collation::NoCase)
        } else if name.eq_ignore_ascii_case("RTRIM") {
            Some(Collation::RTrim)
        } else if name.eq_ignore_ascii_case("decimal") {
            Some(Collation::Decimal)
        } else if name.eq_ignore_ascii_case("uint") {
            Some(Collation::Uint)
        } else {
            None
        }
    }

    /// Compares two byte strings that are already UTF-8.
    pub fn compare_bytes(self, left: &[u8], right: &[u8]) -> Ordering {
        match self {
            Collation::Binary => compare_binary(left, right),
            Collation::NoCase => compare_nocase(left, right),
            Collation::RTrim => compare_rtrim(left, right),
            Collation::Decimal => compare_decimal(left, right),
            Collation::Uint => compare_uint(left, right),
            // A comparator that has gone missing cannot happen - the table only
            // grows - but falling back to BINARY is better than a panic on the
            // comparison path, which is the hottest path in the engine.
            Collation::Custom(id) => match custom_body(id) {
                Some(body) => body(left, right),
                None => compare_binary(left, right),
            },
        }
    }

    /// Returns every built-in, for exhaustive matrices.
    pub fn all() -> [Collation; 5] {
        [
            Collation::Binary,
            Collation::NoCase,
            Collation::RTrim,
            Collation::Decimal,
            Collation::Uint,
        ]
    }

    /// Reports whether ordering under this collation survives the key encoder.
    ///
    /// `BINARY`, `NOCASE` and `RTRIM` each have a byte transformation whose
    /// natural order *is* the collation order, which is what lets an index
    /// answer a range or an `ORDER BY` by walking it. `decimal`, `uint` and an
    /// application comparator have no such transformation - "9" sorts after
    /// "10" by bytes and before it by value - so an index keyed under one of
    /// them is a set rather than a sequence, and the planner must not read an
    /// ordering out of it.
    pub fn is_order_preserving_in_keys(self) -> bool {
        matches!(
            self,
            Collation::Binary | Collation::NoCase | Collation::RTrim
        )
    }
}

impl Default for Collation {
    /// BINARY, which is the collation of a column that names none.
    fn default() -> Collation {
        Collation::Binary
    }
}

/// Compares two text values, converting each to UTF-8 first.
pub fn compare_text(
    left: &[u8],
    left_encoding: TextEncoding,
    right: &[u8],
    right_encoding: TextEncoding,
    collation: Collation,
) -> Ordering {
    let left_utf8 = encoding::to_utf8(left, left_encoding);
    let right_utf8 = encoding::to_utf8(right, right_encoding);
    collation.compare_bytes(left_utf8.as_ref(), right_utf8.as_ref())
}

/// BINARY: compare the shared prefix, then the lengths.
fn compare_binary(left: &[u8], right: &[u8]) -> Ordering {
    let shared = left.len().min(right.len());
    match (left.get(..shared), right.get(..shared)) {
        (Some(left_prefix), Some(right_prefix)) => match left_prefix.cmp(right_prefix) {
            Ordering::Equal => left.len().cmp(&right.len()),
            other => other,
        },
        _ => left.len().cmp(&right.len()),
    }
}

/// NOCASE: fold ASCII letters over the shared prefix, stopping at a NUL both
/// sides hold, then compare lengths.
///
/// **The NUL is SQLite's rule, and it is not the obvious one.** SQLite's
/// `nocaseCollatingFunc` calls `sqlite3StrNICmp`, a C string walk whose loop
/// condition includes `*a != 0`. At a NUL in the left operand it stops and
/// subtracts the two folded bytes at that position. If the right byte is not a
/// NUL, that is an ordinary mismatch and this loop finds it the same way. If it
/// is, the walk answers zero, the bytes after the NUL are never read, and the
/// comparison falls through to the byte lengths. So `x'0061'` sorts before
/// `x'000079'` by length, and `x'0061'` is equal to `x'0062'`. Reading every
/// byte of the shared prefix, which this function did until task-2079, put
/// `x'000079'` first.
///
/// [`nocase_key_bytes`] is the byte transformation whose natural order is this
/// order, and the two have to change together.
///
/// @param left - the left operand, UTF-8
/// @param right - the right operand, UTF-8
fn compare_nocase(left: &[u8], right: &[u8]) -> Ordering {
    let shared = left.len().min(right.len());
    for index in 0..shared {
        let left_byte = left.get(index).copied().unwrap_or(0);
        let right_byte = right.get(index).copied().unwrap_or(0);
        if left_byte == 0 && right_byte == 0 {
            break;
        }
        match left_byte
            .to_ascii_lowercase()
            .cmp(&right_byte.to_ascii_lowercase())
        {
            Ordering::Equal => continue,
            other => return other,
        }
    }
    left.len().cmp(&right.len())
}

/// Appends the bytes whose natural order is NOCASE's order for one value.
///
/// Without a NUL this is the value with `A`-`Z` folded to lower case. With one,
/// it is the folded bytes before the first NUL, then that NUL, then the
/// value's whole length as eight big-endian bytes, and nothing of what follows
/// the NUL.
///
/// **Why that is the collation's order.** Take two values whose folded bytes
/// agree up to some position. If they first differ there by a byte that is not
/// a NUL on both sides, both forms hold those two bytes there - a folded letter
/// is never zero - and both orders are decided by them. If one value ends
/// there, it is the shorter form and the shorter value. If both hold a NUL
/// there, [`Collation::compare_bytes`] stops and compares the lengths, and the
/// two forms hold equal bytes up to the NUL and then two lengths of the same
/// width, which compare as the numbers do. Two values of one length that agree
/// up to a shared NUL are equal under NOCASE and their forms are identical,
/// which is what lets a `UNIQUE` index or a `DISTINCT` see them as one value.
///
/// A key encoder escapes these bytes as it escapes any payload, and escaping
/// keeps byte order. So the key order is the collation order and
/// [`Collation::is_order_preserving_in_keys`] stays true for NOCASE.
///
/// Returns whether the value held a NUL, which is whether anything this
/// appended is a zero byte. A key encoder that escapes zeros can skip its own
/// scan for one when this says no, so the NUL rule costs the index build no
/// second pass over the text.
///
/// @param bytes - the value, UTF-8
/// @param out - the buffer to append to
pub fn nocase_key_bytes(bytes: &[u8], out: &mut Vec<u8>) -> bool {
    let start = out.len();
    let before_nul = match bytes.iter().position(|byte| *byte == 0) {
        None => bytes,
        Some(at) => bytes.get(..at).unwrap_or(bytes),
    };
    out.extend_from_slice(before_nul);
    if let Some(folded) = out.get_mut(start..) {
        folded.make_ascii_lowercase();
    }
    if before_nul.len() == bytes.len() {
        return false;
    }
    out.push(0);
    out.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
    true
}

/// RTRIM: BINARY, except that a longer string whose tail is all spaces is
/// equal rather than greater.
fn compare_rtrim(left: &[u8], right: &[u8]) -> Ordering {
    let shared = left.len().min(right.len());
    for index in 0..shared {
        let left_byte = left.get(index).copied().unwrap_or(0);
        let right_byte = right.get(index).copied().unwrap_or(0);
        match left_byte.cmp(&right_byte) {
            Ordering::Equal => continue,
            other => return other,
        }
    }
    let left_tail = left.get(shared..).unwrap_or(&[]);
    let right_tail = right.get(shared..).unwrap_or(&[]);
    if all_spaces(left_tail) && all_spaces(right_tail) {
        Ordering::Equal
    } else {
        left.len().cmp(&right.len())
    }
}

/// Reports whether every byte is a space.
fn all_spaces(bytes: &[u8]) -> bool {
    bytes.iter().all(|byte| *byte == b' ')
}

/// A collation as the catalog knows it: a name, a behaviour, and a generation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CollationEntry {
    /// The name as registered, in its original case.
    pub name: String,
    /// Which comparison the name resolves to.
    pub collation: Collation,
    /// How many times this name has been defined. A consumer that recorded an
    /// older generation must re-plan or reindex before trusting an ordering.
    pub generation: u64,
}

/// The named collations a connection knows.
///
/// The registry is deliberately not a global: SQLite lets an application
/// replace a collation on one connection, and an index built under the old one
/// is stale for that connection alone.
#[derive(Clone, Debug)]
pub struct CollationRegistry {
    entries: BTreeMap<String, CollationEntry>,
    generation: u64,
}

impl Default for CollationRegistry {
    /// A registry holding the three built-ins, each at generation one.
    fn default() -> CollationRegistry {
        let mut registry = CollationRegistry {
            entries: BTreeMap::new(),
            generation: 0,
        };
        for collation in Collation::all() {
            registry.define(collation.name(), collation);
        }
        registry
    }
}

impl CollationRegistry {
    /// Defines or replaces a named collation, returning its new generation.
    ///
    /// Replacing an existing name bumps the registry's generation as well as
    /// the entry's, so a consumer can hold one number rather than one per
    /// name and still notice that something changed.
    pub fn define(&mut self, name: &str, collation: Collation) -> u64 {
        self.generation = self.generation.saturating_add(1);
        let entry = CollationEntry {
            name: name.to_string(),
            collation,
            generation: self.generation,
        };
        self.entries.insert(name.to_ascii_uppercase(), entry);
        self.generation
    }

    /// Looks a collation up by name, case-insensitively.
    pub fn get(&self, name: &str) -> Option<&CollationEntry> {
        self.entries.get(&name.to_ascii_uppercase())
    }

    /// Returns the registry's current generation.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Reports whether a recorded generation is still current for a name.
    ///
    /// A prepared statement or an index records the generation it was built
    /// under; when this answers false the ordering it assumed may no longer
    /// hold, and it must be re-planned or reindexed rather than trusted.
    pub fn is_current(&self, name: &str, generation: u64) -> bool {
        self.get(name)
            .is_some_and(|entry| entry.generation == generation)
    }

    /// Returns the names currently defined, in a stable order.
    pub fn names(&self) -> Vec<String> {
        self.entries
            .values()
            .map(|entry| entry.name.clone())
            .collect()
    }
}

/// Compares two decimal numbers written as text, at unbounded precision.
///
/// The comparison from `ext/misc/decimal.c`, without its arithmetic: leading
/// space is ignored, an optional sign is read, and the two numbers are then
/// compared by magnitude with the decimal points lined up. Text that is not a
/// number at all compares as zero, which is what the reference produces for it.
///
/// @param left - the first value UTF-8 bytes
/// @param right - the second value UTF-8 bytes
fn compare_decimal(left: &[u8], right: &[u8]) -> Ordering {
    let (left_negative, left_whole, left_fraction) = decimal_parts(left);
    let (right_negative, right_whole, right_fraction) = decimal_parts(right);
    let left_zero = left_whole.is_empty() && left_fraction.is_empty();
    let right_zero = right_whole.is_empty() && right_fraction.is_empty();
    // A signed zero is still zero, so the sign is only read once both sides are
    // known to be something.
    if left_zero && right_zero {
        return Ordering::Equal;
    }
    let magnitude = compare_decimal_magnitude(
        (&left_whole, &left_fraction),
        (&right_whole, &right_fraction),
    );
    match (left_negative && !left_zero, right_negative && !right_zero) {
        (false, true) => Ordering::Greater,
        (true, false) => Ordering::Less,
        (false, false) => magnitude,
        (true, true) => magnitude.reverse(),
    }
}

/// Splits decimal text into a sign and its two runs of digits.
///
/// Leading zeros go from the whole part and trailing zeros from the fraction,
/// so `007.50` and `7.5` come back identical and compare equal.
///
/// @param bytes - the text
fn decimal_parts(bytes: &[u8]) -> (bool, Vec<u8>, Vec<u8>) {
    let mut at = 0usize;
    while bytes.get(at).is_some_and(u8::is_ascii_whitespace) {
        at += 1;
    }
    let negative = match bytes.get(at) {
        Some(b'-') => {
            at += 1;
            true
        }
        Some(b'+') => {
            at += 1;
            false
        }
        _ => false,
    };
    let mut whole = Vec::new();
    while let Some(digit) = bytes.get(at).filter(|byte| byte.is_ascii_digit()) {
        whole.push(*digit);
        at += 1;
    }
    let mut fraction = Vec::new();
    if bytes.get(at) == Some(&b'.') {
        at += 1;
        while let Some(digit) = bytes.get(at).filter(|byte| byte.is_ascii_digit()) {
            fraction.push(*digit);
            at += 1;
        }
    }
    while whole.first() == Some(&b'0') {
        whole.remove(0);
    }
    while fraction.last() == Some(&b'0') {
        fraction.pop();
    }
    (negative, whole, fraction)
}

/// Compares two non-negative decimals given as their two digit runs.
///
/// @param left - the first number whole and fractional digits
/// @param right - the second number whole and fractional digits
fn compare_decimal_magnitude(left: (&[u8], &[u8]), right: (&[u8], &[u8])) -> Ordering {
    let (left_whole, left_fraction) = left;
    let (right_whole, right_fraction) = right;
    // More digits before the point is a larger number, once leading zeros are
    // gone - which is why they are stripped before anything is compared.
    match left_whole.len().cmp(&right_whole.len()) {
        Ordering::Equal => {}
        other => return other,
    }
    match left_whole.cmp(right_whole) {
        Ordering::Equal => {}
        other => return other,
    }
    // The fractions line up at the point, so a plain byte comparison of the
    // digit runs is the right one: the shorter runs out first and the longer is
    // then larger, which the slice ordering already says.
    left_fraction.cmp(right_fraction)
}

/// Compares text whose runs of digits compare as numbers.
///
/// A port of the comparator in `ext/misc/uint.c`: outside a digit run the bytes
/// decide, and inside one the two runs are compared by length once leading
/// zeros are dropped, then by digits.
///
/// @param left - the first value UTF-8 bytes
/// @param right - the second value UTF-8 bytes
fn compare_uint(left: &[u8], right: &[u8]) -> Ordering {
    let mut i = 0usize;
    let mut j = 0usize;
    while i < left.len() && j < right.len() {
        let a = left.get(i).copied().unwrap_or(0);
        let b = right.get(j).copied().unwrap_or(0);
        if a.is_ascii_digit() {
            if !b.is_ascii_digit() {
                return a.cmp(&b);
            }
            while left.get(i) == Some(&b'0') {
                i += 1;
            }
            while right.get(j) == Some(&b'0') {
                j += 1;
            }
            let mut run = 0usize;
            while left.get(i + run).is_some_and(u8::is_ascii_digit)
                && right.get(j + run).is_some_and(u8::is_ascii_digit)
            {
                run += 1;
            }
            // Whichever run is still going once the other has stopped is the
            // longer number, and the longer number is the larger one.
            if left.get(i + run).is_some_and(u8::is_ascii_digit) {
                return Ordering::Greater;
            }
            if right.get(j + run).is_some_and(u8::is_ascii_digit) {
                return Ordering::Less;
            }
            let a_run = left.get(i..i + run).unwrap_or(&[]);
            let b_run = right.get(j..j + run).unwrap_or(&[]);
            match a_run.cmp(b_run) {
                Ordering::Equal => {}
                other => return other,
            }
            i += run;
            j += run;
        } else if a != b {
            return a.cmp(&b);
        } else {
            i += 1;
            j += 1;
        }
    }
    (left.len() - i).cmp(&(right.len() - j))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// BINARY compares bytes and then lengths, so a prefix sorts first.
    #[test]
    fn binary_compares_bytes_then_length() {
        assert_eq!(
            Collation::Binary.compare_bytes(b"abc", b"abc"),
            Ordering::Equal
        );
        assert_eq!(
            Collation::Binary.compare_bytes(b"abc", b"abd"),
            Ordering::Less
        );
        assert_eq!(
            Collation::Binary.compare_bytes(b"ab", b"abc"),
            Ordering::Less
        );
        assert_eq!(Collation::Binary.compare_bytes(b"", b""), Ordering::Equal);
        // Upper case sorts before lower case, because 'A' is 0x41.
        assert_eq!(
            Collation::Binary.compare_bytes(b"ABC", b"abc"),
            Ordering::Less
        );
    }

    /// NOCASE folds only the ASCII letters, and a folded tie falls back to the
    /// length rather than to the unfolded bytes.
    #[test]
    fn nocase_folds_ascii_only() {
        assert_eq!(
            Collation::NoCase.compare_bytes(b"ABC", b"abc"),
            Ordering::Equal
        );
        assert_eq!(
            Collation::NoCase.compare_bytes(b"aBc", b"AbC"),
            Ordering::Equal
        );
        assert_eq!(
            Collation::NoCase.compare_bytes(b"abc", b"abcd"),
            Ordering::Less
        );
        // Not ASCII, so not folded: the two forms of a Turkish dotted i stay
        // different, as they do in SQLite.
        let upper = "\u{130}".as_bytes();
        let lower = "\u{69}".as_bytes();
        assert_ne!(
            Collation::NoCase.compare_bytes(upper, lower),
            Ordering::Equal
        );
        // Accented letters are not folded either.
        assert_ne!(
            Collation::NoCase.compare_bytes("\u{c9}".as_bytes(), "\u{e9}".as_bytes()),
            Ordering::Equal
        );
    }

    /// SQLite's `nocaseCollatingFunc`, transcribed from `main.c` and `util.c`
    /// with the pointer walk kept as it is written there.
    ///
    /// @param left - the left operand
    /// @param right - the right operand
    fn sqlite_nocase(left: &[u8], right: &[u8]) -> Ordering {
        // sqlite3StrNICmp(zLeft, zRight, N):
        //   while( N-- > 0 && *a!=0 && UpperToLower[*a]==UpperToLower[*b]){ a++; b++; }
        //   return N<0 ? 0 : UpperToLower[*a] - UpperToLower[*b];
        let mut remaining = left.len().min(right.len()) as i64;
        let mut at = 0usize;
        let mut difference = 0i32;
        loop {
            let more = remaining > 0;
            remaining -= 1;
            if !more {
                break;
            }
            let a = left[at].to_ascii_lowercase();
            let b = right[at].to_ascii_lowercase();
            if left[at] == 0 || a != b {
                difference = i32::from(a) - i32::from(b);
                break;
            }
            at += 1;
        }
        if remaining < 0 {
            difference = 0;
        }
        // nocaseCollatingFunc: if( 0==r ) r = nKey1-nKey2;
        if difference == 0 {
            left.len().cmp(&right.len())
        } else {
            difference.cmp(&0)
        }
    }

    /// Every string of up to four bytes over NUL, `a`, `A`, `b` and `y`.
    fn short_strings() -> Vec<Vec<u8>> {
        let alphabet = [0u8, b'a', b'A', b'b', b'y'];
        let mut all: Vec<Vec<u8>> = vec![Vec::new()];
        let mut layer: Vec<Vec<u8>> = vec![Vec::new()];
        for _ in 0..4 {
            let mut next = Vec::new();
            for prefix in &layer {
                for byte in alphabet {
                    let mut grown = prefix.clone();
                    grown.push(byte);
                    next.push(grown);
                }
            }
            all.extend(next.iter().cloned());
            layer = next;
        }
        all
    }

    /// NOCASE agrees with SQLite's own walk on every pair of short strings,
    /// NULs included (task-2079).
    ///
    /// 781 strings, so 609,961 ordered pairs. The two cases the ticket names
    /// are asserted by value as well, so a reader does not have to trust the
    /// transcription to see what the rule says.
    #[test]
    fn nocase_matches_sqlite_at_an_embedded_nul() {
        assert_eq!(
            Collation::NoCase.compare_bytes(b"\0a", b"\0\0y"),
            Ordering::Less
        );
        assert_eq!(
            Collation::NoCase.compare_bytes(b"\0a", b"\0b"),
            Ordering::Equal
        );
        assert_eq!(
            Collation::NoCase.compare_bytes(b"A\0z", b"a\0b"),
            Ordering::Equal
        );
        assert_eq!(
            Collation::NoCase.compare_bytes(b"a\0", b"ab"),
            Ordering::Less
        );
        let strings = short_strings();
        assert_eq!(strings.len(), 781);
        for left in &strings {
            for right in &strings {
                assert_eq!(
                    Collation::NoCase.compare_bytes(left, right),
                    sqlite_nocase(left, right),
                    "{left:?} against {right:?}"
                );
            }
        }
    }

    /// The bytes `nocase_key_bytes` produces order every pair exactly as
    /// NOCASE does, which is what keeps a NOCASE index ordered (task-2079).
    #[test]
    fn nocase_key_bytes_order_is_the_nocase_order() {
        let strings = short_strings();
        let keys: Vec<Vec<u8>> = strings
            .iter()
            .map(|value| {
                let mut out = Vec::new();
                nocase_key_bytes(value, &mut out);
                out
            })
            .collect();
        for (left, left_key) in strings.iter().zip(&keys) {
            for (right, right_key) in strings.iter().zip(&keys) {
                assert_eq!(
                    left_key.cmp(right_key),
                    Collation::NoCase.compare_bytes(left, right),
                    "{left:?} against {right:?}"
                );
            }
        }
        // And the form itself, for the case with a NUL: the folded bytes before
        // it, the NUL, the whole length, and nothing after.
        let mut out = vec![0xEE];
        assert!(nocase_key_bytes(b"Ab\0CD", &mut out));
        assert_eq!(out, [0xEE, b'a', b'b', 0, 0, 0, 0, 0, 0, 0, 0, 5]);
        // Without a NUL it is the folded bytes and nothing else, and it says
        // so, which is what lets the key encoder skip its scan for a zero.
        let mut plain = Vec::new();
        assert!(!nocase_key_bytes(b"AbC", &mut plain));
        assert_eq!(plain, b"abc");
    }

    /// RTRIM ignores trailing spaces on either side, and only trailing spaces.
    #[test]
    fn rtrim_ignores_only_trailing_spaces() {
        assert_eq!(
            Collation::RTrim.compare_bytes(b"abc", b"abc   "),
            Ordering::Equal
        );
        assert_eq!(
            Collation::RTrim.compare_bytes(b"abc   ", b"abc"),
            Ordering::Equal
        );
        assert_eq!(
            Collation::RTrim.compare_bytes(b"  abc", b"abc"),
            Ordering::Less
        );
        assert_eq!(
            Collation::RTrim.compare_bytes(b"abc", b"abc\t"),
            Ordering::Less
        );
        assert_eq!(
            Collation::RTrim.compare_bytes(b"abc ", b"abd"),
            Ordering::Less
        );
        // Two strings of spaces of different lengths are equal.
        assert_eq!(Collation::RTrim.compare_bytes(b"", b"   "), Ordering::Equal);
    }

    /// Every collation must be a total order: antisymmetric, transitive, and
    /// reflexive. An index built on one that is not would be unsearchable.
    #[test]
    fn every_collation_is_a_total_order() {
        let samples: [&[u8]; 12] = [
            b"", b" ", b"  ", b"a", b"A", b"a ", b"ab", b"AB", b"b", b"B", b"abc", b"ABC ",
        ];
        for collation in Collation::all() {
            for left in samples {
                assert_eq!(
                    collation.compare_bytes(left, left),
                    Ordering::Equal,
                    "{collation:?} is not reflexive"
                );
                for right in samples {
                    let forward = collation.compare_bytes(left, right);
                    let backward = collation.compare_bytes(right, left);
                    assert_eq!(
                        forward,
                        backward.reverse(),
                        "{collation:?} is not antisymmetric on {left:?} {right:?}"
                    );
                    for third in samples {
                        let first = collation.compare_bytes(left, right);
                        let second = collation.compare_bytes(right, third);
                        if first == second && first != Ordering::Equal {
                            assert_eq!(
                                collation.compare_bytes(left, third),
                                first,
                                "{collation:?} is not transitive on {left:?} {right:?} {third:?}"
                            );
                        }
                    }
                }
            }
        }
    }

    /// Text in a UTF-16 database compares by its UTF-8 form, so the same
    /// string compares equal however it was stored.
    #[test]
    fn text_compares_by_its_utf8_form_whatever_it_was_stored_as() {
        let source = "z\u{e9}\u{1F600}";
        let wide = encoding::from_utf8(source.as_bytes(), TextEncoding::Utf16Be).into_owned();
        assert_eq!(
            compare_text(
                source.as_bytes(),
                TextEncoding::Utf8,
                &wide,
                TextEncoding::Utf16Be,
                Collation::Binary
            ),
            Ordering::Equal
        );
    }

    /// A UTF-16 comparison must order by code point, which is what going
    /// through UTF-8 buys: comparing the code units directly would put an
    /// astral character before one in the private use area.
    #[test]
    fn utf16_text_orders_by_code_point_not_by_code_unit() {
        let astral = "\u{1F600}";
        let high_bmp = "\u{E000}";
        let astral_wide =
            encoding::from_utf8(astral.as_bytes(), TextEncoding::Utf16Be).into_owned();
        let bmp_wide = encoding::from_utf8(high_bmp.as_bytes(), TextEncoding::Utf16Be).into_owned();
        // The raw code units say the astral character is smaller: its leading
        // surrogate is 0xD83D, which is below 0xE000.
        assert!(astral_wide < bmp_wide);
        // Comparing as text says the opposite, which is the correct order.
        assert_eq!(
            compare_text(
                &astral_wide,
                TextEncoding::Utf16Be,
                &bmp_wide,
                TextEncoding::Utf16Be,
                Collation::Binary
            ),
            Ordering::Greater
        );
    }

    /// Names resolve case-insensitively, and an unknown name resolves to
    /// nothing rather than to BINARY.
    #[test]
    fn collation_names_resolve_case_insensitively() {
        assert_eq!(Collation::from_name("binary"), Some(Collation::Binary));
        assert_eq!(Collation::from_name("NoCase"), Some(Collation::NoCase));
        assert_eq!(Collation::from_name("rtrim"), Some(Collation::RTrim));
        assert_eq!(Collation::from_name("unicode61"), None);
    }

    /// The registry starts with the five built-ins and hands out generations.
    #[test]
    fn the_registry_starts_with_the_built_ins() {
        let registry = CollationRegistry::default();
        assert_eq!(
            registry.names(),
            vec!["BINARY", "decimal", "NOCASE", "RTRIM", "uint"]
        );
        assert_eq!(
            registry.get("nocase").map(|entry| entry.collation),
            Some(Collation::NoCase)
        );
        assert!(registry.get("unicode61").is_none());
    }

    /// Replacing a collation must bump the generation, so anything that
    /// recorded the old one can tell it is stale.
    #[test]
    fn replacing_a_collation_invalidates_the_recorded_generation() {
        let mut registry = CollationRegistry::default();
        let before = registry
            .get("NOCASE")
            .map(|entry| entry.generation)
            .unwrap();
        assert!(registry.is_current("NOCASE", before));
        let after = registry.define("NOCASE", Collation::RTrim);
        assert_ne!(before, after);
        assert!(!registry.is_current("NOCASE", before));
        assert!(registry.is_current("NOCASE", after));
        assert_eq!(
            registry.get("NOCASE").map(|entry| entry.collation),
            Some(Collation::RTrim)
        );
        // Redefining one name does not make another name's generation stale.
        let binary = registry
            .get("BINARY")
            .map(|entry| entry.generation)
            .unwrap();
        assert!(registry.is_current("BINARY", binary));
    }

    /// Comparing arbitrary bytes must never panic; text comes off a page.
    #[test]
    fn comparing_arbitrary_bytes_never_panics() {
        let mut state = 0x0123_4567_89ab_cdefu64;
        for _ in 0..inillucent_base::probe::sample_rounds(30_000) {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let make = |seed: u64| -> Vec<u8> {
                let len = (seed % 12) as usize;
                (0..len)
                    .map(|index| {
                        let byte = (seed >> (index % 8 * 8)) as u8;
                        if byte.is_multiple_of(3) {
                            b' '
                        } else {
                            byte
                        }
                    })
                    .collect()
            };
            let left = make(state);
            let right = make(state.rotate_left(17));
            for collation in Collation::all() {
                let forward = collation.compare_bytes(&left, &right);
                let backward = collation.compare_bytes(&right, &left);
                assert_eq!(forward, backward.reverse());
            }
        }
    }
}
