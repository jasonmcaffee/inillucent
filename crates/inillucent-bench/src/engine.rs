//! Both engines behind one trait, so no scenario can accidentally run against
//! only one of them.
//!
//! The pgvector implementation issues the SQL a PostgreSQL application issues
//! for this workload, one statement per retrieval method, and fuses the two with
//! the same Reciprocal Rank Fusion constants inillucent uses, so the baseline is a
//! working system rather than a reconstruction of one. It is graded in two
//! configurations: `Default`, which is the pgvector extension's own defaults
//! with nothing set, and `WellConfigured`, which is pgvector configured as well
//! as pgvector can be configured. Reporting only the defaults would flatter
//! inillucent by comparing against a misconfiguration.
//!
//! `WellConfigured` chooses its settings per query rather than once per
//! connection, because the right settings differ between a query that carries a
//! filter and a query that does not. A query with a filter gets
//! `hnsw.iterative_scan = relaxed_order`, `hnsw.ef_search = 400`,
//! `hnsw.max_scan_tuples = 40000` and `hnsw.scan_mem_multiplier = 4`. A query
//! with no filter beyond `deleted_at IS NULL` gets `hnsw.iterative_scan = off`
//! and `hnsw.ef_search = 100`, because `deleted_at IS NULL` excludes so few
//! chunks that the iterative scan has nothing to recover. `pg_session_settings` is the one place these
//! values live, and it is a pure function so the tests can assert the exact
//! statement list.

use std::collections::HashMap;

use anyhow::{Context, Result};
use inillucent_core::filter::Filter;
use inillucent_core::index::Index;
use inillucent_core::rank::{self, Fusion, PER_DOC_CAP};
use pgvector::Vector;
use postgres::{Client, NoTls};

/// A hit identified the way both engines can agree on: the chunk's identifier in
/// the source database, as text.
#[derive(Debug, Clone)]
pub struct Hit {
    pub key: String,
    pub score: f32,
    /// How good this hit is in absolute terms, in `[0, 1]`, independent of how
    /// good the rest of the candidate list was. See `rank::FusedHit::confidence`.
    /// The baseline computes the same quantity from its own calibrated ceiling, so
    /// the abstention family can ask both engines the same question without
    /// assuming their raw scores are on the same scale.
    pub confidence: f32,
}

pub trait SearchEngine {
    fn name(&self) -> &str;
    fn vector_search(&mut self, query: &[f32], filter: &Filter, k: usize) -> Result<Vec<Hit>>;
    fn lexical_search(&mut self, query: &str, filter: &Filter, k: usize) -> Result<Vec<Hit>>;
    fn hybrid_search(
        &mut self,
        query: &str,
        query_vector: &[f32],
        filter: &Filter,
        k: usize,
    ) -> Result<Vec<Hit>>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PgMode {
    /// The pgvector extension's own defaults, with nothing set: `hnsw.ef_search`
    /// at 40 and no iterative scan. Reported to show what the extension does
    /// before anybody configures it, and never scored against.
    Default,
    /// pgvector configured as well as pgvector can be configured, so that
    /// beating it means something.
    WellConfigured,
}

impl PgMode {
    /// What the score card calls this arm.
    ///
    /// The label says which pgvector is being measured, because the two
    /// differ by more than a setting: one is the extension as installed and
    /// one is the extension as its own documentation asks for it.
    pub fn label(&self) -> &'static str {
        match self {
            PgMode::Default => "pgvector (extension defaults)",
            PgMode::WellConfigured => "pgvector (correctly configured)",
        }
    }
}

/// The only clause every query carries. A `WHERE` clause equal to this one means
/// the query has no filter.
const DELETED_ONLY: &str = "d.deleted_at IS NULL";

/// `hnsw.ef_search` when nothing sets it: the pgvector extension's own default.
const EXTENSION_DEFAULT_EF_SEARCH: usize = 40;

/// `hnsw.ef_search` for a query that carries a filter. `ef_search` is how many
/// candidates the scan collects, and a scan cannot return more rows than it
/// collected, so a filtered query needs a much larger pool than an unfiltered
/// one to find enough rows that pass the filter. Chosen from a sweep against
/// exhaustive cosine over the chunks each filter admits.
const FILTERED_EF_SEARCH: usize = 400;

/// `hnsw.ef_search` for a query with no filter beyond `deleted_at IS NULL`.
/// Nearly every collected candidate passes the one remaining clause, so a larger
/// pool buys very little. Over 50 probes against exhaustive cosine, mean recall at
/// 50 was 0.9644 at 100, 0.9752 at 200 and 0.9792 at 400, while latency went
/// 5.85 ms, 9.58 ms and 16.75 ms: 400 costs nearly three times as much for 0.015
/// more recall, so 100 is the value.
///
/// Those figures come from the private database this change was validated
/// against, not from this repository's corpus, so they are the reason for the
/// value rather than a result this repository reproduces. What this corpus
/// measures is in the score card.
const UNFILTERED_EF_SEARCH: usize = 100;

