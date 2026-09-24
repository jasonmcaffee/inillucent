//! Closure compilation: an expression tree becomes a tree of specialised
//! objects, chosen once per statement rather than dispatched once per row.
//!
//! Invariant: compilation changes speed, never meaning. Every specialised node
//! has a generic node that computes the same thing, and the test
//! `every_specialisation_agrees_with_the_generic_path` runs both over the same
//! inputs - including the type combinations that make a specialisation
//! inapplicable - and requires the same answer. A specialisation that is faster
//! and different is a bug, and it is the kind that only shows up in a digest
//! comparison months later.
//!
//! ## What "closure compilation" means here and why not a code generator
//!
//! The TDD's execution section calls for closure compilation and rules out
//! Cranelift or LLVM. The distinction that matters is where the type dispatch
//! happens. An interpreter looks at an `Add` node, asks what its operands are,
//! branches, and does that for every row. A closure compiler asks once, at
//! prepare time, and builds an object that can only add two integers - so the
//! per-row cost is one indirect call to a body with no branches in it.
//!
//! In Rust that object is a `Box<dyn Eval>` rather than a `Box<dyn Fn>`,
//! because the value a node returns borrows the batch it read from and a boxed
//! closure cannot express that lifetime relationship. A trait method generic
//! over a lifetime can, and stays object-safe.
//!
//! The specialisations exist where the binder can prove the operand types from
//! column affinity: integer arithmetic, integer comparison, comparison against
//! a literal. Everything else compiles to the generic node, which is the same
//! code the old VM ran, and correctness never depends on which was chosen.

use inillucent_base::error::Unwind;
use inillucent_base::DbResult;
use inillucent_tree::datum::{Datum, OwnedDatum};
use inillucent_value::affinity::{self, Affinity};
use inillucent_value::collation::Collation;
use inillucent_value::compare::compare_sql;
use inillucent_value::encoding::TextEncoding;
use inillucent_value::value::Value;

use crate::batch::Batch;

// **The two modules this file is made of (task-1962, A7).** Everything is re-exported under the
// path it had, so no call site in the workspace moved.
mod tree;
pub use tree::*;

/// What a registered aggregate is: every row of the group in, one value out.
pub type AggregateFn = dyn Fn(&[Vec<Value<'static>>]) -> DbResult<Value<'static>> + Send + Sync;

/// A bare column reference.
struct ColumnRef {
    index: usize,
}

impl Eval for ColumnRef {
    fn value<'p>(&self, batch: &Batch<'p>, nth: usize) -> DbResult<Computed<'p>> {
        Ok(Computed::Borrowed(batch.value(nth, self.index)?))
    }

    fn column(&self) -> Option<usize> {
        Some(self.index)
    }
}

/// `sqlite_offset(X)`: where in the file the row holding X lives.
struct RowOffsetOf {
    /// One entry per leaf: the lowest key on it and the offset of its page.
    boundaries: std::sync::Arc<Vec<(i64, i64)>>,
    /// The row's key.
    rowid: Box<dyn Eval>,
}

impl Eval for RowOffsetOf {
    /// Returns the offset of the page the row is on, or NULL when there is
    /// none - a row with no integer key is a row this cannot be asked about,
    /// which is what SQLite answers NULL for too.
    fn value<'p>(&self, batch: &Batch<'p>, nth: usize) -> DbResult<Computed<'p>> {
        let Some(rowid) = self.rowid.value(batch, nth)?.get().as_int() else {
            return Ok(Computed::Owned(OwnedDatum::Null));
        };
        // The last boundary at or below the key: a leaf covers a contiguous run
        // of keys, so that is the leaf the row is on.
        let at = match self
            .boundaries
            .binary_search_by_key(&rowid, |(key, _)| *key)
        {
            Ok(found) => found,
            Err(0) => return Ok(Computed::Owned(OwnedDatum::Null)),
            Err(after) => after.saturating_sub(1),
        };
        match self.boundaries.get(at) {
            Some((_, offset)) => Ok(Computed::Owned(OwnedDatum::Int(*offset))),
            None => Ok(Computed::Owned(OwnedDatum::Null)),
        }
    }
}

/// A constant.
struct Literal {
    value: OwnedDatum,
}

impl Eval for Literal {
    fn value<'p>(&self, _batch: &Batch<'p>, _nth: usize) -> DbResult<Computed<'p>> {
        // Phase 1 could not return a text literal at all: the node owns the
        // bytes and the signature promised `'p`, so a text literal was compared
        // in place and refused anywhere else. `Computed` is what removes the
        // restriction - an owned answer is one of the two things the type
        // admits - and a text literal in a select list now works.
        Ok(match &self.value {
            OwnedDatum::Null => Computed::Borrowed(Datum::Null),
            OwnedDatum::Int(number) => Computed::Borrowed(Datum::Int(*number)),
            OwnedDatum::Real(number) => Computed::Borrowed(Datum::Real(*number)),
            owned => Computed::Owned(owned.clone()),
        })
    }
}

/// One of the statement's bound parameters, read when it is evaluated.
///
/// See [`Expr::Parameter`]. A missing slot reads as NULL, which is what an
/// unbound parameter is.
struct ParamRef {
    /// Where the value sits, zero-based.
    at: usize,
    /// The values the statement is running against.
    bound: crate::physical::Bindings,
}

