//! Turning an immutable base generation and a bounded delta log into the one
//! index a query is answered from.
//!
//! Invariant: a query is answered by **one** index, never by two lists that
//! were scored separately and glued together. That is not a convenience. BM25
//! scores are relative to a corpus - the inverse document frequency of a term
//! is a property of the whole collection - so a hit scored against a base
//! generation and a hit scored against a five-row delta are two numbers on two
//! different scales, and ordering them together produces a ranking that is
//! wrong in a way no test on either half would catch. Merging first and scoring
//! once is the only version of this that is correct.
//!
//! What makes that affordable is that the merge is incremental and cached. The
//! base generation is loaded once; each delta is applied to the loaded index
//! with the same `replace_document`/`tombstone` calls the existing engine
//! already uses for a live sync; and the result is kept, keyed by the exact
//! delta entries that produced it. A second query at the same snapshot pays
//! nothing. A query after one insert pays one append. A query after a rollback
//! finds that the cached entries are no longer a prefix of the visible ones and
//! rebuilds - which is why the key is content-addressed rather than a sequence
//! number, since a rolled-back transaction gives its sequence numbers back.
//!
//! Publishing a new generation is the other half, and there are two ways to do
//! it. A commit whose delta log has passed its threshold **folds**: the built
//! generation is loaded and the pending entries are inserted into it one at a
//! time, so the graph work is one insert per delta entry. The `compact` and
//! `rebuild` commands **build in one pass** over every row, which is one insert
//! per row and leaves no tombstoned chunks in the index.
//!
//! Automatic compaction used to take the second path, which meant one ordinary
//! `INSERT` could pay a full graph build - nine and a half minutes on the
//! 598,560 chunk corpus this engine is deployed on, in the middle of somebody
//! else's transaction. That path was changed to fold pending deltas instead of
//! rebuilding the whole graph; the single-pass build is now only ever asked for
//! explicitly, by `compact` or `rebuild`.

use std::sync::Mutex;

use inillucent_base::DbResult;
use inillucent_core::filter::Filter;
use inillucent_core::index::{Index, IndexConfig};
use inillucent_core::rank::HitOrigin;
use inillucent_core::store::ChunkInput;
use inillucent_ext::vtab::{failure, Context};

use crate::options::{Metric, Mode, Options};
use crate::store::{state, Delta, Op, Row, Store};

/// The source name every row of a search table is filed under.
///
/// `inillucent_core` files a chunk under a source and an external document id, and
/// the filter language selects on the source. A SQL search table is one source
/// with one document per row, so the name is fixed and the row id is the
/// document; a caller who wants several logical corpora makes several tables,
/// which is what a SQL schema is for.
pub const SOURCE: &str = "inillucent_search";

/// The widest traversal the recall control will ask for.
///
/// Beyond this an approximate search is doing more work than the exhaustive
/// scan it was chosen instead of, so a caller asking for more is asking for the
/// exact path and gets it.
const MAX_OVERSAMPLE: f32 = 64.0;

/// One hit, as the module reports it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Hit {
    /// The rowid of the row that matched.
    pub id: i64,
    /// The fused score, which orders the list.
    pub score: f32,
    /// How good the hit is in absolute terms, in `[0, 1]`.
    pub confidence: f32,
    /// Which branch or branches produced it.
    pub origin: HitOrigin,
}

/// What a caller asked the index for.
#[derive(Clone, Debug, Default)]
pub struct Request {
    /// The query text, when there is a lexical branch to run.
    pub text: Option<String>,
    /// The query vector, when there is a vector branch to run.
    pub vector: Vec<f32>,
    /// How many hits to return.
    pub limit: usize,
    /// The recall target, between zero and one, or nothing for the default.
    pub recall: Option<f32>,
}

/// The index one connection is currently answering from.
#[derive(Debug, Default)]
pub struct Cache {
    inner: Mutex<Option<Cached>>,
}

/// What the cache is holding, and exactly which state it describes.
struct Cached {
    generation: i64,
    covered: i64,
    applied: Vec<Delta>,
    rows: usize,
    index: Index,
}

