//! Proving the destination holds what the source held, and answers what the
//! source answered.
//!
//! Invariant: every check here compares the destination against the *source*,
//! never against another copy of the destination. A migration that verified
//! itself would pass however wrong it was.
//!
//! There are two kinds of check and they are not interchangeable. The content
//! checks compare values and ordered digests: every document field, every
//! chunk, every label, every attribute, every flag, every tombstone. Those must
//! be exact, and if one fails nothing else matters. The retrieval checks ask
//! the two indexes the same questions and compare the rankings *and the
//! scores*, which is possible only because the migrated search table is built
//! by the same `inillucent-core` code from the same text: one column holding the
//! chunk verbatim, so the terms, the document lengths and the corpus statistics
//! are the ones the source index had.
//!
//! The retrieval comparison passes the source an `include_deleted` filter, and
//! that is not a loophole - it is the like-for-like. The migrated search table
//! holds every chunk, tombstoned or not, precisely so that its corpus
//! statistics match the source's, whose inverted index also holds them and
//! filters at query time.
//!
//! **What a migrated database does to reproduce the legacy default is constrain
//! the search table's `live` facet, and this module used to say it was a join
//! to `document`** (task-2067). A join cannot do it and the difference is not
//! small. A `WHERE` clause outside the search runs after the ranking, and
//! `Bm25Index::top_k` rescores the best `k * rescore_depth_factor` hits by
//! where the query's terms sit inside them - a rescore that only ever lowers a
//! score, with everything below the window keeping its full score and competing
//! against rescored ones. Which hits are in that window depends on which chunks
//! the scan admitted, so removing rows afterwards produces a different order:
//! measured on a 400 document corpus, one hit of ten survived, and with the
//! position rescoring switched off the two became identical, hit for hit.
//!
//! So the copy carries the flag where the ranking can see it before it ranks.
//! `document.deleted` is still written and still compared, because it is the
//! copy of the source's own row; the facet is the same fact where a query can
//! use it. `filter.deleted` compares the two orders and `filter.unreachable`
//! asks the stricter question underneath, that no tombstoned chunk comes back
//! at all.

use inillucent_base::hash::Sha256;
use inillucent_core::filter::Filter;
use inillucent_core::index::{Branches, Index};
use inillucent_core::store::Store;
use inillucent_engine::connect::Connection;
use inillucent_search::adapter::{Query, RetrievalIndex};
use inillucent_tree::datum::OwnedDatum;

use crate::copy::{self, SEARCH_TABLE};
use crate::index::SqlIndex;

/// How many hits a probe asks for.
const PROBE_K: usize = 10;

/// How many probes are drawn from the corpus.
const PROBES: usize = 12;

/// How many words of a chunk make a query.
const PROBE_WORDS: usize = 6;

/// One check and what it found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    /// The check's name, as the manifest records it.
    pub name: String,
    /// Whether it passed.
    pub passed: bool,
    /// What it found, in words.
    pub detail: String,
}

impl Check {
    /// Returns a passing check.
    ///
    /// @param name - the check's name, as the manifest records it
    /// @param detail - what it found
    pub fn passed(name: &str, detail: impl Into<String>) -> Check {
        Check::pass(name, detail)
    }

    /// Returns a failing check.
    ///
    /// @param name - the check's name, as the manifest records it
    /// @param detail - what it found
    pub fn failed(name: &str, detail: impl Into<String>) -> Check {
        Check::fail(name, detail)
    }

    /// Returns a passing check.
    fn pass(name: &str, detail: impl Into<String>) -> Check {
        Check {
            name: name.to_string(),
            passed: true,
            detail: detail.into(),
        }
    }

    /// Returns a failing check.
    fn fail(name: &str, detail: impl Into<String>) -> Check {
        Check {
            name: name.to_string(),
            passed: false,
            detail: detail.into(),
        }
    }

    /// Returns the line the manifest records.
    pub fn line(&self) -> String {
        format!(
            "{} {} {}",
            self.name,
            if self.passed { "ok" } else { "fail" },
            self.detail
        )
    }
}

/// Runs every check and returns what each one found.
///
/// It does not stop at the first failure. A migration that is wrong is worth
/// describing completely: knowing that the counts are right and the ranking is
/// wrong is a different problem from knowing that nothing arrived.
/// @param source - the legacy index
/// @param sql - the destination's search table, as a retrieval index
pub fn run(source: &Index, sql: &mut SqlIndex) -> Vec<Check> {
    let store = source.store();
    let mut checks = Vec::new();
    checks.extend(counts(&sql.connection(), store));
    checks.push(chunk_digest(&sql.connection(), store));
    checks.push(document_digest(&sql.connection(), store));
    checks.push(tombstones(&sql.connection(), store));
    checks.push(dictionaries(&sql.connection(), store));
    checks.extend(retrieval(source, sql));
    checks
}

/// Compares the row counts of every copied table.
fn counts(connection: &Connection<'_>, store: &Store) -> Vec<Check> {
    let mut labels = 0u64;
    let mut attributes = 0u64;
    let mut flags = 0u64;
    for ordinal in 0..store.n_documents() {
        labels = labels.saturating_add(store.labels_of(ordinal as u32).len() as u64);
        attributes = attributes.saturating_add(store.attributes_of(ordinal as u32).len() as u64);
        if let Some(document) = store.documents.get(ordinal) {
            flags = flags.saturating_add(u64::from(document.flags.count_ones()));
        }
    }
    let expected: [(&str, &str, u64); 5] = [
        ("counts.document", "document", store.n_documents() as u64),
        ("counts.chunk", "chunk", store.n_chunks() as u64),
        ("counts.label", "document_label", labels),
        ("counts.attribute", "document_attribute", attributes),
        ("counts.flag", "document_flag", flags),
    ];
    let mut checks = Vec::new();
    for (name, table, wanted) in expected {
        match scalar(connection, &format!("SELECT count(*) FROM {table}")) {
            Ok(found) if found == wanted as i64 => {
                checks.push(Check::pass(name, format!("{wanted} rows")));
            }
            Ok(found) => checks.push(Check::fail(
                name,
                format!("the source has {wanted} and the destination has {found}"),
            )),
            Err(failure) => checks.push(Check::fail(name, failure)),
        }
    }
    match scalar(
        connection,
        &format!("SELECT count(*) FROM {SEARCH_TABLE}_content"),
    ) {
        Ok(found) if found == store.n_chunks() as i64 => checks.push(Check::pass(
            "counts.search",
            format!("{found} indexed rows"),
        )),
        Ok(found) => checks.push(Check::fail(
            "counts.search",
            format!(
                "the source has {} chunks and the index holds {found}",
                store.n_chunks()
            ),
        )),
        Err(failure) => checks.push(Check::fail("counts.search", failure)),
    }
    checks
}

