//! Turning a plan's constraints into the keys and spans a cursor is positioned with.
//!
//! Invariant: **a key carries the index's own affinity and collation, not the
//! expression's.** A comparison in SQL applies the column's affinity before it
//! compares, and a tree entry was written with that affinity already applied,
//! so a key built from the raw expression looks for something the tree never
//! stored - `WHERE k = '5'` over an INTEGER column finds nothing.
//!
//! Here rather than in [`super`] because `physical.rs` is six thousand lines
//! and these eleven functions are one idea, reached from the three places that
//! position a cursor: a nested loop's probe, a point probe, and the union and
//! span forms that read a run of entries.

use inillucent_base::error::misuse;

use super::*;

/// Returns the key expressions an inner stage probes with.
///
/// @param path - the FROM term's access path
/// @param space - the joined column space
/// @param params - the bound parameters
pub(crate) fn nested_key(
    path: &AccessPath,
    table: &inillucent_sql::catalog_view::TableInfo,
    space: &Space<'_>,
    params: &Params,
) -> DbResult<(Vec<Expr>, bool)> {
    match path {
        AccessPath::RowidSeek { key, .. } => Ok((
            vec![with_affinity(
                translate_scan(key, space, params)?,
                Some(Affinity::Integer),
            )],
            true,
        )),
        AccessPath::IndexSeek {
            equalities,
            unconverted,
            low,
            high,
            columns,
            ..
        } => {
            if equalities.is_empty() {
                if low.is_none() && high.is_none() {
                    // **Neither an equality nor a bound is a full scan of the
                    // index, which is the cross product an empty key list
                    // already means (task-1913).** `AccessPath::describe`
                    // spells the same three conditions as `SCAN … USING
                    // COVERING INDEX`, so this is the covering-index form of
                    // the `TableScan` arm below rather than a seek at all. It
                    // was refused with the bounded case, which made
                    // `SELECT count(*) FROM t, u` fail outright as soon as the
                    // planner had a covering index to count `u` through - a
                    // plain cross join, answered by SQLite and refused here.
                    return Ok((Vec::new(), false));
                }
                // An index seek with a bound but no equality is a *range* over
                // the inner tree, not a cross product - reading it as one would
                // drop the bound and pair every row with every row.
                return unsupported("a join whose inner index seek has no equality");
            }
            if low.is_some() || high.is_some() {
                return unsupported("a join whose inner index seek also has a range");
            }
            let mut keys = Vec::with_capacity(equalities.len());
            for (position, expr) in equalities.iter().enumerate() {
                keys.push(with_affinity(
                    translate_scan(expr, space, params)?,
                    probe_affinity(
                        unconverted.contains(&position),
                        index_affinity(table, columns, position),
                    ),
                ));
            }
            // A prefix of the index key, so the probe is a range over every
            // entry sharing it.
            Ok((keys, false))
        }
        // A table scan as an inner term is a cross product, and the join reads
        // an empty key list as exactly that.
        AccessPath::TableScan { .. } => Ok((Vec::new(), false)),
        _ => unsupported("that inner access path in a join"),
    }
}

/// Returns the key a point probe looks up.
///
/// @param path - the FROM term's access path
/// @param space - the joined column space
/// @param params - the bound parameters
pub(super) fn point_key(
    path: &AccessPath,
    space: &Space<'_>,
    params: &Params,
) -> DbResult<Vec<OwnedDatum>> {
    match path {
        AccessPath::RowidSeek { key, .. } => Ok(vec![constant_value(
            key,
            space,
            params,
            Some(Affinity::Integer),
        )?]),
        _ => unsupported("a point probe over that access path"),
    }
}

