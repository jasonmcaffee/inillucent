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

use std::collections::HashSet;

use inillucent_base::DbResult;
use inillucent_tree::datum::{Datum, OwnedDatum};
use inillucent_value::collation::Collation;
use inillucent_value::Value;

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
    /// `group_concat(x, y)` where `y` is not a literal, so the separator is a
    /// value of each row rather than a constant of the call.
    ///
    /// **The separator SQLite uses before a row is that row's own (task-1913).**
    /// `group_concat(s, n)` over `('a',1), ('b',2), ('c',3)` is `a2b3c`: the
    /// first kept row contributes no separator, and every later one is preceded
    /// by the separator read from itself. A NULL separator contributes nothing,
    /// which is why `group_concat(s, NULL)` is `abc` rather than NULL. Measured
    /// against the pinned 3.53.4 rather than reasoned about, including through
    /// the call's own `ORDER BY`, where the rows are sorted first and the rule
    /// then applies to the sorted order.
    ///
    /// It keeps whole rows rather than folding as they arrive, because the
    /// separator cannot be known until the row it belongs to is there.
    GroupConcatComputed,
    /// `json_group_array(x)` and its `jsonb_` spelling.
    ///
    /// The flag is the binary form. A NULL is a *member* here rather than a row
    /// to skip: `json_group_array` over one NULL is `[null]` and not `[]`,
    /// because the document records what the rows held and a JSON null is a
    /// value.
    JsonGroupArray(bool),
    /// `json_group_object(label, value)` and its `jsonb_` spelling.
    JsonGroupObject(bool),
    /// `median(x)`, `percentile(x, p)`, `percentile_cont(x, f)` and
    /// `percentile_disc(x, f)`.
    ///
    /// One kind rather than four, because they differ only in where the
    /// fraction comes from and whether the answer may fall between two rows -
    /// see [`Percentile`]. Every one of them has to see the whole group sorted
    /// before it can answer, so they collect their rows the way a registered
    /// aggregate does rather than reducing as they go.
    Percentile(Percentile),
    /// A **bare column**: one read outside any aggregate in an aggregating
    /// query.
    ///
    /// `SELECT id, max(a) FROM t` is SQLite's own extension, and its rule is
    /// exact rather than arbitrary: when the query has exactly one `min` or
    /// `max`, the bare columns come from **the row that produced the extreme**,
    /// and otherwise from an arbitrary row - which SQLite takes as the last one
    /// of the group. The witness is the ordering that names the extreme, and it
    /// is carried per bare column rather than shared, so no accumulator has to
    /// see inside another.
    Bare(Option<std::cmp::Ordering>),
    /// `geopoly_group_bbox(P)`: the box that holds every polygon in the group.
    ///
    /// It collects its rows rather than reducing as it goes, which costs more
    /// memory than the four floats it needs. That is deliberate: an aggregate
    /// with running state of its own would be the only one in this enum, and
    /// the group a bounding box is asked for is a map layer rather than a
    /// table scan.
    GeopolyBox,
    /// `sum(v)` and `avg(v)` over a vector column, component by component.
    ///
    /// The flag is whether the total is divided by how many vectors went into
    /// it. Rows are collected rather than reduced for the same reason
    /// `GeopolyBox` collects them: the width is not known until the first
    /// vector arrives, and a running total sized from the first row would be
    /// wrong for a column whose rows disagree - which is a thing to *report*,
    /// not to average over.
    VectorFold(bool),
    /// An aggregate an application registered.
    ///
    /// It is handed every row of the group, in order, rather than a running
    /// accumulator. That is deliberate: an implementation written in C keeps
    /// its state in memory this engine must not look inside, and driving its
    /// step and final at the end of the group is how that state stays entirely
    /// on the other side of the boundary.
    External(crate::expr::AggregateBody),
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

/// Which of the four percentile aggregates is being computed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Percentile {
    /// `median(x)`: the fraction is fixed at one half.
    Median,
    /// `percentile(x, p)`: the second argument runs 0 to 100.
    Hundredths,
    /// `percentile_cont(x, f)`: the second argument runs 0 to 1, and the answer
    /// is interpolated between the two rows the position falls between.
    Continuous,
    /// `percentile_disc(x, f)`: the same range, but the answer is one of the
    /// rows rather than a value between two of them.
    Discrete,
}