/// `hnsw.max_scan_tuples` for a filtered query. A sweep left mean recall unchanged
/// at 0.788 between 20,000 and 200,000, so the larger value buys no accuracy and
/// only costs latency.
///
/// Those two figures were measured against a different corpus on a
/// different database, not against the corpus this repository grades, so they are
/// the reason the value was chosen rather than a number this repository can
/// reproduce. What this repository does measure is in the score card.
const FILTERED_MAX_SCAN_TUPLES: usize = 40_000;

/// `hnsw.scan_mem_multiplier` for a filtered query. At pgvector's default of 1
/// the iterative scan exhausts its memory budget and stops early, which is why a
/// search restricted to the smallest sources returned as few as 30 rows of 50
/// even with the iterative scan on, and it held mean recall at 50 down to 0.788.
/// Raising it to 4 removed the short results and raised mean recall to 0.856.
/// Raising it to 8 changed nothing, so 4 is the value.
///
/// Those figures were measured against a different corpus on a
/// different database. They are why the value is 4, not a result this repository
/// reproduces. What this repository measures is in the score card.
const FILTERED_SCAN_MEM_MULTIPLIER: usize = 4;

/// The `SET` statements a mode runs before one query, in the order they are
/// executed.
///
/// Pure, and the one place these values live, so a test can assert the exact
/// list. The settings depend on the query rather than only on the mode, because
/// whether the iterative scan does anything at all depends on whether the query
/// carries a filter, and because `hnsw.ef_search` has to be at least the number
/// of rows the caller asked for.
///
/// The two settings that apply only to an iterative scan are `RESET` rather than
/// left alone when the iterative scan is off, so this list describes the whole
/// session state for the query that follows it and not just the part of it that
/// changed. Without the `RESET` a filtered query would leave
/// `hnsw.max_scan_tuples` and `hnsw.scan_mem_multiplier` set on the connection
/// for every later unfiltered query on it.
pub fn pg_session_settings(mode: PgMode, filter: &Filter, k: usize) -> Vec<String> {
    let filtered = PgVectorEngine::has_predicate_beyond_deleted(filter);

    let iterative_scan = match (mode, filtered) {
        // `relaxed_order` rather than `strict_order`: at the same cost,
        // `relaxed_order` reached mean recall 0.856 against `strict_order`'s
        // 0.727. Those two figures were measured against a different
        // corpus on a different database, so they are the reason for the choice
        // rather than something this repository reproduces. Nothing downstream
        // depends on pgvector's within-scan ordering in any case, because
        // Reciprocal Rank Fusion recomputes the ranking afterwards.
        (PgMode::WellConfigured, true) => "relaxed_order",
        // Off, because there is nothing for the iterative scan to recover. The
        // only clause left is `deleted_at IS NULL`, which excludes a few hundred
        // chunks of the whole corpus, so nearly every candidate the scan collects
        // already passes it. Over 50 probes against exhaustive cosine, mean recall
        // at 50 was identical with it off and with it on at every
        // `hnsw.ef_search` tried: 0.9644 at 100, 0.9752 at 200, 0.9792 at 400.
        // Latency was the same as well, 5.85 ms against 5.98 ms per query at 100.
        // So it is turned off because it changes nothing on an unfiltered query,
        // and the simpler configuration is the one to report.
        //
        // Those figures are from the private database this was validated against,
        // not this repository's corpus. A 20 probe run of the same measurement
        // appeared to show recall 0.9670 off against 0.9960 on; that was too small
        // a sample and the difference did not survive 50 probes. Recorded so the
        // next person does not act on it.
        (PgMode::WellConfigured, false) => "off",
        (PgMode::Default, _) => "off",
    };

    let ef_search = match (mode, filtered) {
        // Not raised to `k`. This column exists to say what the extension does
        // with nothing set, so raising `hnsw.ef_search` here would report a
        // configuration that is neither the defaults nor a considered choice.
        (PgMode::Default, _) => EXTENSION_DEFAULT_EF_SEARCH,
        // Raised to `k` when a caller asks for more rows than the floor would
        // collect, because a scan cannot return more rows than it collected.
        (PgMode::WellConfigured, true) => FILTERED_EF_SEARCH.max(k),
        (PgMode::WellConfigured, false) => UNFILTERED_EF_SEARCH.max(k),
    };

    let mut statements = vec![
        format!("SET hnsw.iterative_scan = {iterative_scan}"),
        format!("SET hnsw.ef_search = {ef_search}"),
    ];
    if iterative_scan == "off" {
        statements.push("RESET hnsw.max_scan_tuples".to_string());
        statements.push("RESET hnsw.scan_mem_multiplier".to_string());
    } else {
        statements.push(format!(
            "SET hnsw.max_scan_tuples = {FILTERED_MAX_SCAN_TUPLES}"
        ));
        statements.push(format!(
            "SET hnsw.scan_mem_multiplier = {FILTERED_SCAN_MEM_MULTIPLIER}"
        ));
    }
    statements
}

