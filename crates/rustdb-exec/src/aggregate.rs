//! Aggregate accumulators, with the dialect's semantics rather than Rust's.
//!
//! Invariant: every accumulator produces what SQLite 3.53.4 produces, including
//! the cases where that is surprising. The differential gate compares digests,
//! so `sum` of no rows being NULL while `total` of no rows is `0.0` is not a
//! detail - it is the difference between a passing family and a correctness
//! failure that stops the timing being read at all.
//!
//! The documented behaviours this implements:
//!
//! - **`count(*)`** counts rows; **`count(x)`** counts rows where `x` is not
//!   NULL.
//! - **`sum(x)`** of no non-NULL rows is **NULL**. `sum` of integers stays an
//!   integer until it overflows, and then becomes a double - SQLite raises an
//!   "integer overflow" error for `sum` in that case, which Phase 1 does not
//!   reach on any scorecard workload and which is recorded as an open item
//!   rather than silently guessed at.
//! - **`total(x)`** is `sum` that returns `0.0` instead of NULL and is always a
//!   double.
//! - **`avg(x)`** is a double, NULL over no non-NULL rows, and divides by the
//!   count of non-NULL rows.
//! - **`min`/`max`** ignore NULLs, return NULL over no non-NULL rows, and order
//!   by the dialect's class order.
//! - **`group_concat(x, sep)`** joins non-NULL values with `sep` (default
//!   `","`) and is NULL over no non-NULL rows.

use rustdb_base::DbResult;
use rustdb_tree::datum::{Datum, OwnedDatum};

use crate::expr::{format_real, numeric};

/// Which aggregate an accumulator computes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AggregateKind {
    /// `count(*)`: every row.
    CountStar,
    /// `count(x)`: rows where the argument is not NULL.
    Count,
    /// `sum(x)`.
    Sum,
    /// `total(x)`.
    Total,
    /// `avg(x)`.
    Average,
    /// `min(x)`.
    Minimum,
    /// `max(x)`.
    Maximum,
    /// `group_concat(x, sep)`.
    GroupConcat(String),
}

/// The modulus a large integer is split on before it joins a double sum.
///
/// 2^14. The high part is then a multiple of 2^14, so a 63-bit integer needs
/// only 49 bits of mantissa to hold it exactly, and the remainder is small
/// enough to be exact by itself. This is the constant SQLite uses.
const SPLIT_I64: i64 = 16_384;

/// The same, for the `i128` seed.
const SPLIT: i128 = 16_384;

/// The magnitude below which a double holds an integer exactly.
///
/// 2^52. SQLite uses the same bound to decide whether a value needs splitting.
const EXACT_IN_DOUBLE: i64 = 4_503_599_627_370_496;

/// One aggregate's running state.
#[derive(Clone, Debug)]
pub struct Accumulator {
    kind: AggregateKind,
    /// Rows counted, or non-NULL values seen.
    count: i64,
    /// The integer running total, while the sum is still exact.
    integer_sum: i64,
    /// The double running total, once the sum has left the integers.
    real_sum: f64,
    /// The low bits the running total could not hold.
    ///
    /// Kahan-Babuska-Neumaier's compensation term. It is added in at the very
    /// end rather than folded back each step, which is what makes the sum of
    /// `-1e300`, a handful of ordinary values and `+1e300` come out as the
    /// handful rather than as zero - and is what the pinned SQLite answers.
    compensation: f64,
    /// Whether the sum has left the integers.
    is_real: bool,
    /// The extreme value seen, for `min` and `max`.
    extreme: Option<OwnedDatum>,
    /// The joined text, for `group_concat`.
    joined: String,
}

impl Accumulator {
    /// Returns a fresh accumulator.
    ///
    /// @param kind - which aggregate it computes
    pub fn new(kind: AggregateKind) -> Accumulator {
        Accumulator {
            kind,
            count: 0,
            integer_sum: 0,
            real_sum: 0.0,
            compensation: 0.0,
            is_real: false,
            extreme: None,
            joined: String::new(),
        }
    }

    /// Returns which aggregate this computes.
    pub fn kind(&self) -> &AggregateKind {
        &self.kind
    }