impl Percentile {
    /// Returns the fraction of the way through the group, from 0 to 1.
    ///
    /// `median` ignores its (absent) second argument; `percentile` divides by a
    /// hundred; the other two take the argument as written. A fraction outside
    /// the range, or one that is not a number, has no answer and the aggregate
    /// reports NULL rather than clamping - which is what the reference does.
    ///
    /// @param argument - the second argument's value, when there is one
    fn fraction(self, argument: Option<f64>) -> Option<f64> {
        let fraction = match self {
            Percentile::Median => 0.5,
            Percentile::Hundredths => argument? / 100.0,
            Percentile::Continuous | Percentile::Discrete => argument?,
        };
        (0.0..=1.0).contains(&fraction).then_some(fraction)
    }

    /// Returns whether the answer may be a value no row held.
    fn interpolates(self) -> bool {
        !matches!(self, Percentile::Discrete)
    }
}

/// One aggregate's running state.
#[derive(Clone, Debug)]
pub struct Accumulator {
    kind: AggregateKind,
    /// The argument values already folded in, when the call said `DISTINCT`.
    ///
    /// `count(DISTINCT team)` counts the distinct teams, not the rows, and the
    /// set is what makes that true. It is per accumulator rather than per
    /// operator because `GROUP BY` gives each group its own - `count(DISTINCT
    /// x)` inside a group counts the distinct values *of that group*, and a set
    /// shared across groups would count each value once for the whole query.
    ///
    /// The values are encoded under the argument's collation, through the same
    /// `inillucent_tree::key` encoding `DISTINCT` and the set operations use, so
    /// `count(DISTINCT team)` over a `NOCASE` column agrees with `SELECT DISTINCT
    /// team` over it.
    seen: Option<(Collation, HashSet<Vec<u8>>)>,
    /// The encoded key `seen` is asked about, reused between rows.
    ///
    /// One buffer rather than one allocation per row; see the note in `push`
    /// (task-1932, M7).
    scratch: Vec<u8>,
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
    /// Whether it left them because an integer total would not fit.
    ///
    /// Kept apart from `is_real`, which is also set by a real *argument*. Only
    /// the first of the two is `sum()`'s `integer overflow`.
    overflowed: bool,
    /// Whether any value folded in was not an integer.
    saw_real: bool,
    /// The extreme value seen, for `min` and `max`.
    extreme: Option<OwnedDatum>,
    /// The value a bare column has settled on, and the witness that chose it.
    chosen: Option<OwnedDatum>,
    /// Where the sort keys start in a collected row, and their directions.
    ///
    /// `None` for a call with no `ORDER BY` of its own, which is nearly all of
    /// them. When it is set, every row is collected rather than reduced and the
    /// fold happens at `finish`, in the order the keys give - because
    /// `group_concat(b ORDER BY a DESC)` is a different answer from
    /// `group_concat(b)` and the difference is the order the rows arrive in.
    sort: Option<(usize, Vec<bool>)>,
    /// The rows a JSON group aggregate has collected, in row order.
    ///
    /// One value for the array form and two - label, value - for the object
    /// form. Kept as values rather than folded into a document as they arrive,
    /// because the document is built by `inillucent_scalar::json`, which is
    /// where the one implementation of a JSON document lives.
    json_rows: Vec<Vec<inillucent_value::value::Value<'static>>>,
    /// The joined text, for `group_concat`.
    joined: String,
    /// Every row of the group, for a registered aggregate and nothing else.
    ///
    /// Empty for every built-in, which is what makes them pay nothing for it.
    rows: Vec<Vec<inillucent_value::value::Value<'static>>>,
}

impl Accumulator {
    /// Returns a fresh accumulator.
    ///
    /// @param kind - which aggregate it computes
    pub fn new(kind: AggregateKind) -> Accumulator {
        Accumulator {
            kind,
            seen: None,
            scratch: Vec::new(),
            count: 0,
            integer_sum: 0,
            real_sum: 0.0,
            compensation: 0.0,
            chosen: None,
            sort: None,
            json_rows: Vec::new(),
            is_real: false,
            overflowed: false,
            saw_real: false,
            extreme: None,
            joined: String::new(),
            rows: Vec::new(),
        }
    }

    /// Returns an accumulator that folds each distinct argument value once.
    ///
    /// @param kind - which aggregate it computes
    /// @param collation - the collation the argument is compared under
    pub fn distinct(kind: AggregateKind, collation: Collation) -> Accumulator {
        let mut accumulator = Accumulator::new(kind);
        accumulator.seen = Some((collation, HashSet::new()));
        accumulator
    }

