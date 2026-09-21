//! The cursor an FTS5 scan reads.
//!
//! Invariant: **a matched row is decided before the cursor moves onto it.**
//! `filter` resolves the query to a rowid list and the cursor walks that,
//! so a row the cursor is on has already matched and `column` never has to
//! decide whether to skip it.

use inillucent_base::DbResult;
use inillucent_value::Value;

use super::super::{Context, FilterPlan, VirtualCursor};
use crate::shadow::ShadowTables;

use super::expr::Phrase;
use super::expr::Query;
use super::index::*;
use super::tokenize::Tokenizer;
use super::*;

/// One row a query matched, with what it needs to be scored.
#[derive(Clone, Debug)]
pub(crate) struct MatchedRow {
    /// Which row.
    rowid: i64,
    /// The score `rank` reports, which is negative so that smaller is better.
    ///
    /// `None` until something asks for it. Scoring one row costs a read of its
    /// `%_docsize`, and a query that names neither `rank` nor `ORDER BY rank`
    /// never looks at the answer - which is most of them, and `count(*)` in
    /// particular. The one query that needs every score up front is the ranked
    /// one, where the score *is* the sort key.
    score: Option<f64>,
}
/// A cursor over the rows one query matched.
pub(crate) struct Fts5Cursor {
    /// Which surface this table presents.
    pub(crate) dialect: Dialect,
    /// The option this build cannot honour, when the table declares one.
    ///
    /// Every query of the table reports it; see [`super::Options::unsupported`]
    /// for why the refusal is here rather than at open.
    pub(crate) unsupported: Option<String>,
    pub(crate) columns: usize,
    /// The table's own name, so a refusal about its index can say which one.
    pub(crate) table: Vec<u8>,
    /// The shadow suffix the rows are reached under.
    pub(crate) content: Vec<u8>,
    /// Whether the table stores no document text at all.
    ///
    /// A `content=''` table has no `%_content` shadow, so every declared column
    /// reads back NULL - which is what SQLite answers and what task-1979, R5
    /// found this returning the stored document for instead.
    pub(crate) contentless: bool,
    /// Where each declared column sits in a stored row.
    pub(crate) offsets: Vec<usize>,
    /// The table the rows belong to, when they are not this table's.
    pub(crate) external: Option<Vec<u8>>,
    /// The column names, for a `column:term` filter to resolve against.
    pub(crate) names: Vec<Vec<u8>>,
    pub(crate) match_column: i32,
    pub(crate) rank_column: i32,
    pub(crate) tokenizer: Tokenizer,
    pub(crate) shadows: ShadowTables,
    pub(crate) rows: Vec<MatchedRow>,
    pub(crate) at: usize,
    /// The query the rows came from, for the auxiliary functions.
    pub(crate) pattern: Vec<u8>,
    /// What each phrase matched, kept so `bm25(t, w1, w2)` can score the row
    /// again with the weights that call asked for. `rank` is the same score
    /// with every weight one, so the two cannot disagree.
    pub(crate) matched: Vec<expr::Hits>,
    /// The phrases, in the order `matched` holds them.
    pub(crate) phrases: Vec<Phrase>,
    /// The collection totals the score is relative to.
    pub(crate) totals: Totals,
    /// The parsed query, kept so the hits can be built if a score is asked for.
    pub(crate) query: Option<Query>,
    /// Whether `matched` holds this query's hits.
    ///
    /// A query that reads no score never builds them; one that reads a score
    /// after not building them builds them once, here, rather than per row.
    pub(crate) hits_built: bool,
    /// The doclists the transaction has staged, shared with the table.
    ///
    /// A query inside a transaction that has written has to see what it wrote,
    /// and the buffer is where those doclists are until the commit.
    pub(crate) pending: Buffer,
    /// The `%_content` row the cursor is on, kept for the columns after the
    /// first.
    ///
    /// **One read per row, not one per column.** `column` is asked for each
    /// column in turn and read the whole row back for every one of them, so a
    /// two-column table read `%_content` twice per matched row and a
    /// ten-column table ten times. The row is the same row each time.
    pub(crate) held: Option<(i64, Vec<Value<'static>>)>,
}
impl Fts5Cursor {
    /// Builds the phrase hits if the cheap walk did not.
    ///
    /// The one caller that needs them is a score, and a score is asked for per
    /// row - so this runs once and every later row reads what it left.
    ///
    /// @param context - the host
    fn ensure_hits(&mut self, context: &mut Context<'_>) -> DbResult<()> {
        if self.hits_built {
            return Ok(());
        }
        self.hits_built = true;
        let Some(query) = self.query.clone() else {
            return Ok(());
        };
        let (_, hits) =
            expr::evaluate(&query, context, &self.shadows, &self.pending, self.columns)?;
        self.matched = hits;
        Ok(())
    }
}
impl Fts5Cursor {
    /// Answers `highlight(t, column, open, close)` on the current row.
    ///
    /// The column's text is **re-tokenised here** rather than read out of the
    /// index, and that is the design rather than a shortcut. The index stores a
    /// token's *position* - which word it is - and highlighting needs its
    /// *extent* in bytes, which the index never held: a token is folded before
    /// it is stored, and folding changes its length. Re-tokenising is the only
    /// way to get from a position back to a range of the original text, and it
    /// is what SQLite does too.
    ///
    /// @param context - the statement's context
    /// @param arguments - the column index, the opening mark, the closing mark
    fn highlight(
        &mut self,
        context: &mut Context<'_>,
        arguments: &[Value<'static>],
    ) -> DbResult<Value<'static>> {
        let column = arguments
            .first()
            .and_then(Value::as_integer)
            .unwrap_or(0)
            .max(0) as usize;
        let open = text_argument(arguments.get(1));
        let close = text_argument(arguments.get(2));
        let Some(text) = self.column_text(context, column)? else {
            return Ok(Value::Null);
        };
        let marked = self.marked_spans(&text);
        let mut out = Vec::with_capacity(text.len());
        let mut at = 0usize;
        for (start, end) in marked {
            out.extend_from_slice(text.get(at..start).unwrap_or_default());
            out.extend_from_slice(&open);
            out.extend_from_slice(text.get(start..end).unwrap_or_default());
            out.extend_from_slice(&close);
            at = end;
        }
        out.extend_from_slice(text.get(at..).unwrap_or_default());
        Ok(Value::owned_text(&out).unwrap_or(Value::Null))
    }

