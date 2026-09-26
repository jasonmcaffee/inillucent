//! Covering arrays: the fewest rows that hold every pair, or every triple, of
//! axis values at least once.
//!
//! Invariant: **after building an array, [`build`] checks that every allowed
//! combination of `strength` axis values appears in some row, and returns an
//! error naming the first one that does not.** A covering array that silently
//! missed a pair would be a suite that silently stopped testing a combination,
//! which is the failure this whole design exists to prevent. The check is
//! cheap next to the cases it decides, so it runs every time.
//!
//! The builder is IPOG (Lei and Tai, "In-Parameter-Order"): cover the first
//! `strength` axes completely, then add one axis at a time, first extending
//! each existing row with the value that covers the most new combinations
//! (horizontal growth), then adding rows for the combinations still missing
//! (vertical growth). It is deterministic: ties go to the lowest value index,
//! and the order of everything is fixed, so the same axes always produce the
//! same array and the same case ids.
//!
//! **Constraints** are a predicate over a partial row, where an unset axis is
//! `None`. The predicate must refuse a partial row only when the values that
//! are set already cannot go together. A combination the predicate refuses on
//! its own is not required; how many were refused is returned, and
//! `counts.toml` records it per family, so a constraint that starts removing
//! more is a visible change in a reviewed file.

use std::collections::BTreeSet;

/// A partial row: one value index per axis, or `None` where it is not chosen.
pub type Partial = Vec<Option<usize>>;

/// What [`build`] produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Array {
    /// The rows: one value index per axis.
    pub rows: Vec<Vec<usize>>,
    /// How many combinations of `strength` values the constraints refused.
    pub removed: usize,
    /// How many combinations are required, and therefore covered.
    pub required: usize,
}

/// One combination: the axes it sets and their values, in axis order.
type Combination = Vec<(usize, usize)>;

/// Builds a covering array.
///
/// @param sizes - how many values each axis has, in axis order
/// @param strength - two for every pair, three for every triple
/// @param allowed - the constraint over a partial row
pub fn build(
    sizes: &[usize],
    strength: usize,
    allowed: &dyn Fn(&Partial) -> bool,
) -> Result<Array, String> {
    if sizes.iter().any(|size| *size == 0) {
        return Err("an axis has no values".to_string());
    }
    let strength = strength.clamp(1, sizes.len().max(1));
    let (required, removed) = required_combinations(sizes, strength, allowed);
    let mut rows = initial_rows(sizes, strength, allowed);
    for axis in strength..sizes.len() {
        let mut missing: BTreeSet<Combination> = required
            .iter()
            .filter(|combination| combination.last().map(|(last, _)| *last) == Some(axis))
            .cloned()
            .collect();
        grow_horizontally(&mut rows, sizes, axis, &mut missing, allowed);
        grow_vertically(&mut rows, sizes, axis, &mut missing, allowed);
    }
    let rows = fill_free_values(rows, sizes, allowed);
    verify(&rows, &required)?;
    Ok(Array {
        rows,
        removed,
        required: required.len(),
    })
}

/// Every combination of `strength` axis values, split into those the
/// constraint allows and a count of those it refuses.
fn required_combinations(
    sizes: &[usize],
    strength: usize,
    allowed: &dyn Fn(&Partial) -> bool,
) -> (BTreeSet<Combination>, usize) {
    let mut required = BTreeSet::new();
    let mut removed = 0usize;
    for axes in axis_subsets(sizes.len(), strength) {
        for values in value_product(&axes, sizes) {
            let combination: Combination = axes.iter().copied().zip(values).collect();
            if allowed(&partial_of(&combination, sizes.len())) {
                required.insert(combination);
            } else {
                removed = removed.saturating_add(1);
            }
        }
    }
    (required, removed)
}

/// Every set of `strength` axis indexes, in increasing order.
fn axis_subsets(count: usize, strength: usize) -> Vec<Vec<usize>> {
    let mut out = Vec::new();
    let mut current = Vec::with_capacity(strength);
    fn walk(
        start: usize,
        count: usize,
        strength: usize,
        current: &mut Vec<usize>,
        out: &mut Vec<Vec<usize>>,
    ) {
        if current.len() == strength {
            out.push(current.clone());
            return;
        }
        for axis in start..count {
            current.push(axis);
            walk(axis.saturating_add(1), count, strength, current, out);
            current.pop();
        }
    }
    walk(0, count, strength, &mut current, &mut out);
    out
}

