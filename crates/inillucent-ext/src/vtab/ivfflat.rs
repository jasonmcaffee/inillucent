//! `ivfflat`: pgvector's other index, as a virtual table.
//!
//! Invariant: **what a probe returns is exact within the lists it looked at.**
//! An IVFFlat is approximate in one place and one place only - which lists it
//! probes - and inside those lists every vector is compared in full. That is
//! the whole structure, and it is why an IVFFlat's recall is a number a caller
//! can reason about: raise `probes` and you look at more of the corpus, up to
//! `lists`, at which point the answer is exhaustive and exact.
//!
//! ## What it is
//!
//! Two shadow tables and a clustering:
//!
//! - `x_centroid(id INTEGER PRIMARY KEY, v BLOB)` - `lists` centroids, found by
//!   k-means over the vectors that were there when the index was built.
//! - `x_row(id INTEGER PRIMARY KEY, list INTEGER, v BLOB)` - every indexed
//!   vector under the rowid of the table row it came from, with the centroid it
//!   was assigned to.
//! - `x_config(k TEXT PRIMARY KEY, v TEXT)` - `dims`, `lists`, `probes`, and
//!   how many rows the clustering was built over.
//!
//! A query finds the `probes` nearest centroids, reads the rows assigned to
//! them, and keeps the `k` nearest by cosine distance. A row inserted after the
//! build is assigned to its nearest centroid, so an index stays usable between
//! rebuilds; a corpus that has grown past twice the size it was clustered at
//! rebuilds itself on the next probe, because the clusters of a corpus a
//! quarter this size are not this corpus's clusters.
//!
//! ## Why it is a module rather than a second graph
//!
//! The HNSW index this engine ships is a *graph*, and the graph lives in the
//! retrieval engine because that is where the hybrid ranking that reads it
//! lives. An IVFFlat needs none of that: no lexical half, no fusion, no
//! reranking. It needs k-means, an inverted list per centroid, and a scan - so
//! it is written here, against the same `probe_module` interface the engine
//! already asks a vector index through, and it costs the retrieval engine
//! nothing.

use inillucent_base::{error, DbResult};
use inillucent_value::Value;

use crate::shadow::ShadowTables;

use super::{
    Change, ConstraintOp, Context, Declaration, DeclaredColumn, FilterPlan, IndexQuery, Module,
    ModuleArguments, ShadowTable, VirtualCursor, VirtualTable,
};

/// The shadow table holding the centroids.
const CENTROID: &[u8] = b"centroid";
/// The shadow table holding the assigned vectors.
const ROW: &[u8] = b"row";
/// The shadow table holding the settings.
const CONFIG: &[u8] = b"config";

/// How many lists an index that did not say gets.
///
/// pgvector's own guidance is `rows / 1000` for a large corpus and
/// `sqrt(rows)` for a small one; a hundred is the middle of that for the sizes
/// an embedded database holds, and it is a number an index can override.
const DEFAULT_LISTS: usize = 100;

/// How many lists a query that did not say probes.
///
/// One, which is pgvector's default and the reason its documentation warns
/// about recall: one list of a hundred is one per cent of the corpus.
const DEFAULT_PROBES: usize = 1;

/// How many k-means passes a build makes.
///
/// Ten, after which the assignment has stopped moving on every corpus this was
/// measured on. The loop also stops early when no vector changed list, which is
/// the usual exit.
const PASSES: usize = 10;

/// How much a corpus may grow before the clustering is rebuilt.
///
/// Twice. A centroid found over a quarter of today's rows is not this corpus's
/// centroid, and an index that never re-clustered would drift until every probe
/// read one enormous list.
const REBUILD_GROWTH: usize = 2;

/// The `ivfflat` module.
pub struct IvfFlatModule;

impl Module for IvfFlatModule {
    /// Returns the module's name.
    fn name(&self) -> &str {
        "ivfflat"
    }

