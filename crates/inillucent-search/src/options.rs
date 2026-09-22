//! What a `CREATE VIRTUAL TABLE ... USING inillucent_search(...)` said, and what
//! the module therefore promises.
//!
//! Invariant: every behaviour a caller could otherwise only discover by
//! measuring is written down here and stored in the table's own `%_config`
//! shadow table. Whether a result is exact or approximate, which distance is
//! being minimised, which tokenizer produced the terms, and what the score
//! means are all *declarations*, because a retrieval engine that silently
//! answers approximately is one whose answers cannot be checked.
//!
//! This is also the line between this module and FTS5. FTS5 is a parity
//! feature: it exists so a database SQLite wrote can be read, and its answers
//! are compared against the pinned release. `inillucent_search` is not a parity
//! feature and does not impersonate one - it is the BM25/HNSW/hybrid engine
//! this repository already had, given a transactional home and a SQL front
//! door.

use inillucent_base::DbResult;
use inillucent_ext::vtab::failure;

/// The tokenizer this build has, named so a stored table records which one
/// produced its terms.
///
/// One name, not a family: `inillucent-core` has exactly one tokenizer - Porter
/// stemming with an English stop list, compound identifier splitting and
/// address preservation - and a `tokenize=` option that accepted a name it then
/// ignored would be a promise the index does not keep.
pub const TOKENIZER: &str = "porter";

/// The format version stamped into `%_config` by a table that declares no
/// facet column.
///
/// A table written by a later layout is refused rather than misread, which is
/// the same rule `inillucent_core::persist` applies to a generation directory and
/// for the same reason: an index that answers differently from the one that was
/// written is worse than one that will not open.
pub const FORMAT: i64 = 1;

/// The format version stamped into `%_config` by a table that declares a facet
/// column.
///
/// **A second number rather than a bumped one, because the refusal has to be as
/// narrow as the change** (task-2067). A facet column is stored beside the text
/// columns and is deliberately *not* indexed as text, and a build that does not
/// know the word reads the `columns` row, finds the right number of columns in
/// the right order, and indexes the facet's value as prose. Nothing about that
/// misreads the file - it answers a different ranking from the one the table was
/// written to answer, which is the failure [`FORMAT`] exists to prevent, and
/// it does it silently.
///
/// Raising [`FORMAT`] itself would have refused every table already on disk, so
/// the two numbers sit side by side: a table with no facet keeps writing `1` and
/// an older build opens it exactly as before, and a table with a facet writes
/// `2` and an older build refuses it by name and says which release to install.
pub const FORMAT_FACETED: i64 = 2;

/// Every format this build reads, newest last.
pub const FORMATS: [i64; 2] = [FORMAT, FORMAT_FACETED];

/// The word that declares a column a facet, written after its name.
///
/// FTS5 spells a column option this way - `CREATE VIRTUAL TABLE t USING
/// fts5(a, b UNINDEXED)` - and a caller who knows that grammar knows this one.
pub const FACET_WORD: &str = "facet";

/// How many columns of a table may be reached by a facet constraint.
///
/// The access path records one character per claimed constraint, and a facet's
/// character is its column's position, so the position has to fit in one. A
/// search table with twenty-six filterable columns is not a shape anybody
/// writes; a declaration that asks for one is refused rather than silently
/// having its constraint evaluated after the ranking instead of inside it.
pub const MAX_FACET_COLUMN: usize = 26;

/// The largest vector width the module accepts.
///
/// Not a limit of the engine so much as a limit on what a corrupt or hostile
/// `CREATE` statement can make it allocate: `dims` multiplies every row.
pub const MAX_DIMS: usize = 16_384;

/// The default number of hits a search returns when the caller names none.
pub const DEFAULT_K: i64 = 10;

/// How long a delta log grows before a commit flushes it into a segment.
///
/// **A constant, and it stopped being a share of the corpus in task-1911.**
/// The share existed for a reason this comment used to state: a flush rewrote
/// the whole base generation, so folding a two-row delta into a million-row
/// index would spend a linear rebuild to save two linear merges, and the
/// trigger therefore had to grow with the table to keep the amortised cost of a
/// write independent of its size.
///
/// Segmented generations removed that premise. A flush now builds a segment out
/// of its own batch and writes nothing else, so flushing often is no longer
/// expensive - and while the trigger stayed proportional to the table, **the
/// batch was**, which is the thing the share was meant to protect against
/// wearing a different hat. Measured on the 100,000 document arm of
/// `write_latency` with segments in place and the share still set: the worst
/// commit was 15.8 seconds, against 19.3 before segments existed at all. The
/// segments were doing their work and the cadence was undoing it.
///
/// `COMPACT_SHARE` is gone rather than set to a large number, because a share
/// of the corpus is not a tuning of this idea, it is the previous one.
pub const COMPACT_FLOOR: u64 = 1024;

/// How many segments a level holds before they merge into one at the next
/// level, when the declaration names none.
///
/// FTS5's own `automerge` default is 4, for the same trade this is: low
/// enough that a query never has to fold more than a handful of segments
/// together, high enough that an ordinary write does not pay a merge every
/// few commits. A table that writes far more often than it is queried can
/// raise `segment_merge` to spend less time merging and more segments at
/// query time; one that is queried far more than it is written can lower it
/// to the opposite trade. Two is the floor - below that a "merge" would be
/// renaming one segment, not combining anything.
pub const DEFAULT_SEGMENT_MERGE: usize = 4;

/// How many chunks one commit may fold while merging segments, before it
/// checkpoints what it has done and leaves the rest for a later commit to
/// continue, when the declaration names none.
///
/// **Chunks, because that is what a fold actually pays for.** Building an
/// accumulator by folding a segment in costs one graph insertion per live
/// chunk that segment holds (`merge::fold_segment`), and nothing else in a
/// merge scales with anything but that count - not the number of segments,
/// because a segment several levels up holds `segment_fanin` times what one
/// at the level below it does, so a bound stated in segments would let
/// exactly the largest merges - the ones this exists to cut down - blow
/// straight through it.
///
/// Eight batches' worth by default. Large enough that an ordinary level zero
/// or level one merge - the overwhelming majority of them - still finishes
/// inside the single commit that triggered it, so the common case pays no
/// extra checkpoint at all; small enough that the worst commit measured on
/// the 100,000 document arm of `write_latency` (a merge several levels up,
/// tens of thousands of chunks in one go) is cut into several much smaller
/// ones instead of paying for all of it at once.
pub const DEFAULT_MERGE_BUDGET: u64 = 8 * COMPACT_FLOOR;