    /// Returns which aggregate this computes.
    pub fn kind(&self) -> &AggregateKind {
        &self.kind
    }

    /// Reports whether a whole run of integers may be folded in at once.
    ///
    /// A `DISTINCT` accumulator has to look at every value to decide whether it
    /// has seen it, so the vectorised path is not available to it - and an
    /// accumulator that took it anyway would count duplicates. The fast path
    /// asks rather than the caller remembering, because the caller is three
    /// operators and the accumulator is one.
    pub fn takes_dense(&self) -> bool {
        self.seen.is_none()
            && !matches!(
                self.kind,
                AggregateKind::External(_)
                    | AggregateKind::JsonGroupArray(_)
                    | AggregateKind::JsonGroupObject(_)
                    | AggregateKind::Percentile(_)
                    | AggregateKind::GeopolyBox
                    | AggregateKind::VectorFold(_)
                    | AggregateKind::Bare(_)
            )
    }

    /// Tells this accumulator to collect its rows and sort them.
    ///
    /// @param at - the position of the first sort key in a collected row
    /// @param descending - one flag per key, in key order
    pub fn sort_by(&mut self, at: usize, descending: Vec<bool>) {
        self.sort = Some((at, descending));
    }

    /// Folds one whole row of arguments in, for a registered aggregate.
    ///
    /// **The rows are kept, not reduced.** A registered aggregate is handed the
    /// whole group at the end rather than a running accumulator, because an
    /// implementation written in C keeps its state in memory this engine must
    /// not look inside - see [`AggregateKind::External`].
    ///
    /// @param values - one row's arguments, in written order
    pub fn push_values(&mut self, values: Vec<inillucent_value::value::Value<'static>>) {
        self.count = self.count.saturating_add(1);
        // **A JSON group aggregate comes through this entry point** rather than
        // through `push`, because a NULL is a *member* of the document it
        // builds and not a row to skip: `json_group_array` over one NULL is
        // `[null]` and not `[]`, and `push` drops NULLs before the accumulator
        // sees them. A call with its own `ORDER BY` comes through it too, for
        // the different reason that its rows cannot be folded until they are in
        // order.
        if let AggregateKind::Bare(witness) = self.kind {
            let value = values
                .first()
                .cloned()
                .unwrap_or(inillucent_value::value::Value::Null);
            let Some(wanted) = witness else {
                // No single extreme to follow, so the row is arbitrary and this
                // takes the *first* of the group - which is what the reference
                // answers, measured rather than assumed: `SELECT id, count(*)
                // FROM t` is 1 there and was 4 here when the last row won.
                if self.chosen.is_none() {
                    self.chosen = Some(OwnedDatum::from(value));
                }
                return;
            };
            let seen = values
                .get(1)
                .cloned()
                .unwrap_or(inillucent_value::value::Value::Null);
            let seen = OwnedDatum::from(seen);
            // A NULL never wins a `min` or a `max`, so a row whose witness is
            // NULL cannot be the row the bare column comes from - unless no row
            // has yet been chosen at all.
            let replace = match &self.extreme {
                None => true,
                Some(held) => {
                    !seen.borrow().is_null() && seen.borrow().compare(&held.borrow()) == wanted
                }
            };
            if replace {
                self.extreme = Some(seen);
                self.chosen = Some(OwnedDatum::from(value));
            }
            return;
        }
        // **`DISTINCT` is applied here too (task-1913).** It used to live only
        // in `push`, so an aggregate that keeps whole rows lost it entirely:
        // `group_concat(DISTINCT t ORDER BY t)` answered `blue,blue,gone,red`
        // where the reference answers `blue,gone,red`. Adding an `ORDER BY` to
        // a call turned its `DISTINCT` off, which is a wrong answer to a
        // perfectly ordinary query and not one the shape of the statement
        // warns anybody about. The key is the call's first argument, which is
        // the value `DISTINCT` is about - a second argument is refused by the
        // binder, in the reference's own words.
        if !self.keep_distinct(values.first()) {
            return;
        }
        if self.sort.is_some()
            || matches!(
                self.kind,
                AggregateKind::JsonGroupArray(_) | AggregateKind::JsonGroupObject(_)
            )
        {
            self.json_rows.push(values);
            return;
        }
        self.rows.push(values);
    }