    /// Answers `snippet(t, column, open, close, ellipsis, tokens)`.
    ///
    /// The window is the run of `tokens` tokens holding the most matched ones,
    /// earliest when several tie - which is SQLite's rule and the reason a
    /// snippet of a long document lands on the interesting part rather than on
    /// its first sentence. The ellipsis is written only where text was actually
    /// cut, so a snippet of a short column reads as the whole column.
    ///
    /// @param context - the statement's context
    /// @param arguments - column, open, close, ellipsis, token count
    fn snippet(
        &mut self,
        context: &mut Context<'_>,
        arguments: &[Value<'static>],
    ) -> DbResult<Value<'static>> {
        let column = arguments
            .first()
            .and_then(Value::as_integer)
            .unwrap_or(0)
            .max(0) as usize;
        let open = text_argument(arguments.get(1));
        let close = text_argument(arguments.get(2));
        let ellipsis = text_argument(arguments.get(3));
        let wanted = arguments
            .get(4)
            .and_then(Value::as_integer)
            .unwrap_or(15)
            .clamp(1, 64) as usize;
        let Some(text) = self.column_text(context, column)? else {
            return Ok(Value::Null);
        };
        let spans = self.tokenizer.spans(&text);
        if spans.is_empty() {
            return Ok(Value::owned_text(&text).unwrap_or(Value::Null));
        }
        let wanted_terms = self.wanted_terms();
        let hit: Vec<bool> = spans
            .iter()
            .map(|(token, _, _)| matches_a_term(token, &wanted_terms))
            .collect();
        // The best window, by how many matched tokens it holds.
        let mut best = 0usize;
        let mut best_score = usize::MAX;
        for start in 0..spans.len() {
            let end = start.saturating_add(wanted).min(spans.len());
            let score = hit
                .get(start..end)
                .map(|run| run.iter().filter(|held| **held).count())
                .unwrap_or(0);
            if best_score == usize::MAX || score > best_score {
                best_score = score;
                best = start;
            }
            if end == spans.len() {
                break;
            }
        }
        let end = best.saturating_add(wanted).min(spans.len());
        let from = spans.get(best).map(|(_, start, _)| *start).unwrap_or(0);
        let to = spans
            .get(end.saturating_sub(1))
            .map(|(_, _, stop)| *stop)
            .unwrap_or(text.len());
        let mut out = Vec::with_capacity(to.saturating_sub(from).saturating_add(16));
        if best > 0 {
            out.extend_from_slice(&ellipsis);
        }
        let mut at = from;
        for (index, (_, start, stop)) in spans.iter().enumerate() {
            if index < best || index >= end {
                continue;
            }
            if !hit.get(index).copied().unwrap_or(false) {
                continue;
            }
            out.extend_from_slice(text.get(at..*start).unwrap_or_default());
            out.extend_from_slice(&open);
            out.extend_from_slice(text.get(*start..*stop).unwrap_or_default());
            out.extend_from_slice(&close);
            at = *stop;
        }
        out.extend_from_slice(text.get(at..to).unwrap_or_default());
        if end < spans.len() {
            out.extend_from_slice(&ellipsis);
        }
        Ok(Value::owned_text(&out).unwrap_or(Value::Null))
    }