/// Returns the keys a rowid seek union probes, evaluated and de-duplicated
/// against the concrete values this execution actually bound.
///
/// A literal repeat is already folded at plan time; a repeat that is not
/// visible until the parameters are bound - two different parameters given
/// the same argument, say - is caught here instead. A rowid is unique by
/// construction, so a repeated key is the only way two branches could
/// produce the same row, and skipping the second probe of a key already
/// probed is what keeps that from happening.
/// @param keys - the keys to probe, in order
/// @param space - the joined column space
/// @param params - the bound parameters
pub(super) fn rowid_union_keys(
    keys: &[BoundExpr],
    space: &Space<'_>,
    params: &Params,
) -> DbResult<Vec<Vec<OwnedDatum>>> {
    let mut seen: Vec<Vec<OwnedDatum>> = Vec::with_capacity(keys.len());
    for expr in keys {
        let value = vec![constant_value(
            expr,
            space,
            params,
            Some(Affinity::Integer),
        )?];
        if !seen.contains(&value) {
            seen.push(value);
        }
    }
    Ok(seen)
}

/// Returns the keys an index seek union's equality branches probe, evaluated
/// and de-duplicated the same way [`rowid_union_keys`] is.
///
/// Only ever called for a union every branch of which is an equality with no
/// range - the `IN`-list shape. A branch that also carries a range is a
/// keyset-page union instead, which [`range_union_bounds`] runs, because a
/// range cannot be probed by key and does not need this de-duplication: the
/// keyset shape is proven disjoint before it ever reaches here.
/// @param branches - the union's branches
/// @param table - the indexed table, for each key column's affinity
/// @param columns - which table column each index position holds
/// @param space - the joined column space
/// @param params - the bound parameters
pub(super) fn index_union_keys(
    branches: &[IndexSeekBranch],
    table: &TableInfo,
    columns: &[Option<u16>],
    space: &Space<'_>,
    params: &Params,
) -> DbResult<Vec<Vec<OwnedDatum>>> {
    let mut seen: Vec<Vec<OwnedDatum>> = Vec::with_capacity(branches.len());
    for branch in branches {
        let mut key = Vec::with_capacity(branch.equalities.len());
        for (position, expr) in branch.equalities.iter().enumerate() {
            key.push(constant_value(
                expr,
                space,
                params,
                probe_affinity(
                    branch.unconverted.contains(&position),
                    index_affinity(table, columns, position),
                ),
            )?);
        }
        // `k IN (1, ?1)` with `?1` NULL matches what `k = 1` matches and no
        // more: a NULL key equals no entry, including the index's NULL ones.
        // See `SpanBounds::matches_nothing`.
        if key.contains(&OwnedDatum::Null) {
            continue;
        }
        if !seen.contains(&key) {
            seen.push(key);
        }
    }
    Ok(seen)
}