/// Compares an ordered digest of every chunk.
fn chunk_digest(connection: &Connection<'_>, store: &Store) -> Check {
    let mut hasher = Sha256::new();
    for ordinal in 0..store.n_chunks() {
        let Some(chunk) = store.chunks.get(ordinal) else {
            continue;
        };
        integer(&mut hasher, ordinal as i64);
        integer(&mut hasher, i64::from(chunk.doc));
        integer(&mut hasher, i64::from(chunk.chunk_index));
        text(&mut hasher, store.chunk_external_id(ordinal as u32));
        text(&mut hasher, &store.heading_path(ordinal as u32).join(" > "));
        text(&mut hasher, store.content(ordinal as u32).as_ref());
        hasher.update(b"\x1e");
    }
    let wanted = hasher.hex();
    match copy::digest(
        connection,
        "SELECT id, document, chunk_index, external_id, heading, content FROM chunk ORDER BY id",
    ) {
        Ok((rows, found)) if found == wanted => {
            Check::pass("digest.chunk", format!("{rows} chunks, {found}"))
        }
        Ok((rows, found)) => Check::fail(
            "digest.chunk",
            format!("{rows} chunks digest {found}, the source digests {wanted}"),
        ),
        Err(failure) => Check::fail("digest.chunk", failure),
    }
}

/// Compares an ordered digest of every document.
fn document_digest(connection: &Connection<'_>, store: &Store) -> Check {
    let mut hasher = Sha256::new();
    for ordinal in 0..store.n_documents() {
        let Some(document) = store.documents.get(ordinal) else {
            continue;
        };
        integer(&mut hasher, ordinal as i64);
        text(
            &mut hasher,
            store.sources.value(document.source).unwrap_or(""),
        );
        text(&mut hasher, &document.external_id);
        text(&mut hasher, &document.title);
        text(&mut hasher, &document.url);
        optional(
            &mut hasher,
            document.space_key.and_then(|key| store.spaces.value(key)),
        );
        optional(
            &mut hasher,
            document.author.and_then(|key| store.authors.value(key)),
        );
        optional(
            &mut hasher,
            document
                .author_id
                .and_then(|key| store.author_ids.value(key)),
        );
        integer(&mut hasher, document.updated_at);
        integer(&mut hasher, i64::from(document.deleted));
        integer(&mut hasher, i64::from(document.chunk_count));
        hasher.update(b"\x1e");
    }
    let wanted = hasher.hex();
    match copy::digest(
        connection,
        "SELECT id, source, external_id, title, url, space_key, author, author_id, updated_at, \
         deleted, chunk_count FROM document ORDER BY id",
    ) {
        Ok((rows, found)) if found == wanted => {
            Check::pass("digest.document", format!("{rows} documents, {found}"))
        }
        Ok((rows, found)) => Check::fail(
            "digest.document",
            format!("{rows} documents digest {found}, the source digests {wanted}"),
        ),
        Err(failure) => Check::fail("digest.document", failure),
    }
}

/// Compares the set of tombstoned documents.
fn tombstones(connection: &Connection<'_>, store: &Store) -> Check {
    let wanted: Vec<i64> = (0..store.n_documents())
        .filter(|ordinal| {
            store
                .documents
                .get(*ordinal)
                .is_some_and(|document| document.deleted)
        })
        .map(|ordinal| ordinal as i64)
        .collect();
    match integers(
        connection,
        "SELECT id FROM document WHERE deleted = 1 ORDER BY id",
    ) {
        Ok(found) if found == wanted => Check::pass(
            "tombstone",
            format!("{} tombstoned documents preserved", wanted.len()),
        ),
        Ok(found) => Check::fail(
            "tombstone",
            format!(
                "the source has {} tombstones and the destination has {}",
                wanted.len(),
                found.len()
            ),
        ),
        Err(failure) => Check::fail("tombstone", failure),
    }
}

/// Compares the values every dictionary interned, as they come back as text.
fn dictionaries(connection: &Connection<'_>, store: &Store) -> Check {
    let mut problems = Vec::new();
    let mut referenced_sources: Vec<String> = Vec::new();
    for ordinal in 0..store.n_documents() {
        if let Some(document) = store.documents.get(ordinal) {
            if let Some(name) = store.sources.value(document.source) {
                if !referenced_sources.iter().any(|held| held == name) {
                    referenced_sources.push(name.to_string());
                }
            }
        }
    }
    referenced_sources.sort();
    match strings(
        connection,
        "SELECT DISTINCT source FROM document ORDER BY source",
    ) {
        Ok(found) if found == referenced_sources => {}
        Ok(found) => problems.push(format!(
            "sources: {referenced_sources:?} in the source, {found:?} in the destination"
        )),
        Err(failure) => problems.push(failure),
    }
    let mut referenced_labels: Vec<String> = Vec::new();
    for ordinal in 0..store.n_documents() {
        for label in store.labels_of(ordinal as u32) {
            if let Some(name) = store.labels.value(*label) {
                if !referenced_labels.iter().any(|held| held == name) {
                    referenced_labels.push(name.to_string());
                }
            }
        }
    }
    referenced_labels.sort();
    match strings(
        connection,
        "SELECT DISTINCT label FROM document_label ORDER BY label",
    ) {
        Ok(found) if found == referenced_labels => {}
        Ok(found) => problems.push(format!(
            "labels: {} in the source, {} in the destination",
            referenced_labels.len(),
            found.len()
        )),
        Err(failure) => problems.push(failure),
    }
    if problems.is_empty() {
        Check::pass(
            "dictionary",
            format!(
                "{} sources and {} labels resolve to the same text",
                referenced_sources.len(),
                referenced_labels.len()
            ),
        )
    } else {
        Check::fail("dictionary", problems.join("; "))
    }
}

