//! Value comparison and three-valued logic.
//!
//! Invariant: comparison happens in two stages that are never merged. First
//! the operands take an affinity, which may convert one of them; then the
//! converted operands are ordered by storage class and, within a class, by
//! payload. Merging the two would lose the distinction between "these compare
//! equal" and "these are the same value", and it is the second stage alone
//! that an index's key order is built from.
//!
//! The two hard parts are here rather than anywhere else:
//!
//! - an integer and a real are compared *exactly*, not by converting the
//!   integer to a double. `9007199254740993` and `9007199254740992.0` are
//!   different values, and rounding the integer first would call them equal;
//! - NULL is not a value, so `compare_values` cannot describe it. Ordering
//!   places NULL first because a B-tree needs a total order, and the SQL
//!   operators go through `compare_sql`, which answers `Unknown`.

use std::cmp::Ordering;

use crate::affinity::{self, Affinity};
use crate::collation::{self, Collation};
use crate::encoding::TextEncoding;
use crate::value::Value;

/// The result of a SQL comparison, which has three outcomes rather than two.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SqlOrdering {
    /// The left operand sorts first.
    Less,
    /// The operands are equal.
    Equal,
    /// The right operand sorts first.
    Greater,
    /// At least one operand was NULL, so the comparison has no answer.
    Unknown,
}

impl SqlOrdering {
    /// Returns the ordering as a two-valued one, or `None` for `Unknown`.
    pub fn ordering(self) -> Option<Ordering> {
        match self {
            SqlOrdering::Less => Some(Ordering::Less),
            SqlOrdering::Equal => Some(Ordering::Equal),
            SqlOrdering::Greater => Some(Ordering::Greater),
            SqlOrdering::Unknown => None,
        }
    }

    /// Returns the truth value of `left = right`.
    pub fn equals(self) -> Truth {
        match self {
            SqlOrdering::Equal => Truth::True,
            SqlOrdering::Unknown => Truth::Unknown,
            _ => Truth::False,
        }
    }

    /// Returns the truth value of `left < right`.
    pub fn less_than(self) -> Truth {
        match self {
            SqlOrdering::Less => Truth::True,
            SqlOrdering::Unknown => Truth::Unknown,
            _ => Truth::False,
        }
    }

    /// Returns the truth value of `left > right`.
    pub fn greater_than(self) -> Truth {
        match self {
            SqlOrdering::Greater => Truth::True,
            SqlOrdering::Unknown => Truth::Unknown,
            _ => Truth::False,
        }
    }
}

/// A SQL truth value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Truth {
    /// Known false.
    False,
    /// Known true.
    True,
    /// Not known, because a NULL took part.
    Unknown,
}

impl Truth {
    /// Returns the three-valued negation. `NOT unknown` is unknown.
    ///
    /// **Named `not` rather than implementing `std::ops::Not`**, and that is the
    /// point: `!` on a `Truth` would read as two-valued negation to anybody who
    /// has met `!` before, and `NOT unknown` is `unknown` rather than `known`.
    /// A reader who has to look the operator up has been told something; a
    /// reader who assumes has been told nothing.
    #[allow(clippy::should_implement_trait)]
    pub fn not(self) -> Truth {
        match self {
            Truth::False => Truth::True,
            Truth::True => Truth::False,
            Truth::Unknown => Truth::Unknown,
        }
    }

    /// Returns the three-valued conjunction.
    ///
    /// `false AND unknown` is false, because one false operand settles it
    /// whatever the other turns out to be.
    pub fn and(self, other: Truth) -> Truth {
        match (self, other) {
            (Truth::False, _) | (_, Truth::False) => Truth::False,
            (Truth::Unknown, _) | (_, Truth::Unknown) => Truth::Unknown,
            _ => Truth::True,
        }
    }

    /// Returns the three-valued disjunction.
    ///
    /// `true OR unknown` is true, for the mirror-image reason.
    pub fn or(self, other: Truth) -> Truth {
        match (self, other) {
            (Truth::True, _) | (_, Truth::True) => Truth::True,
            (Truth::Unknown, _) | (_, Truth::Unknown) => Truth::Unknown,
            _ => Truth::False,
        }
    }

