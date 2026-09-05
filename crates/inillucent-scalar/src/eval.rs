//! Expression evaluation: arithmetic, comparison, and three-valued logic.
//!
//! Invariant: every rule here is SQLite's rule, not Rust's. Integer overflow
//! falls back to a double rather than wrapping or panicking, division by zero
//! is NULL rather than an error, a non-numeric operand of `+` is zero rather
//! than a failure, and a result computed in floating point narrows back to an
//! integer when neither operand was a real. Each of those is a place where the
//! obvious Rust implementation gives a different answer from SQLite, and each
//! is why this module exists instead of a few inline operators.

use inillucent_sql::ast::BinaryOp;
use inillucent_value::compare::Truth;
use inillucent_value::{
    affinity, cast, compare, numeric, Affinity, Collation, TextEncoding, Value,
};

/// What a value counts as when arithmetic looks at it.
#[derive(Clone, Copy, Debug, PartialEq)]
enum NumericKind {
    /// The value is an integer.
    Integer(i64),
    /// The value is a real.
    Real(f64),
    /// The value is neither and counts as zero.
    NotNumeric,
}

/// Classifies an operand the way SQLite's `numericType` does.
fn classify(value: &Value<'_>) -> NumericKind {
    match value {
        Value::Integer(integer) => NumericKind::Integer(*integer),
        Value::Real(real) => NumericKind::Real(*real),
        Value::Text(_) | Value::Blob(_) => {
            match affinity::apply_numeric_affinity(value.clone(), false) {
                Value::Integer(integer) => NumericKind::Integer(integer),
                Value::Real(real) => NumericKind::Real(real),
                _ => NumericKind::NotNumeric,
            }
        }
        Value::Null => NumericKind::NotNumeric,
    }
}

/// Evaluates an arithmetic, bitwise or concatenation operator.
pub fn arithmetic(
    op: BinaryOp,
    left: &Value<'_>,
    right: &Value<'_>,
    encoding: TextEncoding,
) -> Value<'static> {
    if left.is_null() || right.is_null() {
        return Value::Null;
    }
    match op {
        BinaryOp::Concat => concat(left, right, encoding),
        BinaryOp::BitAnd => Value::Integer(cast::integer_value(left) & cast::integer_value(right)),
        BinaryOp::BitOr => Value::Integer(cast::integer_value(left) | cast::integer_value(right)),
        BinaryOp::ShiftLeft => Value::Integer(shift(
            cast::integer_value(left),
            cast::integer_value(right),
            true,
        )),
        BinaryOp::ShiftRight => Value::Integer(shift(
            cast::integer_value(left),
            cast::integer_value(right),
            false,
        )),
        BinaryOp::Modulo => remainder(left, right),
        _ => numeric_arithmetic(op, left, right),
    }
}

/// Evaluates `+`, `-`, `*` and `/`.
fn numeric_arithmetic(op: BinaryOp, left: &Value<'_>, right: &Value<'_>) -> Value<'static> {
    let kind_left = classify(left);
    let kind_right = classify(right);
    if let (NumericKind::Integer(a), NumericKind::Integer(b)) = (kind_left, kind_right) {
        if let Some(value) = integer_arithmetic(op, a, b) {
            return value;
        }
    }
    let a = cast::real_value(left);
    let b = cast::real_value(right);
    let result = match op {
        BinaryOp::Add => a + b,
        BinaryOp::Subtract => a - b,
        BinaryOp::Multiply => a * b,
        BinaryOp::Divide => {
            if b == 0.0 {
                return Value::Null;
            }
            a / b
        }
        _ => return Value::Null,
    };
    if result.is_nan() {
        return Value::Null;
    }
    // A result computed in floating point narrows back when neither operand
    // was really a real. This is what makes `typeof('abc' + 1)` an integer.
    let either_was_real =
        matches!(kind_left, NumericKind::Real(_)) || matches!(kind_right, NumericKind::Real(_));
    if either_was_real || op == BinaryOp::Divide {
        return Value::Real(result);
    }
    match affinity::integer_affinity(Value::Real(result)) {
        Value::Integer(integer) => Value::Integer(integer),
        other => other.into_owned().unwrap_or(Value::Null),
    }
}

