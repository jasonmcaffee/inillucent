//! Turning a run's collected scores into the embedding score card.
//!
//! This is the counterpart of `report.rs` for the other card: everything from
//! here on has the numbers already and decides how they are read - which row is
//! primary and which is diagnostic, which arm beat which and by enough to say
//! so, what the caveats are, and how the whole of it prints.
//!
//! **Nothing here reads a cache, embeds a query or times a model.** That is the
//! line the split is on, and it is what makes the file readable on its own: a
//! function in here takes scores that are already collected and returns a
//! `Lane`, a `Vec<ArmJudgement>` or a string. If something in here needs to go
//! back to the corpus for an answer, the answer belongs in `gradeembed.rs` and
//! only its result belongs here.
//!
//! The judging rule the whole card rests on: nDCG@10 is the primary row for
//! every family and every other measure is a diagnostic, because six correlated
//! measures of one behaviour are one result and counting each of them
//! separately turns a single win into six.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};

use super::{
    composite, median, ArmFacts, ArmJudgement, EmbeddingCard, EmbeddingRow, Lane, CALIBRATION,
    HEADLINE, M_EVIDENCE_RECALL, M_MRR, M_NDCG, M_NDCG_GRADED, M_PRECISION_10,
    M_RECALL_AGAINST_FULL, M_SUCCESS_1, M_SUCCESS_10, M_TOP_SCORE, PROMOTED, RANKING_THRESHOLD,
    UNANSWERABLE,
};
use crate::corpus::short;
use crate::report::Role;
use crate::stats::{self, Verdict};

pub(super) type LaneScores = BTreeMap<String, BTreeMap<String, BTreeMap<String, Vec<f64>>>>;

/// Turn one lane's accumulated scores into rows.
///
/// nDCG@10 is the primary row for every family and everything else is a
/// diagnostic, for the same reason `grade` makes that distinction: six correlated
/// measures of one behaviour are one result, and counting each of them separately
/// turns a single win into six.
pub(super) fn lane_from(name: &str, rationale: &str, scores: &LaneScores) -> Lane {
    let mut rows = Vec::new();
    for (family, metrics) in scores {
        for metric in [
            M_NDCG,
            M_NDCG_GRADED,
            M_SUCCESS_1,
            M_SUCCESS_10,
            M_MRR,
            M_PRECISION_10,
            M_EVIDENCE_RECALL,
            M_TOP_SCORE,
        ] {
            let Some(by_model) = metrics.get(metric) else {
                continue;
            };
            rows.push(EmbeddingRow {
                family: family.clone(),
                metric: metric.to_string(),
                higher_is_better: true,
                role: if metric == M_NDCG {
                    Role::Primary
                } else {
                    Role::Diagnostic
                },
                values: by_model
                    .iter()
                    .map(|(m, v)| (m.clone(), v.iter().sum::<f64>() / v.len().max(1) as f64))
                    .collect(),
                series: by_model.clone(),
            });
        }
    }
    Lane {
        name: name.to_string(),
        rationale: rationale.to_string(),
        rows,
    }
}