    /// Returns the value a `WHERE` clause acts on: unknown is not true.
    pub fn is_true(self) -> bool {
        self == Truth::True
    }

    /// Builds a truth value from a Rust boolean.
    pub fn from_bool(value: bool) -> Truth {
        if value {
            Truth::True
        } else {
            Truth::False
        }
    }
}

/// Compares two values for ordering, treating NULL as smaller than everything.
///
/// This is the comparison a B-tree key order and an `ORDER BY` use. It is a
/// total order over every value, which the SQL operators are not.
pub fn compare_values(left: &Value<'_>, right: &Value<'_>, collation: Collation) -> Ordering {
    let left_class = left.storage_class();
    let right_class = right.storage_class();
    match left_class.sort_rank().cmp(&right_class.sort_rank()) {
        Ordering::Equal => {}
        other => return other,
    }
    match (left, right) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Integer(left), Value::Integer(right)) => left.cmp(right),
        (Value::Real(left), Value::Real(right)) => compare_reals(*left, *right),
        (Value::Integer(left), Value::Real(right)) => compare_int_and_real(*left, *right),
        (Value::Real(left), Value::Integer(right)) => compare_int_and_real(*right, *left).reverse(),
        (Value::Text(left), Value::Text(right)) => collation::compare_text(
            left.raw(),
            left.encoding(),
            right.raw(),
            right.encoding(),
            collation,
        ),
        (Value::Blob(left), Value::Blob(right)) => compare_blobs(left.raw(), right.raw()),
        _ => Ordering::Equal,
    }
}

/// Compares two values as a SQL operator does, so a NULL makes it unknown.
pub fn compare_sql(left: &Value<'_>, right: &Value<'_>, collation: Collation) -> SqlOrdering {
    if left.is_null() || right.is_null() {
        return SqlOrdering::Unknown;
    }
    match compare_values(left, right, collation) {
        Ordering::Less => SqlOrdering::Less,
        Ordering::Equal => SqlOrdering::Equal,
        Ordering::Greater => SqlOrdering::Greater,
    }
}

/// Compares two operands after applying the affinity a comparison implies.
///
/// SQLite's rule is asymmetric on purpose: if one side has numeric affinity
/// and the other is text with no affinity, the text side takes the numeric
/// affinity; if one side has text affinity and the other is a number with no
/// affinity, the number is rendered as text. Two sides with the same kind of
/// affinity, or none, are compared as they are.
pub fn compare_with_affinity<'a>(
    left: Value<'a>,
    left_affinity: Affinity,
    right: Value<'a>,
    right_affinity: Affinity,
    collation: Collation,
    db_encoding: TextEncoding,
) -> SqlOrdering {
    let applied = comparison_affinity(left_affinity, right_affinity);
    let (left, right) = match applied {
        None => (left, right),
        Some(affinity) => (
            affinity::apply_affinity(left, affinity, db_encoding).unwrap_or(Value::Null),
            affinity::apply_affinity(right, affinity, db_encoding).unwrap_or(Value::Null),
        ),
    };
    compare_sql(&left, &right, collation)
}

/// Returns the affinity a comparison between two operands applies, if any.
///
/// This is SQLite's documented rule from "Affinity Of Comparison Operands":
/// numeric wins over none, text wins over none, and anything else compares
/// with no conversion at all.
pub fn comparison_affinity(left: Affinity, right: Affinity) -> Option<Affinity> {
    let left_numeric = left.is_numeric();
    let right_numeric = right.is_numeric();
    if left_numeric && right_numeric {
        return Some(Affinity::Numeric);
    }
    if left_numeric && right == Affinity::Blob {
        return Some(Affinity::Numeric);
    }
    if right_numeric && left == Affinity::Blob {
        return Some(Affinity::Numeric);
    }
    if left == Affinity::Text && right == Affinity::Blob {
        return Some(Affinity::Text);
    }
    if right == Affinity::Text && left == Affinity::Blob {
        return Some(Affinity::Text);
    }
    None
}