    /// Folds one value in.
    ///
    /// @param value - the argument's value for this row, or NULL for `count(*)`
    pub fn push(&mut self, value: &Datum<'_>) {
        if self.kind == AggregateKind::CountStar {
            self.count = self.count.saturating_add(1);
            return;
        }
        if value.is_null() {
            return;
        }
        self.count = self.count.saturating_add(1);
        match &self.kind {
            AggregateKind::CountStar | AggregateKind::Count => {}
            AggregateKind::Sum | AggregateKind::Total | AggregateKind::Average => {
                self.push_numeric(value)
            }
            AggregateKind::Minimum => self.push_extreme(value, std::cmp::Ordering::Less),
            AggregateKind::Maximum => self.push_extreme(value, std::cmp::Ordering::Greater),
            AggregateKind::GroupConcat(separator) => {
                if self.count > 1 {
                    self.joined.push_str(separator);
                }
                self.joined.push_str(&render(value));
            }
        }
    }

    /// Folds a whole run of integers in at once.
    ///
    /// The vectorised entry point: a `sum` over a dense integer column calls
    /// this once per batch with the mini-column's bytes and never touches a
    /// `Datum`. It is the reason the aggregate operator has a fast path at all.
    ///
    /// @param bytes - a contiguous run of little-endian `i64` values
    pub fn push_dense_ints(&mut self, bytes: &[u8]) {
        let rows = bytes.len() / 8;
        match self.kind {
            AggregateKind::CountStar | AggregateKind::Count => {
                self.count = self.count.saturating_add(rows as i64);
            }
            AggregateKind::Sum | AggregateKind::Total | AggregateKind::Average => {
                self.count = self.count.saturating_add(rows as i64);
                if self.is_real {
                    for chunk in bytes.chunks_exact(8) {
                        self.add_int(i64::from_le_bytes(chunk.try_into().unwrap_or([0; 8])));
                    }
                    return;
                }
                // Accumulate in i128 so one pass can be taken without a
                // checked_add per element, then fall back to the double sum
                // only if the exact total does not fit. The result is the same
                // value the per-row path produces, which the agreement test
                // asserts.
                let mut wide: i128 = i128::from(self.integer_sum);
                for chunk in bytes.chunks_exact(8) {
                    wide += i128::from(i64::from_le_bytes(chunk.try_into().unwrap_or([0; 8])));
                }
                match i64::try_from(wide) {
                    Ok(exact) => self.integer_sum = exact,
                    Err(_) => {
                        // The exact total left `i64`, so the sum becomes a
                        // double from here. Seeding it needs the same split
                        // `seed_real` uses, applied to the wider value.
                        self.is_real = true;
                        let low = (wide % SPLIT) as f64;
                        self.real_sum = (wide - i128::from(low as i64)) as f64;
                        self.compensation = low;
                    }
                }
            }
            AggregateKind::Minimum => {
                let mut best = i64::MAX;
                for chunk in bytes.chunks_exact(8) {
                    let value = i64::from_le_bytes(chunk.try_into().unwrap_or([0; 8]));
                    if value < best {
                        best = value;
                    }
                }
                if rows > 0 {
                    self.count = self.count.saturating_add(rows as i64);
                    self.push_extreme(&Datum::Int(best), std::cmp::Ordering::Less);
                }
            }
            AggregateKind::Maximum => {
                let mut best = i64::MIN;
                for chunk in bytes.chunks_exact(8) {
                    let value = i64::from_le_bytes(chunk.try_into().unwrap_or([0; 8]));
                    if value > best {
                        best = value;
                    }
                }
                if rows > 0 {
                    self.count = self.count.saturating_add(rows as i64);
                    self.push_extreme(&Datum::Int(best), std::cmp::Ordering::Greater);
                }
            }
            AggregateKind::GroupConcat(_) => {
                for chunk in bytes.chunks_exact(8) {
                    let value = i64::from_le_bytes(chunk.try_into().unwrap_or([0; 8]));
                    self.push(&Datum::Int(value));
                }
            }
        }
    }

    /// Reports whether [`Accumulator::push_dense_ints`] is available for this
    /// aggregate.
    ///
    /// Every kind supports it; the method exists so a caller reads a name
    /// rather than a comment when it decides which path to take.
    pub fn takes_dense_ints(&self) -> bool {
        true
    }

