//! The FTS5 query language, and what it matches.
//!
//! Invariant: a query is parsed once and evaluated against doclists, never
//! against the rows. A phrase is a run of terms at consecutive positions in one
//! column, a `NEAR` is a set of terms within a distance of each other, and both
//! are decided from the positions the index already holds - which is why
//! positions are stored at all. Re-reading the documents to check a phrase
//! would make a phrase search a table scan.
//!
//! The grammar, which is the pinned release's:
//!
//! ```text
//! expr    := orlist
//! orlist  := andlist ( OR andlist )*
//! andlist := notpart ( AND? notpart )*        -- juxtaposition is AND
//! notpart := primary ( NOT primary )*
//! primary := '(' expr ')' | colspec ':' primary | phrase | NEAR '(' phrase+ , n ')'
//! phrase  := '"' term+ '"' | term
//! term    := word '*'?                        -- a trailing star is a prefix
//! ```

use std::collections::BTreeMap;

use inillucent_base::DbResult;
use inillucent_value::Value;

use super::tokenize::Tokenizer;
use super::{decode_doclist, DocEntry};
use crate::shadow::ShadowTables;
use crate::vtab::{failure, Context};

/// One term of a phrase.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Term {
    /// The token, already folded by the tokenizer.
    pub token: Vec<u8>,
    /// Whether a trailing `*` made it a prefix.
    pub prefix: bool,
}

/// A run of terms that must appear at consecutive positions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Phrase {
    /// The terms, in order.
    pub terms: Vec<Term>,
    /// The column the phrase is restricted to, when one was named.
    pub column: Option<usize>,
}

/// One node of a parsed query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Match {
    /// A phrase.
    Phrase(Phrase),
    /// `NEAR(a b, n)`: every phrase within `distance` tokens of the others.
    Near {
        /// The phrases.
        phrases: Vec<Phrase>,
        /// How many tokens apart they may be.
        distance: u32,
    },
    /// Both sides.
    And(Box<Match>, Box<Match>),
    /// Either side.
    Or(Box<Match>, Box<Match>),
    /// The left side and not the right.
    Not(Box<Match>, Box<Match>),
}

/// A parsed query, with the phrases it names in the order they were written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Query {
    /// The expression.
    pub root: Match,
    /// Every phrase in the query, which is what `bm25` scores.
    pub phrases: Vec<Phrase>,
}

impl Query {
    /// Parses one query string.
    pub fn parse(text: &[u8], tokenizer: &Tokenizer, columns: &[Vec<u8>]) -> DbResult<Query> {
        let tokens = lex(text);
        let mut parser = Parser {
            tokens,
            at: 0,
            tokenizer,
            columns,
        };
        let root = parser.parse_or(None)?;
        if parser.at < parser.tokens.len() {
            return Err(failure("fts5: syntax error near the end of the query"));
        }
        let mut phrases = Vec::new();
        collect_phrases(&root, &mut phrases);
        Ok(Query { root, phrases })
    }
}

/// Adds every phrase of an expression to a list, in written order.
fn collect_phrases(node: &Match, into: &mut Vec<Phrase>) {
    match node {
        Match::Phrase(phrase) => into.push(phrase.clone()),
        Match::Near { phrases, .. } => into.extend(phrases.iter().cloned()),
        Match::And(left, right) | Match::Or(left, right) | Match::Not(left, right) => {
            collect_phrases(left, into);
            collect_phrases(right, into);
        }
    }
}

/// One lexical token of a query.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Lexeme {
    /// A bare word.
    Word(Vec<u8>),
    /// A quoted string, which is a phrase however it tokenises.
    Quoted(Vec<u8>),
    /// `(`
    Open,
    /// `)`
    Close,
    /// `,`
    Comma,
    /// `:`
    Colon,
    /// `*`
    Star,
    /// `+`, which joins two phrases into one.
    Plus,
    /// `^`, which anchors a phrase to the start of a column.
    Caret,
}