pub struct PgVectorEngine {
    client: Client,
    mode: PgMode,
    name: String,
    /// Candidates drawn from each side before fusion. 50, the same number inillucent
    /// draws, so neither engine is handed a larger pool than the other.
    candidates: usize,
    /// How the two lists are combined. The SAME method inillucent uses, because a
    /// fusion the baseline was denied would make the hybrid family measure ranking
    /// policy rather than retrieval. The harness sets it on both engines together.
    fusion: Fusion,
    /// The ceiling `Fusion::TheoreticalMinMax` divides this engine's lexical
    /// scores by.
    ///
    /// inillucent computes its own analytically: BM25 saturates at the query's idf
    /// mass, and that bound is a property of the query rather than of the results.
    /// `ts_rank_cd` has no such closed form, so the harness measures one instead,
    /// from a sample of this engine's own scores on queries the graded run will
    /// not use. Each engine is therefore scaled by the best bound available for
    /// its own scoring function, which is what keeps the fusion the same policy
    /// for both rather than a policy one of them was denied.
    lexical_ceiling: f32,
}

impl PgVectorEngine {
    /// Opens one connection and holds it for the whole run.
    ///
    /// **One connection rather than a pool, because the session settings are
    /// the arm.** `hnsw.ef_search` and the rest are set on this session per
    /// query, and a pooled connection would hand the next query somebody
    /// else's settings - so the arm being measured would not be the arm named.
    ///
    /// @param url - the PostgreSQL connection string
    /// @param mode - which pgvector configuration this arm is
    pub fn connect(url: &str, mode: PgMode) -> Result<Self> {
        let client = Client::connect(url, NoTls).context("connecting to PostgreSQL")?;
        Ok(PgVectorEngine {
            client,
            mode,
            name: mode.label().to_string(),
            candidates: 50,
            fusion: Fusion::default(),
            lexical_ceiling: 1.0,
        })
    }

    /// Sets the fusion method, so the harness can hand both engines the same one.
    /// @param fusion - the method to use in `hybrid_search`
    pub fn set_fusion(&mut self, fusion: Fusion) {
        self.fusion = fusion;
    }

    /// Sets the bound theoretical min-max normalization divides this engine's
    /// lexical scores by. Only read by that fusion.
    /// @param ceiling - the largest lexical score this engine is expected to produce
    #[allow(dead_code)]
    pub fn set_lexical_ceiling(&mut self, ceiling: f32) {
        self.lexical_ceiling = ceiling;
    }

    /// The largest lexical score this engine produced over a sample of queries,
    /// at the given percentile, which is the empirical stand-in for a bound
    /// `ts_rank_cd` does not analytically have.
    ///
    /// A percentile rather than the maximum, because one query matching a
    /// pathologically repetitive chunk would set a ceiling nothing else ever
    /// approaches, and every later query would then be scaled to near zero.
    /// @param queries - calibration queries, from seeds the graded run does not use
    /// @param percentile - where in the observed scores to put the ceiling
    pub fn calibrate_lexical_ceiling(
        &mut self,
        queries: &[String],
        percentile: f64,
    ) -> Result<f32> {
        let filter = Filter::default();
        let mut scores: Vec<f32> = Vec::new();
        for q in queries {
            for hit in self.lexical_search(q, &filter, 10)? {
                scores.push(hit.score);
            }
        }
        if scores.is_empty() {
            return Ok(self.lexical_ceiling);
        }
        scores.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let at = ((percentile * scores.len() as f64).ceil() as usize)
            .saturating_sub(1)
            .min(scores.len().saturating_sub(1));
        // `at` is clamped to the last index and `scores` is not empty, so the
        // fallback is unreachable; it is written rather than asserted because an
        // absent ceiling is a floor of `f32::EPSILON` either way.
        let ceiling = scores.get(at).copied().unwrap_or(0.0).max(f32::EPSILON);
        self.lexical_ceiling = ceiling;
        Ok(ceiling)
    }

    /// Runs `pg_session_settings` for the query about to be issued. `SET LOCAL`
    /// would need a transaction around every statement, and a plain `SET` on
    /// this session is equivalent here because the harness owns the connection.
    fn apply_settings(&mut self, filter: &Filter, k: usize) -> Result<()> {
        let statements = pg_session_settings(self.mode, filter, k);
        self.client
            .batch_execute(&format!("{};", statements.join("; ")))
            .context("applying the pgvector session settings")?;
        Ok(())
    }

    /// Whether the query carries a predicate beyond `deleted_at IS NULL`, which is
    /// what decides whether the iterative scan earns its cost.
    ///
    /// Answered by building the `WHERE` clause and asking whether anything but
    /// `deleted_at IS NULL` came out of it, rather than by reading the fields of
    /// `Filter` a second time. `Filter::is_empty` is not the same test:
    /// `where_clause` treats an empty `sources`, `authors` or `labels` list as no
    /// filter at all, which is what the SQL does, while `is_empty` only asks
    /// whether the field is absent. Deriving the answer from `where_clause` means
    /// the two cannot disagree.
    pub fn has_predicate_beyond_deleted(filter: &Filter) -> bool {
        Self::where_clause(filter, 2).0 != DELETED_ONLY
    }