    /// Folds one numeric value into the running total.
    ///
    /// @param value - the value to add
    fn push_numeric(&mut self, value: &Datum<'_>) {
        match value {
            Datum::Int(number) if !self.is_real => match self.integer_sum.checked_add(*number) {
                Some(total) => self.integer_sum = total,
                None => {
                    self.seed_real();
                    self.add_int(*number);
                }
            },
            Datum::Int(number) => self.add_int(*number),
            other => {
                if !self.is_real {
                    self.seed_real();
                }
                self.add_real(numeric(other));
            }
        }
    }

    /// Moves the running total out of the integers, keeping its exact value.
    ///
    /// The seed goes through the same split [`Accumulator::add_int`] uses: an
    /// integer sum that has already passed 2^53 would otherwise lose its low
    /// bits on the way into the double, which is the same error the split
    /// exists to prevent and is worth exactly as much.
    fn seed_real(&mut self) {
        self.is_real = true;
        let low = self.integer_sum % SPLIT_I64;
        self.real_sum = (self.integer_sum - low) as f64;
        self.compensation = low as f64;
    }

    /// Folds one integer into the running double total without losing its low bits.
    ///
    /// `value as f64` is not good enough and the corpus proves it. The `people`
    /// table holds `i64::MAX` and `i64::MIN` in one column beside a `-1`, and
    /// the exact total is -2. Cast to doubles, `i64::MAX` rounds *up* to 2^63
    /// and cancels `i64::MIN` exactly, so the total comes out as -1 - a
    /// one-unit error that no amount of compensation recovers, because the
    /// information was gone before the compensation saw it.
    ///
    /// So a large integer is split the way SQLite splits it: a high part that
    /// is a multiple of 2^14 and therefore exactly representable in a double,
    /// and a remainder small enough to be exact on its own. Both go through the
    /// compensated add, and the exact value survives.
    ///
    /// @param value - the integer to add
    fn add_int(&mut self, value: i64) {
        if value > -EXACT_IN_DOUBLE && value < EXACT_IN_DOUBLE {
            // Below 2^52 a double holds the integer exactly, so there is
            // nothing to split.
            self.add_real(value as f64);
            return;
        }
        let low = value % SPLIT_I64;
        self.add_real((value - low) as f64);
        self.add_real(low as f64);
    }

    /// Adds one double to the running total, compensated.
    ///
    /// Kahan-Babuska-Neumaier summation, which is what the pinned SQLite does
    /// for `sum`, `total` and `avg`. It is not an optimisation: the corpus's
    /// `score` column holds `-1e300`, nine ordinary values and `+1e300`, and a
    /// naive total is **0.0** where SQLite answers 124.25, because the small
    /// values are absorbed into the first huge one and cancelled by the second.
    /// The compensation term is what carries them across.
    ///
    /// @param value - the double to add
    fn add_real(&mut self, value: f64) {
        let total = self.real_sum + value;
        // Whichever operand is larger keeps its low bits; the other's are what
        // the compensation recovers.
        if self.real_sum.abs() >= value.abs() {
            self.compensation += (self.real_sum - total) + value;
        } else {
            self.compensation += (value - total) + self.real_sum;
        }
        self.real_sum = total;
    }

    /// Keeps the value if it is more extreme than the one held.
    ///
    /// @param value - the candidate
    /// @param wanted - `Less` for `min`, `Greater` for `max`
    fn push_extreme(&mut self, value: &Datum<'_>, wanted: std::cmp::Ordering) {
        let replace = match &self.extreme {
            None => true,
            Some(held) => value.compare(&held.borrow()) == wanted,
        };
        if replace {
            self.extreme = Some(OwnedDatum::from_datum(value));
        }
    }

    /// Returns the running total with its compensation folded in.
    ///
    /// One place, so `sum`, `total` and `avg` cannot disagree about it - which
    /// they did on the first attempt, when only `sum` was corrected and the
    /// other two answered 3.5 where SQLite answered 124.25.
    fn compensated(&self) -> f64 {
        self.real_sum + self.compensation
    }