/// The composite lane: the declared families, and the promoted set beside it.
pub(super) fn composite_lane(scores: &LaneScores, name: &str) -> Lane {
    let mut rows = Vec::new();
    for (label, set, role) in [
        (
            format!("composite, declared ({})", HEADLINE.join(" + ")),
            HEADLINE,
            Role::Primary,
        ),
        // Primary, not diagnostic. Only primary rows are judged, so while this was
        // diagnostic the promoted composite had values and a per-query series but no
        // paired verdict and no interval - and the promoted set is the one a later
        // ticket's gates are declared on. Task-1818 read the declared two-family
        // composite because it was the only one with a verdict, and three of its runs
        // earned "better" on it while passage evidence regressed by up to 0.064
        // underneath. A row nobody can get a verdict for is a row that gets read as
        // the row beside it.
        //
        // Judging both costs one extra comparison per model per lane and takes nothing
        // away: the declared composite is still judged and still says what it said.
        (
            format!("composite, promoted ({} families)", PROMOTED.len()),
            PROMOTED,
            Role::Primary,
        ),
    ] {
        // family -> model -> series, transposed to model -> concatenated series.
        let mut per_model: BTreeMap<String, BTreeMap<String, Vec<f64>>> = BTreeMap::new();
        for family in set {
            let Some(metrics) = scores.get(*family) else {
                continue;
            };
            let Some(by_model) = metrics.get(M_NDCG) else {
                continue;
            };
            for (model, values) in by_model {
                per_model
                    .entry(model.clone())
                    .or_default()
                    .insert((*family).to_string(), values.clone());
            }
        }
        let series: BTreeMap<String, Vec<f64>> = per_model
            .iter()
            .map(|(m, fams)| (m.clone(), composite(fams, set)))
            .collect();
        // Only models that answered every family in the set, or the concatenated
        // series would be different lengths and no paired test would be valid.
        let expected = series.values().map(|v| v.len()).max().unwrap_or(0);
        let series: BTreeMap<String, Vec<f64>> = series
            .into_iter()
            .filter(|(_, v)| v.len() == expected)
            .collect();
        rows.push(EmbeddingRow {
            family: label,
            metric: M_NDCG.to_string(),
            higher_is_better: true,
            role,
            values: series
                .iter()
                .map(|(m, v)| (m.clone(), v.iter().sum::<f64>() / v.len().max(1) as f64))
                .collect(),
            series,
        });
    }
    Lane {
        name: name.to_string(),
        rationale: format!(
            "One score per question, every family concatenated, so the composite is a paired \
             series rather than a mean of means. The declared composite is {}. Both composites \
             are judged: while only the declared one carried a verdict, the promoted set could \
             be read but not decided on, and a row nobody can get a verdict for is a row that \
             gets read as the row beside it - three of task-1818's runs earned \"better\" on the \
             declared composite while passage evidence regressed by up to 0.064 underneath it.",
            HEADLINE.join(" and ")
        ),
        rows,
    }
}

pub(super) fn matryoshka_lane(
    mrl: &BTreeMap<String, BTreeMap<String, f64>>,
    arms: &[ArmFacts],
) -> Lane {
    let mut widths: Vec<usize> = arms.iter().flat_map(|a| a.mrl_widths.clone()).collect();
    widths.sort_unstable();
    widths.dedup();
    let mut rows = Vec::new();
    for width in widths {
        let key = width.to_string();
        let values: BTreeMap<String, f64> = mrl
            .iter()
            .filter_map(|(model, by_width)| by_width.get(&key).map(|v| (model.clone(), *v)))
            .collect();
        if values.is_empty() {
            continue;
        }
        rows.push(EmbeddingRow {
            family: format!("{width} dims"),
            metric: M_RECALL_AGAINST_FULL.to_string(),
            higher_is_better: true,
            role: Role::Diagnostic,
            values,
            series: BTreeMap::new(),
        });
    }
    Lane {
        name: "Matryoshka".to_string(),
        rationale:
            "Each model's narrowed ranking against its own full-width exact ranking, so the \
             storage saving is priced per model instead of assumed from a model card. A model \
             with no Matryoshka training appears only at its full width, which is the honest \
             way to show that it has none."
                .to_string(),
        rows,
    }
}

/// The value at a percentile of an already sorted series.
///
/// Nearest-rank rather than interpolated, so the threshold is always a score
/// some calibration query actually produced.
/// @param sorted - the series, ascending
/// @param p - the percentile, from zero to one
pub(super) fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let last = sorted.len().saturating_sub(1);
    let rank = (last as f64 * p).round() as usize;
    sorted.get(rank.min(last)).copied().unwrap_or(0.0)
}