    /// The filter clauses, one per predicate the caller set, with values bound
    /// positionally starting at `$start`. The same predicates inillucent compiles, so
    /// both engines are asked for the same rows.
    fn where_clause(filter: &Filter, start: usize) -> (String, Vec<Box<dyn ToSqlSync>>) {
        let mut clauses = vec![DELETED_ONLY.to_string()];
        let mut values: Vec<Box<dyn ToSqlSync>> = Vec::new();
        let mut i = start;

        if let Some(space) = &filter.space_key {
            values.push(Box::new(space.clone()));
            clauses.push(format!("d.space_key = ${i}"));
            i += 1;
        }
        match (&filter.sources, &filter.source) {
            (Some(list), _) if !list.is_empty() => {
                values.push(Box::new(list.clone()));
                clauses.push(format!("d.source = ANY(${i}::text[])"));
                i += 1;
            }
            (_, Some(one)) => {
                values.push(Box::new(one.clone()));
                clauses.push(format!("d.source = ${i}"));
                i += 1;
            }
            _ => {}
        }
        if let Some(authors) = &filter.authors {
            if !authors.is_empty() {
                let sources: Vec<String> = authors
                    .iter()
                    .map(|(s, _)| s.clone())
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .collect();
                values.push(Box::new(sources));
                let src_idx = i;
                i += 1;
                let mut tuples = Vec::new();
                for (s, a) in authors {
                    values.push(Box::new(s.clone()));
                    values.push(Box::new(a.clone()));
                    tuples.push(format!("(d.source = ${} AND d.author_id = ${})", i, i + 1));
                    i += 2;
                }
                clauses.push(format!(
                    "(NOT (d.source = ANY(${src_idx}::text[])) OR {})",
                    tuples.join(" OR ")
                ));
            }
        } else if let Some(author) = &filter.author {
            values.push(Box::new(author.clone()));
            clauses.push(format!("(d.author ILIKE ${i} OR d.author_id = ${i})"));
            i += 1;
        }
        if let Some(after) = filter.updated_after {
            let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(after as u64);
            values.push(Box::new(t));
            clauses.push(format!("d.updated_at >= ${i}"));
            i += 1;
        }
        if let Some(labels) = &filter.labels {
            if !labels.is_empty() {
                values.push(Box::new(labels.clone()));
                clauses.push(format!("d.labels && ${i}::text[]"));
            }
        }
        (clauses.join(" AND "), values)
    }

    /// The tsquery a PostgreSQL application builds for this workload: non word
    /// characters stripped, `:*` appended for prefix matching, terms joined with
    /// `&` so every term must be present. The `&` is `to_tsquery`'s own default
    /// rather than a choice made here, and it is one of the things being measured,
    /// so it is left as it is. Joining with `|` would return more rows and has not
    /// been measured.
    fn ts_query(query: &str) -> String {
        query
            .split_whitespace()
            .map(|t| {
                t.chars()
                    .filter(|c| c.is_alphanumeric() || *c == '_')
                    .collect::<String>()
            })
            .filter(|t| !t.is_empty())
            .map(|t| format!("{t}:*"))
            .collect::<Vec<_>>()
            .join(" & ")
    }
}

/// A small object safe alias so the filter values can be collected heterogeneously.
pub trait ToSqlSync: postgres::types::ToSql + Sync + Send {}
impl<T: postgres::types::ToSql + Sync + Send> ToSqlSync for T {}

impl SearchEngine for PgVectorEngine {
    fn name(&self) -> &str {
        &self.name
    }

    fn vector_search(&mut self, query: &[f32], filter: &Filter, k: usize) -> Result<Vec<Hit>> {
        self.apply_settings(filter, k)?;
        let (where_sql, values) = Self::where_clause(filter, 2);
        let sql = format!(
            "SELECT d.id::text || '#' || c.chunk_index::text AS key,
                    (c.embedding <=> $1::vector) AS distance
             FROM chunks c JOIN documents d ON d.id = c.document_id
             WHERE {where_sql} AND c.embedding IS NOT NULL
             ORDER BY c.embedding <=> $1::vector
             LIMIT {k}"
        );
        let vector = Vector::from(query.to_vec());
        let mut params: Vec<&(dyn postgres::types::ToSql + Sync)> = vec![&vector];
        for v in &values {
            params.push(v.as_ref() as &(dyn postgres::types::ToSql + Sync));
        }
        let rows = self.client.query(&sql, &params)?;
        Ok(rows
            .iter()
            .map(|r| {
                let similarity = 1.0 - r.get::<_, f64>("distance") as f32;
                Hit {
                    key: r.get("key"),
                    score: similarity,
                    confidence: similarity.clamp(0.0, 1.0),
                }
            })
            .collect())
    }

    fn lexical_search(&mut self, query: &str, filter: &Filter, k: usize) -> Result<Vec<Hit>> {
        let ts = Self::ts_query(query);
        if ts.is_empty() {
            return Ok(Vec::new());
        }
        let (where_sql, values) = Self::where_clause(filter, 2);
        let sql = format!(
            "SELECT d.id::text || '#' || c.chunk_index::text AS key,
                    ts_rank_cd(to_tsvector('english', c.content), to_tsquery('english', $1)) AS rank
             FROM chunks c JOIN documents d ON d.id = c.document_id
             WHERE to_tsvector('english', c.content) @@ to_tsquery('english', $1)
               AND {where_sql}
             ORDER BY rank DESC
             LIMIT {k}"
        );
        let mut params: Vec<&(dyn postgres::types::ToSql + Sync)> = vec![&ts];
        for v in &values {
            params.push(v.as_ref() as &(dyn postgres::types::ToSql + Sync));
        }
        // A malformed tsquery is a property of the generated query string, not a
        // failure of the engine, so it counts as an empty result rather than
        // aborting the whole run.
        let rows = match self.client.query(&sql, &params) {
            Ok(r) => r,
            Err(_) => return Ok(Vec::new()),
        };
        let ceiling = self.lexical_ceiling;
        Ok(rows
            .iter()
            .map(|r| {
                let score: f32 = r.get("rank");
                Hit {
                    key: r.get("key"),
                    score,
                    confidence: if ceiling > f32::EPSILON {
                        (score / ceiling).clamp(0.0, 1.0)
                    } else {
                        0.0
                    },
                }
            })
            .collect())
    }