/// Evaluates integer arithmetic, returning `None` when it overflows and the
/// operation has to be redone in floating point.
fn integer_arithmetic(op: BinaryOp, a: i64, b: i64) -> Option<Value<'static>> {
    let value = match op {
        BinaryOp::Add => a.checked_add(b)?,
        BinaryOp::Subtract => a.checked_sub(b)?,
        BinaryOp::Multiply => a.checked_mul(b)?,
        BinaryOp::Divide => {
            if b == 0 {
                return Some(Value::Null);
            }
            if b == -1 && a == i64::MIN {
                return None;
            }
            a / b
        }
        _ => return None,
    };
    Some(Value::Integer(value))
}

/// Evaluates `%`, which SQLite computes on integers whatever it was given.
fn remainder(left: &Value<'_>, right: &Value<'_>) -> Value<'static> {
    let divisor = cast::integer_value(right);
    if divisor == 0 {
        return Value::Null;
    }
    let dividend = cast::integer_value(left);
    if divisor == -1 {
        // `i64::MIN % -1` overflows in Rust and is zero in SQLite.
        return Value::Integer(0);
    }
    Value::Integer(dividend % divisor)
}

/// Shifts, with SQLite's out-of-range and negative-count behaviour.
fn shift(value: i64, amount: i64, left: bool) -> i64 {
    if amount < 0 {
        return shift(value, amount.saturating_neg(), !left);
    }
    if amount >= 64 {
        return if left || value >= 0 { 0 } else { -1 };
    }
    let places = amount as u32;
    if left {
        ((value as u64) << places) as i64
    } else {
        value >> places
    }
}

/// Concatenates two values as text.
fn concat(left: &Value<'_>, right: &Value<'_>, encoding: TextEncoding) -> Value<'static> {
    let mut bytes = text_bytes(left, encoding);
    bytes.extend_from_slice(&text_bytes(right, encoding));
    Value::owned_text(&bytes).unwrap_or(Value::Null)
}

/// Returns the text representation of a value, as bytes.
pub fn text_bytes(value: &Value<'_>, encoding: TextEncoding) -> Vec<u8> {
    let _ = encoding;
    match value {
        Value::Null => Vec::new(),
        Value::Integer(integer) => numeric::integer_to_text(*integer),
        Value::Real(real) => numeric::real_to_text(*real),
        Value::Text(text) => text.utf8_bytes().into_owned(),
        Value::Blob(blob) => blob.raw().to_vec(),
    }
}

/// Evaluates a comparison, returning NULL when either side is NULL.
pub fn comparison(
    op: BinaryOp,
    left: &Value<'_>,
    right: &Value<'_>,
    target: Option<Affinity>,
    collation: Collation,
    encoding: TextEncoding,
) -> Value<'static> {
    if left.is_null() || right.is_null() {
        return Value::Null;
    }
    let ordering = ordered(left, right, target, collation, encoding);
    Value::Integer(i64::from(satisfies(op, ordering)))
}

/// Evaluates `IS` and `IS NOT`, which are never NULL.
pub fn is_comparison(
    negated: bool,
    left: &Value<'_>,
    right: &Value<'_>,
    target: Option<Affinity>,
    collation: Collation,
    encoding: TextEncoding,
) -> Value<'static> {
    let equal = match (left.is_null(), right.is_null()) {
        (true, true) => true,
        (true, false) | (false, true) => false,
        (false, false) => {
            ordered(left, right, target, collation, encoding) == std::cmp::Ordering::Equal
        }
    };
    Value::Integer(i64::from(equal != negated))
}

