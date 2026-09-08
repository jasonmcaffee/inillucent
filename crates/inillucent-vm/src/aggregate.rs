//! Aggregate accumulators.
//!
//! Invariant: an aggregate over no rows returns what SQLite returns, which is
//! not the same answer for each of them. `count` is 0, `sum` is NULL, `total`
//! is 0.0, `avg`, `min` and `max` are NULL, and `group_concat` is NULL. Those
//! five different answers to "there were no rows" are the part of aggregation
//! that is actually hard to get right, and each has a case in the tests.
//!
//! `sum` has a second rule that is easy to miss: it returns an integer while
//! every input is an integer and the running total has not overflowed, and a
//! real as soon as either stops being true. `total` always returns a real.
//!
//! The floating-point total is accumulated with Kahan-Babuska-Neumaier
//! compensation, as SQLite's is. That is not a refinement: adding 10.5, 1e300
//! and -1e300 in that order gives zero with a plain `+=`, because the 10.5
//! disappears into the exponent of the 1e300 and does not come back. The
//! compensation term is what carries it across.

use inillucent_base::DbResult;
use inillucent_ext::json::{self, Answer, Argument, Node};
use inillucent_sql::function::AggregateFunc;
use inillucent_value::{cast, compare, Collation, TextEncoding, Value};

use crate::eval;

/// One accumulator, mid-group.
///
/// An application's aggregate is the one that keeps its rows: see `rows`.
#[derive(Clone, Debug)]
pub struct Accumulator {
    func: AggregateFunc,
    distinct: bool,
    collation: Collation,
    count: i64,
    integer_sum: i64,
    real_sum: f64,
    compensation: f64,
    overflowed: bool,
    saw_real: bool,
    saw_value: bool,
    extreme: Option<Value<'static>>,
    joined: Vec<u8>,
    separator: Option<Vec<u8>>,
    seen: Vec<Value<'static>>,
    /// The name, when this is an aggregate an application registered.
    external: Option<Vec<u8>>,
    /// Every row of the group, kept for an application's aggregate.
    ///
    /// Only an external aggregate fills this: a built-in reduces as it goes,
    /// and an implementation on the other side of the C boundary keeps its own
    /// accumulator, so the rows are what has to be kept until the end.
    rows: Vec<Vec<Value<'static>>>,
    /// The elements a `json_group_array` has collected.
    json_items: Vec<Node>,
    /// The members a `json_group_object` has collected.
    json_members: Vec<(Node, Node)>,
}

impl Accumulator {
    /// Returns a fresh accumulator for one aggregate.
    pub fn new(func: AggregateFunc, distinct: bool, collation: Collation) -> Accumulator {
        Accumulator::named(func, None, distinct, collation)
    }

    /// Returns a fresh accumulator, naming an application's aggregate.
    pub fn named(
        func: AggregateFunc,
        external: Option<Vec<u8>>,
        distinct: bool,
        collation: Collation,
    ) -> Accumulator {
        Accumulator {
            func,
            external,
            rows: Vec::new(),
            distinct,
            collation,
            count: 0,
            integer_sum: 0,
            real_sum: 0.0,
            compensation: 0.0,
            overflowed: false,
            saw_real: false,
            saw_value: false,
            extreme: None,
            joined: Vec::new(),
            separator: None,
            seen: Vec::new(),
            json_items: Vec::new(),
            json_members: Vec::new(),
        }
    }

    /// Resets the accumulator, which starts a new group.
    pub fn reset(&mut self) {
        let fresh = Accumulator::new(self.func, self.distinct, self.collation);
        *self = fresh;
    }