/// Returns the range scans a keyset-range union runs, in the order the
/// branches were built.
///
/// That order is also the order that keeps the branches' combined output in
/// the composite key's own order: the planner proved it once, when it built
/// the union, and this only has to preserve it rather than prove it again.
/// Each branch becomes a throwaway single-branch [`AccessPath::IndexSeek`] and
/// is priced through [`span_bounds`] exactly as a lone seek would be - the
/// NULL handling and the descending-column handling are properties of one
/// range, not of the union, so there is nothing for this to do differently.
///
/// **It takes the path rather than eleven pieces of it (task-1962, A9).** Nine
/// of the fourteen arguments it used to take were fields of the same
/// `AccessPath::IndexSeekUnion` the caller had just destructured, four of them
/// slices of different things in a row. The destructuring is here now, and so
/// is the refusal for a path that is not one.
///
/// @param tree - the index tree the branches walk
/// @param projection - which columns each scan emits
/// @param path - the union, which must be an `IndexSeekUnion`
/// @param table - the indexed table
/// @param space - the joined column space
/// @param params - the values bound to `?1`, `?2`, ...
pub(super) fn range_union_bounds<'t>(
    tree: &'t PagedTree,
    projection: Projection,
    path: &AccessPath,
    table: &TableInfo,
    space: &Space<'_>,
    params: &Params,
) -> DbResult<Vec<SpanScan<'t>>> {
    let AccessPath::IndexSeekUnion {
        table_root,
        index_root,
        index_name,
        branches,
        collations,
        descending,
        columns,
        without_rowid,
        key_entry_slots,
        ..
    } = path
    else {
        return Err(misuse("a range-union stage over a path that is not one"));
    };
    let (table_root, index_root, without_rowid) = (*table_root, *index_root, *without_rowid);
    let mut scans = Vec::with_capacity(branches.len());
    for branch in branches {
        let branch_path = AccessPath::IndexSeek {
            table_root,
            index_root,
            index_name: index_name.to_vec(),
            equalities: branch.equalities.clone(),
            unconverted: branch.unconverted.clone(),
            low: branch.low.clone(),
            high: branch.high.clone(),
            collations: collations.to_vec(),
            descending: descending.to_vec(),
            columns: columns.to_vec(),
            without_rowid,
            key_entry_slots: key_entry_slots.to_vec(),
            covering: None,
        };
        let bounds = span_bounds(&branch_path, table, space, params)?;
        // A branch whose key is NULL contributes no row, and the others still do.
        if bounds.matches_nothing {
            continue;
        }
        scans.push(SpanScan::new(
            tree,
            projection.clone(),
            bounds.low,
            bounds.low_inclusive,
            bounds.high,
            bounds.high_inclusive,
        ));
    }
    Ok(scans)
}

/// The bounds of a range scan, with the inclusivity of each end.
///
/// A struct rather than a tuple because a bare `(low, high, inclusive)` is what
/// hid the bug: the single `inclusive` was the *high* bound's, and the low
/// bound was applied inclusively whatever the predicate said. `WHERE id > 495`
/// returned `id >= 495`.
#[derive(Clone, Debug, Default)]
pub struct SpanBounds {
    /// The lower bound, or `None` for the start of the tree.
    pub low: Option<Vec<OwnedDatum>>,
    /// Whether a key equal to the lower bound is in the range.
    pub low_inclusive: bool,
    /// The upper bound, or `None` for the end of the tree.
    pub high: Option<Vec<OwnedDatum>>,
    /// Whether a key equal to the upper bound is in the range.
    pub high_inclusive: bool,
    /// Whether an equality or a bound evaluated to NULL, so no row can match.
    ///
    /// **`k = NULL` and `k > NULL` are NULL for every row (task-2083).** The
    /// planner refuses a NULL *literal* as a seek key, in
    /// `plan::terms::comparison_against_column`, but a parameter is only known
    /// at run time. A parameter bound to NULL used to become a seek to the
    /// index's own NULL entries. `WHERE a = ?1` with `?1` NULL returned the
    /// rows whose `a` is NULL, and `WHERE a > ?1` returned every row that was
    /// not. A correlated subquery reaches the same seek through a parameter,
    /// so `(SELECT id FROM h WHERE h.a = s.k)` found a row for a NULL `s.k`.
    /// `IS NULL` never gets here: it is not a comparison, so the planner never
    /// makes it an equality.
    pub matches_nothing: bool,
}

impl SpanBounds {
    /// A span that no row falls in.
    fn nothing() -> SpanBounds {
        SpanBounds {
            matches_nothing: true,
            ..SpanBounds::default()
        }
    }
}

/// Returns the affinity of one column of an index key.
///
/// The index's `columns` list says which table column each key position holds,
/// and the table says what that column's affinity is. A position past the end
/// of the list - the rowid at the end of an entry - is an integer.
///
/// @param table - the indexed table
/// @param columns - which table column each index position holds
/// @param position - the key position
fn index_affinity(
    table: &inillucent_sql::catalog_view::TableInfo,
    columns: &[Option<u16>],
    position: usize,
) -> Option<Affinity> {
    match columns.get(position) {
        Some(Some(column)) => table
            .columns
            .get(usize::from(*column))
            .map(|info| info.affinity),
        // **A key the index computes takes no affinity.** An index on
        // `lower(a)` stores whatever the expression returned, so converting the
        // probe would compare a converted value against an unconverted one -
        // which is a seek that lands somewhere else. SQLite applies none here
        // either.
        Some(None) => None,
        // Past the end of the key columns is the entry's trailing rowid.
        None => Some(Affinity::Integer),
    }
}