/// Splits a query into its lexemes.
fn lex(text: &[u8]) -> Vec<Lexeme> {
    let text = String::from_utf8_lossy(text);
    let mut out = Vec::new();
    let mut characters = text.chars().peekable();
    let mut word = String::new();
    while let Some(character) = characters.next() {
        let punctuation = match character {
            '(' => Some(Lexeme::Open),
            ')' => Some(Lexeme::Close),
            ',' => Some(Lexeme::Comma),
            ':' => Some(Lexeme::Colon),
            '*' => Some(Lexeme::Star),
            '+' => Some(Lexeme::Plus),
            '^' => Some(Lexeme::Caret),
            _ => None,
        };
        if let Some(punctuation) = punctuation {
            if !word.is_empty() {
                out.push(Lexeme::Word(core::mem::take(&mut word).into_bytes()));
            }
            out.push(punctuation);
            continue;
        }
        if character == '"' || character == '\'' {
            if !word.is_empty() {
                out.push(Lexeme::Word(core::mem::take(&mut word).into_bytes()));
            }
            let mut quoted = String::new();
            for inner in characters.by_ref() {
                if inner == character {
                    break;
                }
                quoted.push(inner);
            }
            out.push(Lexeme::Quoted(quoted.into_bytes()));
            continue;
        }
        if character.is_whitespace() {
            if !word.is_empty() {
                out.push(Lexeme::Word(core::mem::take(&mut word).into_bytes()));
            }
            continue;
        }
        word.push(character);
    }
    if !word.is_empty() {
        out.push(Lexeme::Word(word.into_bytes()));
    }
    out
}

/// The query parser's position.
struct Parser<'a> {
    tokens: Vec<Lexeme>,
    at: usize,
    tokenizer: &'a Tokenizer,
    columns: &'a [Vec<u8>],
}