    fn hybrid_search(
        &mut self,
        query: &str,
        query_vector: &[f32],
        filter: &Filter,
        k: usize,
    ) -> Result<Vec<Hit>> {
        let vector_hits = self.vector_search(query_vector, filter, self.candidates)?;
        let lexical_hits = self.lexical_search(query, filter, self.candidates)?;
        Ok(fuse_keyed(
            &vector_hits,
            &lexical_hits,
            self.fusion,
            k,
            PER_DOC_CAP,
            self.lexical_ceiling,
        ))
    }
}

/// inillucent behind the same trait. Chunk ordinals are mapped back to the source
/// database identifiers so hits from the two engines are directly comparable.
pub struct InillucentEngine {
    pub index: Index,
    pub keys: Vec<String>,
    pub name: String,
    pub ef_search: Option<usize>,
    /// Traversal width for a query that carries a predicate. The baseline is given a
    /// wider budget on a filtered query than on an unfiltered one — `hnsw.ef_search`
    /// 400 against 100 — because a filtered scan has to walk past the rows the
    /// predicate rejects. inillucent is given the same asymmetry, so the filtered family
    /// compares two engines that were allowed to look equally hard rather than one
    /// that was allowed to look four times harder.
    pub filtered_ef_search: Option<usize>,
    /// Key back to chunk ordinal. Without it the correctness and invariant
    /// scenarios scan every one of the corpus keys per returned row, which costs more than
    /// every query in the suite put together.
    ordinal_of: HashMap<String, u32>,
}

impl InillucentEngine {
    /// Wraps a built index as a graded arm.
    ///
    /// The key list is inverted here rather than searched later: the
    /// correctness and invariant scenarios ask for the ordinal of a returned
    /// key, and scanning the corpus keys per returned row costs more than
    /// every query in the suite put together.
    ///
    /// @param index - the built index this arm searches
    /// @param keys - the corpus key per chunk ordinal, in corpus order
    /// @param name - what the score card calls this arm
    /// @param ef_search - the traversal width, or the index's own default
    pub fn new(index: Index, keys: Vec<String>, name: String, ef_search: Option<usize>) -> Self {
        let ordinal_of = keys
            .iter()
            .enumerate()
            .map(|(i, k)| (k.clone(), i as u32))
            .collect();
        InillucentEngine {
            index,
            keys,
            name,
            ef_search,
            filtered_ef_search: ef_search,
            ordinal_of,
        }
    }

    /// The traversal width for one query: the filtered budget when the predicate
    /// restricts anything, the unfiltered one otherwise.
    /// @param filter - the predicate this query carries
    fn budget_for(&self, filter: &Filter) -> Option<usize> {
        // The same test the PostgreSQL side uses to decide its own session settings, so
        // the two engines widen their search on exactly the same queries.
        if PgVectorEngine::has_predicate_beyond_deleted(filter) {
            self.filtered_ef_search
        } else {
            self.ef_search
        }
    }

    /// The source database identifier a chunk ordinal stands for.
    ///
    /// **Fallible, because an ordinal with no key means the index and the key
    /// list disagree about what was loaded.** Answering an empty string there
    /// would score a hit against a document that is not in the corpus, which
    /// moves a published recall number rather than failing the run.
    ///
    /// @param chunk - the chunk ordinal a search returned
    fn key(&self, chunk: u32) -> Result<String> {
        self.keys.get(chunk as usize).cloned().with_context(|| {
            format!(
                "chunk {chunk} has no key: the index returned an ordinal past the \
                 {} keys the harness loaded with it",
                self.keys.len()
            )
        })
    }

    /// The chunk ordinal a corpus key stands for, or `None` when this arm
    /// was not built with that chunk.
    ///
    /// The inverse of [`InillucentEngine::key`], and the reason `new` builds a
    /// map.
    ///
    /// @param key - the corpus key a hit carried
    pub fn ordinal(&self, key: &str) -> Option<u32> {
        self.ordinal_of.get(key).copied()
    }
}

impl SearchEngine for InillucentEngine {
    fn name(&self) -> &str {
        &self.name
    }

    fn vector_search(&mut self, query: &[f32], filter: &Filter, k: usize) -> Result<Vec<Hit>> {
        let compiled = self.index.compile(filter);
        self.index
            .vector_search(query, &compiled, k, self.budget_for(filter))?
            .into_iter()
            .map(|n| {
                Ok(Hit {
                    key: self.key(n.chunk)?,
                    score: 1.0 - n.distance,
                    confidence: (1.0 - n.distance).clamp(0.0, 1.0),
                })
            })
            .collect()
    }