/// Gate G4: how often each model answers confidently when nothing answers.
///
/// Each model is judged against its own threshold, because a cosine from one
/// model and a cosine from another are not the same number - a model whose
/// vectors sit in a narrower cone scores every pair higher, and a shared
/// threshold would grade that geometry rather than the behaviour. The threshold
/// is the fifth percentile of the model's own top-result confidence over
/// answerable calibration queries this lane never scores.
/// @param confidences - model to family to per-query top confidence
pub(super) fn abstention_lane(confidences: &BTreeMap<String, BTreeMap<String, Vec<f64>>>) -> Lane {
    let mut rates = BTreeMap::new();
    let mut series = BTreeMap::new();
    let mut thresholds = BTreeMap::new();
    let mut gaps = BTreeMap::new();

    for (model, by_family) in confidences {
        let (Some(calibration), Some(negative)) =
            (by_family.get(CALIBRATION), by_family.get(UNANSWERABLE))
        else {
            continue;
        };
        if calibration.is_empty() || negative.is_empty() {
            continue;
        }
        let mut sorted = calibration.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let threshold = percentile(&sorted, 0.05);
        // One value per query, so this row can be tested pairwise like every
        // other primary row rather than compared as two summary numbers.
        let flags: Vec<f64> = negative
            .iter()
            .map(|s| if *s >= threshold { 1.0 } else { 0.0 })
            .collect();
        let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len().max(1) as f64;
        rates.insert(model.clone(), mean(&flags));
        series.insert(model.clone(), flags);
        thresholds.insert(model.clone(), threshold);
        gaps.insert(model.clone(), mean(calibration) - mean(negative));
    }

    let rows = vec![
        EmbeddingRow {
            family: "questions with no answer in the corpus".to_string(),
            metric: "confident answer rate at the model's own threshold".to_string(),
            higher_is_better: false,
            role: Role::Primary,
            values: rates,
            series,
        },
        EmbeddingRow {
            family: "calibration queries, 5th percentile of the top result".to_string(),
            metric: "the model's own abstention threshold".to_string(),
            higher_is_better: true,
            role: Role::Diagnostic,
            values: thresholds,
            series: BTreeMap::new(),
        },
        EmbeddingRow {
            family: "answerable minus unanswerable".to_string(),
            metric: "mean top result confidence gap".to_string(),
            higher_is_better: true,
            role: Role::Diagnostic,
            values: gaps,
            series: BTreeMap::new(),
        },
    ];

    Lane {
        name: "abstention".to_string(),
        rationale: "Gate G4. Queries built by mixing the distinctive words of two documents \
             from two sources the corpus builder draws from disjoint pools, so no chunk \
             holds material from both and the question sounds entirely plausible with no \
             answer. This is the failure that does not announce itself: ten confident \
             looking passages about nothing. Each model is calibrated on its own scale - \
             the threshold is the fifth percentile of its own top result confidence over \
             answerable queries this lane never scores - so the comparison needs no \
             assumption that two models' cosines mean the same thing. Lower is better."
            .to_string(),
        rows,
    }
}

/// The column name the lexical lane prints under. Not a model id, because
/// there is no model: the ranking is BM25 over the corpus text.
pub(super) const LEXICAL_COLUMN: &str = "BM25 alone, no model";

