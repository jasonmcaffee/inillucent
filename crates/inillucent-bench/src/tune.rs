//! Sweeping the ranking settings over one index build.
//!
//! Grading the whole card takes minutes and most of that is the index build,
//! which is the wrong shape for choosing a default: the question "what should the
//! lexical coverage exponent be" does not need a new graph, a new lexical index or
//! a second engine. It needs the same index queried a few hundred more times.
//!
//! So this builds one index and sweeps everything that lives entirely in ranking:
//! the lexical dials, the fusion method and its weight, the ordered-phrase
//! feature, per-query adaptive weighting and diversity selection. Everything it
//! reports is measured on the same graph against the same query sets, so a
//! difference between two rows is the setting and nothing else.
//!
//! Two things changed here alongside the score card, and for the same reasons.
//!
//! The sweep now reports the **hard families** as well as the two document-level
//! ones. A setting chosen on title queries is chosen on a family where any chunk
//! of the right page counts, and that is not the question the engine exists to
//! answer; passage evidence, its perturbations and the multi-source pack are.
//!
//! And every arm is now compared against the baseline arm with a **paired
//! bootstrap interval and a paired randomization test**, not by reading down a
//! sorted column. The winner of a forty-row sweep is the top of a distribution of
//! forty draws from the same noise unless something says otherwise, and picking
//! it without an interval is how a default gets chosen by luck.
//!
//! It does not report latency, filtered recall or the correctness gates, because
//! no setting here touches them. Run `grade` for those.

use std::collections::HashMap;

use anyhow::{Context, Result};
use inillucent_core::embed_onnx::Device;
use inillucent_core::filter::Filter;
use inillucent_core::rank::{AdaptiveWeights, Fusion};

use crate::corpus::Corpus;
use crate::engine::InillucentEngine;
use crate::metrics::{
    graded_recall_at_k, ndcg_at_k_attainable, ndcg_graded_at_k, percentile, reciprocal_rank,
    success_at_k,
};
use crate::queryset::{self, GradedQuery, Perturbation};
use crate::scenarios::build_index;
use crate::scenarios::KeySpace;
use crate::stats;

/// One fully specified point in the sweep.
///
/// Every field is a ranking setting, which is what lets the whole sweep run
/// against one index: nothing here changes the graph, the postings or the codes.
#[derive(Clone)]
pub struct Setting {
    pub label: String,
    pub coverage: f32,
    pub proximity: f32,
    pub prefix: bool,
    pub tier: bool,
    pub phrase: f32,
    pub fusion: Fusion,
    pub adaptive: Option<AdaptiveWeights>,
    pub mmr_lambda: f32,
}

/// What one arm scored, per family, with the per-query series kept so the arm can
/// be compared against the baseline as a paired sample rather than as a mean.
struct ArmScores {
    label: String,
    means: HashMap<String, f64>,
    series: HashMap<String, Vec<f64>>,
}

impl ArmScores {
    fn mean(&self, family: &str) -> f64 {
        self.means.get(family).copied().unwrap_or(0.0)
    }
}

/// The families the sweep reports, in the order they are printed.
///
/// `passage evidence` leads because it is the one that grades the paragraph
/// rather than the page, and it is the measurement a default should be chosen on.
const FAMILIES: &[&str] = &[
    "passage evidence",
    "passage, transposed",
    "passage, keywords",
    "multi-source",
    "heading",
    "identity",
    "identifier MRR",
    "unanswerable rate",
];

/// The one a sweep is sorted and judged by.
const PRIMARY: &str = "passage evidence";