/// Every choice of one value for each of the given axes.
fn value_product(axes: &[usize], sizes: &[usize]) -> Vec<Vec<usize>> {
    let mut out: Vec<Vec<usize>> = vec![Vec::new()];
    for axis in axes {
        let size = sizes.get(*axis).copied().unwrap_or(0);
        let mut next = Vec::with_capacity(out.len().saturating_mul(size));
        for prefix in &out {
            for value in 0..size {
                let mut row = prefix.clone();
                row.push(value);
                next.push(row);
            }
        }
        out = next;
    }
    out
}

/// A partial row holding one combination.
fn partial_of(combination: &Combination, width: usize) -> Partial {
    let mut partial = vec![None; width];
    for (axis, value) in combination {
        if let Some(slot) = partial.get_mut(*axis) {
            *slot = Some(*value);
        }
    }
    partial
}

/// The rows that cover the first `strength` axes completely: every allowed
/// choice of their values, one row each.
fn initial_rows(
    sizes: &[usize],
    strength: usize,
    allowed: &dyn Fn(&Partial) -> bool,
) -> Vec<Partial> {
    let axes: Vec<usize> = (0..strength).collect();
    value_product(&axes, sizes)
        .into_iter()
        .map(|values| {
            let combination: Combination = axes.iter().copied().zip(values).collect();
            partial_of(&combination, sizes.len())
        })
        .filter(|partial| allowed(partial))
        .collect()
}

/// The combinations ending at `axis` that a row, with `axis` set to `value`,
/// would newly cover.
///
/// Looked up by key, one per set of earlier axes, rather than by scanning every
/// missing combination: at strength three a family's array has tens of
/// thousands of them, and the scan made building the array the slowest part of
/// loading the cases.
///
/// @param row - the row being extended
/// @param axis - the axis being added
/// @param value - the value tried for it
/// @param earlier - every set of `strength - 1` axes before `axis`
/// @param missing - the combinations not covered yet
fn gain(
    row: &Partial,
    axis: usize,
    value: usize,
    earlier: &[Vec<usize>],
    missing: &BTreeSet<Combination>,
) -> Vec<Combination> {
    let mut found = Vec::new();
    for axes in earlier {
        let mut key: Combination = Vec::with_capacity(axes.len().saturating_add(1));
        let mut complete = true;
        for at in axes {
            match row.get(*at).copied().flatten() {
                Some(chosen) => key.push((*at, chosen)),
                None => {
                    complete = false;
                    break;
                }
            }
        }
        if !complete {
            continue;
        }
        key.push((axis, value));
        if missing.contains(&key) {
            found.push(key);
        }
    }
    found
}

/// Extends each existing row with the value of the new axis that covers the
/// most missing combinations and keeps the row allowed.
fn grow_horizontally(
    rows: &mut [Partial],
    sizes: &[usize],
    axis: usize,
    missing: &mut BTreeSet<Combination>,
    allowed: &dyn Fn(&Partial) -> bool,
) {
    let size = sizes.get(axis).copied().unwrap_or(0);
    let strength = missing
        .iter()
        .next()
        .map(|combination| combination.len())
        .unwrap_or(1);
    let earlier = axis_subsets(axis, strength.saturating_sub(1));
    for row in rows.iter_mut() {
        let mut best: Option<(usize, Vec<Combination>)> = None;
        for value in 0..size {
            let mut trial = row.clone();
            if let Some(slot) = trial.get_mut(axis) {
                *slot = Some(value);
            }
            if !allowed(&trial) {
                continue;
            }
            let covered = gain(row, axis, value, &earlier, missing);
            if best
                .as_ref()
                .is_none_or(|(_, most)| covered.len() > most.len())
            {
                best = Some((value, covered));
            }
        }
        if let Some((value, covered)) = best {
            if let Some(slot) = row.get_mut(axis) {
                *slot = Some(value);
            }
            for combination in covered {
                missing.remove(&combination);
            }
        }
    }
}