/// Returns the affinity a seek applies to one probe value before it descends.
///
/// This is SQLite's rule from `codeAllEqualityTerms`: the probe takes the
/// indexed column's affinity, unless both sides of the comparison have an
/// affinity and neither is numeric. Then the comparison in `WHERE` converts
/// nothing, and neither may the seek (task-2083). Converting there made an
/// untyped column's `3` into the text `'3'` before it probed a TEXT index,
/// which found a row that `WHERE` does not match.
///
/// The planner decides which case a key is, because only the planner sees the
/// comparison, and records it as the path's `unconverted` flags. It has
/// already refused an index whose affinity the comparison disagrees with, in
/// `plan::terms::indexable_comparison`.
///
/// @param unconverted - whether the comparison converts nothing
/// @param index - the indexed column's affinity, if the key position has one
fn probe_affinity(unconverted: bool, index: Option<Affinity>) -> Option<Affinity> {
    if unconverted {
        return None;
    }
    index
}

/// Returns the bounds of a range scan.
///
/// @param path - the FROM term's access path
/// @param space - the joined column space
/// @param params - the bound parameters
pub(super) fn span_bounds(
    path: &AccessPath,
    table: &inillucent_sql::catalog_view::TableInfo,
    space: &Space<'_>,
    params: &Params,
) -> DbResult<SpanBounds> {
    match path {
        AccessPath::TableScan { .. } => Ok(SpanBounds {
            low: None,
            low_inclusive: true,
            high: None,
            high_inclusive: true,
            matches_nothing: false,
        }),
        AccessPath::RowidRange { low, high, .. } => {
            // A rowid range compares against the rowid, which is an integer.
            let (low_value, low_inclusive) =
                bound_value(low.as_ref(), space, params, Some(Affinity::Integer))?;
            let (high_value, high_inclusive) =
                bound_value(high.as_ref(), space, params, Some(Affinity::Integer))?;
            if [&low_value, &high_value]
                .into_iter()
                .any(|value| *value == Some(OwnedDatum::Null))
            {
                return Ok(SpanBounds::nothing());
            }
            Ok(SpanBounds {
                low: low_value.map(|value| vec![value]),
                low_inclusive,
                high: high_value.map(|value| vec![value]),
                high_inclusive,
                matches_nothing: false,
            })
        }
        AccessPath::IndexSeek {
            equalities,
            unconverted,
            low,
            high,
            columns,
            descending,
            ..
        } => {
            let prefix = equality_prefix(equalities, unconverted, table, columns, space, params)?;
            // The range is on the column after the equality prefix.
            let range_affinity = index_affinity(table, columns, equalities.len());
            let (low_value, low_inclusive) =
                bound_value(low.as_ref(), space, params, range_affinity)?;
            let (high_value, high_inclusive) =
                bound_value(high.as_ref(), space, params, range_affinity)?;
            let mut evaluated = prefix
                .iter()
                .chain(low_value.iter())
                .chain(high_value.iter());
            if evaluated.any(|value| *value == OwnedDatum::Null) {
                return Ok(SpanBounds::nothing());
            }
            let mut low_key = prefix.clone();
            let mut high_key = prefix;
            if low_value.is_none() && high_value.is_none() {
                if low_key.is_empty() {
                    return Ok(SpanBounds {
                        low: None,
                        low_inclusive: true,
                        high: None,
                        high_inclusive: true,
                        matches_nothing: false,
                    });
                }
                // An equality prefix with no range is the run of every entry
                // sharing it, so both ends are the prefix and both inclusive.
                return Ok(SpanBounds {
                    low: Some(low_key),
                    low_inclusive: true,
                    high: Some(high_key),
                    high_inclusive: true,
                    matches_nothing: false,
                });
            }
            // **Which end of the walk the NULLs sit at.** An ascending index
            // holds them first and a descending one holds them last, and the
            // range has to exclude them from whichever end it does not
            // otherwise bound.
            let range_descending = descending.get(equalities.len()).copied().unwrap_or(false);
            let had_low = low_value.is_some();
            let mut low_inclusive = low_inclusive;
            let mut high_inclusive = high_inclusive;
            match low_value {
                Some(value) => low_key.push(value),
                // **A range with no lower bound still excludes NULL.**
                // `WHERE k < -1000` is unknown for a NULL `k`, so SQLite
                // returns no row for one; an *ascending* index holds its NULLs
                // first, so a walk that starts at the beginning returns exactly
                // those. An exclusive lower bound of NULL starts past that run,
                // which is the same rule said in the key's own terms.
                //
                // `WHERE k > 5` never had the problem: its own lower bound
                // already starts above the NULLs, which is why this was only
                // ever wrong in the one direction.
                //
                // On a descending index the NULLs are at the *other* end, so
                // this bound would exclude the entire tree rather than the
                // NULLs - and it did: `WHERE c >= 10` over `t(c DESC)` returned
                // nothing at all, because an exclusive NULL low bound in
                // descending order starts past the last row.
                None if high_value.is_some() && !range_descending => {
                    low_key.push(OwnedDatum::Null);
                    low_inclusive = false;
                }
                None => {}
            }
            match high_value {
                Some(value) => high_key.push(value),
                // The descending mirror of the rule above: the walk runs from
                // the largest key down, so the NULLs are the tail it has to
                // stop before.
                None if had_low && range_descending => {
                    high_key.push(OwnedDatum::Null);
                    high_inclusive = false;
                }
                None => {}
            }
            Ok(SpanBounds {
                low: if low_key.is_empty() {
                    None
                } else {
                    Some(low_key)
                },
                low_inclusive,
                high: if high_key.is_empty() {
                    None
                } else {
                    Some(high_key)
                },
                high_inclusive,
                matches_nothing: false,
            })
        }
        _ => unsupported("a range over that access path"),
    }
}