impl Eval for ParamRef {
    fn value<'p>(&self, _batch: &Batch<'p>, _nth: usize) -> DbResult<Computed<'p>> {
        // The same three arms `Literal` has, and for the same reason: a number
        // is copied out of the cell and owes nothing to it, and a text or a blob
        // cannot be borrowed past the guard so it is cloned.
        let Ok(held) = self.bound.lock() else {
            return Ok(Computed::Borrowed(Datum::Null));
        };
        Ok(match held.get(self.at) {
            None | Some(OwnedDatum::Null) => Computed::Borrowed(Datum::Null),
            Some(OwnedDatum::Int(number)) => Computed::Borrowed(Datum::Int(*number)),
            Some(OwnedDatum::Real(number)) => Computed::Borrowed(Datum::Real(*number)),
            Some(owned) => Computed::Owned(owned.clone()),
        })
    }
}

/// A call to a scalar an application registered.
///
/// The arguments are materialised into owned `Value`s before the call, because
/// the body is somebody else's code and may hold them for as long as it likes -
/// handing it a borrow of a pinned page would be handing it a borrow of a frame
/// the pool may evict.
struct ExternalCall {
    /// What it does.
    body: ScalarBody,
    /// The compiled arguments.
    arguments: Vec<Box<dyn Eval>>,
}

impl Eval for ExternalCall {
    fn value<'p>(&self, batch: &Batch<'p>, nth: usize) -> DbResult<Computed<'p>> {
        let mut values = Vec::with_capacity(self.arguments.len());
        for argument in &self.arguments {
            values.push(Value::from(&argument.value(batch, nth)?.get()).into_owned()?);
        }
        let answer = (self.body.0)(&values)?;
        Ok(Computed::Owned(OwnedDatum::from(answer)))
    }
}

/// The message a `RAISE(IGNORE)` reports.
///
/// **It is a control transfer wearing an error's clothes**, and it is never
/// seen by a caller: `RAISE(IGNORE)` in a trigger body means "stop this trigger
/// and abandon the row the write is on", which no expression can do on its own
/// because an expression does not know what a row is. So it travels as a
/// failure with this exact text, and the trigger firing point - the one place
/// that does know - catches it and skips the row.
///
/// A `RAISE(IGNORE)` outside a trigger body is a parse error, so there is no
/// path by which this can escape to an application.
pub const RAISE_IGNORE: &str = "inillucent: RAISE(IGNORE)";

/// `RAISE(...)`: an expression whose evaluation is the failure.
struct Raise {
    /// The extended result code.
    code: i32,
    /// The message.
    message: String,
    /// What the action undoes.
    unwind: Unwind,
}

impl Eval for Raise {
    fn value<'p>(&self, _batch: &Batch<'p>, _nth: usize) -> DbResult<Computed<'p>> {
        Err(
            inillucent_base::error::DbError::new(inillucent_base::error::ExtendedCode(self.code))
                .with_message(self.message.clone())
                // Written out, so nothing overrides it: a trigger body's
                // `RAISE(ROLLBACK)` rolls the transaction back whatever the
                // statement that fired the trigger asked for.
                .with_raised_unwind(self.unwind),
        )
    }
}

/// Integer arithmetic, both operands proved integral.
struct IntArith {
    op: ArithOp,
    left: Box<dyn Eval>,
    right: Box<dyn Eval>,
}

impl Eval for IntArith {
    fn value<'p>(&self, batch: &Batch<'p>, nth: usize) -> DbResult<Computed<'p>> {
        let left = self.left.value(batch, nth)?;
        let right = self.right.value(batch, nth)?;
        if left.is_null() || right.is_null() {
            return Ok(Computed::Borrowed(Datum::Null));
        }
        // The static type is a claim the binder derived from column *affinity*,
        // and a dynamically typed engine can hold a row that violates it - a
        // string or a real in a column declared INTEGER, which the leaf records
        // as an exception. Producing NULL for those rows would make the
        // specialisation faster and wrong, which is the one thing it may not be,
        // so it falls back to the generic arithmetic instead.
        let (Some(a), Some(b)) = (left.as_int(), right.as_int()) else {
            return generic_arith(self.op, &left.get(), &right.get());
        };
        Ok(Computed::Borrowed(integer_arith(self.op, a, b)))
    }
}

/// Applies an arithmetic operator to two integers.
///
/// SQLite promotes an integer overflow to a double rather than trapping or
/// wrapping, and both the specialised and the generic node route through here
/// so the two cannot drift apart.
///
/// @param op - the operator
/// @param left - the left operand
/// @param right - the right operand
fn integer_arith<'p>(op: ArithOp, left: i64, right: i64) -> Datum<'p> {
    let checked = match op {
        ArithOp::Add => left.checked_add(right),
        ArithOp::Subtract => left.checked_sub(right),
        ArithOp::Multiply => left.checked_mul(right),
    };
    match checked {
        Some(number) => Datum::Int(number),
        None => {
            let (a, b) = (left as f64, right as f64);
            real_or_null(match op {
                ArithOp::Add => a + b,
                ArithOp::Subtract => a - b,
                ArithOp::Multiply => a * b,
            })
        }
    }
}