/// The same query families through BM25 with no embedding model.
///
/// One column, because the ranking depends only on the corpus text and the
/// query. Every row is diagnostic: there is no baseline model in this lane to
/// compare against, so nothing here is judged. What it answers is a question
/// the other lanes cannot: how much of what the shipped pipeline finds would
/// be found with no embedding model at all.
/// @param lexical - family to metric to the per-query scores BM25 produced
pub(super) fn lexical_lane(lexical: &BTreeMap<String, BTreeMap<String, Vec<f64>>>) -> Lane {
    let mut rows = Vec::new();
    let row = |family: String, values: &[f64]| EmbeddingRow {
        family,
        metric: M_NDCG.to_string(),
        higher_is_better: true,
        role: Role::Diagnostic,
        values: [(
            LEXICAL_COLUMN.to_string(),
            values.iter().sum::<f64>() / values.len().max(1) as f64,
        )]
        .into_iter()
        .collect(),
        series: BTreeMap::new(),
    };
    let per_family: BTreeMap<String, Vec<f64>> = lexical
        .iter()
        .filter_map(|(f, m)| m.get(M_NDCG).map(|v| (f.clone(), v.clone())))
        .collect();
    // Both composites the other lanes print, so a reader comparing this lane
    // against the hybrid lane is comparing the same quantity.
    for (label, families) in [
        (
            format!("composite, declared ({})", HEADLINE.join(" + ")),
            HEADLINE,
        ),
        (
            format!("composite, promoted ({} families)", PROMOTED.len()),
            PROMOTED,
        ),
    ] {
        let values = composite(&per_family, families);
        if !values.is_empty() {
            rows.push(row(label, &values));
        }
    }
    for (family, values) in &per_family {
        rows.push(row(family.clone(), values));
    }
    Lane {
        name: "lexical only".to_string(),
        rationale: "The same families through BM25 with no embedding model, over the index the \
             hybrid lane builds and with every ranking setting left as the shipped \
             defaults. One column, because the ranking reads the corpus text and the \
             query and nothing else, so each arm would produce the same numbers. Every \
             row is diagnostic: there is no model in this lane, so there is no baseline \
             to judge against. Read it against the hybrid lane. The difference between \
             the two is what the embedding model adds to the pipeline Inillucent ships, \
             and it is the only place on this card that quantity appears."
            .to_string(),
        rows,
    }
}

pub(super) fn cost_lane(arms: &[ArmFacts]) -> Lane {
    let mut rows = Vec::new();
    let mut devices: Vec<String> = arms
        .iter()
        .flat_map(|a| a.chunks_per_second.keys().cloned())
        .collect();
    devices.sort();
    devices.dedup();
    for device in devices {
        rows.push(EmbeddingRow {
            family: format!("throughput on {device}"),
            metric: "chunks per second, median".to_string(),
            higher_is_better: true,
            role: Role::Diagnostic,
            values: arms
                .iter()
                .filter_map(|a| {
                    a.chunks_per_second
                        .get(&device)
                        .map(|v| (a.model_id.clone(), *v))
                })
                .collect(),
            series: BTreeMap::new(),
        });
        // The spread beside the median, because a reader cannot tell whether a
        // gap between two arms is real without knowing how far apart two runs of
        // one arm land. Printed as a share of the median so arms of very
        // different speeds can be compared on it.
        let spreads: BTreeMap<String, f64> = arms
            .iter()
            .filter_map(|a| {
                let runs = a.chunks_per_second_runs.get(&device)?;
                if runs.len() < 2 {
                    return None;
                }
                let low = runs.iter().cloned().fold(f64::INFINITY, f64::min);
                let high = runs.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
                let middle = median(runs);
                Some((a.model_id.clone(), 100.0 * (high - low) / middle.max(1e-9)))
            })
            .collect();
        if !spreads.is_empty() {
            rows.push(EmbeddingRow {
                family: format!("throughput on {device}"),
                metric: "spread across repeats, % of median".to_string(),
                higher_is_better: false,
                role: Role::Diagnostic,
                values: spreads,
                series: BTreeMap::new(),
            });
        }
    }
    for (label, metric, higher, pick) in [
        (
            "weights on disk",
            "megabytes",
            false,
            Box::new(|a: &ArmFacts| a.model_bytes as f64 / 1e6) as Box<dyn Fn(&ArmFacts) -> f64>,
        ),
        (
            "tokens per chunk",
            "tokens",
            false,
            Box::new(|a: &ArmFacts| a.tokens_per_chunk.unwrap_or(0.0)),
        ),
        (
            "corpus truncated at the model's bound",
            "share of chunks",
            false,
            Box::new(|a: &ArmFacts| a.truncation_share),
        ),
        (
            "bytes per vector at full width",
            "bytes",
            false,
            Box::new(|a: &ArmFacts| (a.dims * 4) as f64),
        ),
    ] {
        rows.push(EmbeddingRow {
            family: label.to_string(),
            metric: metric.to_string(),
            higher_is_better: higher,
            role: Role::Diagnostic,
            values: arms.iter().map(|a| (a.model_id.clone(), pick(a))).collect(),
            series: BTreeMap::new(),
        });
    }
    Lane {
        name: "cost".to_string(),
        rationale:
            "Throughput is timed on distinct stride-sampled chunks, never on repeated text: a \
             repeated-input benchmark on this machine reported 300 chunks a second against a \
             real 85 to 134, because it was measuring a prefix cache. Truncation share is \
             printed beside throughput because a model that is fast for having read less of \
             each chunk is not fast."
                .to_string(),
        rows,
    }
}