/// Builds one index and reports every ranking setting in the sweep against it.
///
/// @param corpus - the cache the engine is graded on
/// @param limit - grade only the first N chunks, for a faster cycle
/// @param per_source - document identity queries per source
/// @param model - the resolved model, with the manifest that says what it is
/// @param device - the processor the queries are embedded on
/// @param settings - the arms to try, the first of which is the baseline
/// @param seed_offset - added to the query set seeds, so the settings are chosen
///   on queries the graded run will not use
/// @param stats_seed - fixes every interval and p-value the sweep prints
pub fn run(
    corpus: &Corpus,
    limit: Option<usize>,
    per_source: usize,
    model: &crate::models::ResolvedModel,
    device: Device,
    settings: &[Setting],
    seed_offset: u64,
    stats_seed: u64,
) -> Result<()> {
    anyhow::ensure!(!settings.is_empty(), "the sweep named no settings");

    eprintln!("building the inillucent index");
    let (index, keys, stats, seconds) = build_index(corpus, limit, true)?;
    eprintln!("  {} chunks in {seconds:.1}s", stats.chunks);

    let mut engine =
        InillucentEngine::new(index, keys.clone(), "inillucent".to_string(), Some(128));

    // Generated from the same slice of the corpus the index holds, exactly as the
    // graded run does, so a number here is comparable with a number on the card.
    let n = limit.unwrap_or(corpus.len()).min(corpus.len());
    let chunks = corpus
        .chunks
        .get(..n)
        .with_context(|| format!("the corpus holds {} chunks, not {n}", corpus.chunks.len()))?;
    let identity = queryset::document_identity_queries(chunks, &keys, per_source, 11 + seed_offset);
    let headings = queryset::heading_queries(chunks, &keys, per_source * 3, 12 + seed_offset);
    let identifiers = queryset::identifier_queries(chunks, &keys, per_source * 3, 13 + seed_offset);
    let df = queryset::document_frequencies(chunks);
    let passage =
        queryset::passage_evidence_queries(chunks, &keys, &df, per_source, 14 + seed_offset);
    let typo = queryset::perturbed_queries(&passage, Perturbation::Typo, &df);
    let shorthand = queryset::perturbed_queries(&passage, Perturbation::Shorthand, &df);
    let unanswerable =
        queryset::unanswerable_queries(chunks, &df, per_source * 2, 15 + seed_offset);
    let multi = queryset::multi_source_queries(chunks, &keys, per_source * 2, 16 + seed_offset);
    let calibration = queryset::heading_queries(chunks, &keys, per_source * 2, 1012 + seed_offset);
    eprintln!(
        "  {} identity, {} heading, {} identifier, {} passage, {} typo, {} shorthand, {} multi-source, {} unanswerable",
        identity.len(),
        headings.len(),
        identifiers.len(),
        passage.len(),
        typo.len(),
        shorthand.len(),
        multi.len(),
        unanswerable.len()
    );

    eprintln!("embedding the query sets on {}", device.label());
    let embedder = queryset::open_query_embedder(
        model,
        &crate::arm::ArmOptions {
            device,
            ..Default::default()
        },
    )?;
    let embed = |qs: &[GradedQuery]| -> Result<Vec<Vec<f32>>> {
        queryset::embed_with(
            &embedder,
            &qs.iter().map(|q| q.text.clone()).collect::<Vec<_>>(),
        )
    };
    let identity_vectors = embed(&identity)?;
    let heading_vectors = embed(&headings)?;
    let passage_vectors = embed(&passage)?;
    let typo_vectors = embed(&typo)?;
    let shorthand_vectors = embed(&shorthand)?;
    let multi_vectors = embed(&multi)?;
    let unanswerable_vectors = embed(&unanswerable)?;
    let calibration_vectors = embed(&calibration)?;

    eprintln!("sweeping {} settings\n", settings.len());

    let mut arms: Vec<ArmScores> = Vec::new();
    for setting in settings {
        apply(&mut engine, setting);
        arms.push(score_arm(
            &engine,
            setting,
            &[
                (
                    "passage evidence",
                    &passage,
                    &passage_vectors,
                    Grading::Graded,
                ),
                ("passage, transposed", &typo, &typo_vectors, Grading::Graded),
                (
                    "passage, keywords",
                    &shorthand,
                    &shorthand_vectors,
                    Grading::Graded,
                ),
                ("multi-source", &multi, &multi_vectors, Grading::AllEvidence),
                ("heading", &headings, &heading_vectors, Grading::Binary),
                ("identity", &identity, &identity_vectors, Grading::Binary),
            ],
            &identifiers,
            (&calibration, &calibration_vectors),
            (&unanswerable, &unanswerable_vectors),
        )?);
        eprint!(".");
    }
    eprintln!();

    print_table(&arms);
    print_comparison(&arms, stats_seed);
    Ok(())
}