/// Returns a double, or NULL when the arithmetic had no answer.
///
/// **SQLite has no NaN.** `1e999 - 1e999` is NULL there and was `NaN` here - a
/// value that is not equal to itself, that no comparison orders and that no
/// caller of an arithmetic expression has any way to handle. The same rule
/// already covered division (`0.0/0.0` is NULL); this is the other operators,
/// which reach it only through an infinity.
///
/// @param value - the computed double
fn real_or_null<'p>(value: f64) -> Datum<'p> {
    if value.is_nan() {
        return Datum::Null;
    }
    Datum::Real(value)
}

/// Applies an arithmetic operator to two values of any class.
///
/// @param op - the operator
/// @param left - the left operand
/// @param right - the right operand
pub(crate) fn generic_arith<'p>(
    op: ArithOp,
    left: &Datum<'_>,
    right: &Datum<'_>,
) -> DbResult<Computed<'p>> {
    if left.is_null() || right.is_null() {
        return Ok(Computed::Borrowed(Datum::Null));
    }
    // **A text or blob operand converts to an integer where SQLite makes one.**
    // `x'00' + x'00'` is `0` there and was `0.0` here, and `'abc' + 1` was
    // `1.0` against `1`: the conversion went straight to a double, so every
    // sum with a non-numeric operand in it came out a real. SQLite's rule is
    // the one `CAST(x AS NUMERIC)` follows - integral text becomes an integer,
    // text with a point or an exponent becomes a real, and text that is not a
    // number at all becomes the integer zero - and the class it produces is
    // the class of the answer.
    let held = (numeric_datum(left), numeric_datum(right));
    let (left, right) = (&held.0, &held.1);
    if let (Some(a), Some(b)) = (left.as_int(), right.as_int()) {
        return Ok(Computed::Borrowed(integer_arith(op, a, b)));
    }
    let (a, b) = (numeric(left), numeric(right));
    Ok(Computed::Borrowed(real_or_null(match op {
        ArithOp::Add => a + b,
        ArithOp::Subtract => a - b,
        ArithOp::Multiply => a * b,
    })))
}

/// Returns a value as the number SQLite's arithmetic reads it as.
///
/// An integer or a real stays what it is; a text or blob becomes an integer
/// when its numeric prefix is integral and fits, a real when it is not, and the
/// integer zero when there is no numeric prefix at all - which is what makes
/// `x'00' + x'00'` an integer.
///
/// @param value - the operand
fn numeric_datum<'p>(value: &Datum<'_>) -> Datum<'p> {
    let (Datum::Text(bytes) | Datum::Blob(bytes)) = value else {
        return match value {
            Datum::Int(number) => Datum::Int(*number),
            Datum::Real(number) => Datum::Real(*number),
            _ => Datum::Null,
        };
    };
    if let Some(number) = prefix_integer(bytes) {
        return Datum::Int(number);
    }
    Datum::Real(prefix_number(bytes))
}

/// Returns the numeric prefix as an integer, when it is one.
///
/// `None` when the prefix is a real - a point with a digit on one side of it,
/// or a complete exponent - and `Some(0)` when there is no number at the front
/// at all, which is what SQLite reads `'abc' + 0` as.
///
/// **The shape is read before it is judged (task-1979, F3).** The scan this
/// replaces refused a point or an exponent only *after* it had seen a digit,
/// so a leading point was not a point at all: `'.5'` fell out of the loop with
/// nothing read and came back as the integer zero, and every leading-dot
/// numeral in arithmetic did the same - `'.5'+0` was 0 where SQLite says 0.5.
/// It also read `'1e'` as a real, because it stopped at the `e` and handed the
/// rest to the real parser; SQLite reads the exponent as incomplete, so the
/// prefix ends at the `1` and the answer is the integer 1.
///
/// Checked against the pinned 3.53.4 for each shape: `'1.'+0` is 1.0 real,
/// `'.'+0` is 0 integer, `'1e'+0` and `'1e+'+0` are 1 integer, `'.5e'+0` is 0.5
/// real, `'1abc'+0` is 1 integer, `'1e2'+0` is 100.0 real.
///
/// @param bytes - the string to read
fn prefix_integer(bytes: &[u8]) -> Option<i64> {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Some(0);
    };
    let raw = text.as_bytes();
    let mut at = 0usize;
    while raw
        .get(at)
        .copied()
        .is_some_and(|byte| byte == b' ' || byte.is_ascii_whitespace())
    {
        at = at.saturating_add(1);
    }
    let began = at;
    if matches!(raw.get(at), Some(b'+') | Some(b'-')) {
        at = at.saturating_add(1);
    }
    let before = digits_from(raw, &mut at);
    // A point is part of the number only when a digit sits on one side of it.
    // `'.'` on its own is not a number, which is why the count is taken before
    // the decision.
    let mut fractional = false;
    if raw.get(at) == Some(&b'.') {
        let mut after_point = at.saturating_add(1);
        let after = digits_from(raw, &mut after_point);
        if before.saturating_add(after) > 0 {
            fractional = true;
            at = after_point;
        }
    }
    if before == 0 && !fractional {
        // No number at all, which SQLite reads as the integer zero.
        return Some(0);
    }
    // An exponent counts only when it has at least one digit of its own;
    // without one the number ends before the `e`.
    if matches!(raw.get(at), Some(b'e') | Some(b'E')) {
        let mut after_e = at.saturating_add(1);
        if matches!(raw.get(after_e), Some(b'+') | Some(b'-')) {
            after_e = after_e.saturating_add(1);
        }
        let mut counting = after_e;
        if digits_from(raw, &mut counting) > 0 {
            return None;
        }
    }
    if fractional {
        return None;
    }
    text.get(began..at)
        .and_then(|prefix| prefix.parse::<i64>().ok())
}