/// Adds rows for the combinations still missing, reusing a row added in this
/// pass when the combination fits in its unset positions.
fn grow_vertically(
    rows: &mut Vec<Partial>,
    sizes: &[usize],
    axis: usize,
    missing: &mut BTreeSet<Combination>,
    allowed: &dyn Fn(&Partial) -> bool,
) {
    let first_new = rows.len();
    let pending: Vec<Combination> = missing.iter().cloned().collect();
    for combination in pending {
        let mut placed = false;
        for row in rows.iter_mut().skip(first_new) {
            let fits = combination.iter().all(|(at, value)| {
                matches!(row.get(*at).copied().flatten(), None)
                    || row.get(*at).copied().flatten() == Some(*value)
            });
            if !fits {
                continue;
            }
            let mut trial = row.clone();
            for (at, value) in &combination {
                if let Some(slot) = trial.get_mut(*at) {
                    *slot = Some(*value);
                }
            }
            if allowed(&trial) {
                *row = trial;
                placed = true;
                break;
            }
        }
        if !placed {
            rows.push(partial_of(&combination, sizes.len()));
        }
        missing.remove(&combination);
    }
    let _ = axis;
}

/// Gives every unset position a value the constraint allows. A row that cannot
/// be completed keeps the combinations it was made for only if some completion
/// exists, so each free position tries every value in order.
fn fill_free_values(
    rows: Vec<Partial>,
    sizes: &[usize],
    allowed: &dyn Fn(&Partial) -> bool,
) -> Vec<Vec<usize>> {
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        if let Some(complete) = complete(row, sizes, 0, allowed) {
            out.push(complete);
        }
    }
    out
}

/// Completes a row by trying values in order at each free position, going
/// back when a position has no allowed value.
fn complete(
    row: Partial,
    sizes: &[usize],
    from: usize,
    allowed: &dyn Fn(&Partial) -> bool,
) -> Option<Vec<usize>> {
    let Some(free) = (from..row.len()).find(|at| row.get(*at).copied().flatten().is_none()) else {
        return Some(row.into_iter().map(|value| value.unwrap_or(0)).collect());
    };
    let size = sizes.get(free).copied().unwrap_or(0);
    for value in 0..size {
        let mut trial = row.clone();
        if let Some(slot) = trial.get_mut(free) {
            *slot = Some(value);
        }
        if allowed(&trial) {
            if let Some(done) = complete(trial, sizes, free.saturating_add(1), allowed) {
                return Some(done);
            }
        }
    }
    None
}

/// Checks that every required combination is in some row.
fn verify(rows: &[Vec<usize>], required: &BTreeSet<Combination>) -> Result<(), String> {
    for combination in required {
        let covered = rows.iter().any(|row| {
            combination
                .iter()
                .all(|(axis, value)| row.get(*axis) == Some(value))
        });
        if !covered {
            return Err(format!(
                "the covering array misses the allowed combination {combination:?}; a constraint \
                 refuses every completion of it, so it should refuse the combination itself"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every pair of three binary axes is covered in far fewer rows than the
    /// full product, which is the reason to build an array at all.
    #[test]
    fn every_pair_is_covered_in_fewer_rows_than_the_product() {
        let sizes = [3, 3, 3, 3, 3];
        let array = build(&sizes, 2, &|_| true).expect("it builds");
        assert!(array.rows.len() < 243, "{} rows", array.rows.len());
        assert!(array.rows.len() >= 9);
        assert_eq!(array.removed, 0);
        assert_eq!(array.required, 10 * 9);
    }

    /// Every triple is covered, and a triple array covers every pair too.
    #[test]
    fn every_triple_is_covered() {
        let sizes = [2, 3, 2, 4];
        let array = build(&sizes, 3, &|_| true).expect("it builds");
        assert!(array.rows.len() >= 3 * 2 * 4);
        let pairs = build(&sizes, 2, &|_| true).expect("it builds");
        assert!(pairs.rows.len() <= array.rows.len());
    }

    /// A constraint removes combinations, says how many, and no row breaks it.
    #[test]
    fn a_constraint_is_never_broken() {
        let sizes = [3, 3, 3];
        // Axis 0 value 0 never goes with axis 2 value 2.
        let rule = |row: &Partial| !(row.first() == Some(&Some(0)) && row.get(2) == Some(&Some(2)));
        let array = build(&sizes, 2, &rule).expect("it builds");
        assert_eq!(array.removed, 1);
        for row in &array.rows {
            assert!(rule(&row.iter().map(|value| Some(*value)).collect()));
        }
    }

    /// The same axes always give the same array.
    #[test]
    fn the_array_is_deterministic() {
        let sizes = [4, 3, 5, 2, 3];
        let one = build(&sizes, 2, &|_| true).expect("it builds");
        let two = build(&sizes, 2, &|_| true).expect("it builds");
        assert_eq!(one, two);
    }
}