/// Put one arm's settings on the index. Nothing here rebuilds anything.
fn apply(engine: &mut InillucentEngine, setting: &Setting) {
    engine.index.set_lexical_coverage(setting.coverage);
    engine.index.set_lexical_proximity(setting.proximity);
    engine.index.set_lexical_prefix(setting.prefix);
    engine.index.set_lexical_tier(setting.tier);
    engine.index.set_lexical_phrase(setting.phrase);
    engine.index.set_fusion(setting.fusion);
    engine.index.set_mmr_lambda(setting.mmr_lambda);
    match setting.adaptive {
        Some(a) => engine.index.set_adaptive_fusion(true, a),
        None => engine
            .index
            .set_adaptive_fusion(false, AdaptiveWeights::default()),
    }
}

/// How a family's ground truth is read.
#[derive(Clone, Copy)]
enum Grading {
    /// Graded judgements, where a passage that answers outranks one from the same
    /// document that does not.
    Graded,
    /// Every correct chunk is required, not just one of them.
    AllEvidence,
    /// One correct answer anywhere in the list, which is what the document-level
    /// families can express.
    Binary,
}

/// The corpus keys a result list stands for.
///
/// **Fallible for the reason `InillucentEngine::key` is.** An ordinal past the
/// end of the key list means the index and the keys disagree about what was
/// loaded, and a sweep that scored an empty key would report a tuning setting
/// as worse than it is.
///
/// @param engine - the engine whose key list the ordinals index
/// @param chunks - the chunk ordinals a search returned, in order
fn keys_of_chunks(
    engine: &InillucentEngine,
    chunks: impl Iterator<Item = u32>,
) -> Result<Vec<String>> {
    chunks
        .map(|chunk| {
            engine.keys.get(chunk as usize).cloned().with_context(|| {
                format!(
                    "chunk {chunk} has no key: the index returned an ordinal past the \
                     {} keys the sweep loaded with it",
                    engine.keys.len()
                )
            })
        })
        .collect()
}