/// How far a result may be from the exact answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Every vector comparison is made. The answer is the exact top k.
    Exact,
    /// The graph is traversed. The answer is the top k the traversal found.
    Approximate,
}

impl Mode {
    /// Returns the mode's name, as `%_config` stores it.
    pub fn name(self) -> &'static str {
        match self {
            Mode::Exact => "exact",
            Mode::Approximate => "approximate",
        }
    }

    /// Reads a mode back from its name.
    pub fn parse(text: &str) -> DbResult<Mode> {
        match text.trim().to_ascii_lowercase().as_str() {
            "exact" => Ok(Mode::Exact),
            "approximate" | "approx" => Ok(Mode::Approximate),
            // The sentence is the message and not only the detail, for the
            // reason given on `Metric::parse` below.
            other => Err(inillucent_base::error::statement_refusal(format!(
                "inillucent_search: mode must be exact or approximate, not {other}"
            ))),
        }
    }
}

/// Which distance the vector branch minimises.
///
/// **The index, not the functions.** `vector_distance_cos` and
/// `vector_distance_l2` both answer for any pair of vectors regardless of
/// this setting - the distance functions were never the limit. What this
/// declares is which one the graph underneath is built to minimise: the HNSW
/// graph is a structure over one distance, and a query asking for the other
/// one has to fall back to comparing every row, which is exactly what
/// `crates/inillucent-sql/src/plan.rs::vector_path` refuses to paper over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Metric {
    /// One minus the cosine similarity of two unit vectors.
    Cosine,
    /// Euclidean distance. Unlike cosine, this needs the stored vectors kept
    /// at their original magnitude - see `inillucent_core::distance::Metric`,
    /// which this maps onto so the store built over a `metric = 'l2'` table
    /// actually keeps that promise.
    L2,
}

impl Metric {
    /// Returns the metric's name, as `%_config` stores it.
    pub fn name(self) -> &'static str {
        match self {
            Metric::Cosine => "cosine",
            Metric::L2 => "l2",
        }
    }

    /// Reads a metric back from its name, refusing one this build cannot honour.
    pub fn parse(text: &str) -> DbResult<Metric> {
        match text.trim().to_ascii_lowercase().as_str() {
            "cosine" => Ok(Metric::Cosine),
            "l2" => Ok(Metric::L2),
            // **The sentence is the message, not only the detail (task-1979,
            // section 8.1, gap 10).** `failure` sets the detail alone, and a
            // `DbError` with no message renders as `SQL logic error` - so
            // `WITH (metric = 'manhattan')` answered three words that name
            // neither the setting, the value nor the two that would have
            // worked. Somebody porting from pgvector writes a metric name
            // pgvector has and this build does not, which is the whole of how
            // this is reached.
            other => Err(inillucent_base::error::statement_refusal(format!(
                "inillucent_search: the only distances this build implements are cosine and l2, not {other}"
            ))),
        }
    }
}

/// Everything the `CREATE VIRTUAL TABLE` statement declared.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    /// The visible columns, in declaration order: the text ones and the facet
    /// ones together, because a row stores one value for each of them and the
    /// stored row's layout is their order.
    pub columns: Vec<Vec<u8>>,
    /// Which of `columns` are facets rather than text, ascending.
    ///
    /// A facet's value is stored and can be constrained inside a search; it is
    /// not indexed as prose, and it is not part of the text a query matches
    /// against. Keeping the positions rather than the names is what the row
    /// decoder and the access path both need, and the names are still
    /// `columns[position]`.
    pub facets: Vec<usize>,
    /// How many dimensions a row's vector has, or zero for a lexical-only table.
    pub dims: usize,
    /// The distance the vector branch minimises.
    pub metric: Metric,
    /// How many neighbours a graph node keeps per layer, when the index said.
    ///
    /// pgvector's `m`, and the same number: the graph uses `2 * m` on layer
    /// zero, as the original algorithm does. `None` leaves the build's own
    /// default, which is what an index that named no parameters gets.
    pub m: Option<usize>,
    /// How wide the build search is, when the index said.
    ///
    /// pgvector's `ef_construction`. Bigger is a slower build and a better
    /// graph, which is the whole trade the parameter exists to let a caller
    /// make.
    pub ef_construction: Option<usize>,
    /// How wide a query search is by default, when the index said.
    ///
    /// pgvector's `hnsw.ef_search`, which is a session setting there and an
    /// index setting here: it is named in `WITH (ef_search = ...)` on the
    /// index, and there is no session form.
    ///
    /// **There used to be a claim that `PRAGMA hnsw_ef_search` was the session
    /// form (task-1979, R13).** No such pragma exists, and an unrecognised
    /// pragma is a silent no-op, so an application that set it got no signal
    /// that the setting had not taken. The claim is gone rather than the
    /// pragma built: the width reaches the search through the index's own
    /// options, and a per statement override would need a channel from the
    /// connection's settings into a module that does not exist yet.
    pub ef_search: Option<usize>,
    /// Whether results are exact or approximate.
    pub mode: Mode,
    /// How many delta rows may accumulate before a commit folds them in, or
    /// zero to compact only when asked.
    pub compact: Option<u64>,
    /// How many segments accumulate at a level before they merge into one at
    /// the next level, when the declaration named one.
    pub segment_merge: Option<usize>,
    /// How many chunks a single commit may fold while merging segments,
    /// before it leaves the rest for a later commit, when the declaration
    /// named none.
    pub merge_budget: Option<u64>,
    /// The table this store is an index *over*, when it is one.
    ///
    /// **How a vector index survives being closed.** `CREATE INDEX ix ON t
    /// USING inillucent_hnsw (v)` is sugar for a store like this one plus the
    /// engine keeping it in step with `t`'s writes, and the engine has to be
    /// able to work out on the next open which stores those are. Recording the
    /// source here puts it in `%_config`, which is written once and read back
    /// with everything else - so the association is durable without a second
    /// place to keep a schema.
    pub source: Option<Vec<u8>>,
    /// Which column of that table holds the vector.
    pub source_column: Option<Vec<u8>>,
    /// How many threads build the graph, when the caller pinned it.
    ///
    /// **A measurement affordance, not a tuning knob.** The default is every
    /// core the machine has, which is what a store wants; pinning it to one is
    /// how a build is measured against the sequential one it replaced, and how
    /// a caller who needs the same graph twice gets it.
    pub threads: Option<usize>,
}