/// Re-score a card that is already on disk, from the per-query series inside it.
///
/// A judging rule can change after a card is made. This ticket changed one: the
/// promoted six family composite was diagnostic, so it had values and a series but no
/// verdict, and the gates are declared on it. Without this, correcting that would mean
/// re-embedding five models over 185,078 chunks - about two hours of card - to recover
/// a number the card already contains everything needed to compute.
///
/// It recomputes rather than patches: every judgement on the card is discarded and the
/// whole set is produced again by the same `judge` a fresh run uses, from the same
/// series, with the card's own recorded stats seed. So a rejudged card and a rerun card
/// agree by construction rather than by inspection, and there is no second
/// implementation of the statistics to drift.
///
/// What it cannot do is change a number that came from a model. The lanes, the values
/// and the series are read and never touched; only the verdicts are rewritten.
/// @param path - the card's JSON
pub fn rejudge(path: &Path) -> Result<(EmbeddingCard, usize, usize)> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut card: EmbeddingCard =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    let before = card.judgements.len();
    let promoted = reapply_roles(&mut card);
    if promoted > 0 {
        eprintln!("  {promoted} row(s) promoted to primary by this binary's role rule");
    }
    card.judgements = judge(&card.lanes, &card.baseline, card.stats_seed);
    let after = card.judgements.len();
    Ok((card, before, after))
}

/// Re-take this binary's role decision on a card an earlier binary wrote.
///
/// A row's role is a decision about what the card is evidence for, not a measurement, so
/// re-scoring a card means re-taking that decision. Without this, `rejudge` re-runs the
/// statistics over the roles stored in the file and cannot fix the thing it exists to fix: a
/// promoted composite that was diagnostic when the card was written stays unjudged, and the only
/// way to get a verdict on it is to embed five corpora again - which is nine hours for a number
/// already in the file.
///
/// Only the promoted composite is re-taken, because it is the only row whose role this ticket
/// changed. A card whose rows were measured differently is a different card and is not repaired
/// here; the digests and the seed on it are what say whether two cards are comparable.
///
/// @param card - the card, changed in place
fn reapply_roles(card: &mut EmbeddingCard) -> usize {
    let promoted = format!("composite, promoted ({} families)", PROMOTED.len());
    let mut changed = 0usize;
    for lane in &mut card.lanes {
        for row in &mut lane.rows {
            if row.family == promoted && row.role != Role::Primary {
                row.role = Role::Primary;
                changed = changed.saturating_add(1);
            }
        }
    }
    changed
}