/// Asks both indexes the same questions and compares the answers.
///
/// One difference between the two is real and has to be reproduced rather than
/// waved away. The legacy engine caps how many chunks of *one document* a
/// result may hold, and it can do that because its store knows which document a
/// chunk belongs to. A `inillucent_search` table has one row per document by
/// construction, so its own cap never binds - and a migrated database expresses
/// the grouping the way a database should, by joining to the `document` table
/// the migration wrote.
///
/// So the lexical family is checked twice over. `bm25.raw` compares the
/// ungrouped rankings, which must be identical because they are the same BM25
/// over the same corpus. `bm25.grouped` compares the legacy answer against the
/// destination's ranking with the cap applied through the relational mapping,
/// which is the claim an application actually depends on: the same question
/// gets the same answer.
/// Returns the chunks the legacy index answers one probe with.
///
/// **One place that reports a refusal, rather than six.** The probes are built
/// from the legacy index's own vectors, so a width mismatch would be a defect in
/// this file rather than a corpus this tool was handed - but a migration tool
/// that panics on a query it could not run tells an operator nothing about a
/// corpus it has half converted, and this crate denies `expect` for that reason.
/// The refusal travels out as a failed check (task-1946, H4).
///
/// @param source - the legacy index
/// @param text - the query text, empty for a vector-only probe
/// @param vector - the query vector, empty for a lexical-only probe
/// @param filter - which chunks the probe may see
/// @param width - the traversal width, or the index's configured default
/// @param branches - which branches to run
fn probe(
    source: &Index,
    text: &str,
    vector: &[f32],
    filter: &inillucent_core::filter::CompiledFilter,
    width: Option<usize>,
    branches: Branches,
) -> Result<Vec<inillucent_core::rank::FusedHit>, String> {
    source
        .search_branches(text, vector, filter, PROBE_K, width, branches)
        .map(|(hits, _)| hits)
        .map_err(|error| error.to_string())
}

/// The same probe with the approximation switched off, which is how both sides
/// are asked whenever their two graphs would otherwise be compared.
///
/// @param source - the legacy index
/// @param text - the query text, empty for a vector-only probe
/// @param vector - the query vector
/// @param filter - which chunks the probe may see
/// @param branches - which branches to run
fn exact_probe(
    source: &Index,
    text: &str,
    vector: &[f32],
    filter: &inillucent_core::filter::CompiledFilter,
    branches: Branches,
) -> Result<Vec<inillucent_core::rank::FusedHit>, String> {
    probe(source, text, vector, filter, Some(usize::MAX), branches)
}

fn retrieval(source: &Index, sql: &mut SqlIndex) -> Vec<Check> {
    match retrieval_checks(source, sql) {
        Ok(checks) => checks,
        // A probe the legacy index refused. It stops the comparison rather than
        // being skipped, because every check below it reads the answer it would
        // have produced.
        Err(reason) => vec![Check::fail("retrieval", reason)],
    }
}

/// The retrieval checks, or the refusal that stopped them.
///
/// @param source - the legacy index
/// @param sql - the migrated index
fn retrieval_checks(source: &Index, sql: &mut SqlIndex) -> Result<Vec<Check>, String> {
    let store = source.store();
    if store.n_chunks() == 0 {
        return Ok(vec![Check::pass("retrieval", "the corpus is empty")]);
    }
    let everything = source.compile(&Filter {
        include_deleted: true,
        ..Filter::default()
    });
    let live = source.compile(&Filter::default());
    let probes = text_probes(store);
    let cap = source.config().per_doc_cap;
    // The legacy engine fuses over this many candidates before it caps and
    // truncates, so the destination has to be asked for the same depth or the
    // two would be capping different lists.
    let wide = source.config().candidates.max(PROBE_K);
    let documents = match chunk_documents(&sql.connection()) {
        Ok(map) => map,
        Err(failure) => return Ok(vec![Check::fail("retrieval", failure)]),
    };
    let deleted = match integers(
        &sql.connection(),
        "SELECT id FROM chunk WHERE document IN (SELECT id FROM document WHERE deleted = 1) \
         ORDER BY id",
    ) {
        Ok(rows) => rows,
        Err(failure) => return Ok(vec![Check::fail("retrieval", failure)]),
    };
    let mut checks = Vec::new();

    // The ungrouped ranking, which is BM25 and nothing else.
    //
    // Asked at `wide` and then truncated, rather than asked for ten. The legacy
    // engine's lexical ranking is a function of the `k` it was given, not just
    // of the corpus: position-aware rescoring reaches `k * lexical_rescore_depth`
    // hits and only ever scales a score *down*, so a chunk just outside that
    // window keeps its full BM25 score and competes against rescored ones. Move
    // the window and the tail of the ranking moves with it.
    //
    // Every path into the engine that a search table can be on the other side of
    // goes through `search_branches`, which retrieves at `candidates` depth. So
    // asking the source for ten and the destination for ten would compare a
    // window of sixty against a window of three hundred, and report the engine's
    // own depth setting as a migration defect. On the small corpus next door the
    // two windows both cover the whole corpus and the difference cannot appear,
    // which is exactly why it took a corpus of real prose to find.
    let mut raw = Vec::new();
    for query in &probes {
        let expected: Vec<i64> = source
            .lexical_search(query, &everything, wide)
            .iter()
            .take(PROBE_K)
            .map(|hit| i64::from(hit.chunk))
            .collect();
        match ranked(sql, query, &[], PROBE_K) {
            Ok(found) if found == expected => {}
            Ok(found) => raw.push(format!("{query:?}: {expected:?} became {found:?}")),
            Err(failure) => raw.push(format!("{query:?}: {failure}")),
        }
    }
    checks.push(if raw.is_empty() {
        Check::pass(
            "bm25.raw",
            format!("{} probes rank identically before grouping", probes.len()),
        )
    } else {
        Check::fail("bm25.raw", raw.join(" | "))
    });

    // The grouped answer, which is what an application asked the legacy engine
    // for and what it must be able to ask a migrated database for.
    let mut grouped = Vec::new();
    for query in &probes {
        let wanted = probe(source, query, &[], &everything, None, Branches::Lexical)?;
        let expected: Vec<i64> = wanted.iter().map(|hit| i64::from(hit.chunk)).collect();
        match ranked(sql, query, &[], wide) {
            Ok(found) => {
                let capped = apply_cap(&found, &documents, cap, PROBE_K);
                if capped != expected {
                    grouped.push(format!("{query:?}: {expected:?} became {capped:?}"));
                }
            }
            Err(failure) => grouped.push(format!("{query:?}: {failure}")),
        }
    }
    checks.push(if grouped.is_empty() {
        Check::pass(
            "bm25.grouped",
            format!(
                "{} probes answer identically once the per-document cap of {cap} is applied \
                 through the document table",
                probes.len()
            ),
        )
    } else {
        Check::fail("bm25.grouped", grouped.join(" | "))
    });

    checks.push(scores_match(source, sql, &probes, &everything, wide)?);

    checks.extend(vector_checks(
        source,
        sql,
        &probes,
        &everything,
        &documents,
        cap,
        wide,
    )?);

    checks.push(live_filter(
        source, sql, &probes, &live, &documents, &deleted, cap, wide,
    )?);
    checks.push(live_reachable(sql, &probes, &deleted, wide));
    Ok(checks)
}