impl Options {
    /// Returns whether this table has a vector branch at all.
    pub fn has_vectors(&self) -> bool {
        self.dims > 0
    }

    /// Reports whether the column at this position is a facet.
    ///
    /// @param position - the column's position in `columns`
    pub fn is_facet(&self, position: usize) -> bool {
        self.facets.contains(&position)
    }

    /// Returns one facet column's name, when that position holds a facet.
    ///
    /// The name is what the core index files the value under, so a query
    /// constraining the column and the build that indexed it agree on one
    /// string rather than on a position.
    /// @param position - the column's position in `columns`
    pub fn facet_name(&self, position: usize) -> Option<String> {
        if !self.is_facet(position) {
            return None;
        }
        self.columns
            .get(position)
            .map(|name| String::from_utf8_lossy(name).into_owned())
    }

    /// Returns the format number a table with this declaration is written in.
    ///
    /// See [`FORMAT_FACETED`]: a declaration with no facet keeps writing the
    /// first format, so nothing that is already on disk and nothing written
    /// without the feature becomes unreadable to an older build.
    pub fn format(&self) -> i64 {
        match self.facets.is_empty() {
            true => FORMAT,
            false => FORMAT_FACETED,
        }
    }

    /// Returns the delta size at which a commit compacts, for a given corpus.
    pub fn compact_threshold(&self, rows: u64) -> Option<u64> {
        match self.compact {
            Some(0) => None,
            Some(explicit) => Some(explicit),
            // **A constant, not a share.** See `COMPACT_FLOOR`: a flush writes
            // its own batch and nothing else now, so the reason this rose with
            // the table is gone - and while it rose, the batch a flush built
            // rose with it, which is exactly the cost segments exist to remove.
            // `rows` is still taken so a declaration can be read against it and
            // so this signature does not churn every caller.
            None => {
                let _ = rows;
                Some(COMPACT_FLOOR)
            }
        }
    }

    /// Returns how many segments a level holds before they merge into one at
    /// the next level.
    ///
    /// Clamped to two rather than refused, the same defensive floor `positive`
    /// enforces at parse time for the graph parameters - a stored value of
    /// zero or one from a future build this one cannot fully read would
    /// otherwise merge forever, one segment at a time, on every single commit.
    pub fn segment_fanin(&self) -> usize {
        self.segment_merge.unwrap_or(DEFAULT_SEGMENT_MERGE).max(2)
    }

    /// Returns how many chunks one commit may fold while merging segments.
    ///
    /// Clamped to one rather than refused, the same defensive floor
    /// `segment_fanin` applies for the same reason: a stored zero from a
    /// build that let it be would otherwise checkpoint after every single
    /// chunk, and a merge would still finish, just at the cost of a
    /// checkpoint per chunk instead of per commit.
    pub fn merge_budget_chunks(&self) -> u64 {
        self.merge_budget.unwrap_or(DEFAULT_MERGE_BUDGET).max(1)
    }

    /// Returns how many segments must pile up at one level before a commit
    /// runs that level's merge to completion regardless of
    /// `merge_budget_chunks` - the escape hatch for when the bounded merge
    /// has fallen behind badly enough that a query's own fold, which walks
    /// every live segment, would otherwise keep growing.
    ///
    /// `segment_fanin` squared. At `segment_fanin` alone a level is exactly
    /// full and the bounded merge is expected to start clearing it this
    /// commit or the next few; reaching the square of that means it has
    /// filled enough times over, unmerged, that a query is already folding as
    /// many segments as `segment_fanin` levels would ever normally let it
    /// hold at once. Past that point, one expensive commit is the better
    /// trade against an ever-growing per-query cost - and it scales with
    /// `segment_fanin` rather than being a fixed number, because a table that
    /// raises its own fanin is choosing to hold more segments per level on
    /// purpose, and the crisis point should move with that choice rather
    /// than second-guess it.
    pub fn crisis_at(&self) -> usize {
        let fanin = self.segment_fanin();
        fanin.saturating_mul(fanin)
    }