/// Compare every candidate against the baseline on every primary row.
pub(super) fn judge(lanes: &[Lane], baseline: &str, seed: u64) -> Vec<ArmJudgement> {
    let mut out = Vec::new();
    for lane in lanes {
        for row in &lane.rows {
            if row.role != Role::Primary {
                continue;
            }
            let Some(base_series) = row.series.get(baseline) else {
                continue;
            };
            let base_value = row.values.get(baseline).copied().unwrap_or(0.0);
            // Oriented so that a positive delta always means the candidate is
            // better. `stats::verdict` reads the interval's sign and has no idea
            // which way a metric runs; the abstention lane is the first primary
            // row where lower is better, and without this its verdicts would come
            // out exactly backwards. `candidate_value` and `baseline_value` stay
            // in the row's own units.
            let orient = |v: &Vec<f64>| -> Vec<f64> {
                if row.higher_is_better {
                    v.clone()
                } else {
                    v.iter().map(|x| -x).collect()
                }
            };
            let base_oriented = orient(base_series);
            for (model, series) in &row.series {
                if model == baseline {
                    continue;
                }
                let paired = stats::compare(&orient(series), &base_oriented, seed);
                let verdict = paired
                    .as_ref()
                    .map(|p| stats::verdict(p, RANKING_THRESHOLD))
                    .unwrap_or(Verdict::Inconclusive);
                out.push(ArmJudgement {
                    lane: lane.name.clone(),
                    family: row.family.clone(),
                    metric: row.metric.clone(),
                    model: model.clone(),
                    baseline: baseline.to_string(),
                    candidate_value: row.values.get(model).copied().unwrap_or(0.0),
                    baseline_value: base_value,
                    verdict,
                    threshold: RANKING_THRESHOLD,
                    paired,
                });
            }
        }
    }
    out
}

pub(super) fn caveats() -> Vec<String> {
    vec![
        "Every arm embedded the same corpus from scratch with its own model, its own prefixes, \
         its own pooling and its own token bound. No cache is reused between arms, and the \
         header of each one names the corpus digest, the model and the manifest digest it was \
         made with; a run refuses before it starts if any two of those disagree."
            .to_string(),
        "The queries are generated once from the shared corpus and are byte-identical across \
         arms. Each arm embeds them with its own model, which is the only honest way to ask two \
         models the same question."
            .to_string(),
        "The dense lane applies no per-document cap and computes its ideal ranking the same way \
         for every arm, so its absolute values sit below the hybrid lane's and its comparisons \
         are exact."
            .to_string(),
        "Throughput is measured on distinct chunks. On this machine a repeated-input embedding \
         benchmark reports roughly three times the real rate, because shared prefixes collapse \
         in the cache."
            .to_string(),
        "A model that wins on this suite and loses on a public retrieval benchmark has \
         overfitted to this corpus. This card cannot see that; it is the reason gate G5 exists \
         outside it."
            .to_string(),
    ]
}

