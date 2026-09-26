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
}

/// Classifies an operand the way SQLite's `numericType` does.
///
/// Every text and blob has a class: `'abc'` is the integer 0 and `'1.5x'` the
/// real 1.5. This used to go through numeric affinity, which leaves text that
/// is not wholly a number as text, and such an operand was then read in
/// floating point: `'abc' / 2` was 0.0 where SQLite answers 0, and
/// `- '12x'` was 0 where SQLite answers -12. The caller has already returned
/// NULL for a NULL operand, which is the only value with no class.
///
/// @param value - the operand, which is not NULL
fn classify(value: &Value<'_>) -> NumericKind {
    match cast::arithmetic_number(value) {
        Value::Real(real) => NumericKind::Real(real),
        Value::Integer(integer) => NumericKind::Integer(integer),
        _ => NumericKind::Integer(0),
    }
}

/// Returns the double an operand of a given class reads as.
///
/// This is `sqlite3VdbeRealValue`, which SQLite's floating point arithmetic
/// uses for both operands whatever class each was given.
///
/// @param kind - the operand's class and value
fn real_of(kind: NumericKind) -> f64 {
    match kind {
        NumericKind::Integer(integer) => integer as f64,
        NumericKind::Real(real) => real,
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
        _ => numeric_arithmetic(op, left, right),
    }
}

/// Evaluates `+`, `-`, `*`, `/` and `%`, as SQLite's `OP_Add` and its
/// neighbours do.
///
/// Two integer operands use integer arithmetic. Anything else, and an integer
/// operation that overflows, is computed in floating point, and the answer
/// narrows back to an integer only when neither operand was a real and the
/// operands were not two integers that overflowed. That one rule covers all
/// five operators: `'abc' / 2` is the integer 0 because both operands are
/// integers, and `7.0 % 2` is the real 1.0 because one operand is a real.
///
/// @param op - the operator
/// @param left - the left operand, which is not NULL
/// @param right - the right operand, which is not NULL
fn numeric_arithmetic(op: BinaryOp, left: &Value<'_>, right: &Value<'_>) -> Value<'static> {
    let kind_left = classify(left);
    let kind_right = classify(right);
    let both_integers = match (kind_left, kind_right) {
        (NumericKind::Integer(a), NumericKind::Integer(b)) => {
            if let Some(value) = integer_arithmetic(op, a, b) {
                return value;
            }
            true
        }
        _ => false,
    };
    let result = match op {
        BinaryOp::Add => real_of(kind_left) + real_of(kind_right),
        BinaryOp::Subtract => real_of(kind_left) - real_of(kind_right),
        BinaryOp::Multiply => real_of(kind_left) * real_of(kind_right),
        BinaryOp::Divide => {
            let divisor = real_of(kind_right);
            if divisor == 0.0 {
                return Value::Null;
            }
            real_of(kind_left) / divisor
        }
        BinaryOp::Modulo => match real_remainder(left, right) {
            Some(remainder) => remainder,
            None => return Value::Null,
        },
        _ => return Value::Null,
    };
    if result.is_nan() {
        return Value::Null;
    }
    let either_was_real =
        matches!(kind_left, NumericKind::Real(_)) || matches!(kind_right, NumericKind::Real(_));
    if either_was_real || both_integers {
        return Value::Real(result);
    }
    match affinity::integer_affinity(Value::Real(result)) {
        Value::Integer(integer) => Value::Integer(integer),
        other => other.into_owned().unwrap_or(Value::Null),
    }
}

/// Evaluates integer arithmetic, returning `None` when it overflows and the
/// operation has to be redone in floating point.
///
/// `%` never overflows here: SQLite turns a divisor of -1 into 1, which is
/// what keeps `i64::MIN % -1` at zero rather than trapping.
///
/// @param op - the operator
/// @param a - the left operand
/// @param b - the right operand
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
        BinaryOp::Modulo => {
            if b == 0 {
                return Some(Value::Null);
            }
            let divisor = if b == -1 { 1 } else { b };
            a % divisor
        }
        _ => return None,
    };
    Some(Value::Integer(value))
}

/// Computes `%` when an operand is a real, as SQLite does: both operands are
/// truncated to integers, and the remainder of those is the answer, as a double.
///
/// Returns `None` when the divisor truncates to zero, which is NULL.
///
/// @param left - the dividend
/// @param right - the divisor
fn real_remainder(left: &Value<'_>, right: &Value<'_>) -> Option<f64> {
    let divisor = cast::integer_value(right);
    if divisor == 0 {
        return None;
    }
    let divisor = if divisor == -1 { 1 } else { divisor };
    Some((cast::integer_value(left) % divisor) as f64)
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
///
/// SQLite compiles `-x` as `0 - x`, so it is that subtraction: `- '12x'` is
/// -12, `- '0.5-1.75'` is -0.5 and `- x'31'` is -1, where reading the operand
/// through numeric affinity made all three zero. A negated literal never gets
/// here, because the binder folds its sign into the literal as SQLite's
/// parser does, which is what keeps `-0.0` a negative zero.
///
/// @param value - the operand
pub fn negate(value: &Value<'_>) -> Value<'static> {
    if value.is_null() {
        return Value::Null;
    }
    numeric_arithmetic(BinaryOp::Subtract, &Value::Integer(0), value)
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
        // `%` truncates its operands to integers, and answers a real when
        // either operand was one.
        assert_same!(
            arithmetic(BinaryOp::Modulo, &Value::Real(5.5), &Value::Real(2.5), utf8),
            Value::Real(1.0)
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