/// Compares two doubles, ordering a NaN as equal to everything numeric.
///
/// A NaN cannot reach a stored value - SQLite stores a NaN double as NULL -
/// so this exists only so that a comparison is total for every bit pattern
/// that could arrive from a corrupt page rather than being a partial order
/// that a sort could loop on.
fn compare_reals(left: f64, right: f64) -> Ordering {
    if left < right {
        Ordering::Less
    } else if left > right {
        Ordering::Greater
    } else {
        Ordering::Equal
    }
}

/// Compares an integer against a double exactly.
///
/// The obvious implementation - convert the integer to a double and compare -
/// is wrong for every integer above 2^53, where the conversion rounds. This
/// one brackets the double against the `i64` range, truncates it, compares the
/// integer parts, and then breaks a tie on the double's fractional part, which
/// is exact for every pair of inputs.
pub fn compare_int_and_real(integer: i64, real: f64) -> Ordering {
    if real.is_nan() {
        return Ordering::Equal;
    }
    if real < -9_223_372_036_854_775_808.0 {
        return Ordering::Greater;
    }
    if real >= 9_223_372_036_854_775_808.0 {
        return Ordering::Less;
    }
    let truncated = real as i64;
    match integer.cmp(&truncated) {
        Ordering::Equal => {}
        other => return other,
    }
    let round_trip = truncated as f64;
    if round_trip < real {
        Ordering::Less
    } else if round_trip > real {
        Ordering::Greater
    } else {
        Ordering::Equal
    }
}