    /// Feeds one row into the accumulator.
    ///
    /// `json_marks` says, per argument, whether the value carries the JSON
    /// mark; it is what makes `json_group_array(json('[1]'))` an array of
    /// arrays rather than an array of strings.
    pub fn step(
        &mut self,
        arguments: &[Value<'static>],
        json_marks: &[bool],
        encoding: TextEncoding,
    ) -> DbResult<()> {
        if self.is_json_group() {
            return self.json_step(arguments, json_marks);
        }
        let value = arguments.first().cloned().unwrap_or(Value::Null);
        if self.func != AggregateFunc::Count && value.is_null() {
            // Every aggregate but `count(*)` ignores NULL inputs entirely.
            return Ok(());
        }
        if self.distinct {
            if self
                .seen
                .iter()
                .any(|candidate| identical(candidate, &value, self.collation))
            {
                return Ok(());
            }
            self.seen.push(value.clone());
        }
        match self.func {
            // The percentile family collects its rows for the same reason a
            // registered aggregate does: none of them can answer until the
            // whole group is in and sorted.
            AggregateFunc::External
            | AggregateFunc::Median
            | AggregateFunc::Percentile
            | AggregateFunc::PercentileCont
            | AggregateFunc::PercentileDisc
            | AggregateFunc::GeopolyGroupBbox
            | AggregateFunc::VectorSum
            | AggregateFunc::VectorAvg => {
                self.count = self.count.saturating_add(1);
                self.rows.push(arguments.to_vec());
            }
            AggregateFunc::Count => {
                if arguments.is_empty() || !value.is_null() {
                    self.count = self.count.saturating_add(1);
                }
            }
            AggregateFunc::Sum | AggregateFunc::Total | AggregateFunc::Avg => {
                self.count = self.count.saturating_add(1);
                self.saw_value = true;
                match &value {
                    Value::Integer(integer) => match self.integer_sum.checked_add(*integer) {
                        Some(total) => self.integer_sum = total,
                        None => self.overflowed = true,
                    },
                    Value::Real(_) => self.saw_real = true,
                    _ => {
                        // Text and blobs are read as numbers, and a value that
                        // is not one counts as zero.
                        match cast::numerify(value.clone()) {
                            Value::Integer(integer) => {
                                match self.integer_sum.checked_add(integer) {
                                    Some(total) => self.integer_sum = total,
                                    None => self.overflowed = true,
                                }
                            }
                            _ => self.saw_real = true,
                        }
                    }
                }
                self.add_real(cast::real_value(&value));
            }
            AggregateFunc::Min | AggregateFunc::Max => {
                self.saw_value = true;
                let replace = match &self.extreme {
                    None => true,
                    Some(current) => {
                        let ordering = compare::compare_values(&value, current, self.collation);
                        if self.func == AggregateFunc::Max {
                            ordering == std::cmp::Ordering::Greater
                        } else {
                            ordering == std::cmp::Ordering::Less
                        }
                    }
                };
                if replace {
                    self.extreme = Some(value.clone());
                }
            }
            AggregateFunc::GroupConcat => {
                if let Some(separator) = arguments.get(1) {
                    self.separator = Some(eval::text_bytes(separator, encoding));
                }
                if self.saw_value {
                    let separator = self.separator.clone().unwrap_or_else(|| b",".to_vec());
                    self.joined.extend_from_slice(&separator);
                }
                self.saw_value = true;
                self.joined
                    .extend_from_slice(&eval::text_bytes(&value, encoding));
            }
            AggregateFunc::JsonGroupArray
            | AggregateFunc::JsonbGroupArray
            | AggregateFunc::JsonGroupObject
            | AggregateFunc::JsonbGroupObject => {}
        }
        Ok(())
    }

    /// Returns whether this accumulator builds a JSON document.
    fn is_json_group(&self) -> bool {
        matches!(
            self.func,
            AggregateFunc::JsonGroupArray
                | AggregateFunc::JsonbGroupArray
                | AggregateFunc::JsonGroupObject
                | AggregateFunc::JsonbGroupObject
        )
    }

    /// Feeds one row into a JSON group aggregate.
    ///
    /// A NULL is a member here rather than a row to skip: `json_group_array`
    /// over one NULL is `[null]` and not `[]`, because the document records
    /// what the rows held and a JSON null is a value.
    fn json_step(&mut self, arguments: &[Value<'static>], json_marks: &[bool]) -> DbResult<()> {
        let null = Value::Null;
        let mark = |index: usize| Argument {
            value: arguments.get(index).unwrap_or(&null),
            json: json_marks.get(index).copied().unwrap_or(false),
        };
        match self.func {
            AggregateFunc::JsonGroupArray | AggregateFunc::JsonbGroupArray => {
                json::group_array_step(&mut self.json_items, &mark(0))
            }
            _ => json::group_object_step(&mut self.json_members, &mark(0), &mark(1)),
        }
    }

    /// Adds one term to the compensated running total.
    ///
    /// This is Neumaier's variant of Kahan summation: it also handles the case
    /// where the new term is larger than the running total, which is exactly
    /// the case that loses 10.5 into 1e300.
    fn add_real(&mut self, term: f64) {
        let total = self.real_sum + term;
        if self.real_sum.abs() >= term.abs() {
            self.compensation += (self.real_sum - total) + term;
        } else {
            self.compensation += (term - total) + self.real_sum;
        }
        self.real_sum = total;
    }

    /// Returns the compensated total.
    fn total(&self) -> f64 {
        self.real_sum + self.compensation
    }

    /// Produces the aggregate's value for the group.
    ///
    /// The answer carries the JSON mark, because a group aggregate that has
    /// built a document has to hand that fact on to whatever consumes it.
    pub fn finish(&self) -> DbResult<Answer> {
        Ok(Answer {
            value: self.finish_value()?,
            json: self.is_json_group(),
        })
    }

    /// Returns the name of the application aggregate this accumulates for.
    pub fn external_name(&self) -> Option<&[u8]> {
        self.external.as_deref()
    }

    /// Returns every row the group collected, for an application's aggregate.
    pub fn group_rows(&self) -> &[Vec<Value<'static>>] {
        &self.rows
    }

    /// Returns what a percentile aggregate settles on, over the sorted group.
    ///
    /// The same arithmetic the vectorised executor's
    /// `Accumulator::finish_percentile` does, and it is written twice for the
    /// reason the crate documentation gives for aggregates generally: the two
    /// executors keep genuinely different state, and this one steps over
    /// `Value`s. The *rule* is stated once, in
    /// `inillucent_exec::aggregate::Percentile`, and the two implementations of
    /// it are checked against each other by the compatibility suite.
    fn finish_percentile(&self) -> Value<'static> {
        let hundredths = self.func == AggregateFunc::Percentile;
        let discrete = self.func == AggregateFunc::PercentileDisc;
        let fraction = if self.func == AggregateFunc::Median {
            0.5
        } else {
            let Some(given) = self
                .rows
                .first()
                .and_then(|row| row.get(1))
                .map(inillucent_value::cast::real_value)
            else {
                return Value::Null;
            };
            if hundredths {
                given / 100.0
            } else {
                given
            }
        };
        if !(0.0..=1.0).contains(&fraction) {
            return Value::Null;
        }
        let mut values: Vec<f64> = self
            .rows
            .iter()
            .filter_map(|row| row.first())
            .filter(|value| !value.is_null())
            .map(inillucent_value::cast::real_value)
            .collect();
        if values.is_empty() {
            return Value::Null;
        }
        values.sort_by(|left, right| left.total_cmp(right));
        let last = values.len().saturating_sub(1);
        let position = fraction * last as f64;
        if discrete {
            let at = (position.floor().max(0.0) as usize).min(last);
            return Value::Real(values.get(at).copied().unwrap_or(f64::NAN));
        }
        let below = (position.floor().max(0.0) as usize).min(last);
        let above = (position.ceil().max(0.0) as usize).min(last);
        let low = values.get(below).copied().unwrap_or(f64::NAN);
        let high = values.get(above).copied().unwrap_or(f64::NAN);
        Value::Real(low + (high - low) * (position - below as f64))
    }

    /// Returns the box that holds every polygon the group held.
    ///
    /// A row that is not a polygon contributes nothing, and a group with no
    /// polygons in it answers NULL - which is what an aggregate that was never
    /// stepped answers.
    fn finish_geopoly_box(&self) -> Value<'static> {
        let mut bounds: Option<[f32; 4]> = None;
        for row in &self.rows {
            let Some(shape) = inillucent_scalar::geopoly::Polygon::parse(row.first()) else {
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
                let blob = inillucent_scalar::geopoly::box_polygon(bounds).to_blob();
                Value::owned_blob(&blob).unwrap_or(Value::Null)
            }
            None => Value::Null,
        }
    }