/// Advances past a run of ASCII digits and returns how many there were.
///
/// @param raw - the bytes being read
/// @param at - the position, moved past the digits
fn digits_from(raw: &[u8], at: &mut usize) -> usize {
    let mut counted = 0usize;
    while raw
        .get(*at)
        .copied()
        .is_some_and(|byte| byte.is_ascii_digit())
    {
        *at = at.saturating_add(1);
        counted = counted.saturating_add(1);
    }
    counted
}

/// Arithmetic over anything.
struct GenericArith {
    op: ArithOp,
    left: Box<dyn Eval>,
    right: Box<dyn Eval>,
}

impl Eval for GenericArith {
    fn value<'p>(&self, batch: &Batch<'p>, nth: usize) -> DbResult<Computed<'p>> {
        let left = self.left.value(batch, nth)?;
        let right = self.right.value(batch, nth)?;
        generic_arith(self.op, &left.get(), &right.get())
    }
}

/// A comparison of a known-integer column against an integer constant.
///
/// The narrowest and most common specialisation: no child call, no allocation,
/// one integer compare.
struct IntColumnAgainstConstant {
    column: usize,
    op: CompareOp,
    constant: i64,
}

impl Eval for IntColumnAgainstConstant {
    fn value<'p>(&self, batch: &Batch<'p>, nth: usize) -> DbResult<Computed<'p>> {
        let value = batch.value(nth, self.column)?;
        Ok(Computed::Borrowed(match value.as_int() {
            Some(number) => Datum::Int(i64::from(self.op.holds(number.cmp(&self.constant)))),
            None if value.is_null() => Datum::Null,
            // The column's affinity said integer and the row holds something
            // else - an exception row. Fall back to the dialect's comparison
            // rather than guessing, so the specialisation stays a speed change.
            None => Datum::Int(i64::from(
                self.op.holds(value.compare(&Datum::Int(self.constant))),
            )),
        }))
    }
}

/// A comparison of two proved-integral expressions.
struct IntCompare {
    op: CompareOp,
    left: Box<dyn Eval>,
    right: Box<dyn Eval>,
}

impl Eval for IntCompare {
    fn value<'p>(&self, batch: &Batch<'p>, nth: usize) -> DbResult<Computed<'p>> {
        let left = self.left.value(batch, nth)?;
        let right = self.right.value(batch, nth)?;
        if left.is_null() || right.is_null() {
            return Ok(Computed::Borrowed(Datum::Null));
        }
        Ok(Computed::Borrowed(Datum::Int(i64::from(
            self.op.holds(left.get().compare(&right.get())),
        ))))
    }
}

/// A comparison of anything, by the dialect's rules.
struct GenericCompare {
    op: CompareOp,
    left: Box<dyn Eval>,
    right: Box<dyn Eval>,
}

impl Eval for GenericCompare {
    fn value<'p>(&self, batch: &Batch<'p>, nth: usize) -> DbResult<Computed<'p>> {
        let left = self.left.value(batch, nth)?;
        let right = self.right.value(batch, nth)?;
        if left.is_null() || right.is_null() {
            return Ok(Computed::Borrowed(Datum::Null));
        }
        Ok(Computed::Borrowed(Datum::Int(i64::from(
            self.op.holds(left.get().compare(&right.get())),
        ))))
    }
}

/// A comparison that applies an affinity and a collation.
///
/// The conversion and the comparison are `inillucent-value`'s, not a second
/// implementation of them: `apply_affinity` and `compare_sql` are the functions
/// the old engine used and the ones the affinity tests are written against. The
/// only thing here is the bridge from a borrowed page value to a `Value` and
/// back to a truth.
struct AffinityCompare {
    op: CompareOp,
    affinity: Option<Affinity>,
    collation: Collation,
    left: Box<dyn Eval>,
    right: Box<dyn Eval>,
}

impl Eval for AffinityCompare {
    fn value<'p>(&self, batch: &Batch<'p>, nth: usize) -> DbResult<Computed<'p>> {
        let left = self.left.value(batch, nth)?;
        let right = self.right.value(batch, nth)?;
        if left.is_null() || right.is_null() {
            return Ok(Computed::Borrowed(Datum::Null));
        }
        let (left, right) = (left.get(), right.get());
        let (left, right) = (Value::from(&left), Value::from(&right));
        let (left, right) = match self.affinity {
            None => (left, right),
            Some(affinity) => (
                affinity::apply_affinity(left, affinity, TextEncoding::Utf8).unwrap_or(Value::Null),
                affinity::apply_affinity(right, affinity, TextEncoding::Utf8)
                    .unwrap_or(Value::Null),
            ),
        };
        let order = compare_sql(&left, &right, self.collation);
        match order.ordering() {
            Some(order) => Ok(Computed::Borrowed(Datum::Int(i64::from(
                self.op.holds(order),
            )))),
            None => Ok(Computed::Borrowed(Datum::Null)),
        }
    }
}

