//! The math built-ins, as the pinned release compiles them.
//!
//! Invariant: every function here returns NULL for an argument that is not a
//! number, and NULL rather than an error for an argument outside its domain.
//! That is SQLite's rule and it is not the obvious one: `sqrt(-1)` is NULL
//! rather than NaN, `log(0)` is NULL rather than negative infinity, and
//! `acos(2)` is NULL rather than a floating-point NaN that would then compare
//! false against itself for ever.
//!
//! The one function that is not a thin wrapper is `mod`, which follows C's
//! `fmod` and so keeps the sign of its left operand - `mod(-7, 3)` is -1, not 2.

use inillucent_sql::function::MathFunc;
use inillucent_value::{cast, numeric as numeric_syntax, Value};

/// Calls a math function.
pub fn call(func: MathFunc, arguments: &[Value<'static>]) -> Value<'static> {
    // `pi` takes no argument, so it is answered before anything is read.
    if func == MathFunc::Pi {
        return Value::Real(core::f64::consts::PI);
    }
    let Some(first) = numeric(arguments.first()) else {
        return Value::Null;
    };
    let second = arguments.get(1).and_then(|value| numeric(Some(value)));
    // `ceil`, `floor` and `trunc` answer in the class they were asked in: an
    // integer argument gives an integer, and a real gives a real even when the
    // result has no fractional part. `floor(3)` is 3 and `floor(3.0)` is 3.0,
    // and returning a real for both would change the type of an ordinary
    // integer column's projection.
    if matches!(func, MathFunc::Ceil | MathFunc::Floor | MathFunc::Trunc) {
        if let Some(integer) = first.integer {
            return Value::Integer(integer);
        }
    }
    let first = first.real;
    let second = second.map(|second| second.real);
    let answer = match func {
        MathFunc::Pi => return Value::Real(core::f64::consts::PI),
        MathFunc::Acos => domain(first, -1.0, 1.0, f64::acos),
        MathFunc::Asin => domain(first, -1.0, 1.0, f64::asin),
        MathFunc::Atan => Some(first.atan()),
        MathFunc::Acosh => (first >= 1.0).then(|| first.acosh()),
        MathFunc::Asinh => Some(first.asinh()),
        // The interval is closed, and the ends are the infinities rather than
        // an error: `atanh(1)` is +Inf in the pinned build.
        MathFunc::Atanh => domain(first, -1.0, 1.0, f64::atanh),
        MathFunc::Atan2 => second.map(|second| first.atan2(second)),
        MathFunc::Ceil => Some(first.ceil()),
        MathFunc::Cos => Some(first.cos()),
        MathFunc::Cosh => Some(first.cosh()),
        MathFunc::Degrees => Some(first.to_degrees()),
        MathFunc::Exp => Some(first.exp()),
        MathFunc::Floor => Some(first.floor()),
        MathFunc::Ln => positive(first, f64::ln),
        MathFunc::Log10 => positive(first, f64::log10),
        MathFunc::Log2 => positive(first, f64::log2),
        // `log(x)` is base 10 and `log(b, x)` is base b, which is the one
        // built-in whose *meaning* changes with its argument count.
        MathFunc::Log => match second {
            Some(value) => {
                if first <= 0.0 || first == 1.0 || value <= 0.0 {
                    None
                } else {
                    Some(value.log(first))
                }
            }
            None => positive(first, f64::log10),
        },
        MathFunc::Mod => second.and_then(|second| (second != 0.0).then(|| first % second)),
        MathFunc::Pow => second.map(|second| first.powf(second)),
        MathFunc::Radians => Some(first.to_radians()),
        MathFunc::Sin => Some(first.sin()),
        MathFunc::Sinh => Some(first.sinh()),
        MathFunc::Sqrt => (first >= 0.0).then(|| first.sqrt()),
        MathFunc::Tan => Some(first.tan()),
        MathFunc::Tanh => Some(first.tanh()),
        MathFunc::Trunc => Some(first.trunc()),
    };
    match answer {
        // An infinity is a real answer - `exp(1000)` and `atanh(1)` both
        // return one. A NaN is not: it would compare false against itself, so
        // it becomes NULL, which is what SQLite returns for `pow(-8, 0.5)`.
        Some(value) if !value.is_nan() => Value::Real(value),
        _ => Value::Null,
    }
}

/// An argument that is a number, and whether it was an exact integer.
struct Number {
    real: f64,
    integer: Option<i64>,
}

/// Returns an argument as a number, or nothing when it is not one.
///
/// Text that looks like a number is one, which is what makes `sqrt('4')` two
/// and `sqrt(' 4 ')` two as well. Text that does not - `'four'`, `'4x'`,
/// `'0x10'` - is not a number, and neither is a blob: the whole call is NULL.
/// Casting instead of testing would turn `sqrt('four')` into zero, which is a
/// plausible-looking wrong answer.
fn numeric(value: Option<&Value<'static>>) -> Option<Number> {
    let value = value?;
    match value {
        Value::Null | Value::Blob(_) => None,
        Value::Integer(integer) => Some(Number {
            real: *integer as f64,
            integer: Some(*integer),
        }),
        Value::Real(real) => Some(Number {
            real: *real,
            integer: None,
        }),
        Value::Text(text) => {
            if !numeric_syntax::looks_numeric(text.raw(), text.encoding()) {
                return None;
            }
            match cast::numerify(value.clone()) {
                Value::Integer(integer) => Some(Number {
                    real: integer as f64,
                    integer: Some(integer),
                }),
                Value::Real(real) => Some(Number {
                    real,
                    integer: None,
                }),
                _ => None,
            }
        }
    }
}

/// Applies a function only inside a closed interval.
fn domain(value: f64, low: f64, high: f64, body: fn(f64) -> f64) -> Option<f64> {
    (value >= low && value <= high).then(|| body(value))
}

/// Applies a function only to a strictly positive argument.
fn positive(value: f64, body: fn(f64) -> f64) -> Option<f64> {
    (value > 0.0).then(|| body(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Calls a function with owned arguments.
    fn run(func: MathFunc, arguments: Vec<Value<'static>>) -> Value<'static> {
        call(func, &arguments)
    }

    /// Returns an owned text value, which only a failed allocation refuses.
    fn text(source: &[u8]) -> Value<'static> {
        Value::owned_text(source).expect("the allocation succeeds")
    }

    #[test]
    fn a_domain_error_is_null_rather_than_a_nan() {
        // `atanh(1)` is not here: its domain is closed, and the pinned build
        // answers with an infinity rather than refusing it.
        assert_same!(run(MathFunc::Sqrt, vec![Value::Integer(-1)]), Value::Null);
        assert_same!(run(MathFunc::Ln, vec![Value::Integer(0)]), Value::Null);
        assert_same!(run(MathFunc::Acos, vec![Value::Integer(2)]), Value::Null);
    }

    #[test]
    fn ceil_and_floor_answer_in_the_class_they_were_asked_in() {
        assert_same!(
            run(MathFunc::Floor, vec![Value::Integer(3)]),
            Value::Integer(3)
        );
        assert_same!(
            run(MathFunc::Floor, vec![Value::Real(3.0)]),
            Value::Real(3.0)
        );
        assert_same!(
            run(MathFunc::Ceil, vec![Value::Real(1.2)]),
            Value::Real(2.0)
        );
    }

    #[test]
    fn the_ends_of_atanh_are_the_infinities() {
        assert!(matches!(
            run(MathFunc::Atanh, vec![Value::Integer(1)]),
            Value::Real(real) if real.is_infinite() && real > 0.0
        ));
        assert_same!(run(MathFunc::Atanh, vec![Value::Real(1.5)]), Value::Null);
    }

    #[test]
    fn text_that_is_a_number_is_a_number() {
        assert_same!(run(MathFunc::Sqrt, vec![text(b"4")]), Value::Real(2.0));
        assert_same!(run(MathFunc::Sqrt, vec![text(b"four")]), Value::Null);
        // Casting rather than testing would make these zero.
        assert_same!(run(MathFunc::Sqrt, vec![text(b"4x")]), Value::Null);
        assert_same!(run(MathFunc::Sqrt, vec![text(b"0x10")]), Value::Null);
    }

    #[test]
    fn mod_keeps_the_sign_of_its_left_operand() {
        assert_same!(
            run(MathFunc::Mod, vec![Value::Integer(-7), Value::Integer(3)]),
            Value::Real(-1.0)
        );
        assert_same!(
            run(MathFunc::Mod, vec![Value::Integer(7), Value::Integer(0)]),
            Value::Null
        );
    }

    #[test]
    fn log_changes_meaning_with_its_argument_count() {
        assert_same!(
            run(MathFunc::Log, vec![Value::Integer(100)]),
            Value::Real(2.0)
        );
        assert_same!(
            run(MathFunc::Log, vec![Value::Integer(2), Value::Integer(8)]),
            Value::Real(3.0)
        );
    }

    #[test]
    fn pi_needs_no_argument() {
        assert_same!(
            run(MathFunc::Pi, Vec::new()),
            Value::Real(core::f64::consts::PI)
        );
    }
}