    /// Produces the aggregate's value, without the mark.
    fn finish_value(&self) -> DbResult<Value<'static>> {
        Ok(match self.func {
            // The machine finishes an external aggregate itself, because only
            // it holds the table the name resolves in.
            AggregateFunc::External => Value::Null,
            AggregateFunc::Median
            | AggregateFunc::Percentile
            | AggregateFunc::PercentileCont
            | AggregateFunc::PercentileDisc => self.finish_percentile(),
            AggregateFunc::GeopolyGroupBbox => self.finish_geopoly_box(),
            // The legacy engine never plans a vector fold: the binder only
            // chooses one for a column declared `VECTOR(n)`, which is the new
            // engine's declaration. Named rather than folded into a catch-all
            // so that a kind added later is a compilation error here too.
            AggregateFunc::VectorSum | AggregateFunc::VectorAvg => Value::Null,
            AggregateFunc::Count => Value::Integer(self.count),
            AggregateFunc::Sum => {
                if !self.saw_value {
                    return Ok(Value::Null);
                }
                if self.saw_real || self.overflowed {
                    return Ok(Value::Real(self.total()));
                }
                Value::Integer(self.integer_sum)
            }
            AggregateFunc::Total => Value::Real(self.total()),
            AggregateFunc::Avg => {
                if !self.saw_value || self.count == 0 {
                    return Ok(Value::Null);
                }
                Value::Real(self.total() / self.count as f64)
            }
            AggregateFunc::Min | AggregateFunc::Max => self.extreme.clone().unwrap_or(Value::Null),
            AggregateFunc::GroupConcat => {
                if !self.saw_value {
                    return Ok(Value::Null);
                }
                Value::owned_text(&self.joined).unwrap_or(Value::Null)
            }
            AggregateFunc::JsonGroupArray | AggregateFunc::JsonbGroupArray => {
                json::group_array_final(
                    self.json_items.clone(),
                    self.func == AggregateFunc::JsonbGroupArray,
                )?
                .value
            }
            AggregateFunc::JsonGroupObject | AggregateFunc::JsonbGroupObject => {
                json::group_object_final(
                    self.json_members.clone(),
                    self.func == AggregateFunc::JsonbGroupObject,
                )?
                .value
            }
        })
    }
}