    fn lexical_search(&mut self, query: &str, filter: &Filter, k: usize) -> Result<Vec<Hit>> {
        let compiled = self.index.compile(filter);
        // The query's own BM25 saturation point, which is what turns a raw score
        // into an absolute one. It does not depend on the results.
        let ceiling = self.index.lexical_score_ceiling(query);
        self.index
            .lexical_search(query, &compiled, k)
            .into_iter()
            .map(|h| {
                Ok(Hit {
                    key: self.key(h.chunk)?,
                    score: h.score,
                    confidence: if ceiling > f32::EPSILON {
                        (h.score / ceiling).clamp(0.0, 1.0)
                    } else {
                        0.0
                    },
                })
            })
            .collect()
    }

    fn hybrid_search(
        &mut self,
        query: &str,
        query_vector: &[f32],
        filter: &Filter,
        k: usize,
    ) -> Result<Vec<Hit>> {
        let compiled = self.index.compile(filter);
        self.index
            .hybrid_search(query, query_vector, &compiled, k, self.budget_for(filter))?
            .into_iter()
            .map(|h| {
                Ok(Hit {
                    key: self.key(h.chunk)?,
                    score: h.score,
                    confidence: h.confidence,
                })
            })
            .collect()
    }
}

/// Exhaustive cosine over the chunks passing the filter, which defines the
/// correct answer for every vector accuracy scenario. Computed inside inillucent
/// because it is exact by construction and identical for both engines.
pub fn exhaustive_reference(
    engine: &InillucentEngine,
    query: &[f32],
    filter: &Filter,
    k: usize,
) -> Result<Vec<String>> {
    let compiled = engine.index.compile(filter);
    engine
        .index
        .exhaustive_search(query, &compiled, k)
        .context("the exhaustive reference search")?
        .into_iter()
        .map(|n| engine.key(n.chunk))
        .collect()
}

/// Fuses two keyed result lists with the same three methods `rank::fuse` offers.
///
/// `rank::fuse` works over chunk ordinals and reads the document from a `Store`,
/// which the PostgreSQL engine does not have; its keys are `document#index`, so
/// the document is the part before the hash and the per document cap needs no
/// extra query. The arithmetic is otherwise identical, deliberately: the two
/// engines have to be fused the same way or the hybrid family stops being a
/// measurement of retrieval.
/// @param vector_hits - the vector side, score already a similarity
/// @param lexical_hits - the lexical side, score already ascending-is-better
/// @param fusion - the method, the same one inillucent is using
/// @param k - how many hits to return
/// @param per_doc_cap - most chunks kept from any one document
/// @param lexical_ceiling - kept so the signature says where the bound comes
///   from; the division itself now happens once, where each hit's confidence is
///   computed, rather than a second time here
pub fn fuse_keyed(
    vector_hits: &[Hit],
    lexical_hits: &[Hit],
    fusion: Fusion,
    k: usize,
    per_doc_cap: usize,
    _lexical_ceiling: f32,
) -> Vec<Hit> {
    fn scale(values: &[f32], how: Fusion) -> Vec<f32> {
        if values.is_empty() {
            return Vec::new();
        }
        match how {
            Fusion::Convex { .. } => {
                let max = values.iter().cloned().fold(f32::MIN, f32::max);
                // Negated so a NaN maximum returns zeros rather than dividing
                // by it, which is the rule `inillucent-core::rank` holds.
                #[allow(clippy::neg_cmp_op_on_partial_ord)]
                if !(max > f32::EPSILON) {
                    return vec![0.0; values.len()];
                }
                values.iter().map(|v| (v / max).clamp(0.0, 1.0)).collect()
            }
            _ => {
                let min = values.iter().cloned().fold(f32::MAX, f32::min);
                let max = values.iter().cloned().fold(f32::MIN, f32::max);
                let span = max - min;
                if span <= f32::EPSILON {
                    return vec![1.0; values.len()];
                }
                values.iter().map(|v| (v - min) / span).collect()
            }
        }
    }

    // Absolute confidence, on bounds the candidate list had no say in, computed
    // whatever fusion is about to order the list. The same quantity inillucent's own
    // fusion attaches, so the abstention family can ask both engines the same
    // question. A rank based fusion has no weight, so the two sides are balanced
    // evenly for this purpose rather than left undefined.
    let weight = match fusion {
        Fusion::ReciprocalRank { .. } => 0.5,
        Fusion::NormalizedScore { vector_weight }
        | Fusion::Convex { vector_weight }
        | Fusion::TheoreticalMinMax { vector_weight } => vector_weight,
    };
    let mut confidence: HashMap<String, f32> = HashMap::new();
    for h in vector_hits {
        *confidence.entry(h.key.clone()).or_insert(0.0) += weight * h.confidence.clamp(0.0, 1.0);
    }
    for h in lexical_hits {
        *confidence.entry(h.key.clone()).or_insert(0.0) +=
            (1.0 - weight) * h.confidence.clamp(0.0, 1.0);
    }

    let mut scores: HashMap<String, f32> = HashMap::new();
    match fusion {
        Fusion::ReciprocalRank { k: rrf_k } => {
            for (rank, h) in vector_hits.iter().enumerate() {
                *scores.entry(h.key.clone()).or_insert(0.0) += 1.0 / (rrf_k + rank as f32 + 1.0);
            }
            for (rank, h) in lexical_hits.iter().enumerate() {
                *scores.entry(h.key.clone()).or_insert(0.0) += 1.0 / (rrf_k + rank as f32 + 1.0);
            }
        }
        Fusion::NormalizedScore { vector_weight } | Fusion::Convex { vector_weight } => {
            let v: Vec<f32> = vector_hits.iter().map(|h| h.score).collect();
            let l: Vec<f32> = lexical_hits.iter().map(|h| h.score).collect();
            for (h, s) in vector_hits.iter().zip(scale(&v, fusion)) {
                *scores.entry(h.key.clone()).or_insert(0.0) += vector_weight * s;
            }
            for (h, s) in lexical_hits.iter().zip(scale(&l, fusion)) {
                *scores.entry(h.key.clone()).or_insert(0.0) += (1.0 - vector_weight) * s;
            }
        }
        Fusion::TheoreticalMinMax { vector_weight } => {
            // The same arithmetic inillucent does, and the same numbers the
            // confidence above is built from: each side already carries its score
            // divided by a bound the results had no say in.
            for h in vector_hits {
                *scores.entry(h.key.clone()).or_insert(0.0) +=
                    vector_weight * h.confidence.clamp(0.0, 1.0);
            }
            for h in lexical_hits {
                *scores.entry(h.key.clone()).or_insert(0.0) +=
                    (1.0 - vector_weight) * h.confidence.clamp(0.0, 1.0);
            }
        }
    }

    let mut all: Vec<Hit> = scores
        .into_iter()
        .map(|(key, score)| {
            let c = confidence.get(&key).copied().unwrap_or(0.0);
            Hit {
                key,
                score,
                confidence: c,
            }
        })
        .collect();
    all.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.key.cmp(&b.key))
    });

    let mut per_doc: HashMap<String, usize> = HashMap::new();
    let mut out = Vec::with_capacity(k);
    for hit in all {
        let doc = hit
            .key
            .split_once('#')
            .map(|(d, _)| d.to_string())
            .unwrap_or_default();
        let used = per_doc.entry(doc).or_insert(0);
        if *used >= per_doc_cap {
            continue;
        }
        *used += 1;
        out.push(hit);
        if out.len() >= k {
            break;
        }
    }
    out
}