/// Compares two values after applying the comparison affinity.
fn ordered(
    left: &Value<'_>,
    right: &Value<'_>,
    target: Option<Affinity>,
    collation: Collation,
    encoding: TextEncoding,
) -> std::cmp::Ordering {
    let (left, right) = match target {
        Some(affinity) => (
            affinity::apply_affinity(left.clone(), affinity, encoding)
                .unwrap_or_else(|_| left.clone()),
            affinity::apply_affinity(right.clone(), affinity, encoding)
                .unwrap_or_else(|_| right.clone()),
        ),
        None => (left.clone(), right.clone()),
    };
    compare::compare_values(&left, &right, collation)
}

/// Returns whether an ordering satisfies a comparison operator.
fn satisfies(op: BinaryOp, ordering: std::cmp::Ordering) -> bool {
    use std::cmp::Ordering;
    match op {
        BinaryOp::Equal => ordering == Ordering::Equal,
        BinaryOp::NotEqual => ordering != Ordering::Equal,
        BinaryOp::Less => ordering == Ordering::Less,
        BinaryOp::LessEqual => ordering != Ordering::Greater,
        BinaryOp::Greater => ordering == Ordering::Greater,
        BinaryOp::GreaterEqual => ordering != Ordering::Less,
        _ => false,
    }
}

/// Returns the truth value of a register, in SQL's three-valued logic.
pub fn truth(value: &Value<'_>) -> Truth {
    match value {
        Value::Null => Truth::Unknown,
        Value::Integer(integer) => Truth::from_bool(*integer != 0),
        Value::Real(real) => Truth::from_bool(*real != 0.0),
        Value::Text(_) | Value::Blob(_) => Truth::from_bool(cast::real_value(value) != 0.0),
    }
}

/// Turns a truth value back into a register value.
pub fn from_truth(truth: Truth) -> Value<'static> {
    match truth {
        Truth::True => Value::Integer(1),
        Truth::False => Value::Integer(0),
        Truth::Unknown => Value::Null,
    }
}

/// Evaluates three-valued `AND`.
pub fn logical_and(left: &Value<'_>, right: &Value<'_>) -> Value<'static> {
    from_truth(truth(left).and(truth(right)))
}

/// Evaluates three-valued `OR`.
pub fn logical_or(left: &Value<'_>, right: &Value<'_>) -> Value<'static> {
    from_truth(truth(left).or(truth(right)))
}