    /// Returns one column's stored text for the current row.
    ///
    /// @param context - the statement's context
    /// Fills in where each declared column sits, the first time a row is read.
    ///
    /// `open` is handed no catalog - a cursor is made before a statement runs -
    /// so an external content table's column positions cannot be resolved until
    /// here, where a context exists.
    ///
    /// @param context - the running statement
    fn resolve_offsets(&mut self, context: &Context<'_>) {
        if self.external.is_some() && self.offsets.contains(&usize::MAX) {
            self.offsets =
                content_offsets_named(self.external.as_deref(), &self.names, context.catalog);
        }
    }

    /// @param column - which declared column
    fn column_text(
        &mut self,
        context: &mut Context<'_>,
        column: usize,
    ) -> DbResult<Option<Vec<u8>>> {
        let Some(row) = self.rows.get(self.at).cloned() else {
            return Ok(None);
        };
        if self.contentless {
            return Ok(None);
        }
        if self.held.as_ref().map(|(rowid, _)| *rowid) != Some(row.rowid) {
            self.resolve_offsets(context);
            self.held = self
                .shadows
                .read_row(context, &self.content.clone(), row.rowid)?
                .map(|values| (row.rowid, values));
        }
        let Some((_, content)) = self.held.as_ref() else {
            return Ok(None);
        };
        Ok(
            match self.offsets.get(column).and_then(|at| content.get(*at)) {
                Some(Value::Text(text)) => Some(text.utf8_bytes().into_owned()),
                Some(Value::Blob(blob)) => Some(blob.raw().to_vec()),
                _ => None,
            },
        )
    }