/// Score one arm on every family.
#[allow(clippy::type_complexity)]
fn score_arm(
    engine: &InillucentEngine,
    setting: &Setting,
    families: &[(&str, &Vec<GradedQuery>, &Vec<Vec<f32>>, Grading)],
    identifiers: &[GradedQuery],
    calibration: (&Vec<GradedQuery>, &Vec<Vec<f32>>),
    unanswerable: (&Vec<GradedQuery>, &Vec<Vec<f32>>),
) -> Result<ArmScores> {
    let filter = Filter::default();
    let compiled = engine.index.compile(&filter);
    let cap = engine.index.config().per_doc_cap;
    let mut means = HashMap::new();
    let mut series: HashMap<String, Vec<f64>> = HashMap::new();

    for (name, queries, vectors, grading) in families {
        let mut values: Vec<f64> = Vec::new();
        for (q, v) in queries.iter().zip(vectors.iter()) {
            let hits = engine
                .index
                .hybrid_search(&q.text, v, &compiled, 10, engine.ef_search)
                .context("the hybrid search this family is scored on")?;
            let keys = keys_of_chunks(engine, hits.iter().map(|h| h.chunk))?;
            let mut space = KeySpace::new();
            let correct = space.set_of(&q.correct);
            let grades = q.grades(&mut space);
            let got = space.ids_of(&keys);
            values.push(match grading {
                Grading::Graded => ndcg_graded_at_k(&got, &grades, 10, cap) as f64,
                Grading::AllEvidence => {
                    graded_recall_at_k(&got, &grades, 10, queryset::GRADE_ANSWER) as f64
                }
                Grading::Binary => ndcg_at_k_attainable(&got, &correct, 10, cap) as f64,
            });
        }
        means.insert(name.to_string(), mean_of(&values));
        series.insert(name.to_string(), values);
    }

    // The lexical side on its own, which only the lexical dials move.
    let mut identifier_mrr: Vec<f64> = Vec::new();
    for q in identifiers {
        let hits = engine.index.lexical_search(&q.text, &compiled, 50);
        let keys = keys_of_chunks(engine, hits.iter().map(|h| h.chunk))?;
        let mut space = KeySpace::new();
        let correct = space.set_of(&q.correct);
        let got = space.ids_of(&keys);
        identifier_mrr.push(reciprocal_rank(&got, &correct) as f64);
    }
    means.insert("identifier MRR".to_string(), mean_of(&identifier_mrr));
    series.insert("identifier MRR".to_string(), identifier_mrr);

    // Abstention, calibrated on this arm's own scale exactly as the card does, and
    // on the top hit's confidence rather than its fused score: the fused score's
    // scale is read out of the candidate list, so it is the same for a good list
    // and a hopeless one and there is no threshold on it to set.
    let top_scores = |queries: &[GradedQuery], vectors: &[Vec<f32>]| -> Result<Vec<f64>> {
        queries
            .iter()
            .zip(vectors)
            .map(|(q, v)| {
                Ok(engine
                    .index
                    .hybrid_search(&q.text, v, &compiled, 10, engine.ef_search)
                    .context("the hybrid search the abstention threshold is read from")?
                    .first()
                    .map(|h| h.confidence as f64)
                    .unwrap_or(0.0))
            })
            .collect()
    };
    let mut answerable = top_scores(calibration.0, calibration.1)?;
    answerable.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let threshold = percentile(&answerable, 0.05);
    let flags: Vec<f64> = top_scores(unanswerable.0, unanswerable.1)?
        .into_iter()
        .map(|s| if s >= threshold { 1.0 } else { 0.0 })
        .collect();
    means.insert("unanswerable rate".to_string(), mean_of(&flags));
    series.insert("unanswerable rate".to_string(), flags);

    // success@10 on the primary family, printed alongside so a graded nDCG can be
    // read against something more concrete.
    let mut hits_at_10: Vec<f64> = Vec::new();
    if let Some((_, queries, vectors, _)) = families.first() {
        for (q, v) in queries.iter().zip(vectors.iter()) {
            let hits = engine
                .index
                .hybrid_search(&q.text, v, &compiled, 10, engine.ef_search)
                .context("the hybrid search success@10 is counted from")?;
            let keys = keys_of_chunks(engine, hits.iter().map(|h| h.chunk))?;
            let mut space = KeySpace::new();
            let correct = space.set_of(&q.correct);
            let got = space.ids_of(&keys);
            hits_at_10.push(success_at_k(&got, &correct, 10) as f64);
        }
    }
    means.insert("passage success@10".to_string(), mean_of(&hits_at_10));

    Ok(ArmScores {
        label: setting.label.clone(),
        means,
        series,
    })
}

fn mean_of(values: &[f64]) -> f64 {
    if values.is_empty() {
        0.0
    } else {
        values.iter().sum::<f64>() / values.len() as f64
    }
}