    /// Returns the rows `%_config` holds for this declaration.
    ///
    /// They are written once, when the table is created, and read back every
    /// time it is connected. The column list is among them even though the
    /// `CREATE` text also carries it, because the two can disagree only if
    /// somebody edited `sqlite_schema` by hand - and then the config is the one
    /// the stored rows were actually built against.
    pub fn config_rows(&self) -> Vec<(String, String)> {
        let names: Vec<String> = self
            .columns
            .iter()
            .map(|name| String::from_utf8_lossy(name).into_owned())
            .collect();
        let facets: Vec<String> = self
            .facets
            .iter()
            .filter_map(|position| self.facet_name(*position))
            .collect();
        vec![
            ("format".to_string(), self.format().to_string()),
            // **The release that wrote the table, beside the format number it
            // wrote** (task-2053). The number alone tells a reader that it
            // cannot read the table; it does not tell anybody what to install.
            // An older build reads this row as one of the options it does not
            // understand and ignores it, which is what every `%_config` key
            // added after a table was created has always done here.
            ("writer".to_string(), env!("CARGO_PKG_VERSION").to_string()),
            ("columns".to_string(), names.join(",")),
            // Named rather than numbered, so the row survives a reader that
            // lists the columns in a different order than this build would.
            ("facets".to_string(), facets.join(",")),
            ("dims".to_string(), self.dims.to_string()),
            ("metric".to_string(), self.metric.name().to_string()),
            (
                "m".to_string(),
                self.m.map(|held| held.to_string()).unwrap_or_default(),
            ),
            (
                "ef_construction".to_string(),
                self.ef_construction
                    .map(|held| held.to_string())
                    .unwrap_or_default(),
            ),
            (
                "ef_search".to_string(),
                self.ef_search
                    .map(|held| held.to_string())
                    .unwrap_or_default(),
            ),
            ("mode".to_string(), self.mode.name().to_string()),
            ("tokenize".to_string(), TOKENIZER.to_string()),
            (
                "compact".to_string(),
                self.compact.map(|n| n.to_string()).unwrap_or_default(),
            ),
            (
                "segment_merge".to_string(),
                self.segment_merge
                    .map(|n| n.to_string())
                    .unwrap_or_default(),
            ),
            (
                "merge_budget".to_string(),
                self.merge_budget.map(|n| n.to_string()).unwrap_or_default(),
            ),
            (
                "source".to_string(),
                self.source
                    .as_ref()
                    .map(|name| String::from_utf8_lossy(name).into_owned())
                    .unwrap_or_default(),
            ),
            (
                "source_column".to_string(),
                self.source_column
                    .as_ref()
                    .map(|name| String::from_utf8_lossy(name).into_owned())
                    .unwrap_or_default(),
            ),
            (
                "threads".to_string(),
                self.threads.map(|n| n.to_string()).unwrap_or_default(),
            ),
        ]
    }
}

/// Reads the arguments of a `CREATE VIRTUAL TABLE ... USING inillucent_search(...)`.
///
/// An argument is either a column - a bare name, optionally followed by
/// `FACET` - or an option, written `name = value`. That is FTS5's grammar and
/// there is no reason to invent a second one; a caller who knows one virtual
/// table's argument syntax knows this one's.
/// @param arguments - the raw argument slices, as written
pub fn parse(arguments: &[Vec<u8>]) -> DbResult<Options> {
    let mut columns: Vec<Vec<u8>> = Vec::new();
    let mut facets: Vec<usize> = Vec::new();
    let mut dims = 0usize;
    let mut source: Option<Vec<u8>> = None;
    let mut threads: Option<usize> = None;
    let mut source_column: Option<Vec<u8>> = None;
    let mut metric = Metric::Cosine;
    let mut m: Option<usize> = None;
    let mut ef_construction: Option<usize> = None;
    let mut ef_search: Option<usize> = None;
    let mut mode = Mode::Exact;
    let mut compact: Option<u64> = None;
    let mut segment_merge: Option<usize> = None;
    let mut merge_budget: Option<u64> = None;
    for argument in arguments {
        let text = String::from_utf8_lossy(argument).trim().to_string();
        if text.is_empty() {
            continue;
        }
        let Some((name, value)) = split_option(&text) else {
            let (column, facet) = split_facet(&text)?;
            if facet {
                if columns.len() >= MAX_FACET_COLUMN {
                    return Err(failure(format!(
                        "inillucent_search: a facet may be declared on the first \
                         {MAX_FACET_COLUMN} columns, and {column} is number {}",
                        columns.len().saturating_add(1)
                    )));
                }
                facets.push(columns.len());
            }
            columns.push(column.into_bytes());
            continue;
        };
        let value = unquote(&value);
        match name.to_ascii_lowercase().as_str() {
            "dims" | "dimensions" => {
                dims = value.parse::<usize>().map_err(|_| {
                    failure(format!(
                        "inillucent_search: dims must be a number, not {value}"
                    ))
                })?;
                if dims > MAX_DIMS {
                    return Err(failure(format!(
                        "inillucent_search: dims must be at most {MAX_DIMS}"
                    )));
                }
            }
            "source" => source = Some(value.clone().into_bytes()),
            "threads" => {
                threads = Some(value.parse::<usize>().map_err(|_| {
                    failure(format!(
                        "inillucent_search: threads must be a number, not {value}"
                    ))
                })?)
            }
            "source_column" => source_column = Some(value.clone().into_bytes()),
            "metric" | "distance" => metric = Metric::parse(&value)?,
            // The three graph parameters, spelled as pgvector spells them. A
            // zero is refused rather than taken: a graph with no neighbours is
            // a list, and a search that looks at nothing finds nothing.
            "m" => m = Some(positive(&value, "m")?),
            "ef_construction" => ef_construction = Some(positive(&value, "ef_construction")?),
            "ef_search" => ef_search = Some(positive(&value, "ef_search")?),
            "mode" => mode = Mode::parse(&value)?,
            "tokenize" | "tokenizer" => {
                if !value.eq_ignore_ascii_case(TOKENIZER) {
                    return Err(failure(format!(
                        "inillucent_search: the only tokenizer this build has is {TOKENIZER}, not {value}"
                    )));
                }
            }
            "compact" => {
                compact = Some(value.parse::<u64>().map_err(|_| {
                    failure(format!(
                        "inillucent_search: compact must be a number, not {value}"
                    ))
                })?);
            }
            "segment_merge" => segment_merge = Some(positive(&value, "segment_merge")?),
            "merge_budget" => {
                merge_budget = Some(value.parse::<u64>().map_err(|_| {
                    failure(format!(
                        "inillucent_search: merge_budget must be a number, not {value}"
                    ))
                })?);
            }
            other => {
                return Err(failure(format!(
                    "inillucent_search: no such option: {other}"
                )))
            }
        }
    }
    if columns.is_empty() {
        return Err(failure(
            "inillucent_search: a search table needs at least one column",
        ));
    }
    if facets.len() >= columns.len() {
        return Err(failure(
            "inillucent_search: a search table needs at least one column that is not a facet, \
             because a facet is not indexed and a table of facets alone answers nothing",
        ));
    }
    Ok(Options {
        columns,
        facets,
        dims,
        metric,
        m,
        ef_construction,
        ef_search,
        mode,
        compact,
        segment_merge,
        merge_budget,
        source,
        source_column,
        threads,
    })
}