impl std::fmt::Debug for Cached {
    /// Reports what the cached index covers, since an index cannot print itself.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Cached")
            .field("generation", &self.generation)
            .field("covered", &self.covered)
            .field("applied", &self.applied.len())
            .field("rows", &self.rows)
            .finish()
    }
}

impl Cache {
    /// Returns an empty cache.
    pub fn new() -> Cache {
        Cache {
            inner: Mutex::new(None),
        }
    }

    /// Forgets whatever is cached.
    ///
    /// Called when a transaction ends, because the cheapest correct thing to do
    /// with an index built from rows that may have just been undone is to stop
    /// holding it. The content-addressed key would catch it anyway; this only
    /// stops the memory being held until the next query notices.
    pub fn forget(&self) {
        if let Ok(mut held) = self.inner.lock() {
            *held = None;
        }
    }

    /// Answers one search, refreshing the merged index first.
    pub fn search(
        &self,
        context: &mut Context<'_>,
        store: &Store,
        options: &Options,
        request: &Request,
    ) -> DbResult<Vec<Hit>> {
        let mut held = self
            .inner
            .lock()
            .map_err(|_| failure("inillucent_search: the index cache is poisoned"))?;
        refresh(&mut held, context, store, options)?;
        let Some(cached) = held.as_ref() else {
            return Ok(Vec::new());
        };
        Ok(run(&cached.index, options, request))
    }

    /// Returns how many live rows the merged index holds.
    pub fn live_rows(
        &self,
        context: &mut Context<'_>,
        store: &Store,
        options: &Options,
    ) -> DbResult<usize> {
        let mut held = self
            .inner
            .lock()
            .map_err(|_| failure("inillucent_search: the index cache is poisoned"))?;
        refresh(&mut held, context, store, options)?;
        Ok(held.as_ref().map(|cached| cached.rows).unwrap_or(0))
    }
}

/// Returns the index configuration a declaration implies.
///
/// Everything about ranking - the fusion method and its weight, the coverage
/// exponent, proximity, phrase weighting, the adaptive weighting - is left at
/// the measured defaults `inillucent_core` ships, because those are the settings
/// the existing quality scorecard was produced with and this phase is required
/// not to change what the reader answers. What the declaration decides is only
/// the shape: how wide a vector is, and whether the vector branch is allowed to
/// approximate.
/// @param options - the table's declaration
pub fn configuration(options: &Options) -> IndexConfig {
    let mut config = IndexConfig {
        // A lexical-only table still has a vector set, because every structure
        // underneath is indexed by chunk ordinal and a zero-width one would
        // have to be special-cased in all of them. One dimension of zero costs
        // four bytes a row and keeps every path identical.
        dims: options.dims.max(1),
        ..IndexConfig::default()
    };
    match options.mode {
        // `exhaustive_below = usize::MAX` is `inillucent_core`'s own way of saying
        // "never traverse": every vector search compares every candidate, which
        // is the only setting that is correct by construction and is what an
        // exact table promises.
        Mode::Exact => config.hnsw.exhaustive_below = usize::MAX,
        Mode::Approximate => {}
    }
    if !options.has_vectors() {
        // The graph is never consulted on a lexical-only table, so building a
        // well-connected one is work with nothing to show for it. It is still
        // built, because every structure is indexed by chunk ordinal and an
        // absent graph would make `append` fall back to a full rebuild.
        config.hnsw.m = 2;
        config.hnsw.ef_construction = 2;
        config.hnsw.exhaustive_below = usize::MAX;
    }
    // **The graph is built on every core this machine has.**
    //
    // `HnswParams::build_threads` defaults to one, and that default is right
    // where it lives: `inillucent-core`'s own gradings compare settings, and a
    // graph that came out differently because the scheduler interleaved two
    // inserts differently would make every comparison a comparison of two
    // things at once. A *store* is not a grading. It is rebuilt whenever
    // compaction runs, over a corpus of whatever size the application has, and
    // the sequential build of the 598,560-chunk mailbox this engine is deployed
    // on takes nine and a half minutes on one core of twenty-four.
    //
    // The parallel build was written for exactly this and was never switched
    // on: a lock per adjacency list, neighbours merged rather than assigned so
    // a concurrent insert's back-edge is not erased, which is what hnswlib and
    // FAISS do. The graph it produces is valid and is not the sequential one -
    // the same property an approximate index has anyway.
    //
    // One thread when the machine will not say how many it has, which is the
    // old behaviour rather than a guess.
    config.hnsw.build_threads = options.threads.unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(|count| count.get())
            .unwrap_or(1)
    });
    // **The three graph parameters an index may have named.** `WITH (m = 32,
    // ef_construction = 128)` on a `CREATE INDEX ... USING inillucent_hnsw`
    // reaches here, which is the only place they mean anything: a graph is
    // built once and then searched, and both halves read these. An index that
    // named none of them leaves the build's own defaults, which is what every
    // store had before the parameters existed.
    if let Some(m) = options.m {
        config.hnsw.m = m;
    }
    if let Some(ef) = options.ef_construction {
        config.hnsw.ef_construction = ef;
    }
    if let Some(ef) = options.ef_search {
        config.hnsw.ef_search = ef;
    }
    let _ = Metric::Cosine;
    config
}