/// `AND` with three-valued logic.
struct Conjunction {
    left: Box<dyn Eval>,
    right: Box<dyn Eval>,
}

impl Eval for Conjunction {
    fn value<'p>(&self, batch: &Batch<'p>, nth: usize) -> DbResult<Computed<'p>> {
        let left = self.left.value(batch, nth)?;
        // FALSE AND anything is FALSE, even when the other side is NULL, so a
        // definite false short-circuits.
        if truth(&left.get()) == Some(false) {
            return Ok(Computed::Borrowed(Datum::Int(0)));
        }
        let right = self.right.value(batch, nth)?;
        Ok(Computed::Borrowed(
            match (truth(&left.get()), truth(&right.get())) {
                (_, Some(false)) => Datum::Int(0),
                (Some(true), Some(true)) => Datum::Int(1),
                _ => Datum::Null,
            },
        ))
    }
}

/// `OR` with three-valued logic.
struct Disjunction {
    left: Box<dyn Eval>,
    right: Box<dyn Eval>,
}

impl Eval for Disjunction {
    fn value<'p>(&self, batch: &Batch<'p>, nth: usize) -> DbResult<Computed<'p>> {
        let left = self.left.value(batch, nth)?;
        if truth(&left.get()) == Some(true) {
            return Ok(Computed::Borrowed(Datum::Int(1)));
        }
        let right = self.right.value(batch, nth)?;
        Ok(Computed::Borrowed(
            match (truth(&left.get()), truth(&right.get())) {
                (_, Some(true)) => Datum::Int(1),
                (Some(false), Some(false)) => Datum::Int(0),
                _ => Datum::Null,
            },
        ))
    }
}

/// `NOT` with three-valued logic.
struct Negation {
    inner: Box<dyn Eval>,
}

impl Eval for Negation {
    fn value<'p>(&self, batch: &Batch<'p>, nth: usize) -> DbResult<Computed<'p>> {
        let inner = self.inner.value(batch, nth)?;
        Ok(Computed::Borrowed(match truth(&inner.get()) {
            Some(true) => Datum::Int(0),
            Some(false) => Datum::Int(1),
            None => Datum::Null,
        }))
    }
}

/// `IS NULL` and `IS NOT NULL`, which are never NULL themselves.
struct NullTest {
    inner: Box<dyn Eval>,
    wanted: bool,
}

impl Eval for NullTest {
    fn value<'p>(&self, batch: &Batch<'p>, nth: usize) -> DbResult<Computed<'p>> {
        let is_null = self.inner.value(batch, nth)?.is_null();
        Ok(Computed::Borrowed(Datum::Int(i64::from(
            is_null == self.wanted,
        ))))
    }
}

/// `length(x)`: characters for text, bytes for a blob, NULL for NULL.
struct Length {
    inner: Box<dyn Eval>,
}

impl Eval for Length {
    fn value<'p>(&self, batch: &Batch<'p>, nth: usize) -> DbResult<Computed<'p>> {
        let inner = self.inner.value(batch, nth)?;
        Ok(Computed::Borrowed(match inner.get() {
            Datum::Null => Datum::Null,
            // SQLite counts characters in text and bytes in a blob. The
            // database encoding is UTF-8, so a character is a non-continuation
            // byte - and the count stops at the first NUL, which is SQLite's
            // documented definition rather than an accident of C strings:
            // `length(char(0))` is 0 and `length(char(65,0,66))` is 1, while
            // `hex()` of the same values shows every byte is still there.
            Datum::Text(bytes) => {
                let counted = match bytes.iter().position(|byte| *byte == 0) {
                    Some(at) => bytes.get(..at).unwrap_or(bytes),
                    None => bytes,
                };
                Datum::Int(counted.iter().filter(|byte| (*byte & 0xC0) != 0x80).count() as i64)
            }
            Datum::Blob(bytes) => Datum::Int(bytes.len() as i64),
            Datum::Int(number) => Datum::Int(number.to_string().len() as i64),
            Datum::Real(number) => Datum::Int(format_real(number).len() as i64),
        }))
    }
}

/// Returns a value's truth, or `None` for NULL.
///
/// SQLite's rule: zero and the empty string are false, every other number and
/// every other string are true, and a string that does not look like a number
/// is false.
///
/// @param value - the value to test
pub fn truth(value: &Datum<'_>) -> Option<bool> {
    match value {
        Datum::Null => None,
        Datum::Int(number) => Some(*number != 0),
        Datum::Real(number) => Some(*number != 0.0),
        Datum::Text(bytes) | Datum::Blob(bytes) => Some(prefix_number(bytes) != 0.0),
    }
}