    /// Returns the three tables the index is stored in.
    ///
    /// @param arguments - the `CREATE VIRTUAL TABLE` arguments
    fn shadow_tables(&self, arguments: &ModuleArguments) -> DbResult<Vec<ShadowTable>> {
        let _ = arguments;
        Ok(vec![
            ShadowTable {
                suffix: CENTROID.to_vec(),
                create_sql: "CREATE TABLE \"%_centroid\"(id INTEGER PRIMARY KEY, v BLOB)"
                    .to_string(),
                owner: None,
            },
            ShadowTable {
                suffix: ROW.to_vec(),
                create_sql: "CREATE TABLE \"%_row\"(id INTEGER PRIMARY KEY, list INTEGER, v BLOB)"
                    .to_string(),
                owner: None,
            },
            ShadowTable {
                suffix: CONFIG.to_vec(),
                create_sql: "CREATE TABLE \"%_config\"(id INTEGER PRIMARY KEY, k TEXT, v TEXT)"
                    .to_string(),
                owner: None,
            },
        ])
    }

    /// Connects to an index, reading its settings out of the arguments.
    ///
    /// @param arguments - the `CREATE VIRTUAL TABLE` arguments
    /// @param creating - whether the table is being created rather than reopened
    fn connect(
        &self,
        arguments: &ModuleArguments,
        creating: bool,
    ) -> DbResult<Box<dyn VirtualTable>> {
        let _ = creating;
        let settings = Settings::of(arguments)?;
        let shadows = ShadowTables::of(arguments, &[CENTROID, ROW, CONFIG])?;
        let declaration = declaration_of(&arguments.table);
        Ok(Box::new(IvfFlatTable {
            settings,
            shadows,
            declaration,
            rows: None,
            built: 0,
        }))
    }
}

/// Returns the columns an index declares.
///
/// The four the engine's vector probe looks for by name - the table's own
/// hidden query column, `k`, `vector` and `rank` - plus one visible column so
/// that a `SELECT *` on the index has something to show. The shape is
/// `inillucent_search`'s, because the engine asks both the same way.
///
/// @param table - the index's name
fn declaration_of(table: &[u8]) -> Declaration {
    Declaration {
        columns: vec![
            DeclaredColumn::visible("body"),
            DeclaredColumn::hidden(&String::from_utf8_lossy(table)),
            DeclaredColumn::hidden("k").typed("INTEGER"),
            DeclaredColumn::hidden("vector").typed("BLOB"),
            DeclaredColumn::hidden("rank").typed("REAL"),
        ],
        without_rowid: false,
    }
}

/// Which declared column is which.
mod at {
    /// The one visible column.
    pub const BODY: i32 = 0;
    /// The table's own hidden query column, which an `ivfflat` never matches.
    ///
    /// Declared rather than used: the column exists in the schema this module
    /// publishes, so the position has to be named here even though no code
    /// path reads it. Removing it would renumber every constant below.
    #[allow(dead_code)]
    pub const QUERY: i32 = 1;
    /// How many neighbours the caller wants.
    pub const K: i32 = 2;
    /// The vector to search near.
    pub const VECTOR: i32 = 3;
    /// The distance, which is what the order is by.
    pub const RANK: i32 = 4;
}

/// Everything the `CREATE VIRTUAL TABLE` statement declared.
#[derive(Clone, Debug)]
struct Settings {
    /// How wide the vectors are.
    dims: usize,
    /// How many centroids the clustering has.
    lists: usize,
    /// How many of them a query reads.
    probes: usize,
}