/// The checks that need a vector branch, or none when the source has none.
///
/// Split out of [`retrieval_checks`] in task-2067, which added a check and
/// took it past the length it is recorded at. It is the whole of one step:
/// every comparison that reads a vector, and nothing that does not. The
/// guard travels with it rather than staying at the call site, because "the
/// source has no vectors" and "the source has fewer vectors than chunks" are
/// both answers to the question this function asks.
///
/// @param source - the legacy index
/// @param sql - the migrated index
/// @param probes - the queries both sides are asked
/// @param everything - the filter that admits tombstoned chunks too
/// @param documents - which document each chunk belongs to, read from the copy
/// @param cap - the legacy per-document cap
/// @param wide - the candidate depth both sides are asked at
fn vector_checks(
    source: &Index,
    sql: &mut SqlIndex,
    probes: &[String],
    everything: &inillucent_core::filter::CompiledFilter,
    documents: &[(i64, i64)],
    cap: usize,
    wide: usize,
) -> Result<Vec<Check>, String> {
    let store = source.store();
    if source.config().dims == 0 || source.vectors().len() != store.n_chunks() {
        return Ok(Vec::new());
    }
    let mut checks = Vec::new();
    // Both sides are asked with their approximation switched off, so what
    // is compared is the data rather than the luck of two graphs.
    //
    // The source's graph grew one insert at a time and the destination's
    // was built in one pass over every row, which is better connected -
    // that is the reason compaction is worth its cost. Two different graphs
    // searched approximately give two slightly different answers, sometimes
    // the destination's better and sometimes the source's, and a check that
    // demanded they match would be demanding the migration reproduce the
    // source's misses. So the graphs are traversed exhaustively here and
    // the answers must be identical.
    //
    // What the approximation is actually worth is measured separately and
    // reported rather than gated: `vector.recall` says how much of the
    // exact answer each index finds at its default width, and the migration
    // fails only if the destination finds less of it than the source did.
    let mut wrong = Vec::new();
    let mut theirs_found = 0.0f64;
    let mut ours_found = 0.0f64;
    let mut probed = 0usize;
    for ordinal in sample(store.n_chunks(), PROBES) {
        let query = source.vectors().copy_of(ordinal as u32);
        let wanted = exact_probe(source, "", &query, everything, Branches::Vector)?;
        let expected: Vec<i64> = wanted.iter().map(|hit| i64::from(hit.chunk)).collect();
        match exhaustive(sql, "", &query, wide) {
            Ok(found) => {
                let capped = apply_cap(&found, documents, cap, PROBE_K);
                if capped != expected {
                    wrong.push(format!("chunk {ordinal}: {expected:?} became {capped:?}"));
                }
            }
            Err(failure) => wrong.push(format!("chunk {ordinal}: {failure}")),
        }

        // The same probe again, at the width each index uses by default,
        // scored against the answer brute force says is right.
        let truth = exact_neighbours(source, &query, everything, PROBE_K);
        let approximate = probe(source, "", &query, everything, None, Branches::Vector)?;
        let theirs: Vec<i64> = approximate.iter().map(|hit| i64::from(hit.chunk)).collect();
        theirs_found += recall(&theirs, &truth);
        if let Ok(found) = ranked(sql, "", &query, wide) {
            let capped = apply_cap(&found, documents, cap, PROBE_K);
            ours_found += recall(&capped, &truth);
        }
        probed = probed.saturating_add(1);
    }
    checks.push(if wrong.is_empty() {
        Check::pass(
            "vector.exact",
            format!("{PROBES} probes rank identically when both graphs are traversed in full"),
        )
    } else {
        Check::fail("vector.exact", wrong.join(" | "))
    });
    let divisor = probed.max(1) as f64;
    let (theirs, ours) = (theirs_found / divisor, ours_found / divisor);
    checks.push(if ours + 1.0e-6 >= theirs {
        Check::pass(
            "vector.recall",
            format!(
                "at the default width the source finds {theirs:.3} of the exact answer and \
                 the copy finds {ours:.3}"
            ),
        )
    } else {
        Check::fail(
            "vector.recall",
            format!("recall fell from {theirs:.3} to {ours:.3} at the default width"),
        )
    });

    // The same rule, for the same reason: a fused ranking inherits the
    // vector branch's approximation, so both sides are asked with it off.
    let mut hybrid_wrong = Vec::new();
    for (position, ordinal) in sample(store.n_chunks(), probes.len())
        .into_iter()
        .enumerate()
    {
        let Some(query) = probes.get(position) else {
            continue;
        };
        let vector = source.vectors().copy_of(ordinal as u32);
        let wanted = exact_probe(source, query, &vector, everything, Branches::Both)?;
        let expected: Vec<i64> = wanted.iter().map(|hit| i64::from(hit.chunk)).collect();
        match exhaustive(sql, query, &vector, wide) {
            Ok(found) => {
                let capped = apply_cap(&found, documents, cap, PROBE_K);
                if capped != expected {
                    hybrid_wrong.push(format!("{query:?}: {expected:?} became {capped:?}"));
                }
            }
            Err(failure) => hybrid_wrong.push(format!("{query:?}: {failure}")),
        }
    }
    checks.push(if hybrid_wrong.is_empty() {
        Check::pass(
            "hybrid.exact",
            format!(
                "{} fused rankings agree when both graphs are traversed in full",
                probes.len()
            ),
        )
    } else {
        Check::fail("hybrid.exact", hybrid_wrong.join(" | "))
    });
    Ok(checks)
}