/// Builds a chunk from one stored row.
///
/// The content is every column joined by newlines. When there is more than one
/// column the first is *also* the heading path, so a chunk's text begins with
/// its heading - which is the shape the existing corpus has and the shape the
/// heading boost and proximity weighting were measured against.
///
/// A one-column table has text and no heading, and that distinction matters
/// rather than being tidiness: the legacy migration declares exactly one column
/// and puts the source chunk's text in it verbatim, so that the terms and the
/// corpus statistics of the migrated index are the ones the source index had.
/// Fabricating a heading equal to the whole content would leave the two indexes
/// scoring differently the moment anybody turned the heading boost on.
///
/// Each row is its own document. That is a real consequence and it is stated
/// where it can be read: the per-document cap in the fusion never binds on a
/// `inillucent_search` table, because no two rows share a document. An application
/// that wants documents made of several chunks models them in SQL - a document
/// table and a join - which is what a relational engine is for.
/// @param id - the rowid, which is the document identity
/// @param row - the stored row
pub fn chunk_of(id: i64, row: &Row) -> ChunkInput {
    let heading = if row.columns.len() > 1 {
        row.columns.first().cloned().unwrap_or_default()
    } else {
        String::new()
    };
    ChunkInput {
        source: SOURCE.to_string(),
        external_doc_id: id.to_string(),
        chunk_index: 0,
        heading_path: if heading.is_empty() {
            Vec::new()
        } else {
            vec![heading.clone()]
        },
        content: row.columns.join("\n"),
        title: heading,
        url: String::new(),
        space_key: None,
        author: None,
        author_id: None,
        updated_at: None,
        external_chunk_id: Some(id.to_string()),
        labels: Vec::new(),
        attributes: Vec::new(),
        flags: Vec::new(),
        deleted: false,
    }
}

/// Returns the embedding one row contributes, padded or replaced as needed.
///
/// A row with no vector in a table that has a vector branch is a legitimate
/// state - a document whose embedding has not been computed yet - and it gets
/// the zero vector, which is orthogonal to nothing and therefore never a near
/// neighbour of anything. That is the honest answer: it is in the corpus
/// lexically and invisible to the vector branch until it is embedded.
pub fn embedding_of(row: &Row, dims: usize) -> Vec<f32> {
    let width = dims.max(1);
    if row.vector.len() == width {
        return row.vector.clone();
    }
    let mut padded = vec![0.0f32; width];
    for (slot, value) in padded.iter_mut().zip(row.vector.iter()) {
        *slot = *value;
    }
    padded
}