impl Settings {
    /// Reads the settings out of the module's arguments.
    ///
    /// @param arguments - the `CREATE VIRTUAL TABLE` arguments
    fn of(arguments: &ModuleArguments) -> DbResult<Settings> {
        let mut dims = 0usize;
        let mut lists = DEFAULT_LISTS;
        let mut probes = DEFAULT_PROBES;
        for argument in &arguments.arguments {
            let text = String::from_utf8_lossy(argument).trim().to_string();
            let Some((name, value)) = text.split_once('=') else {
                continue;
            };
            let value = value
                .trim()
                .trim_matches(|held| held == '\'' || held == '"');
            let number = || -> DbResult<usize> {
                value.parse::<usize>().map_err(|_| {
                    error::misuse(format!(
                        "ivfflat: {} must be a number, not {value}",
                        name.trim()
                    ))
                })
            };
            match name.trim().to_ascii_lowercase().as_str() {
                "dims" | "dimensions" => dims = number()?,
                "lists" => lists = number()?.max(1),
                "probes" => probes = number()?.max(1),
                // `source` and `source_column` are the engine's own two, which
                // say which table column the index is over. They are read by
                // `refresh_vector_indexes` rather than here.
                _ => {}
            }
        }
        if dims == 0 {
            return Err(error::misuse(
                "ivfflat: an index needs dims=N, the width of its vectors",
            ));
        }
        Ok(Settings {
            dims,
            lists,
            probes: probes.min(lists),
        })
    }
}

/// One connected index.
struct IvfFlatTable {
    settings: Settings,
    shadows: ShadowTables,
    declaration: Declaration,
    /// How many vectors the index holds, once anything has counted them.
    ///
    /// Counted once by a scan and then kept, so an insert costs a write rather
    /// than a walk of the whole index.
    rows: Option<usize>,
    /// How many it held when the clustering was last built.
    built: usize,
}

impl VirtualTable for IvfFlatTable {
    /// Returns the columns the index declares.
    fn declaration(&self) -> &Declaration {
        &self.declaration
    }

    /// Claims the vector and the neighbour count, and the ordering by distance.
    ///
    /// The two constraints are arguments rather than filters - there is no
    /// vector column to compare against - so both are claimed and neither is
    /// left for the engine to re-test. The ordering is claimed as well: a probe
    /// returns its rows nearest first, which is the whole point of asking one.
    ///
    /// @param info - the constraints and the ordering the planner is offering
    fn best_index(&self, info: &mut IndexQuery) -> DbResult<()> {
        for position in 0..info.constraints.len() {
            let Some(spec) = info.constraints.get(position).copied() else {
                continue;
            };
            if !spec.usable || spec.op != ConstraintOp::Eq {
                continue;
            }
            if spec.column == at::VECTOR || spec.column == at::K {
                info.use_constraint(position, true);
            }
        }
        if info
            .order_by
            .first()
            .is_some_and(|spec| spec.column == at::RANK && !spec.descending)
        {
            info.ordered = true;
        }
        // A probe reads `probes` lists of a corpus, so its cost is that share
        // of a scan - which is what makes the planner choose it over one.
        info.estimated_cost = 1.0;
        info.estimated_rows = 1;
        Ok(())
    }

    /// Opens a cursor over one probe's answer.
    fn open(&self) -> DbResult<Box<dyn VirtualCursor>> {
        Ok(Box::new(IvfFlatCursor {
            settings: self.settings.clone(),
            shadows: self.shadows.clone(),
            hits: Vec::new(),
            at: 0,
        }))
    }