/// Compares the scores, not only the order.
///
/// The order is what an application sees; the score is what says the two
/// indexes computed the same thing rather than happening to agree. A migrated
/// index whose document frequencies were subtly different would usually rank
/// the same and score differently, and that is the failure this catches.
///
/// What is compared is the *fused* score on both sides, and the two are
/// directly comparable rather than only proportionally so. Both engines fuse
/// over `candidates` hits, both draw the same candidates from the same corpus,
/// and min-max normalisation is decided by that list - so a chunk that appears
/// in both lists must carry the same number. Comparing raw BM25 against a fused
/// score would not work and is not what this does: min-max is affine, so it
/// does not even preserve ratios.
fn scores_match(
    source: &Index,
    sql: &mut SqlIndex,
    probes: &[String],
    filter: &inillucent_core::filter::CompiledFilter,
    wide: usize,
) -> Result<Check, String> {
    let mut wrong = Vec::new();
    let mut compared = 0usize;
    for query in probes {
        let wanted = probe(source, query, &[], filter, None, Branches::Lexical)?;
        let found = match ranked_hits(sql, query, &[], wide) {
            Ok(hits) => hits,
            Err(failure) => {
                wrong.push(format!("{query:?}: {failure}"));
                continue;
            }
        };
        for hit in &wanted {
            let chunk = i64::from(hit.chunk);
            let Some((_, score)) = found.iter().find(|(id, _)| *id == chunk) else {
                wrong.push(format!("{query:?}: chunk {chunk} is missing downstream"));
                break;
            };
            if (hit.score - score).abs() > 1.0e-5 {
                wrong.push(format!(
                    "{query:?}: chunk {chunk} scored {} and {score}",
                    hit.score
                ));
                break;
            }
            compared = compared.saturating_add(1);
        }
    }
    if wrong.is_empty() {
        Ok(Check::pass(
            "bm25.scores",
            format!("{compared} hits carry the same fused score"),
        ))
    } else {
        Ok(Check::fail("bm25.scores", wrong.join(" | ")))
    }
}

/// Checks that the copy's liveness facet reproduces the legacy default filter.
///
/// The legacy engine excludes a tombstoned document's chunks at query time,
/// with the chunks still in the inverted index and still in its corpus
/// statistics. The copy reproduces that by carrying the flag on the search
/// table's own `live` facet column, which the module compiles into the filter
/// the scan runs under - so the two exclude the same rows at the same point in
/// the same pipeline, and this is the check that they agree.
///
/// **It used to ask the two sides different questions, and it was red whenever
/// the corpus was large enough to show it** (task-2067). The legacy side was
/// asked with the filter applied during the search and the copy was asked with
/// no filter at all, its tombstoned rows dropped from the answer afterwards and
/// a deeper draw taken to make up the shortfall. Those are three different
/// questions, and all three differences run through one mechanism:
/// `Bm25Index::top_k` rescores the best `k * rescore_depth_factor` hits by
/// where the query's terms sit inside them, the rescore only ever lowers a
/// score, and a hit outside that window keeps its full score and competes
/// against rescored ones. Which hits are in the window depends on which chunks
/// the scan admitted and on the `k` it was given, so dropping rows afterwards
/// and drawing deeper each move the window.
///
/// Measured on a 400 document corpus: the two orders shared one hit of ten, and
/// with the position rescoring switched off they became identical, hit for hit. That is
/// what says the mechanism is this one and not three.
///
/// So the copy is now asked the same question the legacy side is asked, at the
/// same depth, and the answers have to be equal as they come back, rather than
/// equal once the check has removed rows and drawn deeper to make up what
/// removing them cost.
/// `apply_cap` is still applied because the per-document cap is the legacy
/// engine's own grouping and a search table gives every row its own document,
/// which is the same pairing `bm25.grouped` uses and proves.
///
/// @param source - the legacy index
/// @param sql - the migrated index
/// @param probes - the queries both sides are asked
/// @param live - the legacy default filter, which excludes tombstoned chunks
/// @param documents - which document each chunk belongs to, read from the copy
/// @param deleted - the tombstoned chunks, for the detail line
/// @param cap - the legacy per-document cap
/// @param wide - the candidate depth both sides are asked at
fn live_filter(
    source: &Index,
    sql: &mut SqlIndex,
    probes: &[String],
    live: &inillucent_core::filter::CompiledFilter,
    documents: &[(i64, i64)],
    deleted: &[i64],
    cap: usize,
    wide: usize,
) -> Result<Check, String> {
    if deleted.is_empty() {
        return Ok(Check::pass(
            "filter.deleted",
            "the corpus holds no tombstoned chunk",
        ));
    }
    let mut wrong = Vec::new();
    for query in probes {
        let wanted = probe(source, query, &[], live, None, Branches::Lexical)?;
        let expected: Vec<i64> = wanted.iter().map(|hit| i64::from(hit.chunk)).collect();
        match live_ranked(sql, query, wide) {
            Ok(found) => {
                let capped = apply_cap(&found, documents, cap, PROBE_K);
                if capped != expected {
                    wrong.push(format!("{query:?}: {expected:?} became {capped:?}"));
                }
            }
            Err(failure) => wrong.push(format!("{query:?}: {failure}")),
        }
    }
    if wrong.is_empty() {
        Ok(Check::pass(
            "filter.deleted",
            format!(
                "{} tombstoned chunks excluded identically, inside the search on both sides",
                deleted.len()
            ),
        ))
    } else {
        Ok(Check::fail("filter.deleted", wrong.join(" | ")))
    }
}