    /// Returns the byte ranges the query's phrases occupy in one column.
    ///
    /// **A phrase, not a token.** `MATCH '"quick brown"'` marks the two words
    /// as one range and `MATCH 'quick AND brown'` marks them as two, because
    /// the first is one phrase of two terms and the second is two phrases of
    /// one - and the reference draws exactly that distinction. Marking every
    /// matched token and merging adjacent ones gets the first case right and
    /// the second wrong.
    ///
    /// The ranges come back in order and non-overlapping: two phrases that
    /// claim the same word - `'quick OR "quick brown"'` - would otherwise
    /// produce nested marks and text repeated between them.
    ///
    /// @param text - the column's bytes
    fn marked_spans(&self, text: &[u8]) -> Vec<(usize, usize)> {
        let spans = self.tokenizer.spans(text);
        let mut marked: Vec<(usize, usize)> = Vec::new();
        for phrase in &self.phrases {
            if phrase.terms.is_empty() {
                continue;
            }
            let mut at = 0usize;
            while at.saturating_add(phrase.terms.len()) <= spans.len() {
                let fits = phrase.terms.iter().enumerate().all(|(offset, term)| {
                    spans
                        .get(at.saturating_add(offset))
                        .is_some_and(|(token, _, _)| {
                            matches_a_term(token, std::slice::from_ref(term))
                        })
                });
                if !fits {
                    at = at.saturating_add(1);
                    continue;
                }
                let from = spans.get(at).map(|(_, start, _)| *start).unwrap_or(0);
                let to = spans
                    .get(at.saturating_add(phrase.terms.len()).saturating_sub(1))
                    .map(|(_, _, end)| *end)
                    .unwrap_or(from);
                marked.push((from, to));
                at = at.saturating_add(phrase.terms.len());
            }
        }
        marked.sort_unstable();
        marked.dedup();
        // Overlaps go, keeping the first - which is the longest match starting
        // earliest once the list is sorted by start.
        let mut kept: Vec<(usize, usize)> = Vec::with_capacity(marked.len());
        for (from, to) in marked {
            match kept.last() {
                Some((_, last)) if from < *last => {}
                _ => kept.push((from, to)),
            }
        }
        kept
    }