/// Compares two blobs by bytes, then by length.
pub fn compare_blobs(left: &[u8], right: &[u8]) -> Ordering {
    let shared = left.len().min(right.len());
    match (left.get(..shared), right.get(..shared)) {
        (Some(left_prefix), Some(right_prefix)) => match left_prefix.cmp(right_prefix) {
            Ordering::Equal => left.len().cmp(&right.len()),
            other => other,
        },
        _ => left.len().cmp(&right.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The class order is NULL, then numbers, then text, then blobs.
    #[test]
    fn classes_order_null_numbers_text_blobs() {
        let ordered = [
            Value::Null,
            Value::Integer(-1),
            Value::Real(0.5),
            Value::Integer(1),
            Value::text_utf8(b""),
            Value::text_utf8(b"z"),
            Value::blob(b""),
            Value::blob(b"\x00"),
        ];
        for (index, left) in ordered.iter().enumerate() {
            for (other, right) in ordered.iter().enumerate() {
                let expected = index.cmp(&other);
                assert_eq!(
                    compare_values(left, right, Collation::Binary),
                    expected,
                    "{left:?} vs {right:?}"
                );
            }
        }
    }

    /// An integer and a real are compared exactly. This is the case that
    /// converting the integer to a double would get wrong.
    #[test]
    fn an_integer_and_a_real_compare_exactly() {
        // 2^53 + 1 is not representable as a double; converting it would round
        // it down to 2^53 and call the two equal.
        let integer = 9_007_199_254_740_993i64;
        let real = 9_007_199_254_740_992.0f64;
        assert_eq!(compare_int_and_real(integer, real), Ordering::Greater);
        assert_eq!(
            compare_values(
                &Value::Integer(integer),
                &Value::Real(real),
                Collation::Binary
            ),
            Ordering::Greater
        );
        assert_eq!(
            compare_values(
                &Value::Real(real),
                &Value::Integer(integer),
                Collation::Binary
            ),
            Ordering::Less
        );
        assert_eq!(compare_int_and_real(0, 0.5), Ordering::Less);
        assert_eq!(compare_int_and_real(1, 0.5), Ordering::Greater);
        assert_eq!(compare_int_and_real(-1, -0.5), Ordering::Less);
        assert_eq!(compare_int_and_real(-1, -1.5), Ordering::Greater);
        assert_eq!(compare_int_and_real(1, 1.0), Ordering::Equal);
    }

    /// A double past the integer range compares as its sign says, without
    /// overflowing on the way.
    #[test]
    fn a_double_past_the_integer_range_compares_by_its_sign() {
        assert_eq!(compare_int_and_real(i64::MAX, 1e300), Ordering::Less);
        assert_eq!(compare_int_and_real(i64::MIN, -1e300), Ordering::Greater);
        assert_eq!(
            compare_int_and_real(i64::MAX, f64::INFINITY),
            Ordering::Less
        );
        assert_eq!(
            compare_int_and_real(i64::MIN, f64::NEG_INFINITY),
            Ordering::Greater
        );
    }

    /// The two zeroes compare equal even though their bits differ, because
    /// SQL compares numbers rather than representations.
    #[test]
    fn the_two_zeroes_compare_equal() {
        assert_eq!(
            compare_values(&Value::Real(0.0), &Value::Real(-0.0), Collation::Binary),
            Ordering::Equal
        );
        assert_eq!(
            compare_values(&Value::Integer(0), &Value::Real(-0.0), Collation::Binary),
            Ordering::Equal
        );
        // They are still not the same value, which identity reports.
        assert!(!Value::Real(0.0).identical(&Value::Real(-0.0)));
    }

    /// A NULL makes a SQL comparison unknown but still orders first.
    #[test]
    fn null_is_unknown_to_an_operator_and_first_in_an_order() {
        assert_eq!(
            compare_sql(&Value::Null, &Value::Integer(1), Collation::Binary),
            SqlOrdering::Unknown
        );
        assert_eq!(
            compare_sql(&Value::Null, &Value::Null, Collation::Binary),
            SqlOrdering::Unknown
        );
        assert_eq!(
            compare_values(&Value::Null, &Value::Integer(1), Collation::Binary),
            Ordering::Less
        );
        assert_eq!(SqlOrdering::Unknown.equals(), Truth::Unknown);
    }

    /// Three-valued logic must give false priority in AND and true priority in
    /// OR, which is the part a two-valued implementation gets wrong.
    #[test]
    fn three_valued_logic_short_circuits_the_way_sql_says() {
        assert_eq!(Truth::False.and(Truth::Unknown), Truth::False);
        assert_eq!(Truth::Unknown.and(Truth::False), Truth::False);
        assert_eq!(Truth::True.and(Truth::Unknown), Truth::Unknown);
        assert_eq!(Truth::True.or(Truth::Unknown), Truth::True);
        assert_eq!(Truth::Unknown.or(Truth::True), Truth::True);
        assert_eq!(Truth::False.or(Truth::Unknown), Truth::Unknown);
        assert_eq!(Truth::Unknown.not(), Truth::Unknown);
        assert!(!Truth::Unknown.is_true());
    }

    /// A comparison between a numeric-affinity column and text converts the
    /// text; without an affinity the two stay in different classes.
    #[test]
    fn comparison_affinity_converts_the_operand_that_has_none() {
        let converted = compare_with_affinity(
            Value::Integer(10),
            Affinity::Integer,
            Value::text_utf8(b"10"),
            Affinity::Blob,
            Collation::Binary,
            TextEncoding::Utf8,
        );
        assert_eq!(converted, SqlOrdering::Equal);
        let unconverted = compare_with_affinity(
            Value::Integer(10),
            Affinity::Blob,
            Value::text_utf8(b"10"),
            Affinity::Blob,
            Collation::Binary,
            TextEncoding::Utf8,
        );
        // A number is always less than text when nothing converts them.
        assert_eq!(unconverted, SqlOrdering::Less);
        let as_text = compare_with_affinity(
            Value::Integer(10),
            Affinity::Blob,
            Value::text_utf8(b"10"),
            Affinity::Text,
            Collation::Binary,
            TextEncoding::Utf8,
        );
        assert_eq!(as_text, SqlOrdering::Equal);
    }

    /// The affinity rule table itself, spelled out.
    #[test]
    fn the_comparison_affinity_table_matches_the_documentation() {
        assert_eq!(comparison_affinity(Affinity::Integer, Affinity::Text), None);
        assert_eq!(
            comparison_affinity(Affinity::Integer, Affinity::Blob),
            Some(Affinity::Numeric)
        );
        assert_eq!(
            comparison_affinity(Affinity::Blob, Affinity::Real),
            Some(Affinity::Numeric)
        );
        assert_eq!(
            comparison_affinity(Affinity::Text, Affinity::Blob),
            Some(Affinity::Text)
        );
        assert_eq!(comparison_affinity(Affinity::Blob, Affinity::Blob), None);
        assert_eq!(comparison_affinity(Affinity::Text, Affinity::Text), None);
        assert_eq!(
            comparison_affinity(Affinity::Integer, Affinity::Real),
            Some(Affinity::Numeric)
        );
    }

    /// Text comparison honours the collation, and only for text.
    #[test]
    fn a_collation_applies_to_text_and_to_nothing_else() {
        let upper = Value::text_utf8(b"ABC");
        let lower = Value::text_utf8(b"abc");
        assert_eq!(
            compare_values(&upper, &lower, Collation::Binary),
            Ordering::Less
        );
        assert_eq!(
            compare_values(&upper, &lower, Collation::NoCase),
            Ordering::Equal
        );
        // Blobs ignore the collation entirely: they are bytes, not text.
        let upper_blob = Value::blob(b"ABC");
        let lower_blob = Value::blob(b"abc");
        assert_eq!(
            compare_values(&upper_blob, &lower_blob, Collation::NoCase),
            Ordering::Less
        );
    }

    /// Ordering must be total: comparing a large mixed set has to be
    /// antisymmetric and transitive, or a sort could loop.
    #[test]
    fn ordering_over_mixed_classes_is_a_total_order() {
        let values: Vec<Value<'static>> = vec![
            Value::Null,
            Value::Integer(i64::MIN),
            Value::Integer(-1),
            Value::Integer(0),
            Value::Integer(1),
            Value::Integer(9_007_199_254_740_993),
            Value::Integer(i64::MAX),
            Value::Real(f64::NEG_INFINITY),
            Value::Real(-1.5),
            Value::Real(-0.0),
            Value::Real(0.0),
            Value::Real(1.5),
            Value::Real(9_007_199_254_740_992.0),
            Value::Real(f64::INFINITY),
            Value::owned_text(b"").unwrap(),
            Value::owned_text(b"a").unwrap(),
            Value::owned_text(b"z").unwrap(),
            Value::owned_blob(b"").unwrap(),
            Value::owned_blob(b"\x00").unwrap(),
            Value::owned_blob(b"\xff").unwrap(),
        ];
        for left in &values {
            for right in &values {
                let forward = compare_values(left, right, Collation::Binary);
                let backward = compare_values(right, left, Collation::Binary);
                assert_eq!(forward, backward.reverse(), "{left:?} vs {right:?}");
                for third in &values {
                    let first = compare_values(left, right, Collation::Binary);
                    let second = compare_values(right, third, Collation::Binary);
                    if first == second && first != Ordering::Equal {
                        assert_eq!(
                            compare_values(left, third, Collation::Binary),
                            first,
                            "not transitive on {left:?} {right:?} {third:?}"
                        );
                    }
                }
            }
        }
    }

    /// Blob comparison is by bytes then by length, and a prefix is smaller.
    #[test]
    fn blobs_compare_by_bytes_then_by_length() {
        assert_eq!(compare_blobs(b"", b""), Ordering::Equal);
        assert_eq!(compare_blobs(b"", b"\x00"), Ordering::Less);
        assert_eq!(compare_blobs(b"\x00", b"\x00\x00"), Ordering::Less);
        assert_eq!(compare_blobs(b"\xff", b"\x00\x00"), Ordering::Greater);
    }
}