/// Checks that a tombstoned chunk cannot be reached through the copy at all.
///
/// `filter.deleted` compares two orderings, and an ordering can only be
/// compared where the corpus produces one. This asks the cheaper and stricter
/// question underneath it: over every probe, does any chunk of a tombstoned
/// document come back from a search the copy ran with its liveness facet
/// constrained. One row that does is a deletion a search still returns, which
/// is the failure the whole facet exists to prevent, and it is worth its own
/// line in the manifest because a caller reading the report wants to know that
/// before it wants to know about an order.
///
/// @param sql - the migrated index
/// @param probes - the queries to run
/// @param deleted - the tombstoned chunks
/// @param wide - the candidate depth
fn live_reachable(sql: &mut SqlIndex, probes: &[String], deleted: &[i64], wide: usize) -> Check {
    if deleted.is_empty() {
        return Check::pass("filter.unreachable", "the corpus holds no tombstoned chunk");
    }
    let mut wrong = Vec::new();
    let mut looked = 0usize;
    for query in probes {
        match live_ranked(sql, query, wide) {
            Ok(found) => {
                looked = looked.saturating_add(found.len());
                for chunk in found.iter().filter(|id| deleted.contains(id)) {
                    wrong.push(format!("{query:?} answered tombstoned chunk {chunk}"));
                }
            }
            Err(failure) => wrong.push(format!("{query:?}: {failure}")),
        }
    }
    match wrong.is_empty() {
        true => Check::pass(
            "filter.unreachable",
            format!(
                "{looked} hits across {} probes, none tombstoned",
                probes.len()
            ),
        ),
        false => Check::fail("filter.unreachable", wrong.join(" | ")),
    }
}

/// Returns the copy's ranking over its live rows only, as rowids in order.
///
/// The liveness is a constraint the search runs under rather than a filter on
/// what it answered - see [`live_filter`] for the measurement that says the two
/// are different questions.
/// @param sql - the migrated index
/// @param text - the query text
/// @param limit - how many hits to ask for
fn live_ranked(sql: &mut SqlIndex, text: &str, limit: usize) -> Result<Vec<i64>, String> {
    let hits = sql
        .search(&Query {
            text: text.to_string(),
            vector: Vec::new(),
            limit,
            recall: None,
            include_deleted: false,
        })
        .map_err(|error| {
            error
                .detail()
                .unwrap_or_else(|| error.message())
                .to_string()
        })?;
    Ok(hits
        .iter()
        .filter_map(|hit| hit.id.parse::<i64>().ok())
        .collect())
}

/// Returns the destination's ranking with its graph traversed exhaustively.
///
/// `recall = 1` is the search table's way of saying "do not approximate": the
/// module turns it into an unbounded traversal width, which visits every node
/// rather than the neighbourhood the graph would have led it to.
/// @param sql - the migrated index
/// @param text - the query text, or empty for a vector-only search
/// @param vector - the query vector
/// @param limit - how many hits to ask for
fn exhaustive(
    sql: &mut SqlIndex,
    text: &str,
    vector: &[f32],
    limit: usize,
) -> Result<Vec<i64>, String> {
    let hits = sql
        .search(&Query {
            text: text.to_string(),
            vector: vector.to_vec(),
            limit,
            recall: Some(1.0),
            // Every row, because the caller of this is comparing the two
            // unfiltered rankings against each other - the source is asked
            // with `include_deleted` for the same reason.
            include_deleted: true,
        })
        .map_err(|error| error.message().to_string())?;
    Ok(hits
        .iter()
        .filter_map(|hit| hit.id.parse::<i64>().ok())
        .collect())
}

/// Returns the exactly-nearest chunks to a query, by comparing every vector.
///
/// Brute force on purpose. This is the answer both approximate indexes are
/// scored against, so it cannot itself be approximate - and a corpus small
/// enough to migrate in a test is small enough to scan.
/// @param source - the legacy index, which owns the vectors
/// @param query - the query vector
/// @param filter - the same predicate the searches ran under
/// @param limit - how many neighbours to return
fn exact_neighbours(
    source: &Index,
    query: &[f32],
    filter: &inillucent_core::filter::CompiledFilter,
    limit: usize,
) -> Vec<i64> {
    let store = source.store();
    let vectors = source.vectors();
    let mut scored: Vec<(f32, u32)> = Vec::with_capacity(store.n_chunks());
    for chunk in 0..store.n_chunks() as u32 {
        if !filter.passes(chunk, store) {
            continue;
        }
        scored.push((vectors.distance(chunk, query), chunk));
    }
    // Distance ascending, then chunk ascending, so ties are broken the way the
    // engine breaks them and the comparison is about distance rather than order.
    scored.sort_by(|a, b| {
        a.0.partial_cmp(&b.0)
            .unwrap_or(core::cmp::Ordering::Equal)
            .then(a.1.cmp(&b.1))
    });
    scored
        .into_iter()
        .take(limit)
        .map(|(_, chunk)| i64::from(chunk))
        .collect()
}