    /// Returns the aggregate's value.
    pub fn finish(&self) -> DbResult<OwnedDatum> {
        Ok(match &self.kind {
            AggregateKind::CountStar | AggregateKind::Count => OwnedDatum::Int(self.count),
            AggregateKind::Sum => {
                if self.count == 0 {
                    OwnedDatum::Null
                } else if self.is_real {
                    OwnedDatum::Real(self.compensated())
                } else {
                    OwnedDatum::Int(self.integer_sum)
                }
            }
            AggregateKind::Total => OwnedDatum::Real(if self.is_real {
                self.compensated()
            } else {
                self.integer_sum as f64
            }),
            AggregateKind::Average => {
                if self.count == 0 {
                    OwnedDatum::Null
                } else {
                    let total = if self.is_real {
                        self.compensated()
                    } else {
                        self.integer_sum as f64
                    };
                    OwnedDatum::Real(total / self.count as f64)
                }
            }
            AggregateKind::Minimum | AggregateKind::Maximum => {
                self.extreme.clone().unwrap_or(OwnedDatum::Null)
            }
            AggregateKind::GroupConcat(_) => {
                if self.count == 0 {
                    OwnedDatum::Null
                } else {
                    OwnedDatum::Text(self.joined.clone().into_bytes())
                }
            }
        })
    }
}