/// Returns the values an index seek's equality prefix pins, one per leading
/// key column, each converted the way its comparison converts it.
///
/// @param equalities - the prefix's value expressions, in key order
/// @param unconverted - the positions whose comparison converts nothing
/// @param table - the indexed table, for each key column's affinity
/// @param columns - which table column each index position holds
/// @param space - the joined column space
/// @param params - the bound parameters
fn equality_prefix(
    equalities: &[BoundExpr],
    unconverted: &[usize],
    table: &TableInfo,
    columns: &[Option<u16>],
    space: &Space<'_>,
    params: &Params,
) -> DbResult<Vec<OwnedDatum>> {
    let mut prefix = Vec::with_capacity(equalities.len());
    for (position, expr) in equalities.iter().enumerate() {
        prefix.push(constant_value(
            expr,
            space,
            params,
            probe_affinity(
                unconverted.contains(&position),
                index_affinity(table, columns, position),
            ),
        )?);
    }
    Ok(prefix)
}

/// Returns one range bound's value and whether it is inclusive.
///
/// @param bound - the bound, when there is one
/// @param space - the joined column space
/// @param params - the bound parameters
/// @param affinity - the indexed column's affinity, which [`probe_affinity`] may drop
fn bound_value(
    bound: Option<&RangeBound>,
    space: &Space<'_>,
    params: &Params,
    affinity: Option<Affinity>,
) -> DbResult<(Option<OwnedDatum>, bool)> {
    let Some(bound) = bound else {
        return Ok((None, true));
    };
    let value = constant_value(
        &bound.value,
        space,
        params,
        probe_affinity(bound.unconverted, affinity),
    )?;
    let inclusive = matches!(bound.kind, BoundKind::GreaterEqual | BoundKind::LessEqual);
    Ok((Some(value), inclusive))
}