/// Returns a value as a double, by SQLite's coercion rules.
///
/// @param value - the value to coerce
pub fn numeric(value: &Datum<'_>) -> f64 {
    match value {
        Datum::Null => 0.0,
        Datum::Int(number) => *number as f64,
        Datum::Real(number) => *number,
        Datum::Text(bytes) | Datum::Blob(bytes) => prefix_number(bytes),
    }
}

/// Reads the longest numeric prefix of a byte string, as SQLite does.
///
/// @param bytes - the string to read
fn prefix_number(bytes: &[u8]) -> f64 {
    let text = match std::str::from_utf8(bytes) {
        Ok(text) => text.trim_start(),
        Err(_) => return 0.0,
    };
    let raw = text.as_bytes();
    let mut at = 0usize;
    if matches!(raw.get(at), Some(b'+') | Some(b'-')) {
        at = at.saturating_add(1);
    }
    let before = digits_from(raw, &mut at);
    let mut after = 0usize;
    if raw.get(at) == Some(&b'.') {
        let mut after_point = at.saturating_add(1);
        after = digits_from(raw, &mut after_point);
        if before.saturating_add(after) > 0 {
            at = after_point;
        }
    }
    if before.saturating_add(after) == 0 {
        return 0.0;
    }
    // **An exponent counts only when it has a digit of its own (task-1979,
    // F3).** The scan this replaces accepted a trailing `e`, so `'.5e'` came
    // back as the four bytes `.5e`, which `parse::<f64>` refuses - and the
    // fallback answered 0.0 for a string whose numeric prefix is 0.5. SQLite
    // ends the number before an exponent it cannot complete.
    if matches!(raw.get(at), Some(b'e') | Some(b'E')) {
        let mut after_e = at.saturating_add(1);
        if matches!(raw.get(after_e), Some(b'+') | Some(b'-')) {
            after_e = after_e.saturating_add(1);
        }
        let mut counting = after_e;
        if digits_from(raw, &mut counting) > 0 {
            at = counting;
        }
    }
    text.get(..at)
        .and_then(|prefix| prefix.parse::<f64>().ok())
        .unwrap_or(0.0)
}

/// Renders a double the way the dialect's text conversion does.
///
/// One line, delegating, and it stays that way. What was here before was a
/// hand-rolled approximation - `{:.1}` for whole numbers under 1e15 and Rust's
/// `Display` for everything else - sitting a crate away from
/// [`inillucent_value::numeric::real_to_text`], which is a transcription of
/// SQLite's `%!.17g` down to the double rounding and the round-trip
/// shortening.
///
/// The two agreed on every value anybody had thought to test and disagreed on
/// the ones nobody had. Rust's `Display` renders `1e300` as three hundred and
/// one digits where SQLite renders `1.0e+300`, so `length(score)` came back as
/// 302 instead of 9 and `group_concat` produced a line of zeros; and `{:.1}`
/// renders `-0.0` as `-0.0` where SQLite deliberately drops the sign, because
/// it decides with `r < 0.0` and `-0.0 < 0.0` is false.
///
/// A generated differential sweep found both in its first run. Neither would
/// ever have been found by reading the code, because the code looked right.
///
/// @param number - the value to render
pub fn format_real(number: f64) -> String {
    String::from_utf8_lossy(&inillucent_value::numeric::real_to_text(number)).into_owned()
}

/// Whether `apply_affinity` returns this value exactly as it was given.
///
/// Read off `inillucent_value::affinity::apply_affinity`: BLOB affinity changes
/// nothing, a NULL and a blob are never changed, an integer changes only under
/// TEXT, a real only under the numeric affinities other than FLEXNUM, and text
/// only under a numeric one. `an_affinity_that_changes_nothing_is_skipped`
/// checks this against `apply_affinity` over every pair.
///
/// @param value - the value
/// @param affinity - the affinity about to be applied
fn leaves_unchanged(value: &Datum<'_>, affinity: Affinity) -> bool {
    match value {
        Datum::Null | Datum::Blob(_) => true,
        Datum::Int(_) => affinity != Affinity::Text,
        Datum::Real(_) => matches!(affinity, Affinity::Blob | Affinity::FlexNum),
        Datum::Text(_) => matches!(affinity, Affinity::Blob | Affinity::Text),
    }
}

/// Applies a comparison's affinity to one value.
///
/// See [`Expr::Affinity`] for why this is not a cast.
struct ApplyAffinity {
    /// What to convert.
    operand: Box<dyn Eval>,
    /// The conversion.
    affinity: Affinity,
}