/// Renders the head-to-head card as markdown.
///
/// Every comparison on it carries an interval and a test against a threshold
/// declared before the run, because a difference between two models over one
/// query set is a sample rather than a fact.
///
/// @param card - the finished card
pub fn render(card: &EmbeddingCard) -> String {
    let mut s = String::new();
    s.push_str("# Embedding model head-to-head\n\n");
    s.push_str(&format!(
        "Run `{}`, corpus `{}` ({} chunks across {} documents), baseline `{}`. Every comparison \
         is a paired bootstrap 95% interval plus a paired randomization test over per-query \
         scores, against a practical threshold of {} declared before the run.\n\n",
        card.run_id,
        short(&card.corpus_sha256),
        card.corpus_chunks,
        card.corpus_documents,
        card.baseline,
        card.ranking_threshold
    ));

    s.push_str("## The arms\n\n");
    s.push_str("| model | dims | max tokens | pooling | query prefix | truncated | weights MB | manifest |\n");
    s.push_str("|---|---:|---:|---|---|---:|---:|---|\n");
    for a in &card.arms {
        s.push_str(&format!(
            "| `{}` | {} | {} | {} | `{}` | {:.2}% | {:.0} | `{}`{} |\n",
            a.model_id,
            a.dims,
            a.max_tokens,
            a.pooling,
            a.query_prefix.replace('|', "\\|"),
            100.0 * a.truncation_share,
            a.model_bytes as f64 / 1e6,
            short(&a.manifest_sha256),
            if a.manifest_on_disk { "" } else { " (assumed)" }
        ));
    }
    s.push('\n');

    s.push_str("## Verdicts on the primary rows\n\n");
    if card.judgements.is_empty() {
        s.push_str(
            "No primary row carried per-query scores for both a candidate and the baseline.\n\n",
        );
    } else {
        s.push_str(
            "| lane | family | model | value | baseline | delta | 95% interval | p | verdict |\n",
        );
        s.push_str("|---|---|---|---:|---:|---:|---|---:|---|\n");
        for j in &card.judgements {
            let (delta, interval, p) = match &j.paired {
                Some(p) => (
                    format!("{:+.4}", p.delta),
                    format!("[{:+.4}, {:+.4}]", p.low, p.high),
                    format!("{:.4}", p.p_value),
                ),
                None => ("n/a".into(), "n/a".into(), "n/a".into()),
            };
            s.push_str(&format!(
                "| {} | {} | `{}` | {:.4} | {:.4} | {delta} | {interval} | {p} | {} |\n",
                j.lane,
                j.family,
                j.model,
                j.candidate_value,
                j.baseline_value,
                j.verdict.label()
            ));
        }
        s.push('\n');
    }

    for lane in &card.lanes {
        s.push_str(&format!("## {}\n\n{}\n\n", lane.name, lane.rationale));
        let mut models: Vec<&String> = lane.rows.iter().flat_map(|r| r.values.keys()).collect();
        models.sort();
        models.dedup();
        s.push_str("| family | metric | ");
        s.push_str(
            &models
                .iter()
                .map(|m| format!("`{m}`"))
                .collect::<Vec<_>>()
                .join(" | "),
        );
        s.push_str(" |\n|---|---|");
        s.push_str(&"---:|".repeat(models.len()));
        s.push('\n');
        for row in &lane.rows {
            s.push_str(&format!(
                "| {}{} | {} |",
                row.family,
                if row.role == Role::Primary {
                    " **(primary)**"
                } else {
                    ""
                },
                row.metric
            ));
            for m in &models {
                match row.values.get(*m) {
                    Some(v) => s.push_str(&format!(" {} |", fmt(*v))),
                    None => s.push_str(" - |"),
                }
            }
            s.push('\n');
        }
        s.push('\n');
    }

    s.push_str("## Query families\n\n| family | queries |\n|---|---:|\n");
    for (name, n) in &card.query_counts {
        s.push_str(&format!("| {name} | {n} |\n"));
    }
    s.push('\n');

    s.push_str("## Caveats\n\n");
    for c in &card.caveats {
        s.push_str(&format!("- {c}\n"));
    }
    s.push('\n');

    s.push_str("## Provenance\n\n| | |\n|---|---|\n");
    for (k, v) in &card.provenance {
        s.push_str(&format!("| {k} | {v} |\n"));
    }
    s.push('\n');
    s
}

fn fmt(v: f64) -> String {
    if v >= 1000.0 {
        format!("{v:.0}")
    } else if v >= 1.0 {
        format!("{v:.2}")
    } else {
        format!("{v:.4}")
    }
}

/// Prints the composite verdicts to standard error.
///
/// Only the composite lanes, because those are the rows the run was for; the
/// per-family detail is in the rendered card.
///
/// @param card - the finished card
pub fn print_summary(card: &EmbeddingCard) {
    eprintln!("\nbaseline: {}", card.baseline);
    for j in &card.judgements {
        if j.lane.contains("composite") {
            eprintln!(
                "  {} / {}: {} {:.4} vs {:.4} ({:+.4}) -> {}",
                j.lane,
                j.family,
                j.model,
                j.candidate_value,
                j.baseline_value,
                j.candidate_value - j.baseline_value,
                j.verdict.label()
            );
        }
    }
}