/// Builds a whole index from every row a search table holds, in one pass.
///
/// This is what the `compact` and `rebuild` commands both do. It is deliberately
/// not the incremental path: a graph grown one insert at a time is not the graph
/// a single-pass build produces, and only the single-pass build drops the chunks
/// an update tombstoned.
///
/// It used to say here that the single-pass graph is also better connected, and
/// a recall gate run against it did not find that. At 40,000 documents and 64 dimensions the
/// folded graph answered 0.704 recall at ten against the rebuilt graph's 0.637.
/// One reading over twenty-four probes is not a reversal of the claim, but it is
/// enough to stop the claim being made: what a rebuild is known to buy is the
/// tombstoned chunks going away.
/// @param context - the module's reach into the database
/// @param store - the shadow tables
/// @param options - the table's declaration
pub fn build_from_rows(
    context: &mut Context<'_>,
    store: &Store,
    options: &Options,
) -> DbResult<(Index, usize)> {
    let dims = options.dims.max(1);
    let mut chunks: Vec<ChunkInput> = Vec::new();
    let mut vectors: Vec<Vec<f32>> = Vec::new();
    store.scan_rows(context, |id, row| {
        chunks.push(chunk_of(id, &row));
        vectors.push(embedding_of(&row, dims));
        Ok(true)
    })?;
    let rows = chunks.len();
    let mut index = Index::new(configuration(options));
    if !chunks.is_empty() {
        index.add(chunks, &vectors);
    }
    index.commit();
    Ok((index, rows))
}

/// Folds a bounded batch of deltas into the generation that is already built.
///
/// **This is the automatic path, and the bound is the point.**
/// The graph is not rebuilt. Each pending entry is one `replace_document`,
/// which tombstones the old chunk and inserts one node through `Hnsw::insert` -
/// a descent through the layers and one neighbour selection per layer. So the
/// graph work is **one insert per entry in the delta log**, where the
/// single-pass build it replaced was one insert per row in the table.
///
/// The delta log is a length the declaration sets. `compact = N` fixes it at
/// `N`, and a table declared that way pays exactly `N` graph inserts per
/// published generation however large it grows. The default rule is
/// `max(1024, rows / 8)`, which grows with the corpus - it is a share rather
/// than a constant because publishing a generation writes the whole serialised
/// index, and a constant would write it far too often. An application that
/// needs the write latency pinned names `compact` and accepts more generation
/// writes; `docs/relational-architecture.md` publishes both sides.
///
/// Reading and writing the generation is still proportional to the corpus,
/// because a generation is one serialised index. That is a byte copy rather
/// than graph construction, and the two are reported separately for exactly
/// that reason.
///
/// The graph this produces is not the graph a single-pass build produces. It is
/// a valid approximate graph, and what it accumulates is tombstoned chunks: an
/// update tombstones the old chunk and appends a new one, and only a single-pass
/// build removes the old one. That is what the `compact` command is for, and
/// what `state::FOLDS` and `state::CHUNKS` let an application see coming.
///
/// @param context - the module's reach into the database
/// @param store - the shadow tables
/// @param options - the table's declaration
/// @param generation - the generation to fold into
/// @param pending - the delta entries not yet covered
pub fn fold_generation(
    context: &mut Context<'_>,
    store: &Store,
    options: &Options,
    generation: i64,
    pending: &[Delta],
) -> DbResult<(Index, usize)> {
    let Some(bytes) = store.read_generation(context, generation)? else {
        // Nothing to fold into. A first generation is built in one pass over
        // the rows the transaction itself wrote, which is bounded by that
        // transaction rather than by an index it is adding to.
        let (index, _) = build_from_rows(context, store, options)?;
        let inserted = index.store().n_chunks();
        return Ok((index, inserted));
    };
    let mut index = inillucent_core::persist::read_index(&mut bytes.as_slice())
        .map_err(|error| failure(format!("inillucent_search: unreadable generation: {error}")))?;
    let inserted = apply(&mut index, context, store, options, pending)?;
    Ok((index, inserted))
}