impl Eval for ApplyAffinity {
    fn value<'p>(&self, batch: &Batch<'p>, nth: usize) -> DbResult<Computed<'p>> {
        let value = self.operand.value(batch, nth)?;
        // **A value the affinity leaves alone is handed back as it came
        // (task-2110, bug 8).** Since task-2083 a nested loop's probe key
        // passes through here once per probe, and the conversion below copies
        // the key into an owned `Value` and back even when nothing changes -
        // an integer key probing an INTEGER index, which is every probe
        // `join.range` makes. `leaves_unchanged` is `apply_affinity`'s own
        // rules for the cases it returns its input, so the answer is the same.
        if leaves_unchanged(&value.get(), self.affinity) {
            return Ok(value);
        }
        let converted = inillucent_value::affinity::apply_affinity(
            Value::from(&value.get()).into_owned()?,
            self.affinity,
            inillucent_value::TextEncoding::Utf8,
        )
        .unwrap_or(inillucent_value::Value::Null);
        Ok(Computed::Owned(OwnedDatum::from(converted)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::batch::Vector;

    /// The fast path in `ApplyAffinity` skips a conversion only where
    /// `apply_affinity` would have returned the value unchanged, for every value
    /// class against every affinity (task-2110, bug 8).
    #[test]
    fn an_affinity_that_changes_nothing_is_skipped() {
        let samples = [
            Datum::Null,
            Datum::Int(3),
            Datum::Real(3.0),
            Datum::Real(3.5),
            Datum::Text(b"3"),
            Datum::Text(b"abc"),
            Datum::Blob(b"3"),
        ];
        let affinities = [
            Affinity::Blob,
            Affinity::Text,
            Affinity::Numeric,
            Affinity::Integer,
            Affinity::Real,
            Affinity::FlexNum,
        ];
        for value in samples {
            for affinity in affinities {
                if !leaves_unchanged(&value, affinity) {
                    continue;
                }
                let applied = affinity::apply_affinity(
                    Value::from(&value).into_owned().expect("owned"),
                    affinity,
                    TextEncoding::Utf8,
                )
                .expect("applies");
                assert_eq!(
                    OwnedDatum::from(applied),
                    OwnedDatum::from_datum(&value),
                    "{value:?} under {affinity:?} was skipped and apply_affinity changes it"
                );
            }
        }
        // And the case every `join.range` probe is: an integer key into an INTEGER index.
        assert!(leaves_unchanged(&Datum::Int(7), Affinity::Integer));
        assert!(!leaves_unchanged(&Datum::Int(7), Affinity::Text));
    }

    fn batch_of<'a>(values: &'a [Datum<'a>]) -> Batch<'a> {
        Batch::new(values.len(), vec![Vector::Values(values)])
    }

    /// Every specialisation agrees with the generic node it replaces, over
    /// every value class - including the ones that make the specialisation's
    /// assumption false.
    ///
    /// This is the test that makes closure compilation safe to do at all: the
    /// compiler may pick a faster node, and this asserts it never picks a
    /// different answer.
    #[test]
    fn every_specialisation_agrees_with_the_generic_path() {
        let samples = [
            Datum::Null,
            Datum::Int(-1),
            Datum::Int(0),
            Datum::Int(5),
            Datum::Int(i64::MAX),
            Datum::Real(-1.5),
            Datum::Real(5.0),
            Datum::Text(b"5"),
            Datum::Text(b"apple"),
            Datum::Blob(&[5]),
        ];
        let operators = [
            CompareOp::Equal,
            CompareOp::NotEqual,
            CompareOp::Less,
            CompareOp::LessOrEqual,
            CompareOp::Greater,
            CompareOp::GreaterOrEqual,
        ];
        for op in operators {
            for constant in [-1i64, 0, 5, i64::MAX] {
                let expr = Expr::Compare(
                    op,
                    Box::new(Expr::Column(0)),
                    Box::new(Expr::Literal(OwnedDatum::Int(constant))),
                );
                // Claiming the column is an integer selects the specialisation;
                // claiming nothing selects the generic node.
                let fast = compile(&expr, &[StaticType::Int]).unwrap();
                let slow = compile(&expr, &[StaticType::Unknown]).unwrap();
                for value in samples {
                    let one = [value];
                    let batch = batch_of(&one);
                    let a = fast.value(&batch, 0).unwrap();
                    let b = slow.value(&batch, 0).unwrap();
                    assert_eq!(
                        a.get().compare(&b.get()),
                        std::cmp::Ordering::Equal,
                        "{op:?} {value:?} vs {constant}: {a:?} against {b:?}"
                    );
                    assert_eq!(a.is_null(), b.is_null(), "{op:?} {value:?} vs {constant}");
                }
            }
        }
        // The same for arithmetic.
        for op in [ArithOp::Add, ArithOp::Subtract, ArithOp::Multiply] {
            let expr = Expr::Arith(op, Box::new(Expr::Column(0)), Box::new(Expr::Column(1)));
            let fast = compile(&expr, &[StaticType::Int, StaticType::Int]).unwrap();
            let slow = compile(&expr, &[StaticType::Unknown, StaticType::Unknown]).unwrap();
            for left in samples {
                for right in samples {
                    let batch = Batch::new(1, vec![Vector::Const(left), Vector::Const(right)]);
                    let a = fast.value(&batch, 0).unwrap();
                    let b = slow.value(&batch, 0).unwrap();
                    assert_eq!(
                        a.get().compare(&b.get()),
                        std::cmp::Ordering::Equal,
                        "{op:?} {left:?} {right:?}: {a:?} against {b:?}"
                    );
                }
            }
        }
    }

    /// Integer overflow becomes a double rather than wrapping or trapping, on
    /// both paths.
    #[test]
    fn overflow_promotes_to_a_double() {
        let expr = Expr::Arith(
            ArithOp::Add,
            Box::new(Expr::Column(0)),
            Box::new(Expr::Column(1)),
        );
        let fast = compile(&expr, &[StaticType::Int, StaticType::Int]).unwrap();
        let batch = Batch::new(
            1,
            vec![
                Vector::Const(Datum::Int(i64::MAX)),
                Vector::Const(Datum::Int(1)),
            ],
        );
        let value = fast.value(&batch, 0).unwrap();
        assert!(matches!(value.get(), Datum::Real(_)), "{value:?}");
        assert_eq!(value.get().as_f64().unwrap(), i64::MAX as f64 + 1.0);
    }

    /// Three-valued logic follows SQL: `FALSE AND NULL` is FALSE, and
    /// `TRUE OR NULL` is TRUE, but `TRUE AND NULL` is NULL.
    #[test]
    fn three_valued_logic_is_sql_logic() {
        let and = compile(
            &Expr::And(Box::new(Expr::Column(0)), Box::new(Expr::Column(1))),
            &[StaticType::Unknown, StaticType::Unknown],
        )
        .unwrap();
        let or = compile(
            &Expr::Or(Box::new(Expr::Column(0)), Box::new(Expr::Column(1))),
            &[StaticType::Unknown, StaticType::Unknown],
        )
        .unwrap();
        let cases = [
            (Datum::Int(0), Datum::Null, Some(0i64), None),
            (Datum::Null, Datum::Int(0), Some(0), None),
            (Datum::Int(1), Datum::Null, None, Some(1)),
            (Datum::Null, Datum::Int(1), None, Some(1)),
            (Datum::Int(1), Datum::Int(1), Some(1), Some(1)),
            (Datum::Int(0), Datum::Int(0), Some(0), Some(0)),
            (Datum::Null, Datum::Null, None, None),
        ];
        for (left, right, wanted_and, wanted_or) in cases {
            let batch = Batch::new(1, vec![Vector::Const(left), Vector::Const(right)]);
            assert_eq!(
                and.value(&batch, 0).unwrap().as_int(),
                wanted_and,
                "AND {left:?} {right:?}"
            );
            assert_eq!(
                or.value(&batch, 0).unwrap().as_int(),
                wanted_or,
                "OR {left:?} {right:?}"
            );
        }
    }

    /// `IS NULL` is never NULL, whatever its operand is.
    #[test]
    fn null_tests_are_two_valued() {
        let is_null = compile(
            &Expr::IsNull(Box::new(Expr::Column(0))),
            &[StaticType::Unknown],
        )
        .unwrap();
        let is_not = compile(
            &Expr::IsNotNull(Box::new(Expr::Column(0))),
            &[StaticType::Unknown],
        )
        .unwrap();
        for value in [Datum::Null, Datum::Int(0), Datum::Text(b"")] {
            let batch = Batch::new(1, vec![Vector::Const(value)]);
            let a = is_null.value(&batch, 0).unwrap().as_int().unwrap();
            let b = is_not.value(&batch, 0).unwrap().as_int().unwrap();
            assert_eq!(a + b, 1, "{value:?}");
        }
    }

    /// `length` counts characters in text and bytes in a blob, as the dialect
    /// does.
    #[test]
    fn length_counts_characters_not_bytes() {
        let length = compile(
            &Expr::Length(Box::new(Expr::Column(0))),
            &[StaticType::Unknown],
        )
        .unwrap();
        let cases: [(Datum<'_>, Option<i64>); 5] = [
            (Datum::Null, None),
            (Datum::Text(b"abc"), Some(3)),
            // Three characters, seven bytes.
            (Datum::Text("aé漢".as_bytes()), Some(3)),
            (Datum::Blob(&[1, 2, 3, 4]), Some(4)),
            (Datum::Int(-1234), Some(5)),
        ];
        for (value, wanted) in cases {
            let batch = Batch::new(1, vec![Vector::Const(value)]);
            assert_eq!(
                length.value(&batch, 0).unwrap().as_int(),
                wanted,
                "{value:?}"
            );
        }
    }

    /// A text value's numeric prefix is read the way SQLite reads it.
    #[test]
    fn the_numeric_prefix_is_the_dialect_prefix() {
        assert_eq!(prefix_number(b"12abc"), 12.0);
        assert_eq!(prefix_number(b"  -3.5xyz"), -3.5);
        assert_eq!(prefix_number(b"1e3"), 1000.0);
        assert_eq!(prefix_number(b"abc"), 0.0);
        assert_eq!(prefix_number(b""), 0.0);
        assert_eq!(prefix_number(b"."), 0.0);
        assert_eq!(prefix_number(b"1.2.3"), 1.2);
        assert_eq!(prefix_number(b"+7"), 7.0);
    }

    /// A bare column reference reports which column it is, so the aggregate
    /// operators can take their whole-column path.
    #[test]
    fn a_column_reference_names_its_column() {
        let compiled = compile(&Expr::Column(3), &[StaticType::Int; 4]).unwrap();
        assert_eq!(compiled.column(), Some(3));
        let other = compile(
            &Expr::IsNull(Box::new(Expr::Column(3))),
            &[StaticType::Int; 4],
        )
        .unwrap();
        assert_eq!(other.column(), None);
    }
}