/// Reads one positive graph parameter, or says which one was not a number.
///
/// @param value - the text the option was given
/// @param name - the option's name, for the message
fn positive(value: &str, name: &str) -> DbResult<usize> {
    let held = value.parse::<usize>().map_err(|_| {
        failure(format!(
            "inillucent_search: {name} must be a number, not {value}"
        ))
    })?;
    if held == 0 {
        return Err(failure(format!(
            "inillucent_search: {name} must be greater than zero"
        )));
    }
    Ok(held)
}

/// Refuses when the stored configuration names a format this build does not
/// read.
///
/// **Its own function because the read path has to ask it too** (task-2053).
/// [`from_config`] is called from `begin`, which is the start of a write
/// transaction - so an ordinary `SELECT` against a table written by a later
/// build never asked the question at all, and answered out of an index whose
/// layout it had not checked. That is the same silence the FTS5 layout record
/// exists to end, and it is worse here, because the answer would have looked
/// like rows rather than like none.
///
/// @param rows - the key/value pairs read from `%_config`
pub fn readable(rows: &[(String, String)]) -> DbResult<()> {
    let find = |key: &str| {
        rows.iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    };
    let Some(format) = find("format") else {
        return Ok(());
    };
    if FORMATS
        .iter()
        .any(|known| format.trim() == known.to_string())
    {
        return Ok(());
    }
    Err(wrong_format(format.trim(), find("writer")))
}

/// Returns the refusal a table written in another format reports.
///
/// **`unsupported` when the table is newer, and it names the release that wrote
/// it** (task-2053). It used to be a plain statement error with no status, so a
/// driver reported it the same way it reports a syntax mistake and a caller had
/// to match on the sentence to tell "upgrade inillucent" from "your SQL is
/// wrong". This is the answer `crates/inillucent-pool/src/meta.rs` gives
/// for the database file itself, in the same words, because a caller meeting
/// one of them should not have to learn a second shape to meet the other.
///
/// A *lower* number is a format this build has dropped, and there is none:
/// format 1 is the first. It is named separately rather than folded in, because
/// a zero there is a `%_config` row that has been overwritten rather than a
/// table from the future.
///
/// The sentence names every format this build reads rather than one, because
/// there are two of them now and which one a table is in depends on whether it
/// declared a facet column - so "this build reads format 1" would be false of
/// this build and no use to somebody holding a table in format 2.
///
/// @param found - the format the table's `%_config` carries
/// @param writer - the release the table's `%_config` names, when it names one
fn wrong_format(found: &str, writer: Option<&str>) -> inillucent_base::DbError {
    let by = match writer.filter(|named| !named.is_empty()) {
        Some(named) => format!(", written by inillucent {named}"),
        None => String::new(),
    };
    let reads = FORMATS
        .iter()
        .map(|known| known.to_string())
        .collect::<Vec<String>>()
        .join(" and ");
    let newest = FORMATS.iter().copied().max().unwrap_or(FORMAT);
    let newer = found.parse::<i64>().is_ok_and(|number| number > newest);
    if !newer {
        return failure(format!(
            "inillucent_search: the table is in format {found}{by} and this build reads formats \
             {reads}, and there is no earlier format: the configuration has been overwritten"
        ));
    }
    let said = format!(
        "inillucent_search: the table is in format {found}{by} and this build reads formats \
         {reads}"
    );
    failure(format!("{said}; upgrade inillucent to open it"))
        .with_message(said)
        .with_unsupported(format!("a search index in format {found}"))
}

/// Reads the options back out of the rows `%_config` holds.
///
/// The stored rows win over the `CREATE` text where the two disagree, because
/// the stored rows are what the existing generations were built against.
/// @param rows - the key/value pairs read from `%_config`
/// @param fallback - the declaration parsed from the `CREATE` statement
pub fn from_config(rows: &[(String, String)], fallback: &Options) -> DbResult<Options> {
    let find = |key: &str| {
        rows.iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.clone())
    };
    readable(rows)?;
    let columns = match find("columns") {
        Some(list) if !list.is_empty() => list
            .split(',')
            .map(|name| name.as_bytes().to_vec())
            .collect(),
        _ => fallback.columns.clone(),
    };
    // Resolved against the stored column list rather than against the
    // declaration, so a facet whose column the stored rows put elsewhere is
    // still the same column. A stored name that is not in the column list is
    // dropped: it names a column this table does not have, and treating it as a
    // position would filter on whichever column happened to be there.
    let facets: Vec<usize> = match find("facets") {
        Some(list) if !list.is_empty() => {
            let mut held = Vec::new();
            for name in list
                .split(',')
                .map(str::trim)
                .filter(|name| !name.is_empty())
            {
                let Some(position) = columns
                    .iter()
                    .position(|column| column.as_slice() == name.as_bytes())
                else {
                    continue;
                };
                // The same bound `parse` enforces, enforced again here because
                // the stored rows are a second way in and the access path
                // records a position in one character.
                if position >= MAX_FACET_COLUMN {
                    return Err(failure(format!(
                        "inillucent_search: the stored configuration puts the facet {name} at \
                         column {}, and a facet may be declared on the first {MAX_FACET_COLUMN}",
                        position.saturating_add(1)
                    )));
                }
                held.push(position);
            }
            held
        }
        // A table written before facets existed says nothing here, and a table
        // that declared none stores an empty value. Neither has any.
        Some(_) => Vec::new(),
        None => fallback.facets.clone(),
    };
    let dims = find("dims")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(fallback.dims)
        .min(MAX_DIMS);
    let metric = match find("metric") {
        Some(name) => Metric::parse(&name)?,
        None => fallback.metric,
    };
    let mode = match find("mode") {
        Some(name) => Mode::parse(&name)?,
        None => fallback.mode,
    };
    if let Some(tokenizer) = find("tokenize") {
        if !tokenizer.eq_ignore_ascii_case(TOKENIZER) {
            return Err(failure(format!(
                "inillucent_search: the terms were produced by {tokenizer}, which this build does not have"
            )));
        }
    }
    let compact = find("compact").and_then(|value| value.parse::<u64>().ok());
    let segment_merge = find("segment_merge").and_then(|value| value.parse::<usize>().ok());
    let merge_budget = find("merge_budget").and_then(|value| value.parse::<u64>().ok());
    // An empty stored value is "the build's own default", which is what a store
    // created before these three existed says - see the `%_config` rows above.
    let graph = |key: &str| find(key).and_then(|value| value.parse::<usize>().ok());
    Ok(Options {
        columns,
        facets,
        dims,
        metric,
        m: graph("m").or(fallback.m),
        ef_construction: graph("ef_construction").or(fallback.ef_construction),
        ef_search: graph("ef_search").or(fallback.ef_search),
        mode,
        compact: compact.or(fallback.compact),
        segment_merge: segment_merge.or(fallback.segment_merge),
        merge_budget: merge_budget.or(fallback.merge_budget),
        // An empty stored value is "no source", not a table called nothing:
        // every row of `%_config` is written, including the ones that were
        // never set.
        source: find("source")
            .filter(|value| !value.is_empty())
            .map(String::into_bytes)
            .or_else(|| fallback.source.clone()),
        source_column: find("source_column")
            .filter(|value| !value.is_empty())
            .map(String::into_bytes)
            .or_else(|| fallback.source_column.clone()),
        threads: find("threads")
            .and_then(|value| value.parse::<usize>().ok())
            .or(fallback.threads),
    })
}

