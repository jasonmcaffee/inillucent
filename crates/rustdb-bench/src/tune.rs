//! Sweeping the ranking settings over one index build.
//!
//! Grading the whole card takes twenty minutes and most of that is index builds,
//! which is the wrong shape for choosing a default: the question "what should the
//! lexical coverage exponent be" does not need a new graph, a new lexical index or
//! a second engine. It needs the same index queried a few hundred more times.
//!
//! So this builds one index, then sweeps the settings that live entirely in
//! ranking: how much of the query a lexical hit has to contain, and how the two
//! result lists are fused. Everything it reports is measured on the same graph
//! against the same query sets, so a difference between two rows is the setting
//! and nothing else.
//!
//! It reports the retrieval families the settings can move — the two hybrid query
//! sets, and lexical retrieval on its own. It does not report latency, filtered
//! recall or the correctness gates, because no setting here touches them. Run
//! `grade` for those.

use anyhow::Result;
use rustdb_core::embed_onnx::Device;
use rustdb_core::filter::Filter;
use rustdb_core::rank::Fusion;

use crate::corpus::Corpus;
use crate::engine::{fuse_with, RustDbEngine};
use crate::metrics::{ndcg_at_k_attainable, reciprocal_rank, success_at_k, Accumulator};
use crate::scenarios::KeySpace;
use crate::queryset::{self, GradedQuery};
use crate::scenarios::build_index;

/// One point in the sweep.
struct Setting {
    label: String,
    coverage: f32,
    proximity: f32,
    prefix: bool,
    tier: bool,
    fusion: Fusion,
}

/// What one setting scored on one query set.
struct Row {
    label: String,
    ndcg: f64,
    success_1: f64,
    success_10: f64,
    mrr: f64,
}

/// Builds one index and reports every ranking setting in the sweep against it.
///
/// @param corpus - the cache both engines are graded on
/// @param limit - grade only the first N chunks, for a faster cycle
/// @param per_source - document identity queries per source
/// @param model_dir - directory holding the embedding weights
/// @param model_file - the ONNX file name
/// @param device - the processor the queries are embedded on
/// @param coverages - lexical coverage exponents to try
/// @param weights - vector weights to try for the two score based fusions
/// @param proximities - lexical proximity weights to try
/// @param prefixes - whether a query term also matches the terms it prefixes
/// @param tiers - whether the count of matched query terms outranks the score
/// @param tiers - whether the count of matched query terms outranks the score
/// @param seed_offset - added to the query set seeds, so the settings can be chosen
///   on queries the graded run will not use
pub fn run(
    corpus: &Corpus,
    limit: Option<usize>,
    per_source: usize,
    model_dir: &str,
    model_file: &str,
    device: Device,
    coverages: &[f32],
    weights: &[f32],
    proximities: &[f32],
    prefixes: &[bool],
    tiers: &[bool],
    seed_offset: u64,
) -> Result<()> {
    eprintln!("building the rust-db index");
    let (index, keys, stats, seconds) = build_index(corpus, limit, true)?;
    eprintln!("  {} chunks in {seconds:.1}s", stats.chunks);

    let mut engine = RustDbEngine::new(index, keys.clone(), "rust-db".to_string(), Some(128));

    // Generated from the same slice of the corpus the index holds, exactly as the
    // graded run does, so a number here is comparable with a number on the card.
    let n = limit.unwrap_or(corpus.len()).min(corpus.len());
    let chunks = &corpus.chunks[..n];
    let identity = queryset::document_identity_queries(chunks, &keys, per_source, 11 + seed_offset);
    let headings = queryset::heading_queries(chunks, &keys, per_source * 3, 12 + seed_offset);
    let identifiers = queryset::identifier_queries(chunks, &keys, per_source * 3, 13 + seed_offset);
    eprintln!(
        "  {} identity, {} heading, {} identifier queries",
        identity.len(),
        headings.len(),
        identifiers.len()
    );

    eprintln!("embedding the query sets on {}", device.label());
    let identity_vectors = queryset::embed_queries(
        model_dir,
        model_file,
        &identity.iter().map(|q| q.text.clone()).collect::<Vec<_>>(),
        device,
    )?;
    let heading_vectors = queryset::embed_queries(
        model_dir,
        model_file,
        &headings.iter().map(|q| q.text.clone()).collect::<Vec<_>>(),
        device,
    )?;

    let settings = build_settings(coverages, weights, proximities, prefixes, tiers);
    eprintln!("sweeping {} settings\n", settings.len());

    // Lexical retrieval on its own, which only the coverage exponent moves. The
    // fusion column would repeat the same number for every weight, so this table
    // is per coverage rather than per setting.
    println!("## Lexical retrieval, by coverage exponent\n");
    println!("| coverage | proximity | prefix | tier | headings success@10 | headings MRR | headings rows | identifiers success@10 | identifiers MRR |");
    println!("|---|---|---|---|---|---|---|---|---|");
    for &coverage in coverages {
        for &proximity in proximities {
            for &prefix in prefixes {
                for &tier in tiers {
            engine.index.set_lexical_coverage(coverage);
            engine.index.set_lexical_proximity(proximity);
                engine.index.set_lexical_prefix(prefix);
                    engine.index.set_lexical_tier(tier);
            let h = lexical_scores(&engine, &headings);
            let i = lexical_scores(&engine, &identifiers);
            println!(
                "| {coverage:.2} | {proximity:.2} | {prefix} | {tier} | {:.4} | {:.4} | {:.1} | {:.4} | {:.4} |",
                h.0, h.1, h.2, i.0, i.1
            );
        }
                }
            }
    }
    println!();

    for (name, queries, vectors) in [
        ("document identity, title as query", &identity, &identity_vectors),
        ("natural language, heading as query", &headings, &heading_vectors),
    ] {
        println!("## Hybrid: {name}\n");
        println!("| setting | nDCG@10 | success@1 | success@10 | MRR |");
        println!("|---|---|---|---|---|");
        let mut rows = Vec::new();
        for setting in &settings {
            engine.index.set_lexical_coverage(setting.coverage);
            engine.index.set_lexical_proximity(setting.proximity);
            engine.index.set_lexical_prefix(setting.prefix);
            engine.index.set_lexical_tier(setting.tier);
            rows.push(hybrid_scores(&engine, queries, vectors, setting));
        }
        // Best nDCG first, so the winner is the first line rather than something
        // to be found by reading down a table of forty rows.
        rows.sort_by(|a, b| b.ndcg.partial_cmp(&a.ndcg).unwrap_or(std::cmp::Ordering::Equal));
        for r in &rows {
            println!(
                "| {} | {:.4} | {:.4} | {:.4} | {:.4} |",
                r.label, r.ndcg, r.success_1, r.success_10, r.mrr
            );
        }
        println!();
    }
    Ok(())
}