/// Brings the cached index up to the snapshot the statement is reading.
///
/// Three outcomes, in order of cost: nothing changed and the cache stands; the
/// cached entries are a prefix of the visible ones and the remainder is
/// applied; or the two disagree and the base generation is loaded again. The
/// third is what a rollback produces, and it is the one that has to be right
/// rather than fast.
fn refresh(
    held: &mut Option<Cached>,
    context: &mut Context<'_>,
    store: &Store,
    options: &Options,
) -> DbResult<()> {
    let generation = store.state(context, state::GENERATION)?;
    let covered = store.state(context, state::COVERED)?;
    let visible = store.deltas_above(context, covered)?;
    if let Some(cached) = held.as_mut() {
        if cached.generation == generation
            && cached.covered == covered
            && visible.len() >= cached.applied.len()
            && visible.starts_with(&cached.applied)
        {
            let outstanding: Vec<Delta> = visible
                .get(cached.applied.len()..)
                .unwrap_or_default()
                .to_vec();
            if outstanding.is_empty() {
                return Ok(());
            }
            apply(&mut cached.index, context, store, options, &outstanding)?;
            cached.rows = live_rows_of(&cached.index);
            cached.applied = visible;
            return Ok(());
        }
    }
    let mut index = match store.read_generation(context, generation)? {
        Some(bytes) => {
            inillucent_core::persist::read_index(&mut bytes.as_slice()).map_err(|error| {
                failure(format!("inillucent_search: unreadable generation: {error}"))
            })?
        }
        None => {
            let mut fresh = Index::new(configuration(options));
            fresh.commit();
            fresh
        }
    };
    apply(&mut index, context, store, options, &visible)?;
    let rows = live_rows_of(&index);
    *held = Some(Cached {
        generation,
        covered,
        applied: visible,
        rows,
        index,
    });
    Ok(())
}

/// Applies a run of delta entries to a loaded index.
///
/// A put and an update are the same call, because `replace_document` tombstones
/// whatever was there and appends the new version - which is what an
/// append-only index does with an edit, and what the live sync path already
/// does.
fn apply(
    index: &mut Index,
    context: &mut Context<'_>,
    store: &Store,
    options: &Options,
    entries: &[Delta],
) -> DbResult<usize> {
    let dims = options.dims.max(1);
    let mut inserted = 0usize;
    for entry in entries {
        match entry.op {
            Op::Delete => {
                index.tombstone(SOURCE, &entry.id.to_string());
            }
            Op::Put => {
                let Some(row) = store.read_row(context, entry.id)? else {
                    // The log says the row was written and it is not there. The
                    // row store is authoritative, so this is a delete: it can
                    // only happen if the two were written by different
                    // transactions, which the module never does, or if
                    // something outside the module edited a shadow table.
                    index.tombstone(SOURCE, &entry.id.to_string());
                    continue;
                };
                let chunk = chunk_of(entry.id, &row);
                let vector = embedding_of(&row, dims);
                let stats =
                    index.replace_document(SOURCE, &entry.id.to_string(), vec![chunk], &[vector]);
                inserted = inserted.saturating_add(stats.chunks_added);
            }
        }
    }
    Ok(inserted)
}

/// Returns how many live chunks an index holds.
fn live_rows_of(index: &Index) -> usize {
    let store = index.store();
    let total = store.n_chunks();
    let deleted = (store.deleted_ratio() * total as f32).round() as usize;
    total.saturating_sub(deleted)
}

/// Runs one request against a loaded index.
fn run(index: &Index, options: &Options, request: &Request) -> Vec<Hit> {
    let limit = request.limit.max(1);
    let filter = index.compile(&Filter::default());
    let ef = traversal_width(options, request, limit);
    let text = request.text.clone().unwrap_or_default();
    let has_text = !text.trim().is_empty();
    let has_vector = !request.vector.is_empty();
    let branches = match (has_text, has_vector) {
        (true, true) => inillucent_core::index::Branches::Both,
        (true, false) => inillucent_core::index::Branches::Lexical,
        (false, true) => inillucent_core::index::Branches::Vector,
        (false, false) => return Vec::new(),
    };
    let (hits, _) = index.search_branches(&text, &request.vector, &filter, limit, ef, branches);
    let store = index.store();
    hits.into_iter()
        .filter_map(|hit| {
            let external = store.chunk_external_id(hit.chunk);
            let id = external.parse::<i64>().ok()?;
            Some(Hit {
                id,
                score: hit.score,
                confidence: hit.confidence,
                origin: hit.origin,
            })
        })
        .collect()
}