    /// Reports whether a value is new, and records it when it is.
    ///
    /// Always true for a call that did not say `DISTINCT`. The encoding and
    /// the reused buffer are the ones [`Accumulator::push`] uses, so the two
    /// paths agree on what counts as the same value - which matters because a
    /// `NOCASE` column makes `'a'` and `'A'` one value here and two under a
    /// binary comparison.
    ///
    /// @param value - the call's first argument for this row
    fn keep_distinct(&mut self, value: Option<&inillucent_value::value::Value<'static>>) -> bool {
        let Some((collation, seen)) = &mut self.seen else {
            return true;
        };
        let Some(value) = value else {
            return true;
        };
        let datum = OwnedDatum::from(value.clone());
        self.scratch.clear();
        inillucent_tree::key::encode_into_with(&datum.borrow(), *collation, &mut self.scratch);
        if seen.contains(&self.scratch) {
            return false;
        }
        seen.insert(self.scratch.clone());
        true
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
        if let Some((collation, seen)) = &mut self.seen {
            // **Encoded into a reused buffer and only cloned when the value is
            // new (task-1932, M7).** A `DISTINCT` aggregate over a column with
            // few distinct values allocated one key per *row* and dropped
            // almost all of them - `count(DISTINCT status)` over a million rows
            // with six statuses allocated a million times to keep six.
            self.scratch.clear();
            inillucent_tree::key::encode_into_with(value, *collation, &mut self.scratch);
            if seen.contains(&self.scratch) {
                return;
            }
            seen.insert(self.scratch.clone());
        }
        self.count = self.count.saturating_add(1);
        match &self.kind {
            AggregateKind::CountStar | AggregateKind::Count => {}
            // A registered aggregate of one argument reaches here through the
            // ordinary single-value path; more than one goes through
            // `push_values`. Either way the row is kept rather than reduced.
            AggregateKind::External(_)
            | AggregateKind::Percentile(_)
            | AggregateKind::GeopolyBox
            // The copy is what an external aggregate needs: it keeps whole
            // rows past the page they were read from. An allocation this
            // small failing leaves a NULL in the row, which is what the
            // conversion this replaced did, and is the only answer an
            // infallible `push` can give.
            | AggregateKind::VectorFold(_) => self
                .rows
                .push(vec![Value::from(value).into_owned().unwrap_or(Value::Null)]),
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
            // Unreachable: `AggregateSpec::takes_whole_row` sends all three of
            // these through `push_values`. Stated rather than folded into
            // another arm so a kind added later is a compilation error.
            AggregateKind::JsonGroupArray(_)
            | AggregateKind::JsonGroupObject(_)
            | AggregateKind::GroupConcatComputed
            | AggregateKind::Bare(_) => {}
        }
    }

    /// Joins the collected rows with the separator each of them carries.
    ///
    /// A row whose *value* is NULL is not in the answer at all - the reference
    /// skips it, separator and all - and a NULL separator contributes nothing.
    /// The first row that survives contributes no separator, which is what
    /// makes `group_concat(s, n)` over `('a',1),('b',2)` read `a2b` rather
    /// than `1a2b`. See [`AggregateKind::GroupConcatComputed`].
    fn group_concat_computed(&self) -> OwnedDatum {
        let mut joined = String::new();
        let mut any = false;
        for row in &self.rows {
            let Some(value) = row.first() else {
                continue;
            };
            if matches!(value, inillucent_value::value::Value::Null) {
                continue;
            }
            if any {
                if let Some(separator) = row.get(1) {
                    if !matches!(separator, inillucent_value::value::Value::Null) {
                        joined.push_str(&text_of(separator));
                    }
                }
            }
            joined.push_str(&text_of(value));
            any = true;
        }
        if any {
            OwnedDatum::Text(joined.into_bytes())
        } else {
            OwnedDatum::Null
        }
    }