    /// Returns every term the query asked for, across its phrases.
    /// Returns `offsets(t)`: where every hit in the row is, as four numbers.
    ///
    /// **Column, term, byte offset, byte length**, one quadruple per hit,
    /// separated by single spaces and in column-then-position order. It is
    /// FTS3's most direct answer - the caller gets the positions and does its
    /// own marking - and the term number is the *query's*, so a two-word query
    /// reports 0 for one word and 1 for the other however they interleave in
    /// the text.
    ///
    /// @param context - the running statement
    fn offsets(&mut self, context: &mut Context<'_>) -> DbResult<Value<'static>> {
        let wanted = self.wanted_terms();
        let mut out: Vec<String> = Vec::new();
        for column in 0..self.columns {
            let Some(text) = self.column_text(context, column)? else {
                continue;
            };
            for (token, start, stop) in self.tokenizer.spans(&text) {
                let Some(term) = wanted
                    .iter()
                    .position(|term| matches_a_term(&token, std::slice::from_ref(term)))
                else {
                    continue;
                };
                out.push(format!(
                    "{column} {term} {start} {}",
                    stop.saturating_sub(start)
                ));
            }
        }
        Value::owned_text(out.join(" ").as_bytes())
    }

    /// Returns `matchinfo(t, format)`: the counts a caller ranks with.
    ///
    /// A blob of native-endian 32-bit integers, one group per format letter,
    /// in the order the letters were written. `pcx` is the default and is what
    /// FTS3's own documentation builds its example ranking function from:
    ///
    /// | letter | what it contributes |
    /// |---|---|
    /// | `p` | how many phrases the query has |
    /// | `c` | how many columns the table has |
    /// | `x` | three numbers per phrase and column: hits in this row, hits in every row, and rows with at least one |
    /// | `n` | how many rows the table holds |
    /// | `a` | the average token count of each column |
    /// | `l` | this row's token count per column |
    ///
    /// @param context - the running statement
    /// @param arguments - the format string, when one was written
    fn matchinfo(
        &mut self,
        context: &mut Context<'_>,
        arguments: &[Value<'static>],
    ) -> DbResult<Value<'static>> {
        let format = match arguments.first() {
            Some(Value::Text(text)) => text.utf8_bytes().into_owned(),
            _ => b"pcx".to_vec(),
        };
        let Some(row) = self.rows.get(self.at).cloned() else {
            return Ok(Value::Null);
        };
        self.ensure_hits(context)?;
        let sizes = bm25::row_sizes(context, &self.shadows, row.rowid, self.columns)?;
        let mut out: Vec<u8> = Vec::new();
        let mut push = |number: i64| out.extend_from_slice(&(number as u32).to_ne_bytes());
        for letter in format {
            match letter {
                b'p' => push(self.phrases.len() as i64),
                b'c' => push(self.columns as i64),
                b'n' => push(self.totals.rows),
                b'a' => {
                    for column in 0..self.columns {
                        let total = self.totals.tokens.get(column).copied().unwrap_or(0);
                        let average = if self.totals.rows > 0 {
                            total / self.totals.rows
                        } else {
                            0
                        };
                        push(average);
                    }
                }
                b'l' => {
                    for column in 0..self.columns {
                        push(sizes.get(column).copied().unwrap_or(0));
                    }
                }
                b'x' => {
                    for at in 0..self.phrases.len() {
                        let hits = self.matched.get(at);
                        for column in 0..self.columns {
                            let (here, every, rows) = match hits {
                                Some(hits) => phrase_counts(hits, row.rowid, column),
                                None => (0, 0, 0),
                            };
                            push(here);
                            push(every);
                            push(rows);
                        }
                    }
                }
                _ => {}
            }
        }
        Value::owned_blob(&out)
    }

    /// Returns every term the query asked for, in order.
    fn wanted_terms(&self) -> Vec<expr::Term> {
        self.phrases
            .iter()
            .flat_map(|phrase| phrase.terms.iter().cloned())
            .collect()
    }
}
impl VirtualCursor for Fts5Cursor {
    /// Runs the query and collects every row it matched.
    fn filter(&mut self, context: &mut Context<'_>, plan: &FilterPlan) -> DbResult<()> {
        if let Some(what) = self.unsupported.clone() {
            return Err(super::unsupported_option(&what));
        }
        self.rows.clear();
        self.matched.clear();
        self.phrases.clear();
        self.at = 0;
        self.held = None;
        if plan.index_number == PLAN_ROWID {
            let Some(rowid) = plan.arguments.first().and_then(Value::as_integer) else {
                return Ok(());
            };
            // `%_docsize` holds one row per indexed document and a contentless
            // table has no `%_content` to ask, so it is the record of which
            // rowids exist for both.
            let suffix: &[u8] = match self.contentless {
                true => b"docsize",
                false => &self.content.clone(),
            };
            if self.shadows.read_row(context, suffix, rowid)?.is_some() {
                self.rows.push(MatchedRow { rowid, score: None });
            }
            return Ok(());
        }
        if plan.index_number & PLAN_MATCH == 0 {
            // **The index's own rows, not the owner's.** An external content
            // table's owner may hold rows this index has never seen, and a
            // scan that returned them would answer with documents no query
            // could match. `%_docsize` has one row per indexed document.
            let suffix: &[u8] = if self.external.is_some() || self.contentless {
                b"docsize"
            } else {
                b"content"
            };
            self.shadows.scan(context, suffix, |rowid, _| {
                self.rows.push(MatchedRow { rowid, score: None });
                Ok(true)
            })?;
            return Ok(());
        }
        // **Here rather than at the top of `filter`, because this is where the
        // index is read.** The two plans above answer out of `%_content` and
        // `%_docsize`, which every layout holds the same way - which is why
        // 0.1.1 answered `count(*)` over a newer index correctly and answered
        // the `MATCH` with nothing. The refusal belongs on the one query whose
        // answer depends on the dictionary. See `layout.rs`.
        super::layout::readable(context, &self.shadows, &self.pending, &self.table)?;
        let Some(pattern) = plan.arguments.first().and_then(text_of) else {
            return Ok(());
        };
        self.pattern = pattern.clone();
        let query = Query::parse(&pattern, &self.tokenizer, &self.names)?;
        // **The buffered totals, not the row.** A score is computed from the
        // document count and the token totals, and those are staged rather than
        // written per document - so a query inside the same transaction that
        // read `%_data` directly would score against the state the transaction
        // started from. That is what a buffer has to be transparent about.
        let totals = buffered_totals(context, &self.shadows, &self.pending, self.columns);
        let ranked = plan.index_number & PLAN_RANKED != 0;
        // **A ranked plan needs the positions, and most plans do not.** The
        // cheap walk answers `None` for the queries whose answer depends on
        // them, and those fall through to the full evaluation.
        let cheap = if ranked {
            None
        } else {
            expr::evaluate_rows(&query, context, &self.shadows, &self.pending, self.columns)?
        };
        let (rows, hits) = match cheap {
            Some(rows) => (rows, Vec::new()),
            None => expr::evaluate(&query, context, &self.shadows, &self.pending, self.columns)?,
        };
        self.hits_built = !hits.is_empty();
        if ranked {
            // The sort key has to exist before the sort, so this is the one
            // plan that scores every row up front.
            let scores = bm25::score(
                &rows,
                &hits,
                &query,
                context,
                &self.shadows,
                &totals,
                self.columns,
            )?;
            self.rows = scores
                .into_iter()
                .map(|(rowid, score)| MatchedRow {
                    rowid,
                    score: Some(score),
                })
                .collect();
        } else {
            self.rows = rows
                .into_iter()
                .map(|rowid| MatchedRow { rowid, score: None })
                .collect();
        }
        self.phrases = query.phrases.clone();
        self.totals = totals;
        self.matched = hits;
        self.query = Some(query);
        if ranked {
            self.rows.sort_by(|left, right| {
                // Both sides are `Some` here: this arm is only reached when
                // `filter` scored every row, which it does for exactly this
                // plan. An unscored row sorts as zero rather than panicking.
                left.score
                    .unwrap_or(0.0)
                    .partial_cmp(&right.score.unwrap_or(0.0))
                    .unwrap_or(core::cmp::Ordering::Equal)
                    .then(left.rowid.cmp(&right.rowid))
            });
        } else {
            self.rows.sort_by_key(|row| row.rowid);
        }
        Ok(())
    }