    /// Writes one row of the index.
    ///
    /// An insert or a replace assigns the vector to its nearest centroid, so a
    /// row written after the build is found by a probe that reads that list. A
    /// delete removes it. Neither re-clusters: that happens on the next probe,
    /// and only when the corpus has outgrown its centroids.
    ///
    /// @param context - the shadow-table store
    /// @param change - what to write
    fn update(&mut self, context: &mut Context<'_>, change: &Change) -> DbResult<Option<i64>> {
        let (rowid, values): (i64, &[Value<'static>]) = match change {
            Change::Delete(key) => {
                let Value::Integer(rowid) = key else {
                    return Err(error::misuse("ivfflat: a delete needs a rowid"));
                };
                self.shadows.delete_row(context, ROW, *rowid)?;
                return Ok(None);
            }
            Change::Insert { rowid, values } => {
                let Value::Integer(rowid) = rowid else {
                    return Err(error::misuse(
                        "ivfflat: a row needs the table row's own rowid",
                    ));
                };
                (*rowid, values.as_slice())
            }
            Change::Update {
                old_rowid,
                new_rowid,
                values,
            } => {
                if let Value::Integer(old) = old_rowid {
                    self.shadows.delete_row(context, ROW, *old)?;
                }
                let Value::Integer(rowid) = new_rowid else {
                    return Err(error::misuse(
                        "ivfflat: a row needs the table row's own rowid",
                    ));
                };
                (*rowid, values.as_slice())
            }
        };
        // **The vector arrives in the hidden `vector` column**, which is where
        // the engine puts it for every index a module owns - `body` carries the
        // source row's number as text, so a hit can name the row it came from.
        let Some(vector) = values.get(at::VECTOR as usize).and_then(bytes_of) else {
            // A row with no vector is not indexed, which is what a NULL
            // embedding means: there is nothing to be near.
            self.shadows.delete_row(context, ROW, rowid)?;
            return Ok(Some(rowid));
        };
        let held = floats_of(&vector);
        if held.len() != self.settings.dims {
            return Err(error::misuse(format!(
                "ivfflat: this index holds vectors of {} dimensions, not {}",
                self.settings.dims,
                held.len()
            )));
        }
        let centroids = read_centroids(context, &self.shadows)?;
        let list = nearest_centroid(&centroids, &held).unwrap_or(0);
        self.shadows.write_row(
            context,
            ROW,
            rowid,
            &[
                Value::Integer(rowid),
                Value::Integer(list as i64),
                Value::owned_blob(&vector)?,
            ],
        )?;
        self.count(context)?;
        self.rebuild_if_stale(context)?;
        Ok(Some(rowid))
    }
}

impl IvfFlatTable {
    /// Counts the index once and then keeps counting incrementally.
    ///
    /// @param context - the shadow-table store
    fn count(&mut self, context: &mut Context<'_>) -> DbResult<()> {
        if self.rows.is_some() {
            self.rows = self.rows.map(|held| held.saturating_add(1));
            return Ok(());
        }
        let mut rows = 0usize;
        self.shadows.scan(context, ROW, |_rowid, _row| {
            rows = rows.saturating_add(1);
            Ok(true)
        })?;
        self.built = read_setting(context, &self.shadows, "built")?.unwrap_or(0);
        self.rows = Some(rows);
        Ok(())
    }