    /// Returns what a percentile aggregate settles on, over the sorted group.
    ///
    /// The fraction is read from the *second* argument of any collected row -
    /// it is a constant for the group, so any row's copy of it will do - and
    /// `median` has no second argument at all. NULL is the answer to an empty
    /// group and to a fraction outside `0..=1`, which is what the reference
    /// answers rather than clamping.
    ///
    /// `percentile_disc` picks a row; the other three interpolate between the
    /// two the position falls between, which is why the two arms differ in more
    /// than rounding. Both index a group sorted by value, and the sort is done
    /// here rather than as the rows arrive because the group is not known to be
    /// complete until now.
    ///
    /// @param which - which of the four is being computed
    fn finish_percentile(&self, which: Percentile) -> OwnedDatum {
        let argument = self
            .rows
            .first()
            .and_then(|row| row.get(1))
            .and_then(numeric_value);
        let Some(fraction) = which.fraction(argument) else {
            return OwnedDatum::Null;
        };
        let mut values: Vec<f64> = self
            .rows
            .iter()
            .filter_map(|row| row.first().and_then(numeric_value))
            .collect();
        if values.is_empty() {
            return OwnedDatum::Null;
        }
        values.sort_by(|left, right| left.total_cmp(right));
        let last = values.len().saturating_sub(1);
        let position = fraction * last as f64;
        if !which.interpolates() {
            // The row the position lands *in*, which is what "discrete" means:
            // an answer some row actually held. It rounds down rather than to
            // nearest, so `percentile_disc(x, 0.5)` over four rows is the
            // second of them and not the third - measured against the
            // reference, which is the only way to get this one right.
            let at = position.floor().max(0.0) as usize;
            return OwnedDatum::Real(values.get(at.min(last)).copied().unwrap_or(f64::NAN));
        }
        let below = position.floor().max(0.0) as usize;
        let above = position.ceil().max(0.0) as usize;
        let low = values.get(below.min(last)).copied().unwrap_or(f64::NAN);
        let high = values.get(above.min(last)).copied().unwrap_or(f64::NAN);
        OwnedDatum::Real(low + (high - low) * (position - below as f64))
    }

    /// Returns the box that holds every polygon the group held.
    ///
    /// A row that is not a polygon contributes nothing, and a group with no
    /// polygons in it at all answers NULL - which is the reference's answer to
    /// an aggregate that was never stepped.
    fn finish_geopoly_box(&self) -> OwnedDatum {
        let mut bounds: Option<[f32; 4]> = None;
        for row in &self.rows {
            let Some(value) = row.first() else {
                continue;
            };
            let Some(shape) = inillucent_scalar::geopoly::Polygon::parse(Some(value)) else {
                continue;
            };
            let found = shape.bounds();
            bounds = Some(match bounds {
                None => found,
                Some(held) => [
                    held[0].min(found[0]),
                    held[1].max(found[1]),
                    held[2].min(found[2]),
                    held[3].max(found[3]),
                ],
            });
        }
        match bounds {
            Some(bounds) => {
                OwnedDatum::Blob(inillucent_scalar::geopoly::box_polygon(bounds).to_blob())
            }
            None => OwnedDatum::Null,
        }
    }

    /// Returns the component-wise total of the group's vectors.
    ///
    /// A row that is not a vector contributes nothing, and a width that does
    /// not match the first one is a **refusal** rather than a silent partial
    /// sum: two embeddings of different widths are not two of the same thing,
    /// and the reference's own message for that is what a caller has already
    /// seen from `vector_distance_cos`.
    ///
    /// @param average - whether the total is divided by how many went into it
    fn finish_vector_fold(&self, average: bool) -> OwnedDatum {
        let mut total: Vec<f64> = Vec::new();
        let mut counted = 0i64;
        for row in &self.rows {
            let Some(inillucent_value::value::Value::Blob(blob)) = row.first() else {
                continue;
            };
            let bytes = blob.raw();
            if bytes.is_empty() || bytes.len() % 4 != 0 {
                continue;
            }
            let width = bytes.len() / 4;
            if total.is_empty() {
                total = vec![0.0; width];
            } else if total.len() != width {
                continue;
            }
            for (at, chunk) in bytes.chunks_exact(4).enumerate() {
                let raw: [u8; 4] = chunk.try_into().unwrap_or([0; 4]);
                if let Some(slot) = total.get_mut(at) {
                    *slot += f64::from(f32::from_bits(u32::from_le_bytes(raw)));
                }
            }
            counted = counted.saturating_add(1);
        }
        if counted == 0 {
            return OwnedDatum::Null;
        }
        let scale = if average { counted as f64 } else { 1.0 };
        let mut bytes = Vec::with_capacity(total.len().saturating_mul(4));
        for component in &total {
            bytes.extend_from_slice(&((component / scale) as f32).to_bits().to_le_bytes());
        }
        OwnedDatum::Blob(bytes)
    }