/// Splits a column declaration into its name and whether it said `FACET`.
///
/// The word is matched without case and only as the last whitespace-separated
/// token, so a column genuinely called `facet` is still a text column.
///
/// **The suffix is read before the quoting is stripped, which is what keeps a
/// quoted name whole.** A column name may contain a space if it is quoted, the
/// way FTS5's may, and a declaration that read `FACET` off the unquoted name
/// would turn `"live facet"` - one column with a space in its name - into a
/// facet called `live`. Reading the raw argument, the closing quote is part of
/// the last token, so it does not match and the whole thing stays a name.
/// Quoting is therefore how a column called `live facet` is declared, and
/// `'live' FACET` is still a facet called `live`.
/// @param raw - the bare argument, exactly as written
fn split_facet(raw: &str) -> DbResult<(String, bool)> {
    let trimmed = raw.trim();
    let (name, facet) = match trimmed.rsplit_once(char::is_whitespace) {
        Some((head, last)) if last.eq_ignore_ascii_case(FACET_WORD) && !head.trim().is_empty() => {
            (head.trim(), true)
        }
        _ => (trimmed, false),
    };
    let name = unquote(name);
    if name.is_empty() {
        return Err(failure(
            "inillucent_search: a facet needs a column name in front of it",
        ));
    }
    Ok((name, facet))
}

/// Splits `name = value`, which is how an option is written.
fn split_option(text: &str) -> Option<(String, String)> {
    let (name, value) = text.split_once('=')?;
    let name = name.trim();
    if name.is_empty() || name.contains(char::is_whitespace) {
        return None;
    }
    Some((name.to_string(), value.trim().to_string()))
}