/// Evaluates `NOT`, which propagates NULL.
pub fn logical_not(value: &Value<'_>) -> Value<'static> {
    from_truth(truth(value).not())
}

/// Evaluates unary minus.
pub fn negate(value: &Value<'_>) -> Value<'static> {
    if value.is_null() {
        return Value::Null;
    }
    match classify(value) {
        NumericKind::Integer(integer) => match integer.checked_neg() {
            Some(negated) => Value::Integer(negated),
            None => Value::Real(-(integer as f64)),
        },
        NumericKind::Real(real) => Value::Real(-real),
        NumericKind::NotNumeric => Value::Integer(0),
    }
}

/// Evaluates `~`.
pub fn bit_not(value: &Value<'_>) -> Value<'static> {
    if value.is_null() {
        return Value::Null;
    }
    Value::Integer(!cast::integer_value(value))
}

/// Evaluates `IS NULL` and `NOT NULL`, which are never NULL themselves.
pub fn is_null(value: &Value<'_>, negated: bool) -> Value<'static> {
    Value::Integer(i64::from(value.is_null() != negated))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The four surprising arithmetic answers, all of which differ from what
    /// the obvious Rust implementation would produce.
    #[test]
    fn arithmetic_follows_sqlite_rather_than_rust() {
        let utf8 = TextEncoding::Utf8;
        // Division by zero is NULL, not a panic.
        assert_same!(
            arithmetic(
                BinaryOp::Divide,
                &Value::Integer(1),
                &Value::Integer(0),
                utf8
            ),
            Value::Null
        );
        // A non-numeric operand is zero, and the result narrows to an integer.
        assert_same!(
            arithmetic(
                BinaryOp::Add,
                &Value::text_utf8(b"abc"),
                &Value::Integer(1),
                utf8
            ),
            Value::Integer(1)
        );
        // Integer overflow becomes a real rather than wrapping.
        assert_same!(
            arithmetic(
                BinaryOp::Add,
                &Value::Integer(i64::MAX),
                &Value::Integer(1),
                utf8
            ),
            Value::Real(9.223372036854776e18)
        );
        // `%` truncates its operands to integers.
        assert_same!(
            arithmetic(BinaryOp::Modulo, &Value::Real(5.5), &Value::Real(2.5), utf8),
            Value::Integer(1)
        );
    }

    /// Integer division stays integer, and real division stays real.
    #[test]
    fn division_keeps_the_class_sqlite_keeps() {
        let utf8 = TextEncoding::Utf8;
        assert_same!(
            arithmetic(
                BinaryOp::Divide,
                &Value::Integer(7),
                &Value::Integer(2),
                utf8
            ),
            Value::Integer(3)
        );
        assert_same!(
            arithmetic(
                BinaryOp::Divide,
                &Value::Real(7.0),
                &Value::Integer(2),
                utf8
            ),
            Value::Real(3.5)
        );
    }

    /// Shifting by 64 or more is zero, and a negative count shifts the other
    /// way, both of which are undefined behaviour in Rust.
    #[test]
    fn shifts_saturate_and_reverse() {
        assert_eq!(shift(1, 64, true), 0);
        assert_eq!(shift(-1, 64, false), -1);
        assert_eq!(shift(1, 64, false), 0);
        assert_eq!(shift(4, -1, true), 2);
        assert_eq!(shift(4, -1, false), 8);
    }

    /// NULL propagates through arithmetic and comparison, but not through
    /// `IS` or `IS NULL`.
    #[test]
    fn null_propagates_where_sqlite_says_it_does() {
        let utf8 = TextEncoding::Utf8;
        assert_same!(
            arithmetic(BinaryOp::Add, &Value::Null, &Value::Integer(1), utf8),
            Value::Null
        );
        assert_same!(
            comparison(
                BinaryOp::Equal,
                &Value::Null,
                &Value::Null,
                None,
                Collation::Binary,
                utf8
            ),
            Value::Null
        );
        assert_same!(
            is_comparison(
                false,
                &Value::Null,
                &Value::Null,
                None,
                Collation::Binary,
                utf8
            ),
            Value::Integer(1)
        );
        assert_same!(is_null(&Value::Null, false), Value::Integer(1));
    }

    /// Three-valued logic: NULL AND false is false, NULL OR true is true, and
    /// everything else with a NULL is NULL.
    #[test]
    fn three_valued_logic_is_three_valued() {
        assert_same!(
            logical_and(&Value::Null, &Value::Integer(0)),
            Value::Integer(0)
        );
        assert_same!(logical_and(&Value::Null, &Value::Integer(1)), Value::Null);
        assert_same!(
            logical_or(&Value::Null, &Value::Integer(1)),
            Value::Integer(1)
        );
        assert_same!(logical_or(&Value::Null, &Value::Integer(0)), Value::Null);
        assert_same!(logical_not(&Value::Null), Value::Null);
    }

    /// Comparison applies the affinity it was given to both sides, which is
    /// what makes a text column compare equal to an integer literal.
    #[test]
    fn comparison_applies_its_affinity_to_both_sides() {
        let utf8 = TextEncoding::Utf8;
        assert_same!(
            comparison(
                BinaryOp::Equal,
                &Value::text_utf8(b"5"),
                &Value::Integer(5),
                None,
                Collation::Binary,
                utf8
            ),
            Value::Integer(0)
        );
        assert_same!(
            comparison(
                BinaryOp::Equal,
                &Value::text_utf8(b"5"),
                &Value::Integer(5),
                Some(Affinity::Integer),
                Collation::Binary,
                utf8
            ),
            Value::Integer(1)
        );
    }
}