/// Returns what share of the exact answer a ranking found.
/// @param found - the ranking to score
/// @param truth - the exact answer
fn recall(found: &[i64], truth: &[i64]) -> f64 {
    if truth.is_empty() {
        return 1.0;
    }
    let hits = truth.iter().filter(|id| found.contains(id)).count();
    hits as f64 / truth.len() as f64
}

/// Returns the destination's ranking for one query, as rowids in order.
fn ranked(
    sql: &mut SqlIndex,
    text: &str,
    vector: &[f32],
    limit: usize,
) -> Result<Vec<i64>, String> {
    let hits = sql
        .search(&Query {
            text: text.to_string(),
            vector: vector.to_vec(),
            limit,
            recall: None,
            // Every row: these are the unfiltered comparisons, and the source
            // side of each of them is asked with `include_deleted` too.
            include_deleted: true,
        })
        .map_err(|error| {
            error
                .detail()
                .unwrap_or_else(|| error.message())
                .to_string()
        })?;
    Ok(hits
        .iter()
        .filter_map(|hit| hit.id.parse::<i64>().ok())
        .collect())
}

/// Returns the destination's ranking with the score each hit carries.
fn ranked_hits(
    sql: &mut SqlIndex,
    text: &str,
    vector: &[f32],
    limit: usize,
) -> Result<Vec<(i64, f32)>, String> {
    let hits = sql
        .search(&Query {
            text: text.to_string(),
            vector: vector.to_vec(),
            limit,
            recall: None,
            // Every row: these are the unfiltered comparisons, and the source
            // side of each of them is asked with `include_deleted` too.
            include_deleted: true,
        })
        .map_err(|error| error.message().to_string())?;
    Ok(hits
        .iter()
        .filter_map(|hit| hit.id.parse::<i64>().ok().map(|id| (id, hit.score)))
        .collect())
}

/// Returns which document each chunk belongs to, read once from the copy.
fn chunk_documents(connection: &Connection<'_>) -> Result<Vec<(i64, i64)>, String> {
    let mut statement = connection
        .prepare("SELECT id, document FROM chunk ORDER BY id")
        .map_err(|error| error.message().to_string())?;
    let mut rows = Vec::new();
    while statement
        .step()
        .map_err(|error| error.message().to_string())?
    {
        let row = statement.row();
        if let (Some(chunk), Some(document)) = (
            row.first().and_then(as_integer),
            row.get(1).and_then(as_integer),
        ) {
            rows.push((chunk, document));
        }
    }
    Ok(rows)
}

/// Applies the legacy per-document cap to a ranking, in order.
///
/// Capping walks the list once and emits, so the first `k` it emits depend only
/// on the prefix it has seen. That is what makes every caller here sound: each
/// one asks the destination at the candidate depth the legacy engine fuses over
/// and then caps to `PROBE_K`, which is the same answer as capping a list drawn
/// to `PROBE_K` would be. `a_cap_depends_only_on_the_prefix` is the test.
///
/// **It used to take a list of chunks to leave out, and that is gone**
/// (task-2067). It existed for one caller, which removed the tombstoned rows
/// from the destination's answer because the destination had no way to exclude
/// them before it ranked. It has one now - the search table's `live` facet - so
/// every caller passes the same unfiltered ranking through the same cap, and a
/// parameter that would let somebody reintroduce filtering after the ranking is
/// worse than no parameter.
///
/// @param order - the ranking to cap, best first
/// @param documents - which document each chunk belongs to
/// @param cap - the legacy per-document cap, or zero for none
/// @param k - how many hits to emit
fn apply_cap(order: &[i64], documents: &[(i64, i64)], cap: usize, k: usize) -> Vec<i64> {
    // **Asked for none, emit none** (task-2067). The loop below tests its
    // length after it has pushed, so a `k` of zero used to come back with one
    // row. No caller passes zero, which is why nothing caught it; it was found
    // by writing the test for the prefix rule, and the rule is not true without
    // this line.
    if k == 0 {
        return Vec::new();
    }
    let mut seen: Vec<(i64, usize)> = Vec::new();
    let mut out = Vec::new();
    for chunk in order {
        let document = documents
            .iter()
            .find(|(id, _)| id == chunk)
            .map(|(_, document)| *document)
            .unwrap_or(*chunk);
        let count = match seen.iter_mut().find(|(id, _)| *id == document) {
            Some((_, count)) => {
                *count = count.saturating_add(1);
                *count
            }
            None => {
                seen.push((document, 1));
                1
            }
        };
        if cap > 0 && count > cap {
            continue;
        }
        out.push(*chunk);
        if out.len() >= k {
            break;
        }
    }
    out
}

/// Returns the queries the probes use, drawn from the corpus itself.
///
/// Drawn rather than invented: a query made of words that are in the corpus is
/// one both engines will answer with something, and a query made up would as
/// often as not compare two empty lists and call it a match.
fn text_probes(store: &Store) -> Vec<String> {
    let mut probes = Vec::new();
    for (position, ordinal) in sample(store.n_chunks(), PROBES).into_iter().enumerate() {
        let content = store.content(ordinal as u32);
        // The window slides with the probe number, so a corpus whose chunks
        // begin with the same words still produces distinct queries. A probe
        // pack that collapsed to two questions would report a pass it had not
        // earned.
        let all: Vec<&str> = content.split_whitespace().collect();
        if all.is_empty() {
            continue;
        }
        let offset = position.saturating_mul(2) % all.len();
        let mut words: Vec<&str> = all
            .iter()
            .cycle()
            .skip(offset)
            .take(PROBE_WORDS.min(all.len()))
            .copied()
            .collect();
        words.dedup();
        let query = words.join(" ");
        if !query.trim().is_empty() && !probes.contains(&query) {
            probes.push(query);
        }
    }
    probes
}

