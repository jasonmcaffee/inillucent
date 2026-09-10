//! `bm25()`: the score FTS5 ranks by.
//!
//! Invariant: the formula is the pinned release's, constants and sign included.
//! FTS5 returns a *negative* score, so that `ORDER BY rank` - which is
//! ascending - puts the best match first, and an application that sorted by the
//! absolute value would get the worst matches. That sign is the single most
//! surprising thing about the function and the reason it is written down here.
//!
//! The formula, from `fts5_aux.c`:
//!
//! ```text
//! idf(q)   = log( (N - n(q) + 0.5) / (n(q) + 0.5) )        -- clamped at 1e-6
//! score    = - SUM over phrases q of
//!              idf(q) * SUM over columns c of
//!                w(c) * f(q,c) * (k1 + 1) / (f(q,c) + k1 * (1 - b + b * D/avgdl))
//! ```
//!
//! with `k1 = 1.2` and `b = 0.75`, `N` the number of rows, `n(q)` the number of
//! rows the phrase appears in, `f(q,c)` how often it appears in column `c` of
//! this row, `D` the row's length and `avgdl` the average length. `w(c)` is the
//! per-column weight an application may pass, and is one by default.

use std::collections::BTreeMap;

use inillucent_base::DbResult;
use inillucent_value::Value;

use super::expr::{Phrase, Query};
use super::{decode_sizes, Totals};
use crate::shadow::ShadowTables;
use crate::vtab::Context;

/// The term-frequency saturation constant.
const K1: f64 = 1.2;
/// The length-normalisation constant.
const B: f64 = 0.75;

/// Scores every matched row, returning `(rowid, score)` pairs.
pub fn score(
    rows: &[i64],
    hits: &[BTreeMap<i64, Vec<(usize, Vec<u32>)>>],
    query: &Query,
    context: &mut Context<'_>,
    shadows: &ShadowTables,
    totals: &Totals,
    columns: usize,
) -> DbResult<Vec<(i64, f64)>> {
    let weights = vec![1.0f64; columns];
    let mut out = Vec::with_capacity(rows.len());
    for rowid in rows {
        let sizes = row_sizes(context, shadows, *rowid, columns)?;
        out.push((
            *rowid,
            score_row(*rowid, hits, &query.phrases, totals, &sizes, &weights),
        ));
    }
    Ok(out)
}

/// Returns how many tokens each column of one row holds.
pub fn row_sizes(
    context: &mut Context<'_>,
    shadows: &ShadowTables,
    rowid: i64,
    columns: usize,
) -> DbResult<Vec<i64>> {
    let Some(row) = shadows.read_row(context, b"docsize", rowid)? else {
        return Ok(vec![0; columns]);
    };
    Ok(row
        .get(1)
        .and_then(Value::as_blob)
        .map(|blob| decode_sizes(blob.raw(), columns))
        .unwrap_or_else(|| vec![0; columns]))
}

/// Scores one row against every phrase of a query.
pub fn score_row(
    rowid: i64,
    hits: &[BTreeMap<i64, Vec<(usize, Vec<u32>)>>],
    phrases: &[Phrase],
    totals: &Totals,
    sizes: &[i64],
    weights: &[f64],
) -> f64 {
    let rows = totals.rows.max(1) as f64;
    let total_tokens: i64 = totals.tokens.iter().sum();
    let average = if totals.rows > 0 {
        total_tokens as f64 / totals.rows as f64
    } else {
        0.0
    };
    let length: i64 = sizes.iter().sum();
    let mut score = 0.0f64;
    for (index, _phrase) in phrases.iter().enumerate() {
        let Some(found) = hits.get(index) else {
            continue;
        };
        // How many rows the phrase appears in at all, which is what makes a
        // common word count for less than a rare one.
        let appearances = found.len() as f64;
        let idf = ((rows - appearances + 0.5) / (appearances + 0.5))
            .ln()
            .max(1e-6);
        let Some(columns) = found.get(&rowid) else {
            continue;
        };
        let mut term = 0.0f64;
        for (column, positions) in columns {
            let weight = weights.get(*column).copied().unwrap_or(1.0);
            let frequency = positions.len() as f64;
            let normalised = if average > 0.0 {
                1.0 - B + B * (length as f64 / average)
            } else {
                1.0
            };
            term += weight * frequency * (K1 + 1.0) / (frequency + K1 * normalised);
        }
        score += idf * term;
    }
    -score
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds the hits of one phrase in one row and column.
    fn hits(rowid: i64, column: usize, count: usize) -> BTreeMap<i64, Vec<(usize, Vec<u32>)>> {
        let mut found = BTreeMap::new();
        found.insert(
            rowid,
            vec![(column, (0..count as u32).collect::<Vec<u32>>())],
        );
        found
    }

    /// The score is negative, so that ascending order is best first.
    #[test]
    fn the_score_is_negative() {
        let totals = Totals {
            rows: 10,
            tokens: vec![100],
        };
        let phrase = Phrase {
            terms: Vec::new(),
            column: None,
        };
        let score = score_row(1, &[hits(1, 0, 1)], &[phrase], &totals, &[10], &[1.0]);
        assert!(score < 0.0, "{score}");
    }

    /// A row where the phrase appears more often scores better.
    #[test]
    fn more_occurrences_score_better() {
        let totals = Totals {
            rows: 100,
            tokens: vec![1000],
        };
        let phrase = Phrase {
            terms: Vec::new(),
            column: None,
        };
        let once = score_row(
            1,
            &[hits(1, 0, 1)],
            std::slice::from_ref(&phrase),
            &totals,
            &[10],
            &[1.0],
        );
        let often = score_row(1, &[hits(1, 0, 5)], &[phrase], &totals, &[10], &[1.0]);
        assert!(often < once, "{often} should beat {once}");
    }

    /// A shorter row scores better for the same number of occurrences.
    #[test]
    fn a_shorter_row_scores_better() {
        let totals = Totals {
            rows: 100,
            tokens: vec![1000],
        };
        let phrase = Phrase {
            terms: Vec::new(),
            column: None,
        };
        let short = score_row(
            1,
            &[hits(1, 0, 2)],
            std::slice::from_ref(&phrase),
            &totals,
            &[5],
            &[1.0],
        );
        let long = score_row(1, &[hits(1, 0, 2)], &[phrase], &totals, &[50], &[1.0]);
        assert!(short < long, "{short} should beat {long}");
    }

    /// A row the phrase is not in scores zero rather than something.
    #[test]
    fn an_unmatched_row_scores_nothing() {
        let totals = Totals {
            rows: 10,
            tokens: vec![100],
        };
        let phrase = Phrase {
            terms: Vec::new(),
            column: None,
        };
        let score = score_row(2, &[hits(1, 0, 3)], &[phrase], &totals, &[10], &[1.0]);
        assert_eq!(score, 0.0);
    }
}