/// Renders a value as text, the way the dialect's text conversion does.
///
/// @param value - the value to render
fn render(value: &Datum<'_>) -> String {
    match value {
        Datum::Null => String::new(),
        Datum::Int(number) => number.to_string(),
        Datum::Real(number) => format_real(*number),
        Datum::Text(bytes) | Datum::Blob(bytes) => String::from_utf8_lossy(bytes).into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fold(kind: AggregateKind, values: &[Datum<'_>]) -> OwnedDatum {
        let mut accumulator = Accumulator::new(kind);
        for value in values {
            accumulator.push(value);
        }
        accumulator.finish().unwrap()
    }

    /// `sum` of nothing is NULL and `total` of nothing is 0.0, which is the
    /// difference the dialect actually draws.
    #[test]
    fn empty_aggregates_follow_the_dialect() {
        assert!(matches!(fold(AggregateKind::Sum, &[]), OwnedDatum::Null));
        assert!(matches!(
            fold(AggregateKind::Average, &[]),
            OwnedDatum::Null
        ));
        assert!(matches!(
            fold(AggregateKind::Minimum, &[]),
            OwnedDatum::Null
        ));
        assert!(matches!(
            fold(AggregateKind::Maximum, &[]),
            OwnedDatum::Null
        ));
        assert!(matches!(
            fold(AggregateKind::GroupConcat(",".into()), &[]),
            OwnedDatum::Null
        ));
        match fold(AggregateKind::Total, &[]) {
            OwnedDatum::Real(number) => assert_eq!(number, 0.0),
            other => panic!("total of nothing was {other:?}"),
        }
        assert!(matches!(
            fold(AggregateKind::Count, &[]),
            OwnedDatum::Int(0)
        ));
        assert!(matches!(
            fold(AggregateKind::CountStar, &[]),
            OwnedDatum::Int(0)
        ));
    }

    /// NULLs are counted by `count(*)` and ignored by everything else.
    #[test]
    fn nulls_are_counted_only_by_count_star() {
        let values = [Datum::Int(1), Datum::Null, Datum::Int(3)];
        assert!(matches!(
            fold(AggregateKind::CountStar, &values),
            OwnedDatum::Int(3)
        ));
        assert!(matches!(
            fold(AggregateKind::Count, &values),
            OwnedDatum::Int(2)
        ));
        assert!(matches!(
            fold(AggregateKind::Sum, &values),
            OwnedDatum::Int(4)
        ));
        match fold(AggregateKind::Average, &values) {
            OwnedDatum::Real(number) => assert_eq!(number, 2.0),
            other => panic!("avg was {other:?}"),
        }
    }

    /// An integer sum stays an integer until it overflows, and then becomes a
    /// double rather than wrapping.
    #[test]
    fn an_integer_sum_leaves_the_integers_only_on_overflow() {
        assert!(matches!(
            fold(AggregateKind::Sum, &[Datum::Int(i64::MAX), Datum::Int(0)]),
            OwnedDatum::Int(i64::MAX)
        ));
        match fold(AggregateKind::Sum, &[Datum::Int(i64::MAX), Datum::Int(1)]) {
            OwnedDatum::Real(number) => assert_eq!(number, i64::MAX as f64 + 1.0),
            other => panic!("overflowing sum was {other:?}"),
        }
    }

    /// A single real in the input makes the whole sum real.
    #[test]
    fn one_real_makes_the_sum_real() {
        match fold(
            AggregateKind::Sum,
            &[Datum::Int(1), Datum::Real(0.5), Datum::Int(2)],
        ) {
            OwnedDatum::Real(number) => assert_eq!(number, 3.5),
            other => panic!("mixed sum was {other:?}"),
        }
    }

    /// `min` and `max` order by the dialect's class order, not by class.
    #[test]
    fn extremes_use_the_dialect_ordering() {
        let values = [
            Datum::Text(b"apple"),
            Datum::Int(5),
            Datum::Blob(&[0]),
            Datum::Real(-1.0),
        ];
        match fold(AggregateKind::Minimum, &values) {
            OwnedDatum::Real(number) => assert_eq!(number, -1.0),
            other => panic!("min was {other:?}"),
        }
        match fold(AggregateKind::Maximum, &values) {
            OwnedDatum::Blob(bytes) => assert_eq!(bytes, vec![0]),
            other => panic!("max was {other:?}"),
        }
    }

    /// `group_concat` joins with its separator and skips NULLs.
    #[test]
    fn group_concat_joins_non_nulls() {
        match fold(
            AggregateKind::GroupConcat("-".into()),
            &[Datum::Int(1), Datum::Null, Datum::Text(b"x")],
        ) {
            OwnedDatum::Text(bytes) => assert_eq!(bytes, b"1-x".to_vec()),
            other => panic!("group_concat was {other:?}"),
        }
    }

    /// The vectorised path agrees with the per-row path on every aggregate,
    /// over runs that do and do not overflow.
    ///
    /// This is what licenses the fast path to exist: it is only a speed change.
    #[test]
    fn the_dense_path_agrees_with_the_per_row_path() {
        let runs: [Vec<i64>; 5] = [
            vec![],
            vec![7],
            (0..1000).collect(),
            vec![i64::MAX, 1, 1],
            vec![-5, 0, 5, i64::MIN + 1],
        ];
        let kinds = [
            AggregateKind::CountStar,
            AggregateKind::Count,
            AggregateKind::Sum,
            AggregateKind::Total,
            AggregateKind::Average,
            AggregateKind::Minimum,
            AggregateKind::Maximum,
            AggregateKind::GroupConcat(",".into()),
        ];
        for run in &runs {
            let bytes: Vec<u8> = run.iter().flat_map(|value| value.to_le_bytes()).collect();
            for kind in &kinds {
                let mut per_row = Accumulator::new(kind.clone());
                for value in run {
                    per_row.push(&Datum::Int(*value));
                }
                let mut dense = Accumulator::new(kind.clone());
                dense.push_dense_ints(&bytes);
                let a = per_row.finish().unwrap();
                let b = dense.finish().unwrap();
                assert_eq!(
                    a.borrow().compare(&b.borrow()),
                    std::cmp::Ordering::Equal,
                    "{kind:?} over {run:?}: per-row {a:?}, dense {b:?}"
                );
                assert_eq!(
                    matches!(a, OwnedDatum::Null),
                    matches!(b, OwnedDatum::Null),
                    "{kind:?} over {run:?}"
                );
            }
        }
    }

    /// The dense path folds several batches into one answer, which is what a
    /// multi-leaf scan does.
    #[test]
    fn the_dense_path_accumulates_across_batches() {
        let mut dense = Accumulator::new(AggregateKind::Sum);
        let mut per_row = Accumulator::new(AggregateKind::Sum);
        for batch in 0..5i64 {
            let run: Vec<i64> = (0..100).map(|n| batch * 100 + n).collect();
            let bytes: Vec<u8> = run.iter().flat_map(|value| value.to_le_bytes()).collect();
            dense.push_dense_ints(&bytes);
            for value in &run {
                per_row.push(&Datum::Int(*value));
            }
        }
        assert_eq!(
            dense
                .finish()
                .unwrap()
                .borrow()
                .compare(&per_row.finish().unwrap().borrow()),
            std::cmp::Ordering::Equal
        );
        assert!(matches!(dense.finish().unwrap(), OwnedDatum::Int(124_750)));
    }
}