/// Removes one layer of SQL quoting from an argument.
fn unquote(text: &str) -> String {
    let bytes = text.as_bytes();
    let quoted = matches!(
        (bytes.first(), bytes.last()),
        (Some(b'\''), Some(b'\'')) | (Some(b'"'), Some(b'"')) | (Some(b'['), Some(b']'))
    );
    if !quoted || text.len() < 2 {
        return text.to_string();
    }
    text.get(1..text.len().saturating_sub(1))
        .unwrap_or(text)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bare name is a column and `name = value` is an option.
    #[test]
    fn columns_and_options_are_told_apart() {
        let parsed = parse(&[
            b"body".to_vec(),
            b"title".to_vec(),
            b"dims = 8".to_vec(),
            b"mode = 'approximate'".to_vec(),
        ])
        .expect("parsed");
        assert_eq!(parsed.columns, vec![b"body".to_vec(), b"title".to_vec()]);
        assert_eq!(parsed.dims, 8);
        assert_eq!(parsed.mode, Mode::Approximate);
        assert_eq!(parsed.metric, Metric::Cosine);
    }

    /// A distance this build cannot compute is refused when the table is made,
    /// not silently replaced with one it can.
    #[test]
    fn an_unimplemented_metric_is_refused() {
        assert!(parse(&[b"body".to_vec(), b"metric = 'euclidean'".to_vec()]).is_err());
    }

    /// `l2` is spelled, parses, and round trips through the same accessors
    /// `cosine` does - the structure this ticket adds to, not a special case
    /// beside it.
    #[test]
    fn l2_is_a_real_metric_now() {
        let parsed = parse(&[b"body".to_vec(), b"metric = 'l2'".to_vec()]).expect("parsed");
        assert_eq!(parsed.metric, Metric::L2);
        assert_eq!(Metric::L2.name(), "l2");
        assert_eq!(Metric::parse("L2").expect("case insensitive"), Metric::L2);
        assert_eq!(
            parsed
                .config_rows()
                .iter()
                .find(|(key, _)| key == "metric")
                .map(|(_, value)| value.as_str()),
            Some("l2")
        );
    }

    /// `distance` is the alias `metric` has always accepted, and it takes `l2`
    /// exactly the way it takes `cosine`.
    #[test]
    fn l2_is_accepted_through_the_distance_alias_too() {
        let parsed = parse(&[b"body".to_vec(), b"distance = 'l2'".to_vec()]).expect("parsed");
        assert_eq!(parsed.metric, Metric::L2);
    }

    /// A table with no columns has nothing to index.
    #[test]
    fn a_table_needs_a_column() {
        assert!(parse(&[b"dims = 4".to_vec()]).is_err());
    }

    /// The stored configuration is what a reopened table believes.
    #[test]
    fn the_stored_configuration_wins() {
        let fallback = parse(&[b"body".to_vec()]).expect("parsed");
        let stored = vec![
            ("format".to_string(), "1".to_string()),
            ("columns".to_string(), "body,title".to_string()),
            ("dims".to_string(), "16".to_string()),
            ("mode".to_string(), "approximate".to_string()),
        ];
        let read = from_config(&stored, &fallback).expect("read");
        assert_eq!(read.columns.len(), 2);
        assert_eq!(read.dims, 16);
        assert_eq!(read.mode, Mode::Approximate);
    }

    /// A table whose `%_config` was written before this ticket has no `metric`
    /// row at all - not an empty one, an absent one, the same way a table
    /// written before `compact` existed has no `compact` row. It has to keep
    /// reading as cosine, which is the only metric that table could ever have
    /// been built with.
    ///
    /// This is deliberately not the same claim as `an_index_declares_cosine_by_default`
    /// below: that one is about a `CREATE` that never named a metric, and this
    /// one is about `%_config` rows a real pre-existing table would have, which
    /// is the scenario a stored index actually presents on reopen.
    #[test]
    fn a_table_with_no_stored_metric_row_reads_as_cosine() {
        let fallback = parse(&[b"body".to_vec()]).expect("parsed");
        let stored = vec![
            ("format".to_string(), "1".to_string()),
            ("columns".to_string(), "body".to_string()),
            ("dims".to_string(), "8".to_string()),
            ("mode".to_string(), "exact".to_string()),
            // No "metric" row at all.
        ];
        let read = from_config(&stored, &fallback).expect("read");
        assert_eq!(read.metric, Metric::Cosine);
    }

    /// The same claim, but for a `CREATE` that never named a metric at all -
    /// the declaration-time default, which `from_config` above falls back to
    /// when `%_config` itself has nothing to say.
    #[test]
    fn an_index_declares_cosine_by_default() {
        let declared = parse(&[b"body".to_vec()]).expect("parsed");
        assert_eq!(
            declared.metric,
            Metric::Cosine,
            "cosine remains the default"
        );
    }

    /// A column followed by `FACET` is a facet, and the rest are text.
    #[test]
    fn a_column_declared_facet_is_one() {
        let declared = parse(&[
            b"body".to_vec(),
            b"live FACET".to_vec(),
            b"dims = 8".to_vec(),
        ])
        .expect("parsed");
        assert_eq!(declared.columns.len(), 2, "a facet is still a column");
        assert_eq!(declared.facets, vec![1]);
        assert_eq!(declared.facet_name(1).as_deref(), Some("live"));
        assert!(!declared.is_facet(0), "body is text");
        assert_eq!(declared.facet_name(0), None);
    }

    /// The word is matched without case, and only as the last word.
    ///
    /// A column genuinely called `facet` is a text column: the word has to
    /// follow a name to declare anything, so one on its own is the name.
    #[test]
    fn the_facet_word_is_read_without_case_and_only_at_the_end() {
        let declared = parse(&[b"body".to_vec(), b"live facet".to_vec()]).expect("parsed");
        assert_eq!(declared.facets, vec![1]);

        let plain = parse(&[b"body".to_vec(), b"facet".to_vec()]).expect("parsed");
        assert!(
            plain.facets.is_empty(),
            "a column called facet is a text column"
        );
        assert_eq!(plain.columns.len(), 2);
    }

    /// Quoting keeps a name whole, including one that ends in the word.
    ///
    /// A column name may hold a space when it is quoted, the way FTS5's may,
    /// and reading the suffix off the unquoted name would turn one column
    /// called `live facet` into a facet called `live`.
    #[test]
    fn a_quoted_name_is_a_name_even_when_it_ends_in_the_word() {
        let declared = parse(&[b"body".to_vec(), b"\"live facet\"".to_vec()]).expect("parsed");
        assert!(declared.facets.is_empty(), "the quotes make it a name");
        assert_eq!(
            declared.columns.get(1).map(|held| held.as_slice()),
            Some(b"live facet".as_slice())
        );

        let faceted = parse(&[b"body".to_vec(), b"\"live\" FACET".to_vec()]).expect("parsed");
        assert_eq!(faceted.facets, vec![1]);
        assert_eq!(faceted.facet_name(1).as_deref(), Some("live"));
    }

    /// A declaration of nothing but facets is refused.
    ///
    /// A facet is not indexed, so a table of them alone has no text for a
    /// query to match and would answer every search with nothing.
    #[test]
    fn a_table_of_facets_alone_is_refused() {
        let failed = parse(&[b"live FACET".to_vec()]).expect_err("refused");
        let said = failed
            .detail()
            .unwrap_or_else(|| failed.message())
            .to_string();
        assert!(
            said.contains("not a facet"),
            "should say a text column is needed: {said}"
        );
    }

    /// A facet moves the stored format, and a plain table does not.
    ///
    /// See `FORMAT_FACETED`: the refusal has to be as narrow as the change, so
    /// a table that declares no facet keeps writing the format every build
    /// already reads.
    #[test]
    fn only_a_faceted_table_moves_the_format() {
        let plain = parse(&[b"body".to_vec()]).expect("parsed");
        assert_eq!(plain.format(), FORMAT);
        let faceted = parse(&[b"body".to_vec(), b"live FACET".to_vec()]).expect("parsed");
        assert_eq!(faceted.format(), FORMAT_FACETED);
        let rows = faceted.config_rows();
        let find = |key: &str| {
            rows.iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.as_str())
        };
        assert_eq!(find("format"), Some(FORMAT_FACETED.to_string().as_str()));
        assert_eq!(find("facets"), Some("live"));
        assert_eq!(find("columns"), Some("body,live"));
    }

    /// The facets come back from `%_config` as the positions they were written
    /// at, read against the stored column list rather than the declaration.
    #[test]
    fn a_stored_facet_is_resolved_against_the_stored_columns() {
        let fallback = parse(&[b"body".to_vec()]).expect("parsed");
        let stored = vec![
            ("format".to_string(), FORMAT_FACETED.to_string()),
            ("columns".to_string(), "body,region,live".to_string()),
            ("facets".to_string(), "live,region".to_string()),
        ];
        let held = from_config(&stored, &fallback).expect("read back");
        assert_eq!(held.columns.len(), 3);
        assert_eq!(held.facets, vec![2, 1], "resolved by name, in stored order");
        assert_eq!(held.facet_name(2).as_deref(), Some("live"));
    }

    /// A stored facet naming a column the table does not have is dropped.
    ///
    /// Taking it as a position instead would filter on whichever column
    /// happened to sit there, which is a wrong answer rather than a missing
    /// feature.
    #[test]
    fn a_stored_facet_naming_no_column_is_dropped() {
        let fallback = parse(&[b"body".to_vec()]).expect("parsed");
        let stored = vec![
            ("format".to_string(), FORMAT_FACETED.to_string()),
            ("columns".to_string(), "body,live".to_string()),
            ("facets".to_string(), "live,missing".to_string()),
        ];
        let held = from_config(&stored, &fallback).expect("read back");
        assert_eq!(held.facets, vec![1]);
    }

    /// A table written before facets existed says nothing about them.
    #[test]
    fn a_table_written_before_facets_has_none() {
        let fallback = parse(&[b"body".to_vec()]).expect("parsed");
        let stored = vec![
            ("format".to_string(), FORMAT.to_string()),
            ("columns".to_string(), "body".to_string()),
        ];
        let held = from_config(&stored, &fallback).expect("read back");
        assert!(held.facets.is_empty());
    }

    /// A table written by another format version is refused rather than read.
    #[test]
    fn another_format_is_refused() {
        let fallback = parse(&[b"body".to_vec()]).expect("parsed");
        let stored = vec![("format".to_string(), "99".to_string())];
        assert!(from_config(&stored, &fallback).is_err());
    }

    /// A table from a later format refuses as `unsupported` and names the
    /// release that wrote it.
    ///
    /// **The status is the claim, not the sentence** (task-2053). A caller
    /// telling "upgrade inillucent" from "your SQL is wrong" reads
    /// `DbError::unsupported()`; before this the refusal carried none, so the
    /// only way to tell was to match on the words.
    #[test]
    fn a_later_format_refuses_as_unsupported_and_names_the_release() {
        let fallback = parse(&[b"body".to_vec()]).expect("parsed");
        let stored = vec![
            ("format".to_string(), "3".to_string()),
            ("writer".to_string(), "9.9.9".to_string()),
        ];
        let refused = from_config(&stored, &fallback).expect_err("a later format is refused");
        assert_eq!(
            refused.unsupported(),
            Some("a search index in format 3"),
            "the refusal carries the status, so the command line exits 3"
        );
        let message = refused.message();
        assert!(message.contains("format 3"), "{message}");
        assert!(
            message.contains("9.9.9"),
            "the refusal names the release to install: {message}"
        );
    }

    /// A format below this build's is corruption, not a newer table.
    ///
    /// Format 1 is the first there is, so a zero or a negative number in
    /// `%_config` is a row that has been overwritten - and answering
    /// `unsupported` there would send somebody looking for a release that does
    /// not exist.
    #[test]
    fn a_format_below_this_one_is_not_reported_as_newer() {
        let fallback = parse(&[b"body".to_vec()]).expect("parsed");
        let stored = vec![("format".to_string(), "0".to_string())];
        let refused = from_config(&stored, &fallback).expect_err("format zero is refused");
        assert_eq!(
            refused.unsupported(),
            None,
            "a lower number is not a build to upgrade to"
        );
    }

    /// The configuration a table is created with records which release wrote
    /// it, so the refusal above has a release to name.
    #[test]
    fn the_configuration_records_the_release_that_wrote_it() {
        let options = parse(&[b"body".to_vec()]).expect("parsed");
        assert_eq!(
            options
                .config_rows()
                .iter()
                .find(|(key, _)| key == "writer")
                .map(|(_, value)| value.as_str()),
            Some(env!("CARGO_PKG_VERSION"))
        );
    }

    /// The default compaction trigger is the constant floor, at any corpus
    /// size - segmented generations removed the reason it used to scale with
    /// the table (see `COMPACT_FLOOR`'s own doc comment).
    ///
    /// **Corrected in this ticket:** this test used to assert
    /// `compact_threshold(80_000) == Some(10_000)`, the old `rows / 8` share,
    /// which the code stopped computing when `COMPACT_FLOOR` replaced it -
    /// the test was simply never updated to match, and was failing on an
    /// otherwise working engine before this fix.
    #[test]
    fn the_compaction_trigger_is_the_constant_floor_at_any_size() {
        let options = parse(&[b"body".to_vec()]).expect("parsed");
        assert_eq!(options.compact_threshold(0), Some(COMPACT_FLOOR));
        assert_eq!(options.compact_threshold(80_000), Some(COMPACT_FLOOR));
        let never = parse(&[b"body".to_vec(), b"compact = 0".to_vec()]).expect("parsed");
        assert_eq!(never.compact_threshold(80_000), None);
    }

    /// A merge's per commit chunk budget defaults to eight batches' worth,
    /// and an explicit `merge_budget` overrides it.
    #[test]
    fn the_merge_budget_defaults_and_can_be_overridden() {
        let default = parse(&[b"body".to_vec()]).expect("parsed");
        assert_eq!(default.merge_budget_chunks(), DEFAULT_MERGE_BUDGET);
        let overridden =
            parse(&[b"body".to_vec(), b"merge_budget = 500".to_vec()]).expect("parsed");
        assert_eq!(overridden.merge_budget_chunks(), 500);
    }

    /// A merge_budget of zero is clamped to one rather than checkpointing
    /// forever on nothing - the same defensive floor `segment_fanin` applies.
    #[test]
    fn a_zero_merge_budget_is_clamped_to_one() {
        let options = parse(&[b"body".to_vec(), b"merge_budget = 0".to_vec()]).expect("parsed");
        assert_eq!(options.merge_budget_chunks(), 1);
    }

    /// The crisis threshold is the fanin squared, and moves with an explicit
    /// `segment_merge`.
    #[test]
    fn the_crisis_threshold_is_the_fanin_squared() {
        let default = parse(&[b"body".to_vec()]).expect("parsed");
        assert_eq!(default.segment_fanin(), DEFAULT_SEGMENT_MERGE);
        assert_eq!(
            default.crisis_at(),
            DEFAULT_SEGMENT_MERGE * DEFAULT_SEGMENT_MERGE
        );
        let raised = parse(&[b"body".to_vec(), b"segment_merge = 6".to_vec()]).expect("parsed");
        assert_eq!(raised.crisis_at(), 36);
    }
}