/// The cross product of coverage exponents and fusion methods.
/// @param coverages - lexical coverage exponents
/// @param weights - vector weights for the two score based fusions
/// @param proximities - lexical proximity weights
/// @param prefixes - whether a query term also matches the terms it prefixes
fn build_settings(coverages: &[f32], weights: &[f32], proximities: &[f32], prefixes: &[bool], tiers: &[bool]) -> Vec<Setting> {
    let mut settings = Vec::new();
    for &coverage in coverages {
        for &proximity in proximities {
            for &prefix in prefixes {
                for &tier in tiers {
            settings.push(Setting {
                label: format!("cov {coverage:.2}, prox {proximity:.2}, prefix {prefix}, tier {tier}, RRF k=60"),
                coverage,
                proximity,
                    prefix,
                        tier,
                fusion: Fusion::ReciprocalRank { k: 60.0 },
            });
            for &w in weights {
                settings.push(Setting {
                    label: format!("cov {coverage:.2}, prox {proximity:.2}, prefix {prefix}, tier {tier}, convex w={w:.2}"),
                    coverage,
                    proximity,
                    prefix,
                        tier,
                    fusion: Fusion::Convex { vector_weight: w },
                });
                settings.push(Setting {
                    label: format!("cov {coverage:.2}, prox {proximity:.2}, prefix {prefix}, tier {tier}, min-max w={w:.2}"),
                    coverage,
                    proximity,
                    prefix,
                        tier,
                    fusion: Fusion::NormalizedScore { vector_weight: w },
                });
            }
        }
                }
            }
    }
    settings
}

/// success@10, mean reciprocal rank and mean rows returned for the lexical side
/// on its own.
/// @param engine - the built index, already carrying the coverage under test
/// @param queries - the query set
fn lexical_scores(engine: &RustDbEngine, queries: &[GradedQuery]) -> (f64, f64, f64) {
    let filter = Filter::default();
    let compiled = engine.index.compile(&filter);
    let mut success = Accumulator::default();
    let mut mrr = Accumulator::default();
    let mut rows = Accumulator::default();
    for q in queries {
        let hits = engine.index.lexical_search(&q.text, &compiled, 50);
        rows.push(hits.len() as f32);
        let keys: Vec<String> = hits.iter().map(|h| engine.keys[h.chunk as usize].clone()).collect();
        let mut space = KeySpace::new();
        let correct = space.set_of(&q.correct);
        let got = space.ids_of(&keys);
        success.push(success_at_k(&got, &correct, 10));
        mrr.push(reciprocal_rank(&got, &correct));
    }
    (success.mean() as f64, mrr.mean() as f64, rows.mean() as f64)
}

/// The four hybrid metrics for one setting on one query set.
/// @param engine - the built index, already carrying the coverage under test
/// @param queries - the query set
/// @param vectors - one embedding per query, in the same order
/// @param setting - the setting being measured, for its label and its fusion
fn hybrid_scores(
    engine: &RustDbEngine,
    queries: &[GradedQuery],
    vectors: &[Vec<f32>],
    setting: &Setting,
) -> Row {
    let filter = Filter::default();
    let cap = engine.index.config().per_doc_cap;
    let mut ndcg = Accumulator::default();
    let mut s1 = Accumulator::default();
    let mut s10 = Accumulator::default();
    let mut mrr = Accumulator::default();
    for (q, v) in queries.iter().zip(vectors) {
        let hits = fuse_with(engine, &q.text, v, &filter, 10, setting.fusion);
        let keys: Vec<String> = hits.iter().map(|h| h.key.clone()).collect();
        let mut space = KeySpace::new();
        let correct = space.set_of(&q.correct);
        let got = space.ids_of(&keys);
        ndcg.push(ndcg_at_k_attainable(&got, &correct, 10, cap));
        s1.push(success_at_k(&got, &correct, 1));
        s10.push(success_at_k(&got, &correct, 10));
        mrr.push(reciprocal_rank(&got, &correct));
    }
    Row {
        label: setting.label.clone(),
        ndcg: ndcg.mean() as f64,
        success_1: s1.mean() as f64,
        success_10: s10.mean() as f64,
        mrr: mrr.mean() as f64,
    }
}