/// Returns the traversal width one request asks for.
///
/// `None` means "the configured default"; an exact table has already been
/// configured never to traverse at all, so the number only matters to an
/// approximate one. Recall is a monotone control on breadth, not a guarantee:
/// the only value with a guarantee behind it is 1.0, which asks for every
/// comparison to be made and therefore for the exact answer.
fn traversal_width(options: &Options, request: &Request, limit: usize) -> Option<usize> {
    let recall = request.recall?;
    if options.mode == Mode::Exact {
        return None;
    }
    if recall >= 1.0 {
        return Some(usize::MAX);
    }
    let clamped = recall.clamp(0.0, 0.999);
    let oversample = (1.0 / (1.0 - clamped)).min(MAX_OVERSAMPLE);
    Some(((limit as f32 * oversample).ceil() as usize).max(limit))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::options;

    fn declaration(source: &[&str]) -> Options {
        let arguments: Vec<Vec<u8>> = source.iter().map(|text| text.as_bytes().to_vec()).collect();
        options::parse(&arguments).expect("parsed")
    }

    /// An exact table is configured never to traverse the graph.
    #[test]
    fn an_exact_table_never_traverses() {
        let config = configuration(&declaration(&["body", "dims = 8"]));
        assert_eq!(config.hnsw.exhaustive_below, usize::MAX);
        assert_eq!(config.dims, 8);
    }

    /// An approximate table keeps the graph's own cost model.
    #[test]
    fn an_approximate_table_keeps_the_cost_model() {
        let config = configuration(&declaration(&["body", "dims = 8", "mode = approximate"]));
        assert!(config.hnsw.exhaustive_below < usize::MAX);
    }

    /// A lexical-only table still has one dimension, so every structure is
    /// indexed the same way.
    #[test]
    fn a_lexical_table_has_one_dimension() {
        let config = configuration(&declaration(&["body"]));
        assert_eq!(config.dims, 1);
    }

    /// A chunk's text begins with its heading, which is what the ranking
    /// settings were measured against.
    #[test]
    fn a_chunk_begins_with_its_heading() {
        let row = Row {
            columns: vec!["Offer eligibility".to_string(), "who qualifies".to_string()],
            vector: Vec::new(),
        };
        let chunk = chunk_of(7, &row);
        assert!(chunk.content.starts_with("Offer eligibility"));
        assert_eq!(chunk.heading_path, vec!["Offer eligibility".to_string()]);
        assert_eq!(chunk.external_chunk_id.as_deref(), Some("7"));
    }

    /// A row with no embedding joins the corpus lexically and is invisible to
    /// the vector branch rather than being refused.
    #[test]
    fn a_row_with_no_embedding_gets_the_zero_vector() {
        let row = Row {
            columns: vec!["text".to_string()],
            vector: Vec::new(),
        };
        assert_eq!(embedding_of(&row, 4), vec![0.0, 0.0, 0.0, 0.0]);
    }

    /// Recall is a monotone control on breadth, and one asks for the exact path.
    #[test]
    fn recall_widens_the_traversal() {
        let table = declaration(&["body", "dims = 4", "mode = approximate"]);
        let narrow = traversal_width(
            &table,
            &Request {
                recall: Some(0.5),
                ..Request::default()
            },
            10,
        );
        let wide = traversal_width(
            &table,
            &Request {
                recall: Some(0.9),
                ..Request::default()
            },
            10,
        );
        assert!(narrow < wide);
        assert_eq!(
            traversal_width(
                &table,
                &Request {
                    recall: Some(1.0),
                    ..Request::default()
                },
                10
            ),
            Some(usize::MAX)
        );
    }

    /// An exact table ignores recall, because it is already exact.
    #[test]
    fn an_exact_table_ignores_recall() {
        let table = declaration(&["body", "dims = 4"]);
        assert_eq!(
            traversal_width(
                &table,
                &Request {
                    recall: Some(0.1),
                    ..Request::default()
                },
                10
            ),
            None
        );
    }
}