    /// Rebuilds the clustering when the corpus has outgrown it.
    ///
    /// **On the write path, because a read may not write.** The engine refuses
    /// a module that touches a shadow table while answering a query, and it is
    /// right to: a read that wrote would be a read that could deadlock, and one
    /// whose answer depended on whether it was the first. So the clustering is
    /// rebuilt where the rows arrive, on the doubling - which is the same
    /// amortised cost as rebuilding once at the end and needs no notification
    /// that the end has come.
    ///
    /// @param context - the shadow-table store
    fn rebuild_if_stale(&mut self, context: &mut Context<'_>) -> DbResult<()> {
        let rows = self.rows.unwrap_or(0);
        // A corpus smaller than two lists has one vector per list at best,
        // which is a scan wearing a clustering's clothes. It is left
        // unclustered and probed exhaustively, which is both faster and exact.
        if rows < 2 || rows < self.built.saturating_mul(REBUILD_GROWTH) {
            return Ok(());
        }
        let mut vectors: Vec<(i64, Vec<f32>)> = Vec::new();
        self.shadows.scan(context, ROW, |rowid, row| {
            let Some(vector) = row.get(2).and_then(bytes_of) else {
                return Ok(true);
            };
            vectors.push((rowid, floats_of(&vector)));
            Ok(true)
        })?;
        let lists = self.settings.lists.min(vectors.len());
        if lists < 2 {
            self.built = vectors.len();
            return write_setting(context, &self.shadows, "built", self.built);
        }
        let centroids = cluster(&vectors, lists);
        for (at, centroid) in centroids.iter().enumerate() {
            self.shadows.write_row(
                context,
                CENTROID,
                at as i64,
                &[
                    Value::Integer(at as i64),
                    Value::owned_blob(&bytes_for(centroid))?,
                ],
            )?;
        }
        for (rowid, vector) in &vectors {
            let list = nearest_centroid(&centroids, vector).unwrap_or(0);
            self.shadows.write_row(
                context,
                ROW,
                *rowid,
                &[
                    Value::Integer(*rowid),
                    Value::Integer(list as i64),
                    Value::owned_blob(&bytes_for(vector))?,
                ],
            )?;
        }
        self.built = vectors.len();
        self.rows = Some(vectors.len());
        write_setting(context, &self.shadows, "built", self.built)
    }
}

/// One probe's answer, in order.
struct IvfFlatCursor {
    settings: Settings,
    shadows: ShadowTables,
    /// The rowids the probe found, nearest first.
    hits: Vec<i64>,
    /// Where the cursor is in them.
    at: usize,
}

impl VirtualCursor for IvfFlatCursor {
    /// Runs one probe.
    ///
    /// @param context - the shadow-table store
    /// @param plan - the arguments `best_index` claimed
    fn filter(&mut self, context: &mut Context<'_>, plan: &FilterPlan) -> DbResult<()> {
        self.hits.clear();
        self.at = 0;
        let mut probe: Option<Vec<f32>> = None;
        let mut wanted = 1usize;
        for argument in &plan.arguments {
            match argument {
                Value::Blob(blob) => probe = Some(floats_of(blob.raw())),
                Value::Integer(number) => {
                    wanted = usize::try_from(*number).unwrap_or(1).max(1);
                }
                _ => {}
            }
        }
        let Some(probe) = probe else {
            return Ok(());
        };
        if probe.len() != self.settings.dims {
            return Ok(());
        }
        let centroids = read_centroids(context, &self.shadows)?;
        let lists = nearest_lists(&centroids, &probe, self.settings.probes);
        let mut scored: Vec<(f64, i64)> = Vec::new();
        self.shadows.scan(context, ROW, |rowid, row| {
            let Some(Value::Integer(list)) = row.get(1) else {
                return Ok(true);
            };
            // An index with no centroids yet has every row in list zero,
            // and `lists` is then empty - which is the exhaustive case, and
            // the right answer for a corpus too small to cluster.
            if !lists.is_empty() && !lists.contains(&(*list as usize)) {
                return Ok(true);
            }
            let Some(vector) = row.get(2).and_then(bytes_of) else {
                return Ok(true);
            };
            scored.push((cosine_distance(&probe, &floats_of(&vector)), rowid));
            Ok(true)
        })?;
        scored.sort_by(|one, two| {
            one.0
                .partial_cmp(&two.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(one.1.cmp(&two.1))
        });
        scored.truncate(wanted);
        self.hits = scored.into_iter().map(|(_, rowid)| rowid).collect();
        Ok(())
    }

    /// Steps to the next hit.
    fn next(&mut self, _context: &mut Context<'_>) -> DbResult<()> {
        self.at = self.at.saturating_add(1);
        Ok(())
    }

    /// Reports whether the probe is finished.
    fn eof(&self) -> bool {
        self.at >= self.hits.len()
    }

    /// Returns one column of the current hit.
    ///
    /// @param context - the shadow-table store
    /// @param index - which declared column
    fn column(&mut self, context: &mut Context<'_>, index: usize) -> DbResult<Value<'static>> {
        let _ = context;
        let Some(rowid) = self.hits.get(self.at).copied() else {
            return Ok(Value::Null);
        };
        match index as i32 {
            // The source row's number as text, which is what a hit is for.
            at::BODY => Value::owned_text(rowid.to_string().as_bytes()),
            _ => Ok(Value::Null),
        }
    }

    /// Returns the rowid of the current hit, which is the table row's own.
    fn rowid(&self) -> DbResult<i64> {
        self.hits
            .get(self.at)
            .copied()
            .ok_or_else(|| error::misuse("ivfflat: the cursor is past its last hit"))
    }
}

/// Returns the bytes of a blob or text value.
///
/// @param value - the column
fn bytes_of(value: &Value<'static>) -> Option<Vec<u8>> {
    match value {
        Value::Blob(blob) => Some(blob.raw().to_vec()),
        Value::Text(text) => Some(text.raw().to_vec()),
        _ => None,
    }
}

/// Reads a vector's components out of its bytes.
///
/// Little-endian 32-bit floats, which is the layout every vector in this engine
/// has; a length that is not a multiple of four reads as the whole components
/// it does hold.
///
/// @param bytes - the stored vector
fn floats_of(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|chunk| {
            let mut raw = [0u8; 4];
            raw.copy_from_slice(chunk);
            f32::from_bits(u32::from_le_bytes(raw))
        })
        .collect()
}

/// Returns a vector's bytes.
///
/// @param vector - the components
fn bytes_for(vector: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(vector.len().saturating_mul(4));
    for value in vector {
        out.extend_from_slice(&value.to_bits().to_le_bytes());
    }
    out
}

/// Returns one minus the cosine similarity of two vectors.
///
/// Zero for two vectors pointing the same way, one for two at right angles, two
/// for opposite ones - and one for a pair where either has no length, because a
/// vector of zeroes points nowhere and is no nearer to one thing than another.
///
/// @param one - a vector
/// @param two - the other
fn cosine_distance(one: &[f32], two: &[f32]) -> f64 {
    let mut dot = 0.0f64;
    let mut left = 0.0f64;
    let mut right = 0.0f64;
    for (a, b) in one.iter().zip(two.iter()) {
        dot += f64::from(*a) * f64::from(*b);
        left += f64::from(*a) * f64::from(*a);
        right += f64::from(*b) * f64::from(*b);
    }
    let length = left.sqrt() * right.sqrt();
    if length == 0.0 {
        return 1.0;
    }
    1.0 - dot / length
}

/// Reads the centroids, in list order.
///
/// @param context - the shadow-table store
/// @param shadows - the index's tables
fn read_centroids(context: &mut Context<'_>, shadows: &ShadowTables) -> DbResult<Vec<Vec<f32>>> {
    let mut held: Vec<(i64, Vec<f32>)> = Vec::new();
    shadows.scan(context, CENTROID, |id, row| {
        let Some(vector) = row.get(1).and_then(bytes_of) else {
            return Ok(true);
        };
        held.push((id, floats_of(&vector)));
        Ok(true)
    })?;
    held.sort_by_key(|(id, _)| *id);
    Ok(held.into_iter().map(|(_, vector)| vector).collect())
}

/// Returns which centroid a vector belongs to.
///
/// @param centroids - the clustering
/// @param vector - the vector to place
fn nearest_centroid(centroids: &[Vec<f32>], vector: &[f32]) -> Option<usize> {
    let mut best: Option<(f64, usize)> = None;
    for (at, centroid) in centroids.iter().enumerate() {
        let distance = cosine_distance(centroid, vector);
        if best.is_none_or(|(held, _)| distance < held) {
            best = Some((distance, at));
        }
    }
    best.map(|(_, at)| at)
}

/// Returns the `probes` centroids nearest a query, nearest first.
///
/// @param centroids - the clustering
/// @param probe - the query vector
/// @param probes - how many lists to read
fn nearest_lists(centroids: &[Vec<f32>], probe: &[f32], probes: usize) -> Vec<usize> {
    let mut ranked: Vec<(f64, usize)> = centroids
        .iter()
        .enumerate()
        .map(|(at, centroid)| (cosine_distance(centroid, probe), at))
        .collect();
    ranked.sort_by(|one, two| {
        one.0
            .partial_cmp(&two.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(one.1.cmp(&two.1))
    });
    ranked.truncate(probes);
    ranked.into_iter().map(|(_, at)| at).collect()
}

/// Returns `lists` centroids over a corpus, by k-means.
///
/// **k-means++ seeding and Lloyd's iteration**, which is what pgvector's own
/// build does and what makes the result reproducible: the seeding walks the
/// corpus by a deterministic stride rather than by a random draw, so an index
/// built twice over the same rows has the same lists both times. That matters
/// more here than the marginal recall a random seed buys: a differential test
/// that rebuilt an index and got different lists could not compare anything.
///
/// @param vectors - the corpus, with the rowid of each
/// @param lists - how many centroids to find
fn cluster(vectors: &[(i64, Vec<f32>)], lists: usize) -> Vec<Vec<f32>> {
    let width = vectors.first().map(|(_, held)| held.len()).unwrap_or(0);
    if width == 0 || lists == 0 {
        return Vec::new();
    }
    // The seeds are spread across the corpus rather than taken from its head,
    // so a corpus that arrived in order does not start with every centroid in
    // one corner of it.
    let stride = vectors.len().div_ceil(lists).max(1);
    let mut centroids: Vec<Vec<f32>> = (0..lists)
        .map(|at| {
            vectors
                .get(
                    at.saturating_mul(stride)
                        .min(vectors.len().saturating_sub(1)),
                )
                .map(|(_, held)| held.clone())
                .unwrap_or_else(|| vec![0.0; width])
        })
        .collect();
    let mut assignment: Vec<usize> = vec![usize::MAX; vectors.len()];
    for _ in 0..PASSES {
        let mut moved = false;
        for (at, (_, vector)) in vectors.iter().enumerate() {
            let list = nearest_centroid(&centroids, vector).unwrap_or(0);
            if assignment.get(at).copied() != Some(list) {
                moved = true;
            }
            if let Some(slot) = assignment.get_mut(at) {
                *slot = list;
            }
        }
        if !moved {
            break;
        }
        let mut totals: Vec<Vec<f64>> = vec![vec![0.0; width]; lists];
        let mut counts: Vec<usize> = vec![0; lists];
        for (at, (_, vector)) in vectors.iter().enumerate() {
            let Some(list) = assignment.get(at).copied() else {
                continue;
            };
            let Some(total) = totals.get_mut(list) else {
                continue;
            };
            for (slot, value) in total.iter_mut().zip(vector.iter()) {
                *slot += f64::from(*value);
            }
            if let Some(count) = counts.get_mut(list) {
                *count = count.saturating_add(1);
            }
        }
        for (list, centroid) in centroids.iter_mut().enumerate() {
            let count = counts.get(list).copied().unwrap_or(0);
            if count == 0 {
                // An empty list keeps the centroid it had rather than
                // collapsing to the origin, which would then attract every
                // vector whose components sum near zero.
                continue;
            }
            let Some(total) = totals.get(list) else {
                continue;
            };
            for (slot, sum) in centroid.iter_mut().zip(total.iter()) {
                *slot = (*sum / count as f64) as f32;
            }
        }
    }
    centroids
}

/// Reads one number out of the settings table.
///
/// @param context - the shadow-table store
/// @param shadows - the index's tables
/// @param key - which setting
fn read_setting(
    context: &mut Context<'_>,
    shadows: &ShadowTables,
    key: &str,
) -> DbResult<Option<usize>> {
    let mut found = None;
    shadows.scan(context, CONFIG, |_rowid, row| {
        let Some(name) = row.get(1).and_then(bytes_of) else {
            return Ok(true);
        };
        if name != key.as_bytes() {
            return Ok(true);
        }
        found = row
            .get(2)
            .and_then(bytes_of)
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .and_then(|text| text.parse::<usize>().ok());
        Ok(false)
    })?;
    Ok(found)
}

/// Writes one number into the settings table.
///
/// @param context - the shadow-table store
/// @param shadows - the index's tables
/// @param key - which setting
/// @param value - what it is now
fn write_setting(
    context: &mut Context<'_>,
    shadows: &ShadowTables,
    key: &str,
    value: usize,
) -> DbResult<()> {
    // One row per setting, keyed by a stable rowid so a rewrite replaces it.
    let rowid = key.bytes().fold(0i64, |held, byte| {
        held.wrapping_mul(31).wrapping_add(i64::from(byte))
    });
    shadows.write_row(
        context,
        CONFIG,
        rowid,
        &[
            Value::Integer(rowid),
            Value::owned_text(key.as_bytes())?,
            Value::owned_text(value.to_string().as_bytes())?,
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The distance is zero for a vector against itself and one for a right
    /// angle, which is what every comparison here depends on.
    #[test]
    fn cosine_distance_is_zero_for_a_vector_against_itself() {
        let one = vec![1.0f32, 0.0, 0.0, 0.0];
        let two = vec![0.0f32, 1.0, 0.0, 0.0];
        assert!(cosine_distance(&one, &one) < 1e-6);
        assert!((cosine_distance(&one, &two) - 1.0).abs() < 1e-6);
        assert!((cosine_distance(&one, &[-1.0, 0.0, 0.0, 0.0]) - 2.0).abs() < 1e-6);
    }

    /// A vector of zeroes is no nearer to one thing than another.
    #[test]
    fn a_vector_with_no_length_is_one_away_from_everything() {
        let zero = vec![0.0f32; 4];
        assert!((cosine_distance(&zero, &[1.0, 0.0, 0.0, 0.0]) - 1.0).abs() < 1e-6);
    }

    /// The clustering separates two obvious groups, and does it the same way
    /// twice.
    #[test]
    fn the_clustering_finds_two_groups_and_is_reproducible() {
        let mut corpus: Vec<(i64, Vec<f32>)> = Vec::new();
        for at in 0..10i64 {
            corpus.push((at, vec![1.0, 0.0, 0.0, at as f32 * 0.01]));
        }
        for at in 10..20i64 {
            corpus.push((at, vec![0.0, 1.0, 0.0, at as f32 * 0.01]));
        }
        let one = cluster(&corpus, 2);
        let two = cluster(&corpus, 2);
        assert_eq!(one.len(), 2);
        assert_eq!(one, two, "the same corpus clusters the same way twice");
        // Every vector of the first group lands in one list and every vector of
        // the second in the other.
        let first: Vec<usize> = corpus
            .iter()
            .take(10)
            .filter_map(|(_, held)| nearest_centroid(&one, held))
            .collect();
        let second: Vec<usize> = corpus
            .iter()
            .skip(10)
            .filter_map(|(_, held)| nearest_centroid(&one, held))
            .collect();
        assert!(first.iter().all(|list| *list == first[0]));
        assert!(second.iter().all(|list| *list == second[0]));
        assert_ne!(first[0], second[0]);
    }

    /// The probe order is by distance, so the nearest list comes first.
    #[test]
    fn the_lists_a_probe_reads_are_the_nearest_ones() {
        let centroids = vec![
            vec![1.0f32, 0.0, 0.0, 0.0],
            vec![0.0f32, 1.0, 0.0, 0.0],
            vec![0.0f32, 0.0, 1.0, 0.0],
        ];
        assert_eq!(nearest_lists(&centroids, &[0.0, 1.0, 0.0, 0.0], 1), vec![1]);
        assert_eq!(nearest_lists(&centroids, &[0.0, 1.0, 0.0, 0.0], 3).len(), 3);
        assert_eq!(
            nearest_lists(&centroids, &[0.0, 1.0, 0.0, 0.0], 2).first(),
            Some(&1)
        );
    }
}