    /// Folds a whole run of integers in at once.
    ///
    /// The vectorised entry point: a `sum` over a dense integer column calls
    /// this once per batch with the mini-column's bytes and never touches a
    /// `Datum`. It is the reason the aggregate operator has a fast path at all.
    ///
    /// @param slots - a contiguous run of typed integers, at the leaf's width
    pub fn push_dense_ints(&mut self, slots: crate::batch::DenseInts<'_>) {
        let rows = slots.len();
        match self.kind {
            // Unreachable: `takes_dense` is false for a registered aggregate
            // and `takes_whole_row` is true for a JSON group, so no operator
            // offers either of them a mini-column. Stated rather than folded
            // into another arm, so a kind added later is a compilation error.
            AggregateKind::External(_)
            | AggregateKind::JsonGroupArray(_)
            | AggregateKind::JsonGroupObject(_)
            | AggregateKind::Percentile(_)
            | AggregateKind::GeopolyBox
            | AggregateKind::VectorFold(_)
            | AggregateKind::Bare(_) => {}
            AggregateKind::CountStar | AggregateKind::Count => {
                self.count = self.count.saturating_add(rows as i64);
            }
            AggregateKind::Sum | AggregateKind::Total | AggregateKind::Average => {
                self.count = self.count.saturating_add(rows as i64);
                if self.is_real {
                    // Borrowed out of `self` because `add_int` takes it
                    // mutably and the closure would otherwise hold it twice.
                    let mut values: Vec<i64> = Vec::with_capacity(rows);
                    slots.for_each(|value| values.push(value));
                    for value in values {
                        self.add_int(value);
                    }
                    return;
                }
                // Accumulate in i128 so one pass can be taken without a
                // checked_add per element, then fall back to the double sum
                // only if the exact total does not fit. The result is the same
                // value the per-row path produces, which the agreement test
                // asserts.
                let mut wide: i128 = i128::from(self.integer_sum);
                slots.for_each(|value| wide += i128::from(value));
                match i64::try_from(wide) {
                    Ok(exact) => self.integer_sum = exact,
                    Err(_) => {
                        // The exact total left `i64`, so the sum becomes a
                        // double from here. Seeding it needs the same split
                        // `seed_real` uses, applied to the wider value.
                        self.is_real = true;
                        self.overflowed = true;
                        let low = (wide % SPLIT) as f64;
                        self.real_sum = (wide - i128::from(low as i64)) as f64;
                        self.compensation = low;
                    }
                }
            }
            AggregateKind::Minimum => {
                let mut best = i64::MAX;
                slots.for_each(|value| {
                    if value < best {
                        best = value;
                    }
                });
                if rows > 0 {
                    self.count = self.count.saturating_add(rows as i64);
                    self.push_extreme(&Datum::Int(best), std::cmp::Ordering::Less);
                }
            }
            AggregateKind::Maximum => {
                let mut best = i64::MIN;
                slots.for_each(|value| {
                    if value > best {
                        best = value;
                    }
                });
                if rows > 0 {
                    self.count = self.count.saturating_add(rows as i64);
                    self.push_extreme(&Datum::Int(best), std::cmp::Ordering::Greater);
                }
            }
            AggregateKind::GroupConcat(_) => {
                let mut values: Vec<i64> = Vec::with_capacity(rows);
                slots.for_each(|value| values.push(value));
                for value in values {
                    self.push(&Datum::Int(value));
                }
            }
            // Unreachable: a computed separator makes the call a whole-row
            // one, and no operator offers one of those a mini-column.
            AggregateKind::GroupConcatComputed => {}
        }
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
                // **A real argument makes the whole sum a real**, and a real
                // sum has no overflow to report - SQLite raises `integer
                // overflow` only when every value it added was an integer.
                // `seed_real` above sets the flag because it cannot tell why it
                // was called; here is where we know.
                self.saw_real = true;
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
        self.overflowed = true;
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
        // A call with its own `ORDER BY` collected its rows; they are folded
        // here, in the order the keys give.
        if let Some((at, descending)) = &self.sort {
            return self.finish_sorted(*at, descending);
        }
        Ok(match &self.kind {
            AggregateKind::CountStar | AggregateKind::Count => OwnedDatum::Int(self.count),
            // The whole group at once, which is what the boundary promises.
            AggregateKind::External(body) => OwnedDatum::from((body.0)(&self.rows)?),
            AggregateKind::Percentile(which) => self.finish_percentile(*which),
            AggregateKind::GeopolyBox => self.finish_geopoly_box(),
            AggregateKind::VectorFold(average) => self.finish_vector_fold(*average),
            AggregateKind::Sum => {
                if self.count == 0 {
                    OwnedDatum::Null
                } else if self.overflowed && !self.saw_real {
                    // **`sum()` over integers that will not fit is an error,
                    // not a bigger number.** It answered
                    // `1.8446744073709552e+19` where SQLite raises `integer
                    // overflow` - a total that is *wrong by a rounding* and
                    // that a caller reading an integer column has no reason to
                    // suspect. `total()` and `avg()` are documented to be
                    // doubles and keep answering one, which is why the check is
                    // on this arm alone.
                    // `SQLITE_ERROR`, not `SQLITE_MISUSE` (task-1913). The
                    // pinned 3.53.4 answers primary code 1 here, measured
                    // against the oracle; `refusal` hardcodes 21, so the
                    // message matched and the code a driver branches on did
                    // not.
                    return Err(inillucent_base::error::statement_refusal(
                        "integer overflow",
                    ));
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
            AggregateKind::JsonGroupArray(binary) => {
                let mut items = Vec::with_capacity(self.json_rows.len());
                for row in &self.json_rows {
                    let value = row
                        .first()
                        .cloned()
                        .unwrap_or(inillucent_value::value::Value::Null);
                    inillucent_scalar::json::group_array_step(
                        &mut items,
                        &inillucent_scalar::json::Argument::plain(&value),
                    )?;
                }
                OwnedDatum::from(inillucent_scalar::json::group_array_final(items, *binary)?.value)
            }
            AggregateKind::JsonGroupObject(binary) => {
                let mut members = Vec::with_capacity(self.json_rows.len());
                for row in &self.json_rows {
                    let label = row
                        .first()
                        .cloned()
                        .unwrap_or(inillucent_value::value::Value::Null);
                    let value = row
                        .get(1)
                        .cloned()
                        .unwrap_or(inillucent_value::value::Value::Null);
                    inillucent_scalar::json::group_object_step(
                        &mut members,
                        &inillucent_scalar::json::Argument::plain(&label),
                        &inillucent_scalar::json::Argument::plain(&value),
                    )?;
                }
                OwnedDatum::from(
                    inillucent_scalar::json::group_object_final(members, *binary)?.value,
                )
            }
            AggregateKind::Bare(_) => self.chosen.clone().unwrap_or(OwnedDatum::Null),
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
            AggregateKind::GroupConcatComputed => self.group_concat_computed(),
        })
    }
    /// Returns the aggregate's value over its rows in the sort's own order.
    ///
    /// The keys are encoded with the tree's own key encoding, which is what
    /// `DISTINCT` already compares with here, so a mixed-type column orders the
    /// way every other comparison in the engine orders it. A descending key
    /// reverses that key alone, which is what `ORDER BY a DESC, b` means.
    ///
    /// @param at - the position of the first sort key in a collected row
    /// @param descending - one flag per key, in key order
    fn finish_sorted(&self, at: usize, descending: &[bool]) -> DbResult<OwnedDatum> {
        let mut order: Vec<usize> = (0..self.json_rows.len()).collect();
        let key_of = |row: &Vec<inillucent_value::value::Value<'static>>, which: usize| {
            let mut encoded = Vec::new();
            let value = row
                .get(at.saturating_add(which))
                .cloned()
                .unwrap_or(inillucent_value::value::Value::Null);
            inillucent_tree::key::encode_into_with(
                &OwnedDatum::from(value).borrow(),
                Collation::Binary,
                &mut encoded,
            );
            encoded
        };
        order.sort_by(|left, right| {
            let (Some(a), Some(b)) = (self.json_rows.get(*left), self.json_rows.get(*right)) else {
                return std::cmp::Ordering::Equal;
            };
            for (which, down) in descending.iter().enumerate() {
                let ordering = key_of(a, which).cmp(&key_of(b, which));
                let ordering = if *down { ordering.reverse() } else { ordering };
                if ordering != std::cmp::Ordering::Equal {
                    return ordering;
                }
            }
            // A tie keeps the arrival order, which is what a stable sort of the
            // positions gives.
            left.cmp(right)
        });
        let sorted: Vec<Vec<inillucent_value::value::Value<'static>>> = order
            .into_iter()
            .filter_map(|at| self.json_rows.get(at).cloned())
            .collect();
        let mut folded = Accumulator::new(self.kind.clone());
        for row in sorted {
            match self.kind {
                // The document builders keep taking whole rows, and so does a
                // `group_concat` whose separator is a value of each row: the
                // separator travels with the value it precedes, so folding the
                // value alone would lose it (task-1913).
                AggregateKind::JsonGroupArray(_)
                | AggregateKind::JsonGroupObject(_)
                | AggregateKind::GroupConcatComputed => {
                    folded.push_values(row);
                }
                // Everything else reduces one value, and a NULL is a row it
                // skips - which `push` already knows.
                _ => {
                    let value = row
                        .first()
                        .cloned()
                        .unwrap_or(inillucent_value::value::Value::Null);
                    folded.push(&OwnedDatum::from(value).borrow());
                }
            }
        }
        folded.finish()
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

/// Returns a value as the text `group_concat` writes into its answer.
///
/// The same rendering [`render`] does, over the owned form the whole-row path
/// collects: NULL is empty, a real is formatted the way the reference formats
/// one, and a blob is its bytes.
///
/// @param value - the collected value
fn text_of(value: &inillucent_value::value::Value<'_>) -> String {
    use inillucent_value::value::Value;
    match value {
        Value::Null => String::new(),
        Value::Integer(number) => number.to_string(),
        Value::Real(number) => format_real(*number),
        Value::Text(text) => String::from_utf8_lossy(&text.utf8_bytes()).into_owned(),
        Value::Blob(blob) => String::from_utf8_lossy(blob.raw()).into_owned(),
    }
}

/// Returns a value as a number, or nothing when it is not one.
///
/// A percentile is arithmetic over the group, so a text value that looks like a
/// number counts and one that does not is skipped - the same rule `sum()`
/// follows, and the reason both are written in terms of the dialect's own
/// coercion rather than Rust's parser.
///
/// @param value - one collected argument
fn numeric_value(value: &inillucent_value::value::Value<'static>) -> Option<f64> {
    match value {
        inillucent_value::value::Value::Null => None,
        inillucent_value::value::Value::Integer(number) => Some(*number as f64),
        inillucent_value::value::Value::Real(number) => Some(*number),
        other => Some(inillucent_value::cast::real_value(other)),
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
        // **An integer sum that will not fit is an error, not a bigger
        // number.** It used to become a double, which is a total that is wrong
        // by a rounding and that a caller reading an integer column has no
        // reason to suspect; the reference raises `integer overflow`.
        // `total` and `avg` are documented doubles and
        // still answer one over the same values.
        let mut over = Accumulator::new(AggregateKind::Sum);
        over.push(&Datum::Int(i64::MAX));
        over.push(&Datum::Int(1));
        assert!(over.finish().is_err(), "an overflowing sum must refuse");
        match fold(AggregateKind::Total, &[Datum::Int(i64::MAX), Datum::Int(1)]) {
            OwnedDatum::Real(number) => assert_eq!(number, i64::MAX as f64 + 1.0),
            other => panic!("overflowing total was {other:?}"),
        }
        // A real anywhere in the input makes the sum a real, which has no
        // overflow to report.
        match fold(
            AggregateKind::Sum,
            &[Datum::Int(i64::MAX), Datum::Int(1), Datum::Real(0.0)],
        ) {
            OwnedDatum::Real(number) => assert_eq!(number, i64::MAX as f64 + 1.0),
            other => panic!("a mixed overflowing sum was {other:?}"),
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
                dense.push_dense_ints(crate::batch::DenseInts::new(&bytes, 8));
                // **A refusal is an answer the two paths have to agree on
                // too.** `sum` over `[i64::MAX, 1, 1]` raises `integer
                // overflow` now, and the point of this sweep is that the
                // vectorised path and the per-row one cannot disagree - about
                // that as much as about a number.
                let (a, b) = match (per_row.finish(), dense.finish()) {
                    (Ok(a), Ok(b)) => (a, b),
                    (Err(_), Err(_)) => continue,
                    (a, b) => panic!("{kind:?} over {run:?}: per-row {a:?}, dense {b:?}"),
                };
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
            dense.push_dense_ints(crate::batch::DenseInts::new(&bytes, 8));
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