/// Returns evenly spread ordinals, so a probe set covers the whole corpus.
fn sample(total: usize, wanted: usize) -> Vec<usize> {
    if total == 0 {
        return Vec::new();
    }
    let wanted = wanted.min(total).max(1);
    let step = total / wanted;
    (0..wanted)
        .map(|index| {
            index
                .saturating_mul(step.max(1))
                .min(total.saturating_sub(1))
        })
        .collect()
}

/// Adds one integer to a digest, tagged the way `copy::digest` tags it.
fn integer(hasher: &mut Sha256, value: i64) {
    hasher.update(b"\x01");
    hasher.update(&value.to_le_bytes());
    hasher.update(b"\x1f");
}

/// Adds one text value to a digest, tagged the way `copy::digest` tags it.
fn text(hasher: &mut Sha256, value: &str) {
    hasher.update(b"\x03");
    hasher.update(value.as_bytes());
    hasher.update(b"\x1f");
}

/// Adds one optional text value, NULL when there is none.
fn optional(hasher: &mut Sha256, value: Option<&str>) {
    match value {
        Some(found) => text(hasher, found),
        None => {
            hasher.update(b"\x00");
            hasher.update(b"\x1f");
        }
    }
}

/// Returns one integer from a query.
fn scalar(connection: &Connection<'_>, sql: &str) -> Result<i64, String> {
    let mut statement = connection
        .prepare(sql)
        .map_err(|error| format!("{sql}: {}", error.message()))?;
    if !statement
        .step()
        .map_err(|error| format!("{sql}: {}", error.message()))?
    {
        return Ok(0);
    }
    Ok(statement.row().first().and_then(as_integer).unwrap_or(0))
}

/// Returns the first column of every row as integers.
fn integers(connection: &Connection<'_>, sql: &str) -> Result<Vec<i64>, String> {
    let mut statement = connection
        .prepare(sql)
        .map_err(|error| format!("{sql}: {}", error.message()))?;
    let mut rows = Vec::new();
    while statement
        .step()
        .map_err(|error| format!("{sql}: {}", error.message()))?
    {
        if let Some(value) = statement.row().first().and_then(as_integer) {
            rows.push(value);
        }
    }
    Ok(rows)
}

/// Returns the first column of every row as text.
fn strings(connection: &Connection<'_>, sql: &str) -> Result<Vec<String>, String> {
    let mut statement = connection
        .prepare(sql)
        .map_err(|error| format!("{sql}: {}", error.message()))?;
    let mut rows = Vec::new();
    while statement
        .step()
        .map_err(|error| format!("{sql}: {}", error.message()))?
    {
        if let Some(OwnedDatum::Text(value)) = statement.row().first() {
            rows.push(String::from_utf8_lossy(value).into_owned());
        }
    }
    Ok(rows)
}

/// Returns a datum's integer value, when it holds one.
///
/// @param value - the datum
fn as_integer(value: &OwnedDatum) -> Option<i64> {
    match value {
        OwnedDatum::Int(number) => Some(*number),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sample spreads across the corpus rather than clustering at the start.
    #[test]
    fn a_sample_spreads_across_the_corpus() {
        let drawn = sample(100, 5);
        assert_eq!(drawn, vec![0, 20, 40, 60, 80]);
        assert_eq!(sample(3, 12).len(), 3);
        assert!(sample(0, 5).is_empty());
    }

    /// Capping to `k` is capping deeper and taking the first `k`.
    ///
    /// **Every caller in this module depends on it** (task-2067). Each one asks
    /// the destination at the candidate depth the legacy engine fuses over -
    /// which is deeper than the ten hits being compared - and then caps to
    /// `PROBE_K`. That is only the same question as capping a ten-deep draw
    /// because the cap walks the list once and emits, so what it emits first
    /// depends on the prefix and not on how much came after it. The claim was
    /// written in `apply_cap`'s own doc comment and nothing checked it.
    #[test]
    fn a_cap_depends_only_on_the_prefix() {
        // Three documents, interleaved, so the cap bites in the middle of the
        // list rather than at its end.
        let documents: Vec<(i64, i64)> = vec![
            (1, 10),
            (2, 10),
            (3, 10),
            (4, 20),
            (5, 20),
            (6, 30),
            (7, 30),
            (8, 30),
        ];
        let order: Vec<i64> = vec![1, 4, 2, 6, 5, 3, 7, 8];
        let deep = apply_cap(&order, &documents, 2, order.len());
        assert_eq!(deep, vec![1, 4, 2, 6, 5, 7], "the third of each is dropped");
        for k in 0..=deep.len() {
            assert_eq!(
                apply_cap(&order, &documents, 2, k),
                deep.get(..k).unwrap_or_default().to_vec(),
                "capping to {k} is not the first {k} of capping deeper"
            );
        }
    }

    /// A cap of zero caps nothing, and a chunk with no document row is its own.
    ///
    /// Both are reachable: `per_doc_cap` is a configured number a caller may
    /// set to zero, and the document map is read from the copy, so a chunk the
    /// copy does not know about has to land somewhere rather than be dropped.
    #[test]
    fn a_cap_of_zero_caps_nothing_and_an_unknown_chunk_is_its_own_document() {
        let documents: Vec<(i64, i64)> = vec![(1, 10), (2, 10), (3, 10)];
        let order: Vec<i64> = vec![1, 2, 3];
        assert_eq!(apply_cap(&order, &documents, 0, 10), vec![1, 2, 3]);
        assert_eq!(apply_cap(&order, &documents, 1, 10), vec![1]);

        // 7 is in no row of the map, so it is its own document and the cap of
        // one lets it through beside the first chunk of document 10.
        let order = vec![1, 7, 2];
        assert_eq!(apply_cap(&order, &documents, 1, 10), vec![1, 7]);
    }

    /// A failing check renders a line the manifest can hold.
    #[test]
    fn a_check_renders_one_line() {
        let check = Check::fail("bm25.exact", "the top ten differed");
        assert_eq!(check.line(), "bm25.exact fail the top ten differed");
        assert_eq!(
            Check::pass("counts.chunk", "12 rows").line(),
            "counts.chunk ok 12 rows"
        );
    }
}