    /// Moves to the next matching row.
    fn next(&mut self, _context: &mut Context<'_>) -> DbResult<()> {
        self.at = self.at.saturating_add(1);
        self.held = None;
        Ok(())
    }

    /// Returns whether the walk is finished.
    fn eof(&self) -> bool {
        self.at >= self.rows.len()
    }

    /// Returns one column of the current row.
    fn column(&mut self, context: &mut Context<'_>, index: usize) -> DbResult<Value<'static>> {
        let Some(row) = self.rows.get(self.at).cloned() else {
            return Ok(Value::Null);
        };
        if index as i32 == self.rank_column {
            // In FTS3 this slot is `docid`, which is the rowid under another
            // name and is the way that dialect writes a row's identity.
            if self.dialect == Dialect::Three {
                return Ok(Value::Integer(row.rowid));
            }
            if let Some(score) = row.score {
                return Ok(Value::Real(score));
            }
            if self.phrases.is_empty() {
                // A row reached without a `MATCH` has nothing to score against,
                // which is what SQLite answers zero for.
                return Ok(Value::Real(0.0));
            }
            // The same arithmetic `bm25::score` would have done in `filter`,
            // for this row alone: `rank` *is* `bm25` with every weight one, so
            // the two cannot disagree.
            self.ensure_hits(context)?;
            let weights = vec![1.0f64; self.columns];
            let sizes = bm25::row_sizes(context, &self.shadows, row.rowid, self.columns)?;
            return Ok(Value::Real(bm25::score_row(
                row.rowid,
                &self.matched,
                &self.phrases,
                &self.totals,
                &sizes,
                &weights,
            )));
        }
        if index as i32 == self.match_column {
            // The hidden column that carries the query is NULL when it is read
            // as a value; it exists to be *constrained*, not to be selected.
            return Ok(Value::Null);
        }
        if self.contentless {
            return Ok(Value::Null);
        }
        if self.held.as_ref().map(|(rowid, _)| *rowid) != Some(row.rowid) {
            self.resolve_offsets(context);
            self.held = self
                .shadows
                .read_row(context, &self.content.clone(), row.rowid)?
                .map(|values| (row.rowid, values));
        }
        let Some((_, content)) = self.held.as_ref() else {
            return Ok(Value::Null);
        };
        Ok(self
            .offsets
            .get(index)
            .and_then(|at| content.get(*at))
            .cloned()
            .unwrap_or(Value::Null))
    }

    /// Returns the row's rowid.
    fn rowid(&self) -> DbResult<i64> {
        Ok(self.rows.get(self.at).map(|row| row.rowid).unwrap_or(0))
    }

    /// Answers `bm25(t [, weight...])` on the current row.
    ///
    /// The weights are per call, which is why this cannot be a column: the
    /// same row scores differently in `bm25(docs)` and `bm25(docs, 10.0, 1.0)`
    /// in the same SELECT. A row reached without a `MATCH` scores zero, which
    /// is what SQLite answers for a function that had no query to score
    /// against.
    fn auxiliary(
        &mut self,
        context: &mut Context<'_>,
        name: &[u8],
        arguments: &[Value<'static>],
    ) -> DbResult<Value<'static>> {
        if name == b"highlight" {
            return self.highlight(context, arguments);
        }
        if name == b"snippet" {
            // **The same snippet, and a different argument order.** FTS3 takes
            // the markers first and the column fourth; FTS5 takes the column
            // first. Rewriting the call here rather than writing the routine
            // twice is what keeps the two dialects from drifting apart.
            if self.dialect == Dialect::Three {
                let start = arguments.first().cloned().unwrap_or(text_value(b"<b>"));
                let end = arguments.get(1).cloned().unwrap_or(text_value(b"</b>"));
                let ellipsis = arguments
                    .get(2)
                    .cloned()
                    .unwrap_or(text_value(b"<b>...</b>"));
                let column = arguments
                    .get(3)
                    .and_then(Value::as_integer)
                    .unwrap_or(-1)
                    .max(0);
                let tokens = arguments.get(4).and_then(Value::as_integer).unwrap_or(15);
                return self.snippet(
                    context,
                    &[
                        Value::Integer(column),
                        start,
                        end,
                        ellipsis,
                        Value::Integer(tokens),
                    ],
                );
            }
            return self.snippet(context, arguments);
        }
        if name == b"offsets" && self.dialect == Dialect::Three {
            return self.offsets(context);
        }
        if name == b"matchinfo" && self.dialect == Dialect::Three {
            return self.matchinfo(context, arguments);
        }
        // **`optimize(t)`, which is FTS3/4's and always has nothing to do
        // here.** SQLite merges an FTS3/4 index's b-tree segments into one and
        // answers `Index already optimal` when there is nothing to merge. This
        // index keeps one doclist per term - see `stage_doclist` - so there are
        // never segments to merge and the answer is the second case, always.
        // Saying so is the truthful answer; refusing the name was not, and it
        // was the last function of SQLite's register that this engine answered
        // to by another route and did not name.
        if name == b"optimize" && self.dialect == Dialect::Three {
            return Value::owned_text(b"Index already optimal");
        }
        if name != b"bm25" {
            return Err(crate::vtab::failure(format!(
                "no such function: {}",
                String::from_utf8_lossy(name)
            )));
        }
        let Some(row) = self.rows.get(self.at).cloned() else {
            return Ok(Value::Null);
        };
        if self.phrases.is_empty() {
            return Ok(Value::Real(0.0));
        }
        let mut weights = vec![1.0f64; self.columns];
        for (slot, argument) in weights.iter_mut().zip(arguments.iter()) {
            *slot = argument.as_real().unwrap_or(1.0);
        }
        // The hits are the query's, so the same maps score every row of it.
        self.ensure_hits(context)?;
        let hits = self.matched.clone();
        let sizes = bm25::row_sizes(context, &self.shadows, row.rowid, self.columns)?;
        Ok(Value::Real(bm25::score_row(
            row.rowid,
            &hits,
            &self.phrases,
            &self.totals,
            &sizes,
            &weights,
        )))
    }
}