/// Fuse with an explicit method, used by the scenario that grades the two fusion
/// methods against each other.
pub fn fuse_with(
    engine: &InillucentEngine,
    query: &str,
    query_vector: &[f32],
    filter: &Filter,
    k: usize,
    fusion: Fusion,
) -> Result<Vec<Hit>> {
    let compiled = engine.index.compile(filter);
    let candidates = engine.index.config().candidates.max(k);
    let vector_hits = engine
        .index
        .vector_search(query_vector, &compiled, candidates, engine.ef_search)
        .context("the vector half of the fusion under test")?;
    let lexical_hits = engine.index.lexical_search(query, &compiled, candidates);
    rank::fuse(
        &vector_hits,
        &lexical_hits,
        engine.index.store(),
        Some(engine.index.vectors()),
        rank::FusionParams {
            fusion,
            top_k: k,
            per_doc_cap: engine.index.config().per_doc_cap,
            bounds: rank::ScoreBounds {
                lexical_ceiling: engine.index.lexical_score_ceiling(query),
            },
            mmr_lambda: engine.index.config().mmr_lambda,
        },
    )
    .into_iter()
    .map(|h| {
        Ok(Hit {
            key: engine.key(h.chunk)?,
            score: h.score,
            confidence: h.confidence,
        })
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact statement list for a filtered query. This is the test the weak
    /// baseline shipped without: `hnsw.scan_mem_multiplier` was never set at all,
    /// so the iterative scan ran at pgvector's default of 1, exhausted its memory
    /// budget and stopped early, and nothing failed.
    #[test]
    fn a_filtered_query_sets_all_four_iterative_scan_settings() {
        let settings =
            pg_session_settings(PgMode::WellConfigured, &Filter::source("confluence"), 50);
        assert_eq!(
            settings,
            vec![
                "SET hnsw.iterative_scan = relaxed_order",
                "SET hnsw.ef_search = 400",
                "SET hnsw.max_scan_tuples = 40000",
                "SET hnsw.scan_mem_multiplier = 4",
            ]
        );
    }

    /// Named on its own, because dropping this one setting is the regression that
    /// produced the baseline this replaced and it is invisible in a latency number.
    #[test]
    fn scan_mem_multiplier_is_set_on_every_query_that_scans_iteratively() {
        for filter in filters_with_a_predicate() {
            let settings = pg_session_settings(PgMode::WellConfigured, &filter, 50);
            assert!(
                settings.contains(&"SET hnsw.scan_mem_multiplier = 4".to_string()),
                "hnsw.scan_mem_multiplier missing for {filter:?}: {settings:?}"
            );
        }
    }

    /// The exact statement list for a query with no filter beyond
    /// `deleted_at IS NULL`. The iterative scan is off, and the two settings that
    /// apply only to an iterative scan are reset rather than left over from an
    /// earlier filtered query on the same connection.
    #[test]
    fn an_unfiltered_query_turns_the_iterative_scan_off() {
        let settings = pg_session_settings(PgMode::WellConfigured, &Filter::default(), 50);
        assert_eq!(
            settings,
            vec![
                "SET hnsw.iterative_scan = off",
                "SET hnsw.ef_search = 100",
                "RESET hnsw.max_scan_tuples",
                "RESET hnsw.scan_mem_multiplier",
            ]
        );
    }

    /// The defaults column is the extension's defaults and nothing else, in both
    /// cases, because it exists to say what pgvector does before anybody
    /// configures it.
    #[test]
    fn the_extension_defaults_are_unchanged_by_the_filter() {
        let expected = vec![
            "SET hnsw.iterative_scan = off",
            "SET hnsw.ef_search = 40",
            "RESET hnsw.max_scan_tuples",
            "RESET hnsw.scan_mem_multiplier",
        ];
        assert_eq!(
            pg_session_settings(PgMode::Default, &Filter::default(), 50),
            expected
        );
        assert_eq!(
            pg_session_settings(PgMode::Default, &Filter::source("confluence"), 50),
            expected
        );
    }

    /// A scan cannot return more rows than it collected, so a caller asking for
    /// more rows than `hnsw.ef_search` would otherwise collect raises it.
    #[test]
    fn ef_search_is_raised_to_the_requested_row_count() {
        let filtered = pg_session_settings(PgMode::WellConfigured, &Filter::source("jira"), 1000);
        assert!(
            filtered.contains(&"SET hnsw.ef_search = 1000".to_string()),
            "{filtered:?}"
        );
        let unfiltered = pg_session_settings(PgMode::WellConfigured, &Filter::default(), 250);
        assert!(
            unfiltered.contains(&"SET hnsw.ef_search = 250".to_string()),
            "{unfiltered:?}"
        );
        // Below the floor it stays at the floor rather than following k down.
        let small = pg_session_settings(PgMode::WellConfigured, &Filter::default(), 10);
        assert!(
            small.contains(&"SET hnsw.ef_search = 100".to_string()),
            "{small:?}"
        );
    }

    /// Every field `where_clause` turns into a clause has to count as a filter.
    /// A field added to `Filter` and not to `where_clause`, or the other way
    /// round, would otherwise send a filtered query down the unfiltered path.
    #[test]
    fn every_filter_field_selects_the_filtered_settings() {
        for filter in filters_with_a_predicate() {
            let settings = pg_session_settings(PgMode::WellConfigured, &filter, 50);
            assert_eq!(
                settings[0], "SET hnsw.iterative_scan = relaxed_order",
                "{filter:?} was treated as unfiltered: {settings:?}"
            );
            let (where_sql, _) = PgVectorEngine::where_clause(&filter, 2);
            assert_ne!(
                where_sql, DELETED_ONLY,
                "{filter:?} produced no clause beyond deleted_at IS NULL"
            );
        }
    }

    /// `include_deleted` is not a predicate the harness emits: `where_clause`
    /// writes `d.deleted_at IS NULL` whatever it is set to, so it must not flip a
    /// query onto the filtered path.
    #[test]
    fn include_deleted_alone_is_not_a_filter() {
        let filter = Filter {
            include_deleted: true,
            ..Default::default()
        };
        assert_eq!(
            pg_session_settings(PgMode::WellConfigured, &filter, 50),
            pg_session_settings(PgMode::WellConfigured, &Filter::default(), 50)
        );
    }

    /// An empty `sources` list means no source filter, in `where_clause` and in
    /// pgvector's plan, so it must not turn the iterative scan on for nothing.
    #[test]
    fn an_empty_sources_list_is_not_a_filter() {
        let filter = Filter {
            sources: Some(Vec::new()),
            ..Default::default()
        };
        let (where_sql, _) = PgVectorEngine::where_clause(&filter, 2);
        assert_eq!(where_sql, DELETED_ONLY);
        assert_eq!(
            pg_session_settings(PgMode::WellConfigured, &filter, 50)[0],
            "SET hnsw.iterative_scan = off"
        );
    }

    /// The score card column headings. They are strings in generated output and
    /// in `report.rs`, so a rename that missed one would leave the card claiming
    /// the baseline is something it is not.
    #[test]
    fn the_labels_say_which_configuration_each_column_is() {
        assert_eq!(PgMode::Default.label(), "pgvector (extension defaults)");
        assert_eq!(
            PgMode::WellConfigured.label(),
            "pgvector (correctly configured)"
        );
    }

    /// One filter per field `where_clause` reads, each carrying exactly one
    /// predicate.
    fn filters_with_a_predicate() -> Vec<Filter> {
        vec![
            Filter::source("confluence"),
            Filter {
                sources: Some(vec!["slack".into(), "jira".into()]),
                ..Default::default()
            },
            Filter {
                space_key: Some("ENG".into()),
                ..Default::default()
            },
            Filter {
                author: Some("someone".into()),
                ..Default::default()
            },
            Filter {
                authors: Some(vec![("slack".into(), "U123".into())]),
                ..Default::default()
            },
            Filter {
                updated_after: Some(1_700_000_000),
                ..Default::default()
            },
            Filter {
                labels: Some(vec!["runbook".into()]),
                ..Default::default()
            },
        ]
    }
}