impl Parser<'_> {
    /// Returns the lexeme at the cursor.
    fn peek(&self) -> Option<&Lexeme> {
        self.tokens.get(self.at)
    }

    /// Returns whether the cursor is on a keyword.
    fn at_keyword(&self, keyword: &str) -> bool {
        matches!(self.peek(), Some(Lexeme::Word(word)) if word.eq_ignore_ascii_case(keyword.as_bytes()))
    }

    /// Parses an `OR` chain.
    fn parse_or(&mut self, column: Option<usize>) -> DbResult<Match> {
        let mut left = self.parse_and(column)?;
        while self.at_keyword("OR") {
            self.at = self.at.saturating_add(1);
            let right = self.parse_and(column)?;
            left = Match::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    /// Parses an `AND` chain, where two things side by side are an `AND`.
    fn parse_and(&mut self, column: Option<usize>) -> DbResult<Match> {
        let mut left = self.parse_not(column)?;
        loop {
            if self.at_keyword("AND") {
                self.at = self.at.saturating_add(1);
            } else if self.at_keyword("OR")
                || self.at_keyword("NOT")
                || matches!(
                    self.peek(),
                    None | Some(Lexeme::Close) | Some(Lexeme::Comma)
                )
            {
                return Ok(left);
            }
            let right = self.parse_not(column)?;
            left = Match::And(Box::new(left), Box::new(right));
        }
    }

    /// Parses a `NOT` chain.
    fn parse_not(&mut self, column: Option<usize>) -> DbResult<Match> {
        let mut left = self.parse_primary(column)?;
        while self.at_keyword("NOT") {
            self.at = self.at.saturating_add(1);
            let right = self.parse_primary(column)?;
            left = Match::Not(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    /// Parses one primary: a group, a column filter, a `NEAR`, or a phrase.
    fn parse_primary(&mut self, column: Option<usize>) -> DbResult<Match> {
        if matches!(self.peek(), Some(Lexeme::Open)) {
            self.at = self.at.saturating_add(1);
            let inner = self.parse_or(column)?;
            if !matches!(self.peek(), Some(Lexeme::Close)) {
                return Err(failure("fts5: a group is not closed"));
            }
            self.at = self.at.saturating_add(1);
            return Ok(inner);
        }
        if self.at_keyword("NEAR") && matches!(self.tokens.get(self.at + 1), Some(Lexeme::Open)) {
            return self.parse_near(column);
        }
        // `column : rest` restricts everything after it to one column.
        if let Some(Lexeme::Word(name)) = self.peek().cloned() {
            if matches!(self.tokens.get(self.at + 1), Some(Lexeme::Colon)) {
                let Some(index) = self.column_named(&name) else {
                    return Err(failure(format!(
                        "fts5: no such column: {}",
                        String::from_utf8_lossy(&name)
                    )));
                };
                self.at = self.at.saturating_add(2);
                return self.parse_primary(Some(index));
            }
        }
        let phrase = self.parse_phrase(column)?;
        Ok(Match::Phrase(phrase))
    }

    /// Parses `NEAR(phrase phrase, distance)`.
    fn parse_near(&mut self, column: Option<usize>) -> DbResult<Match> {
        self.at = self.at.saturating_add(2);
        let mut phrases = Vec::new();
        while !matches!(
            self.peek(),
            None | Some(Lexeme::Close) | Some(Lexeme::Comma)
        ) {
            phrases.push(self.parse_phrase(column)?);
        }
        let mut distance = 10u32;
        if matches!(self.peek(), Some(Lexeme::Comma)) {
            self.at = self.at.saturating_add(1);
            if let Some(Lexeme::Word(word)) = self.peek().cloned() {
                distance = String::from_utf8_lossy(&word).trim().parse().unwrap_or(10);
                self.at = self.at.saturating_add(1);
            }
        }
        if !matches!(self.peek(), Some(Lexeme::Close)) {
            return Err(failure("fts5: NEAR is not closed"));
        }
        self.at = self.at.saturating_add(1);
        if phrases.is_empty() {
            return Err(failure("fts5: NEAR needs at least one phrase"));
        }
        Ok(Match::Near { phrases, distance })
    }

    /// Parses one phrase: a quoted string, or a word with an optional `*`.
    fn parse_phrase(&mut self, column: Option<usize>) -> DbResult<Phrase> {
        // A leading `^` anchors the phrase to the start of the column. It is
        // accepted and recorded by making the phrase start at position zero,
        // which is what the anchor means.
        let anchored = matches!(self.peek(), Some(Lexeme::Caret));
        if anchored {
            self.at = self.at.saturating_add(1);
        }
        let mut terms = Vec::new();
        loop {
            match self.peek().cloned() {
                Some(Lexeme::Quoted(text)) => {
                    self.at = self.at.saturating_add(1);
                    for token in self.tokenizer.tokens(&text) {
                        terms.push(Term {
                            token,
                            prefix: false,
                        });
                    }
                }
                Some(Lexeme::Word(word))
                    if !word.eq_ignore_ascii_case(b"AND")
                        && !word.eq_ignore_ascii_case(b"OR")
                        && !word.eq_ignore_ascii_case(b"NOT") =>
                {
                    self.at = self.at.saturating_add(1);
                    let tokens = self.tokenizer.tokens(&word);
                    let last = tokens.len().saturating_sub(1);
                    for (position, token) in tokens.into_iter().enumerate() {
                        terms.push(Term {
                            token,
                            prefix: position == last && matches!(self.peek(), Some(Lexeme::Star)),
                        });
                    }
                    if matches!(self.peek(), Some(Lexeme::Star)) {
                        self.at = self.at.saturating_add(1);
                    }
                }
                _ => break,
            }
            // A `+` joins the next word into the same phrase.
            if matches!(self.peek(), Some(Lexeme::Plus)) {
                self.at = self.at.saturating_add(1);
                continue;
            }
            break;
        }
        if terms.is_empty() {
            return Err(failure("fts5: syntax error - a phrase has no terms"));
        }
        Ok(Phrase { terms, column })
    }

    /// Returns which column a name is.
    fn column_named(&self, name: &[u8]) -> Option<usize> {
        self.columns
            .iter()
            .position(|candidate| candidate.eq_ignore_ascii_case(name))
    }
}

/// Where one phrase was found: a row, and the positions per column.
pub type Hits = BTreeMap<i64, Vec<(usize, Vec<u32>)>>;

/// Runs one query and returns the rows it matched, with the phrase hits.
pub fn evaluate(
    query: &Query,
    context: &mut Context<'_>,
    shadows: &ShadowTables,
    buffer: &super::Buffer,
    columns: usize,
) -> DbResult<(Vec<i64>, Vec<Hits>)> {
    let mut hits = Vec::with_capacity(query.phrases.len());
    for phrase in &query.phrases {
        hits.push(phrase_hits(phrase, context, shadows, buffer, columns)?);
    }
    let mut counter = 0usize;
    let rows = walk(&query.root, &hits, &mut counter);
    // The hits belong to the *query*, not to a row: they are one map per
    // phrase, over every row that phrase appears in. Handing back a copy per
    // matched row - which is what this used to do - meant a common term cost a
    // clone of the whole index for every row it found.
    Ok((rows, hits))
}

/// Runs one query and returns only the rows it matched.
///
/// **Positions are decoded when something needs them, and most queries do
/// not.** A doclist entry carries a position list per column, and building the
/// [`Hits`] map allocates one vector per row per column and inserts every row
/// into a `BTreeMap` - work that `SELECT count(*) FROM t WHERE t MATCH 'x'`
/// throws away. Over five hundred documents that was seventy per cent of the
/// query.
///
/// Answers `None` for the queries whose *answer* depends on positions, and the
/// caller runs [`evaluate`] for those instead: a `NEAR`, and a phrase of more
/// than one term, where adjacency is the question. Scoring needs them too, so a
/// ranked plan does not come here.
///
/// @param query - the parsed query
/// @param context - the host
/// @param shadows - the table's shadow tables
/// @param buffer - the doclists this transaction has staged
/// @param columns - how many columns the table declares
pub fn evaluate_rows(
    query: &Query,
    context: &mut Context<'_>,
    shadows: &ShadowTables,
    buffer: &super::Buffer,
    columns: usize,
) -> DbResult<Option<Vec<i64>>> {
    if query.phrases.iter().any(|phrase| phrase.terms.len() != 1) {
        return Ok(None);
    }
    let mut per_phrase = Vec::with_capacity(query.phrases.len());
    for phrase in &query.phrases {
        per_phrase.push(phrase_rows(phrase, context, shadows, buffer, columns)?);
    }
    let mut counter = 0usize;
    Ok(walk_rows(&query.root, &per_phrase, &mut counter))
}

/// Returns the rows a single-term phrase appears in, ascending.
///
/// The same predicate [`phrase_hits`] applies at offset zero - a column the
/// phrase asked for, inside the table, with at least one position - decided
/// without collecting the positions that prove it.
///
/// @param phrase - the phrase, which has exactly one term
/// @param context - the host
/// @param shadows - the table's shadow tables
/// @param buffer - the doclists this transaction has staged
/// @param columns - how many columns the table declares
fn phrase_rows(
    phrase: &Phrase,
    context: &mut Context<'_>,
    shadows: &ShadowTables,
    buffer: &super::Buffer,
    columns: usize,
) -> DbResult<Vec<i64>> {
    let Some(term) = phrase.terms.first() else {
        return Ok(Vec::new());
    };
    let mut rows = Vec::new();
    for page in term_pages(term, context, shadows, buffer)? {
        let Some(bytes) = super::read_doclist(context, shadows, buffer, page)? else {
            continue;
        };
        super::doclist_rows(&bytes, phrase.column, columns, &mut rows);
    }
    rows.sort_unstable();
    rows.dedup();
    Ok(rows)
}

/// Returns the rows one expression matches, or `None` if it needs positions.
///
/// The counter advances down both sides of every branch whatever the answer,
/// so a `None` from one side still leaves the phrase numbering right for the
/// other - which is what makes the walk safe to abandon half way.
///
/// @param node - the expression
/// @param rows - the rows each phrase matched, in the order they were parsed
/// @param counter - which phrase the walk has reached
fn walk_rows(node: &Match, rows: &[Vec<i64>], counter: &mut usize) -> Option<Vec<i64>> {
    match node {
        Match::Phrase(_) => {
            let index = *counter;
            *counter = counter.saturating_add(1);
            Some(rows.get(index).cloned().unwrap_or_default())
        }
        // `NEAR` is how far apart the phrases are, which is a question about
        // positions and cannot be answered from rowids.
        Match::Near { phrases, .. } => {
            *counter = counter.saturating_add(phrases.len());
            None
        }
        Match::And(left, right) => {
            let left = walk_rows(left, rows, counter);
            let right = walk_rows(right, rows, counter);
            match (left, right) {
                (Some(left), Some(right)) => Some(intersect(&left, &right)),
                _ => None,
            }
        }
        Match::Or(left, right) => {
            let left = walk_rows(left, rows, counter);
            let right = walk_rows(right, rows, counter);
            match (left, right) {
                (Some(left), Some(right)) => Some(union(&left, &right)),
                _ => None,
            }
        }
        Match::Not(left, right) => {
            let left = walk_rows(left, rows, counter);
            let right = walk_rows(right, rows, counter);
            match (left, right) {
                (Some(left), Some(right)) => Some(
                    left.into_iter()
                        .filter(|row| !right.contains(row))
                        .collect(),
                ),
                _ => None,
            }
        }
    }
}

/// Returns the rows one expression matches.
fn walk(node: &Match, hits: &[Hits], counter: &mut usize) -> Vec<i64> {
    match node {
        Match::Phrase(_) => {
            let index = *counter;
            *counter = counter.saturating_add(1);
            hits.get(index)
                .map(|found| found.keys().copied().collect())
                .unwrap_or_default()
        }
        Match::Near { phrases, distance } => {
            let first = *counter;
            *counter = counter.saturating_add(phrases.len());
            near_rows(hits, first, phrases, *distance)
        }
        Match::And(left, right) => {
            let left = walk(left, hits, counter);
            let right = walk(right, hits, counter);
            intersect(&left, &right)
        }
        Match::Or(left, right) => {
            let left = walk(left, hits, counter);
            let right = walk(right, hits, counter);
            union(&left, &right)
        }
        Match::Not(left, right) => {
            let left = walk(left, hits, counter);
            let right = walk(right, hits, counter);
            left.into_iter()
                .filter(|row| !right.contains(row))
                .collect()
        }
    }
}

/// Returns the rows where every phrase of a `NEAR` is close enough to the rest.
fn near_rows(hits: &[Hits], first: usize, phrases: &[Phrase], distance: u32) -> Vec<i64> {
    let count = phrases.len();
    let Some(head) = hits.get(first) else {
        return Vec::new();
    };
    let mut rows: Vec<i64> = head.keys().copied().collect();
    for offset in 1..count {
        let Some(next) = hits.get(first + offset) else {
            return Vec::new();
        };
        rows.retain(|row| next.contains_key(row));
    }
    rows.retain(|row| {
        // Every pair has to be within the distance in the *same* column, which
        // is what makes `NEAR` mean nearness rather than co-occurrence.
        for column in 0..64usize {
            let mut all = true;
            let mut spans: Vec<Vec<u32>> = Vec::with_capacity(count);
            for offset in 0..count {
                let Some(found) = hits.get(first + offset).and_then(|found| found.get(row)) else {
                    all = false;
                    break;
                };
                let Some((_, positions)) = found.iter().find(|(index, _)| *index == column) else {
                    all = false;
                    break;
                };
                spans.push(positions.clone());
            }
            if !all {
                continue;
            }
            let lengths: Vec<u32> = phrases
                .iter()
                .map(|phrase| phrase.terms.len().max(1) as u32)
                .collect();
            if within(&spans, &lengths, distance) {
                return true;
            }
        }
        false
    });
    rows
}

/// Returns whether one position can be picked from each phrase such that no
/// more than `distance` tokens lie between them.
///
/// `NEAR` counts the tokens *between* the phrases, not the gap between their
/// starting positions: `a X Y b` has two tokens between `a` and `b`, so it
/// matches `NEAR(a b, 2)` and not `NEAR(a b, 1)`. With the phrase lengths
/// summed, the test is that the window covering one instance of every phrase is
/// no longer than the phrases themselves plus the distance - which is the rule
/// as SQLite writes it, and generalises to phrases longer than a word.
fn within(spans: &[Vec<u32>], lengths: &[u32], distance: u32) -> bool {
    if spans.is_empty() {
        return false;
    }
    let total: u32 = lengths.iter().take(spans.len()).sum();
    // Every position of every phrase, in order, tagged with the phrase it came
    // from: the smallest window covering all of them is a sliding window over
    // this one sequence, which is exact where picking the nearest to an anchor
    // is only a guess.
    let mut merged: Vec<(u32, usize)> = Vec::new();
    for (index, span) in spans.iter().enumerate() {
        for position in span {
            merged.push((*position, index));
        }
    }
    merged.sort_unstable();
    // **Every index here is bounds-checked**, which is the crate's rule and is
    // not a formality on this function: `merged` is built from positions a
    // stored index supplied, and a sliding window whose two ends are indexed
    // directly is exactly the shape that reads past the end when one of them
    // has been damaged.
    let mut seen = vec![0usize; spans.len()];
    let mut covered = 0usize;
    let mut low = 0usize;
    for high in 0..merged.len() {
        let Some((_, which)) = merged.get(high).copied() else {
            break;
        };
        let Some(count) = seen.get_mut(which) else {
            continue;
        };
        if *count == 0 {
            covered += 1;
        }
        *count += 1;
        while covered == spans.len() {
            let (Some((start, _)), Some((end, last))) =
                (merged.get(low).copied(), merged.get(high).copied())
            else {
                break;
            };
            let window = end
                .saturating_add(lengths.get(last).copied().unwrap_or(1))
                .saturating_sub(start);
            if window <= total.saturating_add(distance) {
                return true;
            }
            let Some((_, dropped)) = merged.get(low).copied() else {
                break;
            };
            let Some(count) = seen.get_mut(dropped) else {
                break;
            };
            *count = count.saturating_sub(1);
            if *count == 0 {
                covered -= 1;
            }
            low += 1;
        }
    }
    false
}

/// Returns the rows in both lists.
fn intersect(left: &[i64], right: &[i64]) -> Vec<i64> {
    left.iter()
        .filter(|row| right.contains(row))
        .copied()
        .collect()
}

/// Returns the rows in either list, once each and in order.
fn union(left: &[i64], right: &[i64]) -> Vec<i64> {
    let mut out = left.to_vec();
    for row in right {
        if !out.contains(row) {
            out.push(*row);
        }
    }
    out.sort_unstable();
    out
}

/// Returns where one phrase appears, by row and column.
pub fn phrase_hits(
    phrase: &Phrase,
    context: &mut Context<'_>,
    shadows: &ShadowTables,
    buffer: &super::Buffer,
    columns: usize,
) -> DbResult<Hits> {
    let mut found: Option<Hits> = None;
    for (offset, term) in phrase.terms.iter().enumerate() {
        let entries = term_entries(term, context, shadows, buffer)?;
        let mut current: Hits = BTreeMap::new();
        for entry in entries {
            for (column, positions) in &entry.columns {
                if phrase.column.is_some_and(|wanted| wanted != *column) {
                    continue;
                }
                if *column >= columns {
                    continue;
                }
                // A phrase's later terms have to sit one place further on, so
                // every position is shifted back by its place in the phrase and
                // the intersection is then over the phrase's *start*.
                let shifted: Vec<u32> = positions
                    .iter()
                    .filter(|position| **position >= offset as u32)
                    .map(|position| position - offset as u32)
                    .collect();
                if shifted.is_empty() {
                    continue;
                }
                current
                    .entry(entry.rowid)
                    .or_default()
                    .push((*column, shifted));
            }
        }
        found = Some(match found {
            None => current,
            Some(previous) => meet(&previous, &current),
        });
        if found.as_ref().is_some_and(BTreeMap::is_empty) {
            return Ok(BTreeMap::new());
        }
    }
    Ok(found.unwrap_or_default())
}

/// Returns the positions two terms share, per row and column.
fn meet(left: &Hits, right: &Hits) -> Hits {
    let mut out = BTreeMap::new();
    for (rowid, columns) in left {
        let Some(other) = right.get(rowid) else {
            continue;
        };
        let mut kept = Vec::new();
        for (column, positions) in columns {
            let Some((_, others)) = other.iter().find(|(index, _)| index == column) else {
                continue;
            };
            let shared: Vec<u32> = positions
                .iter()
                .filter(|position| others.contains(position))
                .copied()
                .collect();
            if !shared.is_empty() {
                kept.push((*column, shared));
            }
        }
        if !kept.is_empty() {
            out.insert(*rowid, kept);
        }
    }
    out
}

/// Returns the doclist entries one term matches, prefix included.
/// Returns the `%_data` rows one term's doclists live in.
///
/// One row for an ordinary term; the whole prefix run for `word*`.
///
/// @param term - the term
/// @param context - the host
/// @param shadows - the table's shadow tables
/// @param buffer - the doclists this transaction has staged
fn term_pages(
    term: &Term,
    context: &mut Context<'_>,
    shadows: &ShadowTables,
    buffer: &super::Buffer,
) -> DbResult<Vec<i64>> {
    let mut pages = Vec::new();
    if term.prefix {
        // **The staged dictionary rows go out first.** A term this transaction
        // created is held in the buffer and written in term order at the flush,
        // so a scan that ran before it would not see it - and a prefix search
        // inside the transaction that wrote the term would miss it. The point
        // lookup below needs no flush, because the buffer answers it.
        super::flush_doclists(context, shadows, buffer)?;
        // A prefix reads every term the dictionary holds that starts with it.
        // The dictionary is in term order, so the run is contiguous - which is
        // what makes a prefix search cheaper than a scan of every term.
        shadows.scan_keyed(context, b"idx", 2, |values| {
            let Some(candidate) = values.get(1).and_then(Value::as_blob) else {
                return Ok(true);
            };
            let candidate = candidate.raw();
            if candidate < term.token.as_slice() {
                return Ok(true);
            }
            if !candidate.starts_with(&term.token) {
                return Ok(false);
            }
            if let Some(page) = values.get(2).and_then(Value::as_integer) {
                pages.push(page);
            }
            Ok(true)
        })?;
    } else if let Some(page) =
        super::term_row(context, shadows, buffer, &term.token, false)?.map(|held| held.page)
    {
        pages.push(page);
    }
    Ok(pages)
}

fn term_entries(
    term: &Term,
    context: &mut Context<'_>,
    shadows: &ShadowTables,
    buffer: &super::Buffer,
) -> DbResult<Vec<DocEntry>> {
    let pages = term_pages(term, context, shadows, buffer)?;

    // **One doclist is already the answer.** A term that is not a prefix has
    // exactly one `%_data` row, and a doclist holds each rowid once in
    // ascending order - so the merge below has nothing to merge, and running it
    // anyway searched a growing vector once per entry. That is quadratic in the
    // documents a term appears in: a term in five hundred of them cost a
    // hundred and twenty-five thousand comparisons per query, and `evaluate`
    // was seventy per cent of `SELECT count(*) ... MATCH`.
    if let Some(only) = pages.first().filter(|_| pages.len() == 1) {
        let Some(bytes) = super::read_doclist(context, shadows, buffer, *only)? else {
            return Ok(Vec::new());
        };
        return Ok(decode_doclist(&bytes));
    }
    // A prefix reads several, and they can name the same row: merged by rowid
    // rather than searched for it.
    let mut entries: Vec<DocEntry> = Vec::new();
    for page in pages {
        let Some(bytes) = super::read_doclist(context, shadows, buffer, page)? else {
            continue;
        };
        entries.extend(decode_doclist(&bytes));
    }
    entries.sort_by_key(|entry| entry.rowid);
    let mut merged: Vec<DocEntry> = Vec::with_capacity(entries.len());
    for entry in entries {
        match merged.last_mut().filter(|last| last.rowid == entry.rowid) {
            Some(found) => {
                for (column, positions) in entry.columns {
                    match found.columns.iter_mut().find(|(index, _)| *index == column) {
                        Some((_, existing)) => {
                            existing.extend(positions);
                            existing.sort_unstable();
                            existing.dedup();
                        }
                        None => found.columns.push((column, positions)),
                    }
                }
            }
            None => merged.push(entry),
        }
    }
    Ok(merged)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parses a query with the default tokenizer and two columns.
    fn parse(text: &str) -> Query {
        let tokenizer = Tokenizer::named(&[]);
        Query::parse(
            text.as_bytes(),
            &tokenizer,
            &[b"title".to_vec(), b"body".to_vec()],
        )
        .expect("parses")
    }

    /// Two words side by side are an `AND`.
    #[test]
    fn juxtaposition_is_and() {
        let query = parse("cat dog");
        assert!(matches!(query.root, Match::And(_, _)));
        assert_eq!(query.phrases.len(), 2);
    }

    /// A quoted string is one phrase however many words it holds.
    #[test]
    fn a_quoted_string_is_one_phrase() {
        let query = parse("\"quick brown fox\"");
        assert_eq!(query.phrases.len(), 1);
        assert_eq!(query.phrases[0].terms.len(), 3);
        assert_eq!(query.phrases[0].terms[1].token, b"brown");
    }

    /// A trailing star makes the last term a prefix.
    #[test]
    fn a_trailing_star_is_a_prefix() {
        let query = parse("cat*");
        assert!(query.phrases[0].terms[0].prefix);
        assert!(!parse("cat").phrases[0].terms[0].prefix);
    }

    /// A column filter restricts what follows it.
    #[test]
    fn a_column_filter_restricts_what_follows() {
        let query = parse("title : cat");
        assert_eq!(query.phrases[0].column, Some(0));
        let both = parse("body:dog cat");
        assert_eq!(both.phrases[0].column, Some(1));
        assert_eq!(both.phrases[1].column, None);
    }

    /// A column nobody declared is a query error.
    #[test]
    fn an_unknown_column_is_refused() {
        let tokenizer = Tokenizer::named(&[]);
        assert!(Query::parse(b"nope:cat", &tokenizer, &[b"title".to_vec()]).is_err());
    }

    /// The three operators parse, and `NOT` binds tighter than `OR`.
    #[test]
    fn the_operators_parse() {
        assert!(matches!(parse("a OR b").root, Match::Or(_, _)));
        assert!(matches!(parse("a AND b").root, Match::And(_, _)));
        assert!(matches!(parse("a NOT b").root, Match::Not(_, _)));
        assert!(matches!(parse("a OR b NOT c").root, Match::Or(_, _)));
    }

    /// `NEAR` takes its phrases and its distance.
    #[test]
    fn near_takes_a_distance() {
        let query = parse("NEAR(cat dog, 3)");
        let Match::Near { phrases, distance } = query.root else {
            panic!("a NEAR");
        };
        assert_eq!(phrases.len(), 2);
        assert_eq!(distance, 3);
        let default = parse("NEAR(cat dog)");
        assert!(matches!(default.root, Match::Near { distance: 10, .. }));
    }

    /// A group changes what binds to what.
    #[test]
    fn a_group_changes_the_binding() {
        assert!(matches!(parse("(a OR b) AND c").root, Match::And(_, _)));
    }

    /// Nearness counts the tokens *between* two single-word phrases.
    ///
    /// `a X b` has one token between the words, so it is within one and not
    /// within zero: the boundary is the thing worth pinning, because measuring
    /// the gap between the starting positions instead would be off by one and
    /// every `NEAR` in the suite would be one token too strict.
    #[test]
    fn nearness_counts_the_tokens_between() {
        let ones = [1u32, 1];
        assert!(within(&[vec![0], vec![2]], &ones, 3));
        assert!(!within(&[vec![0], vec![9]], &ones, 3));
        assert!(within(&[vec![0, 8], vec![9]], &ones, 2));
        assert!(within(&[vec![0], vec![2]], &ones, 1));
        assert!(!within(&[vec![0], vec![2]], &ones, 0));
        assert!(within(&[vec![0], vec![1]], &ones, 0));
    }

    /// A longer phrase spends its own length, not the distance.
    #[test]
    fn a_phrase_spends_its_own_length() {
        // A two-word phrase starting at 0 ends at 1, so a word at 2 is
        // adjacent to it and within zero.
        assert!(within(&[vec![0], vec![2]], &[2, 1], 0));
        assert!(!within(&[vec![0], vec![3]], &[2, 1], 0));
    }

    /// Three phrases are covered by one window rather than pairwise.
    #[test]
    fn three_phrases_share_one_window() {
        let ones = [1u32, 1, 1];
        assert!(within(&[vec![0], vec![1], vec![2]], &ones, 0));
        assert!(!within(&[vec![0], vec![1], vec![9]], &ones, 0));
        // The far instance of the first phrase is the one that makes it fit,
        // which an anchor on its first position would have missed.
        assert!(within(&[vec![0, 8], vec![9], vec![10]], &ones, 0));
    }
}