/// Returns whether two values are the same for `DISTINCT` purposes.
///
/// Two NULLs are the same here, which is what `count(DISTINCT x)` needs even
/// though `x = x` is NULL for a NULL.
fn identical(left: &Value<'_>, right: &Value<'_>, collation: Collation) -> bool {
    if left.is_null() || right.is_null() {
        return left.is_null() && right.is_null();
    }
    compare::compare_values(left, right, collation) == std::cmp::Ordering::Equal
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs an aggregate over a list of values.
    fn run(func: AggregateFunc, values: Vec<Value<'static>>) -> Value<'static> {
        let mut accumulator = Accumulator::new(func, false, Collation::Binary);
        for value in values {
            accumulator
                .step(&[value], &[false], TextEncoding::Utf8)
                .expect("the aggregate accepts the value");
        }
        accumulator.finish().expect("the aggregate finishes").value
    }

    /// The five different answers to "there were no rows".
    #[test]
    fn an_empty_group_gives_five_different_answers() {
        assert_same!(run(AggregateFunc::Count, Vec::new()), Value::Integer(0));
        assert_same!(run(AggregateFunc::Sum, Vec::new()), Value::Null);
        assert_same!(run(AggregateFunc::Total, Vec::new()), Value::Real(0.0));
        assert_same!(run(AggregateFunc::Avg, Vec::new()), Value::Null);
        assert_same!(run(AggregateFunc::Min, Vec::new()), Value::Null);
        assert_same!(run(AggregateFunc::GroupConcat, Vec::new()), Value::Null);
    }

    /// `sum` stays an integer until it cannot, and `total` never is one.
    #[test]
    fn sum_stays_integral_until_it_cannot() {
        assert_same!(
            run(
                AggregateFunc::Sum,
                vec![Value::Integer(1), Value::Integer(2)]
            ),
            Value::Integer(3)
        );
        assert_same!(
            run(
                AggregateFunc::Sum,
                vec![Value::Integer(1), Value::Real(0.5)]
            ),
            Value::Real(1.5)
        );
        assert_same!(
            run(
                AggregateFunc::Sum,
                vec![Value::Integer(i64::MAX), Value::Integer(1)]
            ),
            Value::Real(9.223372036854776e18)
        );
        assert_same!(
            run(AggregateFunc::Total, vec![Value::Integer(2)]),
            Value::Real(2.0)
        );
    }

    /// Summation is compensated, so a small term is not lost into a large
    /// one. A plain `+=` gives zero for this input.
    #[test]
    fn summation_is_compensated() {
        let sum = run(
            AggregateFunc::Total,
            vec![
                Value::Real(10.5),
                Value::Real(1e300),
                Value::Real(-1e300),
                Value::Real(2.5),
            ],
        );
        assert_same!(sum, Value::Real(13.0));
    }

    /// NULL inputs are ignored by every aggregate but `count(*)`.
    #[test]
    fn nulls_are_ignored() {
        assert_same!(
            run(
                AggregateFunc::Sum,
                vec![Value::Integer(1), Value::Null, Value::Integer(2)]
            ),
            Value::Integer(3)
        );
        assert_same!(
            run(
                AggregateFunc::Count,
                vec![Value::Integer(1), Value::Null, Value::Integer(2)]
            ),
            Value::Integer(2)
        );
        let mut star = Accumulator::new(AggregateFunc::Count, false, Collation::Binary);
        star.step(&[], &[], TextEncoding::Utf8)
            .expect("the aggregate accepts the row");
        star.step(&[], &[], TextEncoding::Utf8)
            .expect("the aggregate accepts the row");
        assert_same!(
            star.finish().expect("the aggregate finishes").value,
            Value::Integer(2)
        );
    }

    /// `DISTINCT` treats two NULLs as the same value, which ordinary equality
    /// would not.
    #[test]
    fn distinct_counts_nulls_once() {
        let mut accumulator = Accumulator::new(AggregateFunc::Count, true, Collation::Binary);
        for value in [
            Value::Integer(1),
            Value::Integer(1),
            Value::Integer(2),
            Value::Null,
        ] {
            accumulator
                .step(&[value], &[false], TextEncoding::Utf8)
                .expect("the aggregate accepts the value");
        }
        assert_same!(
            accumulator.finish().expect("the aggregate finishes").value,
            Value::Integer(2)
        );
    }

    /// `group_concat` joins with a comma unless told otherwise.
    #[test]
    fn group_concat_joins_with_a_comma() {
        let mut accumulator =
            Accumulator::new(AggregateFunc::GroupConcat, false, Collation::Binary);
        for value in [b"a".to_vec(), b"b".to_vec()] {
            accumulator
                .step(
                    &[Value::owned_text(&value).expect("owned")],
                    &[false],
                    TextEncoding::Utf8,
                )
                .expect("the aggregate accepts the value");
        }
        assert_same!(
            accumulator.finish().expect("the aggregate finishes").value,
            Value::owned_text(b"a,b").expect("owned")
        );

        let mut custom = Accumulator::new(AggregateFunc::GroupConcat, false, Collation::Binary);
        for value in [b"a".to_vec(), b"b".to_vec()] {
            custom
                .step(
                    &[
                        Value::owned_text(&value).expect("owned"),
                        Value::owned_text(b"-").expect("owned"),
                    ],
                    &[false, false],
                    TextEncoding::Utf8,
                )
                .expect("the aggregate accepts the value");
        }
        assert_same!(
            custom.finish().expect("the aggregate finishes").value,
            Value::owned_text(b"a-b").expect("owned")
        );
    }

    /// Resetting starts a genuinely new group.
    #[test]
    fn reset_starts_a_new_group() {
        let mut accumulator = Accumulator::new(AggregateFunc::Sum, false, Collation::Binary);
        accumulator
            .step(&[Value::Integer(5)], &[false], TextEncoding::Utf8)
            .expect("the aggregate accepts the value");
        accumulator.reset();
        assert_same!(
            accumulator.finish().expect("the aggregate finishes").value,
            Value::Null
        );
    }
}