/// Every arm, best on the primary family first.
fn print_table(arms: &[ArmScores]) {
    let mut order: Vec<&ArmScores> = arms.iter().collect();
    order.sort_by(|a, b| {
        b.mean(PRIMARY)
            .partial_cmp(&a.mean(PRIMARY))
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    println!("## Every arm\n");
    println!("Sorted by graded nDCG@10 on the passage evidence family, which is the one that grades the paragraph rather than the page. `unanswerable rate` is how often the arm produced a confident result for a question with no answer, so lower is better there and only there.\n");
    print!("| setting |");
    for f in FAMILIES {
        print!(" {f} |");
    }
    println!(" passage success@10 |");
    print!("|---|");
    for _ in FAMILIES {
        print!("---|");
    }
    println!("---|");
    for arm in order {
        print!("| {} |", arm.label);
        for f in FAMILIES {
            print!(" {:.4} |", arm.mean(f));
        }
        println!(" {:.4} |", arm.mean("passage success@10"));
    }
    println!();
}

/// Every arm against the baseline, with the uncertainty attached.
///
/// The baseline is the first setting given, which the caller arranges to be the
/// configuration currently shipped. Without this table a sweep says which arm
/// scored highest, and the highest of forty draws from the same noise scores
/// highest too.
fn print_comparison(arms: &[ArmScores], stats_seed: u64) {
    let Some(baseline) = arms.first() else { return };
    println!("## Against the baseline arm, paired\n");
    println!("Baseline: `{}`. `delta` is the arm minus the baseline on the passage evidence family, oriented so positive is better. The interval is the 95% paired bootstrap; `p` is the paired randomization test. An arm is only worth adopting when the interval clears both zero and the 0.01 practical threshold, and when nothing in the regression columns went backwards.\n", baseline.label);
    println!("| setting | passage delta | 95% interval | p | verdict | multi-source delta | identifier MRR delta | unanswerable delta |");
    println!("|---|---|---|---|---|---|---|---|");

    let mut order: Vec<&ArmScores> = arms.iter().skip(1).collect();
    order.sort_by(|a, b| {
        b.mean(PRIMARY)
            .partial_cmp(&a.mean(PRIMARY))
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    for arm in order {
        let empty: Vec<f64> = Vec::new();
        let a = arm.series.get(PRIMARY).unwrap_or(&empty);
        let b = baseline.series.get(PRIMARY).unwrap_or(&empty);
        let (delta, interval, p, verdict) = match stats::compare(a, b, stats_seed) {
            Some(paired) => (
                format!("{:+.4}", paired.delta),
                format!("{:+.4} to {:+.4}", paired.low, paired.high),
                format!("{:.4}", paired.p_value),
                stats::verdict(&paired, 0.01).label().to_string(),
            ),
            None => ("n/a".into(), "n/a".into(), "n/a".into(), "n/a".into()),
        };
        println!(
            "| {} | {} | {} | {} | {} | {:+.4} | {:+.4} | {:+.4} |",
            arm.label,
            delta,
            interval,
            p,
            verdict,
            arm.mean("multi-source") - baseline.mean("multi-source"),
            arm.mean("identifier MRR") - baseline.mean("identifier MRR"),
            arm.mean("unanswerable rate") - baseline.mean("unanswerable rate"),
        );
    }
    println!();
}

/// The values each ranking dial is swept over.
///
/// **A type rather than nine slices in a row (task-1962, A9).** Five of the
/// nine are `&[f32]`, so a call site that swapped the proximity weights and the
/// phrase weights would compile and would sweep the wrong dial - and the sweep
/// would report a winner for a setting nobody varied.
pub struct Sweep<'a> {
    /// Lexical coverage exponents.
    pub coverages: &'a [f32],
    /// Vector weights, for the score based fusions.
    pub weights: &'a [f32],
    /// Lexical proximity weights.
    pub proximities: &'a [f32],
    /// Whether a query term also matches the terms it prefixes.
    pub prefixes: &'a [bool],
    /// Whether the count of matched query terms outranks the score.
    pub tiers: &'a [bool],
    /// Ordered-phrase weights.
    pub phrases: &'a [f32],
    /// Fusion method names.
    pub fusions: &'a [String],
    /// Diversity lambdas.
    pub mmrs: &'a [f32],
    /// Adaptive weighting rules to try; empty sweeps fixed weighting only.
    pub adaptive: &'a [AdaptiveWeights],
}

/// One arm of the sweep, as the label names it.
struct Dials<'a> {
    /// The lexical coverage exponent.
    coverage: f32,
    /// The lexical proximity weight.
    proximity: f32,
    /// Whether a query term also matches the terms it prefixes.
    prefix: bool,
    /// Whether the count of matched query terms outranks the score.
    tier: bool,
    /// The ordered-phrase weight.
    phrase: f32,
    /// The fusion method's name.
    fusion: &'a str,
    /// The vector weight.
    weight: f32,
    /// The diversity lambda.
    mmr: f32,
    /// The adaptive weighting rule, when this arm has one.
    adaptive: &'a Option<AdaptiveWeights>,
}

/// The cross product of every axis the caller asked for, with the baseline arm
/// first so the comparison table has something to compare against.
///
/// The baseline is spelled out rather than taken from the first point of the
/// sweep, because a sweep that does not happen to include the current defaults
/// would otherwise silently compare its arms against one of themselves.
/// @param baseline - the configuration currently shipped
/// @param sweep - the values each dial is swept over
pub fn build_settings(baseline: Setting, sweep: &Sweep<'_>) -> Vec<Setting> {
    let Sweep {
        coverages,
        weights,
        proximities,
        prefixes,
        tiers,
        phrases,
        fusions,
        mmrs,
        adaptive,
    } = *sweep;
    let mut out = vec![baseline];
    // No adaptive rule at all is always one of the options, so a sweep over the
    // gains still contains the arm that turns the mechanism off.
    let adaptive_options: Vec<Option<AdaptiveWeights>> = std::iter::once(None)
        .chain(adaptive.iter().copied().map(Some))
        .collect();

    for &coverage in coverages {
        for &proximity in proximities {
            for &prefix in prefixes {
                for &tier in tiers {
                    for &phrase in phrases {
                        for name in fusions {
                            for &mmr in mmrs {
                                for rule in &adaptive_options {
                                    for &w in weights {
                                        let Some(fusion) = fusion_named(name, w) else {
                                            continue;
                                        };
                                        // Reciprocal rank fusion has no weight, so
                                        // sweeping one would repeat the same arm
                                        // once per weight.
                                        if matches!(fusion, Fusion::ReciprocalRank { .. })
                                            && Some(w) != weights.first().copied()
                                        {
                                            continue;
                                        }
                                        if rule.is_some()
                                            && matches!(fusion, Fusion::ReciprocalRank { .. })
                                        {
                                            continue;
                                        }
                                        let rule = rule.map(|r| AdaptiveWeights { base: w, ..r });
                                        out.push(Setting {
                                            label: label_for(&Dials {
                                                coverage,
                                                proximity,
                                                prefix,
                                                tier,
                                                phrase,
                                                fusion: name,
                                                weight: w,
                                                mmr,
                                                adaptive: &rule,
                                            }),
                                            coverage,
                                            proximity,
                                            prefix,
                                            tier,
                                            phrase,
                                            fusion,
                                            adaptive: rule,
                                            mmr_lambda: mmr,
                                        });
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    out
}

/// A fusion by the name the command line uses.
pub fn fusion_named(name: &str, vector_weight: f32) -> Option<Fusion> {
    match name.trim().to_ascii_lowercase().as_str() {
        "rrf" => Some(Fusion::ReciprocalRank {
            k: inillucent_core::rank::RRF_K,
        }),
        "minmax" => Some(Fusion::NormalizedScore { vector_weight }),
        "convex" => Some(Fusion::Convex { vector_weight }),
        "tmm" => Some(Fusion::TheoreticalMinMax { vector_weight }),
        _ => None,
    }
}

/// A label that names every setting the arm differs by, so a table row can be
/// turned back into a command line.
fn label_for(dials: &Dials<'_>) -> String {
    let Dials {
        coverage,
        proximity,
        prefix,
        tier,
        phrase,
        fusion,
        weight,
        mmr,
        adaptive,
    } = *dials;
    let mut parts = vec![format!("cov {coverage:.2}"), format!("prox {proximity:.2}")];
    if prefix {
        parts.push("prefix".into());
    }
    if tier {
        parts.push("tier".into());
    }
    if phrase > 0.0 {
        parts.push(format!("phrase {phrase:.2}"));
    }
    parts.push(if fusion == "rrf" {
        "rrf".to_string()
    } else {
        format!("{fusion} w={weight:.2}")
    });
    if mmr < 1.0 {
        parts.push(format!("mmr {mmr:.2}"));
    }
    if let Some(a) = adaptive {
        parts.push(format!(
            "adaptive oov={:.2} id={:.2} sep={:.2} cov={:.2}",
            a.out_of_vocabulary_gain, a.identifier_gain, a.separation_gain, a.coverage_gain
        ));
    }
    parts.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn baseline() -> Setting {
        Setting {
            label: "baseline".into(),
            coverage: 3.0,
            proximity: 1.0,
            prefix: false,
            tier: false,
            phrase: 0.0,
            fusion: Fusion::NormalizedScore {
                vector_weight: 0.35,
            },
            adaptive: None,
            mmr_lambda: 1.0,
        }
    }

    #[test]
    fn the_baseline_is_always_the_first_arm() {
        let settings = build_settings(
            baseline(),
            &Sweep {
                coverages: &[3.0],
                weights: &[0.35, 0.5],
                proximities: &[1.0],
                prefixes: &[false],
                tiers: &[false],
                phrases: &[0.0],
                fusions: &["minmax".to_string()],
                mmrs: &[1.0],
                adaptive: &[],
            },
        );
        assert_eq!(settings[0].label, "baseline");
        assert!(settings.len() > 1);
    }

    /// Reciprocal rank fusion has no weight, so sweeping weights against it would
    /// measure the same arm several times and give it several chances to be the
    /// highest of the sweep.
    #[test]
    fn rank_fusion_is_not_repeated_once_per_weight() {
        let settings = build_settings(
            baseline(),
            &Sweep {
                coverages: &[3.0],
                weights: &[0.2, 0.35, 0.5, 0.7],
                proximities: &[1.0],
                prefixes: &[false],
                tiers: &[false],
                phrases: &[0.0],
                fusions: &["rrf".to_string()],
                mmrs: &[1.0],
                adaptive: &[],
            },
        );
        assert_eq!(settings.len(), 2, "the baseline plus one rank fusion arm");
    }

    /// A sweep over adaptive gains has to contain the arm that turns the mechanism
    /// off, or it cannot show that turning it on helped.
    #[test]
    fn an_adaptive_sweep_still_contains_the_fixed_weight_arm() {
        let rule = AdaptiveWeights {
            identifier_gain: 0.3,
            ..Default::default()
        };
        let settings = build_settings(
            baseline(),
            &Sweep {
                coverages: &[3.0],
                weights: &[0.35],
                proximities: &[1.0],
                prefixes: &[false],
                tiers: &[false],
                phrases: &[0.0],
                fusions: &["minmax".to_string()],
                mmrs: &[1.0],
                adaptive: &[rule],
            },
        );
        assert!(settings
            .iter()
            .any(|s| s.adaptive.is_none() && s.label != "baseline"));
        assert!(settings.iter().any(|s| s.adaptive.is_some()));
    }

    /// The base weight of an adaptive rule is the weight being swept, so an arm
    /// with every gain at zero is exactly the fixed arm beside it.
    #[test]
    fn an_adaptive_arm_takes_its_base_from_the_weight_being_swept() {
        let rule = AdaptiveWeights {
            identifier_gain: 0.3,
            base: 0.0,
            ..Default::default()
        };
        let settings = build_settings(
            baseline(),
            &Sweep {
                coverages: &[3.0],
                weights: &[0.6],
                proximities: &[1.0],
                prefixes: &[false],
                tiers: &[false],
                phrases: &[0.0],
                fusions: &["minmax".to_string()],
                mmrs: &[1.0],
                adaptive: &[rule],
            },
        );
        let adaptive = settings.iter().find(|s| s.adaptive.is_some()).unwrap();
        assert!((adaptive.adaptive.unwrap().base - 0.6).abs() < 1e-6);
    }

    #[test]
    fn every_fusion_name_the_command_line_accepts_resolves() {
        for name in ["rrf", "minmax", "convex", "tmm"] {
            assert!(fusion_named(name, 0.5).is_some(), "{name} did not resolve");
        }
        assert!(fusion_named("nonsense", 0.5).is_none());
    }

    #[test]
    fn a_label_names_every_setting_that_is_not_a_default() {
        let label = label_for(&Dials {
            coverage: 2.0,
            proximity: 0.5,
            prefix: true,
            tier: true,
            phrase: 0.4,
            fusion: "tmm",
            weight: 0.42,
            mmr: 0.8,
            adaptive: &Some(AdaptiveWeights {
                identifier_gain: 0.3,
                ..Default::default()
            }),
        });
        assert!(label.contains("cov 2.00"));
        assert!(label.contains("prefix"));
        assert!(label.contains("tier"));
        assert!(label.contains("phrase 0.40"));
        assert!(label.contains("tmm w=0.42"));
        assert!(label.contains("mmr 0.80"));
        assert!(label.contains("id=0.30"));
    }
}