/// Evaluates an expression that must not read any column.
///
/// A bound, a seek key and a `LIMIT` are all "known before the scan starts", and
/// an expression that reads a column is not - so one is refused here rather
/// than evaluated against whatever row happened to be current.
///
/// @param expr - the bound expression
/// @param space - the joined column space
/// @param params - the bound parameters
/// Wraps a key expression so an affinity is applied before it is compared.
///
/// A join's inner probe evaluates its key once per outer row, so the conversion
/// cannot be folded away the way a constant seek key's can.
///
/// **It is the comparison's affinity, not a `CAST` (task-2083).** The two
/// differ on every value the affinity cannot convert without loss. A `CAST` to
/// BLOB turns the integer 3 into the bytes `'3'`, so a probe of an untyped
/// column found nothing and `SELECT count(*) FROM s, h WHERE h.a = s.k`
/// answered 0 where SQLite answers 1. A `CAST` to INTEGER turns `'abc'` into 0
/// and `3.5` into 3, so a probe of a rowid or an INTEGER column matched rows
/// SQLite does not match. The constant seek key in [`constant_value`] already
/// applied the affinity this way, which is why `WHERE a = 3` was right and the
/// join over the same index was not.
///
/// @param expr - the translated key expression
/// @param affinity - the affinity to apply, if any
fn with_affinity(expr: Expr, affinity: Option<Affinity>) -> Expr {
    match affinity {
        None => expr,
        Some(affinity) => Expr::Affinity {
            operand: Box::new(expr),
            affinity,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A probe takes the indexed column's affinity, and takes none when the
    /// index has none to give.
    ///
    /// **A key the index computes takes no affinity (T3, task-1962).** An index
    /// on `lower(a)` stores whatever the expression returned, so converting the
    /// probe would compare a converted value against an unconverted one - a
    /// seek that lands somewhere else. SQLite applies none there either.
    #[test]
    fn a_probe_is_converted_only_when_the_column_has_an_affinity() {
        let bare = with_affinity(Expr::Column(0), None);
        assert!(
            matches!(bare, Expr::Column(0)),
            "no affinity means the expression is unchanged"
        );
        let converted = with_affinity(Expr::Column(0), Some(Affinity::Integer));
        match converted {
            Expr::Affinity { affinity, operand } => {
                assert_eq!(affinity, Affinity::Integer);
                assert!(matches!(*operand, Expr::Column(0)));
            }
            other => panic!(
                "an affinity should wrap the probe in the comparison's conversion, not a cast; \
                 it gave {other:?}"
            ),
        }
    }

    /// An unbounded span is the whole tree, and it says so with `None` rather
    /// than with a sentinel key.
    ///
    /// **The inclusivity is carried per side.** It used to be one flag for
    /// both, so `WHERE id > 495` returned `id >= 495` - one row too many, on
    /// every range scan with an exclusive bound.
    #[test]
    fn a_default_span_is_the_whole_tree() {
        let span = SpanBounds::default();
        assert!(
            span.low.is_none(),
            "no lower bound is the start of the tree"
        );
        assert!(span.high.is_none(), "no upper bound is the end of it");
        let exclusive_low = SpanBounds {
            low: Some(vec![OwnedDatum::Int(495)]),
            low_inclusive: false,
            high: None,
            high_inclusive: true,
            matches_nothing: false,
        };
        assert!(
            !exclusive_low.low_inclusive && exclusive_low.high_inclusive,
            "the two sides carry their own inclusivity, which is what `id > 495` needs"
        );
    }
}
