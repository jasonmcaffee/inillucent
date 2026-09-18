//! The score card structures and their rendering.
//!
//! The verdict this card reaches used to be a count of measurements won. That
//! shape had three defects and all three flattered the result. It called any
//! difference above `1e-4` a win, which is a hundredth of what one query changing
//! its mind moves a mean by on ninety queries. It gave every row a vote, so nDCG,
//! success@1, success@10 and reciprocal rank turned one behaviour into four wins.
//! And it scored "rows returned" as higher-is-better, so returning fifty
//! irrelevant chunks beat returning ten useful ones.
//!
//! What replaced it: each family declares one **primary** metric and the rest are
//! **diagnostics** that are reported and never voted on; a primary comparison is
//! decided by a paired bootstrap interval and a paired randomization test over the
//! per-query scores, against a practical threshold declared before the run; and
//! completeness is a gate rather than a score.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::stats::{self, Paired, Verdict};

/// One measured number for one engine.
#[derive(Serialize, Clone)]
pub struct Measure {
    pub engine: String,
    pub value: f64,
}

/// What a row is for.
///
/// The distinction the old card lacked. A diagnostic is measured, printed and
/// argued about; it is not evidence for or against shipping, because several
/// diagnostics move together whenever one behaviour changes and counting each of
/// them separately turns one result into several.
#[derive(Serialize, serde::Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    /// The one measurement this family is decided by.
    Primary,
    /// Reported for understanding, never voted on.
    Diagnostic,
}

/// One engine's per-query scores for a row, which is what a paired test needs.
///
/// A mean cannot be compared honestly to another mean without knowing how the two
/// varied across the same queries. These are also written into the run's
/// per-query file, so a comparison can be recomputed without rerunning retrieval.
#[derive(Serialize, Clone)]
pub struct Series {
    pub engine: String,
    pub values: Vec<f64>,
}

/// One row of a scenario: a metric measured across engines.
#[derive(Serialize, Clone)]
pub struct MetricRow {
    pub label: String,
    pub metric: String,
    pub measures: Vec<Measure>,
    /// Higher is better for accuracy, lower is better for latency.
    pub higher_is_better: bool,
    pub role: Role,
    /// Per-query scores per engine. Empty for a row that is not a per-query
    /// measurement at all, such as a build statistic, in which case the row can
    /// still be reported but cannot be judged.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub series: Vec<Series>,
}

impl MetricRow {
    /// A row that is reported but never voted on.
    /// @param label - what was measured
    /// @param metric - the measure's name
    /// @param measures - one value per engine or setting
    /// @param higher_is_better - the direction
    pub fn diagnostic(
        label: String,
        metric: &str,
        measures: Vec<Measure>,
        higher_is_better: bool,
    ) -> MetricRow {
        MetricRow {
            label,
            metric: metric.to_string(),
            measures,
            higher_is_better,
            role: Role::Diagnostic,
            series: Vec::new(),
        }
    }

    /// The one row a family is decided by, with the per-query scores behind it.
    /// @param label - what was measured
    /// @param metric - the measure's name
    /// @param measures - one value per engine
    /// @param higher_is_better - the direction
    /// @param series - per-query scores per engine
    pub fn primary(
        label: String,
        metric: &str,
        measures: Vec<Measure>,
        higher_is_better: bool,
        series: Vec<Series>,
    ) -> MetricRow {
        MetricRow {
            label,
            metric: metric.to_string(),
            measures,
            higher_is_better,
            role: Role::Primary,
            series,
        }
    }

    /// The per-query scores for one engine, if this row carries them.
    fn values_for(&self, engine: &str) -> Option<&[f64]> {
        self.series
            .iter()
            .find(|s| s.engine == engine)
            .map(|s| s.values.as_slice())
    }
}

/// The smallest difference in a metric that is worth acting on.
///
/// Declared here, before any run, because a threshold chosen after seeing the
/// numbers is not a threshold. With a large enough query set every difference
/// eventually becomes statistically detectable, including differences far below
/// what any user could notice, and without this the card would drift back into
/// calling noise a win by a more sophisticated route.
///
/// A hundredth of a point on the ranking measures: about one query in a hundred
/// moving from a miss to a hit, which is the smallest change that would show up in
/// a product at all. Latency is relative, because a tenth of a millisecond means
/// something different at 0.5 ms than at 50 ms.
/// @param metric - the measure's name
/// @param baseline - the value being compared against, for the relative case
pub fn practical_threshold(metric: &str, baseline: f64) -> f64 {
    if metric.contains("ms") {
        // Five per cent of the baseline, with a floor so a sub-millisecond
        // measurement is not decided by scheduler noise.
        (baseline.abs() * 0.05).max(0.05)
    } else if metric.contains("rows") || metric.contains("count") {
        0.5
    } else {
        0.01
    }
}

/// Whether a row is a comparison between the engines at all. A row whose columns
/// are inillucent configurations, such as the quantization ladder or the `ef_search`
/// sweep, must not count towards the verdict: inillucent would be compared against
/// itself and every family would report a win.
fn is_engine_comparison(row: &MetricRow, engines: &[String]) -> bool {
    let named: usize = row
        .measures
        .iter()
        .filter(|m| engines.contains(&m.engine))
        .count();
    named >= 2
}

#[derive(Serialize, Clone)]
pub struct Judgement {
    pub scenario: String,
    pub metric: String,
    pub label: String,
    pub inillucent: f64,
    pub best_baseline: f64,
    pub best_baseline_engine: String,
    pub verdict: Verdict,
    /// The threshold this verdict was decided against.
    pub threshold: f64,
    /// The paired comparison, when the row carried per-query scores. A row that
    /// does not is reported with its point estimate and no verdict stronger than
    /// inconclusive, because there is nothing to test.
    pub paired: Option<Paired>,
    /// Set when both engines are at the metric's ceiling, which is a different
    /// thing from being indistinguishable in the middle of the range: neither can
    /// do better, so there is nothing left to win.
    pub at_ceiling: bool,
}

#[derive(Serialize, Clone)]
pub struct Scenario {
    pub name: String,
    /// What this family measures and why, in prose, for the score card.
    pub rationale: String,
    pub rows: Vec<MetricRow>,
    /// Set for correctness families, which gate rather than score.
    pub gate: Option<GateResult>,
}

#[derive(Serialize, Clone)]
pub struct GateResult {
    pub passed: bool,
    pub detail: String,
}

#[derive(Serialize, Clone)]
pub struct BuildFacts {
    pub engine: String,
    pub chunks: usize,
    pub documents: usize,
    pub build_seconds: f64,
    pub graph_layers: usize,
    pub graph_edges: usize,
    pub lexical_terms: usize,
    pub lexical_postings: usize,
    pub vector_megabytes: f64,
    pub quantized_megabytes: f64,
}

#[derive(Serialize)]
pub struct ScoreCard {
    pub corpus_chunks: usize,
    pub corpus_documents: usize,
    pub dimensions: usize,
    /// Which embedding model produced the vectors this card was graded on. It
    /// used to be a string literal in this file, which was true of every run
    /// until the day it was not.
    pub model_id: String,
    pub engines: Vec<String>,
    pub scenarios: Vec<Scenario>,
    pub build: Vec<BuildFacts>,
    pub caveats: Vec<String>,
    pub generated_at: String,
    /// Fixes every bootstrap interval and p-value on the card, so two readings of
    /// the same run reach the same verdict.
    pub stats_seed: u64,
    /// The run this card came from: commit, corpus, model, seeds, settings,
    /// hardware and the per-query file. A number without this is not reproducible.
    pub provenance: BTreeMap<String, String>,
    /// How many queries each family rests on.
    pub query_counts: BTreeMap<String, usize>,
}

fn fmt(v: f64) -> String {
    if v >= 1000.0 {
        format!("{v:.0}")
    } else if v >= 1.0 {
        format!("{v:.3}")
    } else {
        format!("{v:.4}")
    }
}

/// Compare inillucent against the better of the two pgvector configurations on every
/// primary row that is genuinely a comparison between engines.
///
/// The pass condition is parity or better across all families, so the baseline is
/// deliberately the *best* pgvector column rather than the production one.
/// Comparing against the misconfiguration would be easy and would prove nothing.
///
/// Only primary rows are judged. The diagnostics are still rendered; they are just
/// not evidence, because four correlated measures of one behaviour are one result
/// and not four.
/// @param card - the measurements
/// @param seed - fixes the resampling behind every interval and p-value
pub fn judge(card: &ScoreCard) -> Vec<Judgement> {
    let mut out = Vec::new();

    for sc in &card.scenarios {
        for row in &sc.rows {
            if row.role != Role::Primary {
                continue;
            }
            if !is_engine_comparison(row, &card.engines) {
                continue;
            }
            let Some(inillucent) = row.measures.iter().find(|m| m.engine == "inillucent") else {
                continue;
            };
            let baselines: Vec<&Measure> = row
                .measures
                .iter()
                .filter(|m| m.engine != "inillucent" && card.engines.contains(&m.engine))
                .collect();
            // `reduce` answers `None` for an empty iterator and nothing
            // else, so this is the same guard the explicit `is_empty` check
            // used to be, written as the one the compiler can see.
            let Some(best) = baselines.iter().copied().reduce(|a, b| {
                let a_better = if row.higher_is_better {
                    a.value >= b.value
                } else {
                    a.value <= b.value
                };
                if a_better {
                    a
                } else {
                    b
                }
            }) else {
                continue;
            };

            let threshold = practical_threshold(&row.metric, best.value);
            // Both series oriented so that higher is better, whatever the metric's
            // own direction, because the statistics only understand one direction.
            let orient = |v: &[f64]| -> Vec<f64> {
                if row.higher_is_better {
                    v.to_vec()
                } else {
                    v.iter().map(|x| -x).collect()
                }
            };
            let paired = match (row.values_for("inillucent"), row.values_for(&best.engine)) {
                (Some(a), Some(b)) => stats::compare(&orient(a), &orient(b), card.stats_seed),
                _ => None,
            };

            // A metric both engines have maxed out is not a tie in the ordinary
            // sense. Recall 1.000 against recall 1.000 is two engines that have
            // both found everything there was, and calling it inconclusive would
            // suggest a longer run could separate them.
            let at_ceiling = row.higher_is_better
                && (inillucent.value - 1.0).abs() < 1e-9
                && (best.value - 1.0).abs() < 1e-9;

            let verdict = match (&paired, at_ceiling) {
                (_, true) => Verdict::Equivalent,
                (Some(p), _) => stats::verdict(p, threshold),
                (None, _) => {
                    // No per-query scores, so no test. Fall back to the point
                    // estimate against the same threshold, and never claim more
                    // than the threshold supports.
                    let delta = if row.higher_is_better {
                        inillucent.value - best.value
                    } else {
                        best.value - inillucent.value
                    };
                    if delta > threshold {
                        Verdict::Better
                    } else if delta < -threshold {
                        Verdict::Worse
                    } else {
                        Verdict::Equivalent
                    }
                }
            };

            out.push(Judgement {
                scenario: sc.name.clone(),
                metric: row.metric.clone(),
                label: row.label.clone(),
                inillucent: inillucent.value,
                best_baseline: best.value,
                best_baseline_engine: best.engine.clone(),
                verdict,
                threshold,
                paired,
                at_ceiling,
            });
        }
    }
    out
}

/// The column headings for one scenario: the engines the card declares, kept in
/// the card's order and only when this family measured them, followed by any other
/// label its rows carry. A family whose rows name settings rather than engines gets
/// its settings as columns.
/// @param scenario - the family being rendered
/// @param engines - the engines the whole card compares
fn scenario_columns(scenario: &Scenario, engines: &[String]) -> Vec<String> {
    let mut columns: Vec<String> = Vec::new();
    for e in engines {
        if scenario
            .rows
            .iter()
            .any(|r| r.measures.iter().any(|m| &m.engine == e))
        {
            columns.push(e.clone());
        }
    }
    for row in &scenario.rows {
        for m in &row.measures {
            if !columns.contains(&m.engine) {
                columns.push(m.engine.clone());
            }
        }
    }
    columns
}

/// Renders the score card as markdown.
///
/// **One function per section, in the order a reader meets them.** The card is
/// a sequence of independent tables, and the thing that used to make this hard
/// to change was that all of them shared one `String` and one 260-line
/// function - so a change to the verdict paragraph was a change inside the
/// same body as the build-cost table. Each helper below appends its own
/// section and reads nothing the others wrote.
///
/// @param card - the finished score card
pub fn render(card: &ScoreCard) -> String {
    let judgements = judge(card);
    let mut s = String::new();
    push_header(&mut s, card);
    push_verdict(&mut s, card, &judgements);
    push_every_comparison(&mut s, &judgements);
    push_query_counts(&mut s, card);
    push_gates(&mut s, card);
    push_families(&mut s, card);
    push_build_cost(&mut s, card);
    push_provenance(&mut s, card);
    push_caveats(&mut s, card);
    s
}

/// What the run was and what makes the comparison fair.
///
/// @param s - the card being built
/// @param card - the finished score card
fn push_header(s: &mut String, card: &ScoreCard) {
    s.push_str("# inillucent Score Card\n\n");
    s.push_str(&format!(
        "Generated {}. Corpus: {} chunks across {} documents, {} dimensional embeddings from `{}` run in process at full precision.\n\n",
        card.generated_at,
        card.corpus_chunks,
        card.corpus_documents,
        card.dimensions,
        card.model_id
    ));

    s.push_str("The corpus is assembled from public data by this repository and embedded once. The identical vectors are written to the cache inillucent reads and to the PostgreSQL column pgvector reads, and every query is embedded once and handed to both engines, so the embedding model cancels out of the comparison entirely. A score difference is therefore attributable to indexing and ranking.\n\n");

    s.push_str("Engines graded:\n\n");
    for e in &card.engines {
        s.push_str(&format!("- {e}\n"));
    }
    s.push('\n');
}

/// The answer before the evidence: the verdict counts, and every measurement
/// that lost.
///
/// **The losses are a table rather than a sentence, and they are here rather
/// than at the end.** A score card that cannot report a loss is not measuring
/// anything, and one that reports it after nine tables is reporting it where
/// nobody reads.
///
/// @param s - the card being built
/// @param card - the finished score card
/// @param judgements - the primary comparisons, already judged
fn push_verdict(s: &mut String, card: &ScoreCard, judgements: &[Judgement]) {
    let gates_pass = card
        .scenarios
        .iter()
        .filter_map(|sc| sc.gate.as_ref())
        .all(|g| g.passed);
    let count = |v: Verdict| judgements.iter().filter(|j| j.verdict == v).count();
    let better = count(Verdict::Better);
    let equivalent = count(Verdict::Equivalent);
    let inconclusive = count(Verdict::Inconclusive);
    let worse: Vec<&Judgement> = judgements
        .iter()
        .filter(|j| j.verdict == Verdict::Worse)
        .collect();

    s.push_str("## Verdict\n\n");
    s.push_str("Every family below declares **one** primary measurement, and only those are judged. The rest are diagnostics: they are measured and printed, and they do not vote, because nDCG, success@1, success@10 and reciprocal rank all move together when one behaviour changes and counting each of them separately turns one result into four.\n\n");
    s.push_str("Each primary comparison is against the better of the two pgvector configurations, never the production one, because beating a misconfiguration proves nothing. It is decided by a **paired bootstrap interval** and a **paired randomization test** over the per-query scores, against a practical threshold declared before the run: 0.01 on the ranking measures, five per cent on latency. A difference is called *better* only when the 95% interval clears both zero and that threshold, *equivalent* only when the whole interval sits inside it, and *inconclusive* otherwise. A run that cannot separate two engines says so.\n\n");
    s.push_str(&format!(
        "**{} primary comparisons: {} better, {} equivalent, {} inconclusive, {} worse. Correctness gates: {}.**\n\n",
        judgements.len(),
        better,
        equivalent,
        inconclusive,
        worse.len(),
        if gates_pass { "all pass" } else { "**A GATE FAILED**" }
    ));

    if worse.is_empty() {
        s.push_str("No primary measurement was worse than the best the configured PostgreSQL baseline can do.\n\n");
    } else {
        s.push_str("Measurements where inillucent is worse than the best pgvector configuration, stated because a score card that cannot report a loss is not measuring anything:\n\n");
        s.push_str("| scenario | measurement | metric | inillucent | best baseline | which baseline |\n|---|---|---|---|---|---|\n");
        for j in &worse {
            s.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} |\n",
                j.scenario,
                j.label,
                j.metric,
                fmt(j.inillucent),
                fmt(j.best_baseline),
                j.best_baseline_engine
            ));
        }
        s.push('\n');
    }
}

/// Every primary comparison with its interval, its p-value and how many
/// queries it rests on.
///
/// @param s - the card being built
/// @param judgements - the primary comparisons, already judged
fn push_every_comparison(s: &mut String, judgements: &[Judgement]) {
    // The evidence behind the headline, one line per primary comparison.
    if !judgements.is_empty() {
        s.push_str("### Every primary comparison, with its uncertainty\n\n");
        s.push_str("`delta` is inillucent minus the baseline, oriented so positive is better whatever the metric's own direction. The interval is the 95% paired bootstrap on that delta; `p` is the paired randomization test. `n` is the queries behind it and `moved` is how many of them the two engines answered differently — a comparison resting on three queries is worth reading with suspicion however small its p-value.\n\n");
        s.push_str("| family | measurement | metric | inillucent | baseline | delta | 95% interval | p | n | moved | threshold | verdict |\n");
        s.push_str("|---|---|---|---|---|---|---|---|---|---|---|---|\n");
        for j in judgements {
            let (delta, interval, pv, n, moved) = match &j.paired {
                Some(p) => (
                    fmt(p.delta),
                    format!("{} to {}", fmt(p.low), fmt(p.high)),
                    format!("{:.4}", p.p_value),
                    p.queries.to_string(),
                    p.disagreements.to_string(),
                ),
                None => {
                    let d = if j.inillucent.is_finite() {
                        j.inillucent - j.best_baseline
                    } else {
                        0.0
                    };
                    (
                        fmt(d),
                        "not paired".to_string(),
                        "n/a".into(),
                        "n/a".into(),
                        "n/a".into(),
                    )
                }
            };
            let verdict = if j.at_ceiling {
                "equivalent, at the ceiling"
            } else {
                j.verdict.label()
            };
            s.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
                j.scenario,
                j.label,
                j.metric,
                fmt(j.inillucent),
                fmt(j.best_baseline),
                delta,
                interval,
                pv,
                n,
                moved,
                fmt(j.threshold),
                verdict
            ));
        }
        s.push('\n');
    }
}

/// How many queries each family was scored on.
///
/// @param s - the card being built
/// @param card - the finished score card
fn push_query_counts(s: &mut String, card: &ScoreCard) {
    if !card.query_counts.is_empty() {
        s.push_str("### Queries behind each family\n\n");
        s.push_str("| family | queries |\n|---|---|\n");
        for (family, n) in &card.query_counts {
            s.push_str(&format!("| {family} | {n} |\n"));
        }
        s.push('\n');
    }
}

/// The correctness gates, before any accuracy number.
///
/// A gate failure changes how every other number on the card should be read,
/// so it is printed before them rather than among them.
///
/// @param s - the card being built
/// @param card - the finished score card
fn push_gates(s: &mut String, card: &ScoreCard) {
    // Gates first: a correctness failure changes how every other number should be read.
    let gates: Vec<&Scenario> = card
        .scenarios
        .iter()
        .filter(|sc| sc.gate.is_some())
        .collect();
    if !gates.is_empty() {
        s.push_str("## Correctness gates\n\n");
        s.push_str("These pass or fail rather than scoring. An engine that returns rows it was told to exclude is not a faster engine, it is a wrong one, so a failure here caps the result regardless of any accuracy number.\n\n");
        s.push_str("| gate | result | detail |\n|---|---|---|\n");
        for sc in gates {
            // `gates` is filtered on `gate.is_some()`, so this is that filter
            // restated where the compiler can check it.
            let Some(g) = sc.gate.as_ref() else {
                continue;
            };
            s.push_str(&format!(
                "| {} | {} | {} |\n",
                sc.name,
                if g.passed { "pass" } else { "**FAIL**" },
                g.detail
            ));
        }
        s.push('\n');
    }
}

/// One table per scenario family, with whichever columns that family measured.
///
/// @param s - the card being built
/// @param card - the finished score card
fn push_families(s: &mut String, card: &ScoreCard) {
    for sc in &card.scenarios {
        if sc.gate.is_some() && sc.rows.is_empty() {
            continue;
        }
        s.push_str(&format!("## {}\n\n{}\n\n", sc.name, sc.rationale));
        if sc.rows.is_empty() {
            s.push_str("No measurements in this family.\n\n");
            continue;
        }
        // The columns are whatever this family measured, in the card's engine order
        // first and then anything else in the order it appears. Three families do not
        // compare engines at all: the ef_search sweep, the fusion comparison and the
        // quantization ladder all have inillucent settings in the engine slot. Rendering
        // those against the fixed engine list filled every cell with n/a, so three
        // tables the card spends a paragraph introducing said nothing at all.
        let columns = scenario_columns(sc, &card.engines);
        s.push_str("| measurement | metric |");
        for e in &columns {
            s.push_str(&format!(" {e} |"));
        }
        s.push_str("\n|---|---|");
        for _ in &columns {
            s.push_str("---|");
        }
        s.push('\n');

        for row in &sc.rows {
            s.push_str(&format!("| {} | {} |", row.label, row.metric));
            // Find the best value so it can be marked.
            let best = row
                .measures
                .iter()
                .map(|m| m.value)
                .fold(None::<f64>, |acc, v| {
                    Some(match acc {
                        None => v,
                        Some(a) => {
                            if row.higher_is_better {
                                a.max(v)
                            } else {
                                a.min(v)
                            }
                        }
                    })
                });
            for engine in &columns {
                match row.measures.iter().find(|m| &m.engine == engine) {
                    Some(m) => {
                        let is_best = best.map(|b| (b - m.value).abs() < 1e-9).unwrap_or(false);
                        if is_best && row.measures.len() > 1 {
                            s.push_str(&format!(" **{}** |", fmt(m.value)));
                        } else {
                            s.push_str(&format!(" {} |", fmt(m.value)));
                        }
                    }
                    None => s.push_str(" n/a |"),
                }
            }
            s.push('\n');
        }
        s.push('\n');
    }
}

/// What each engine paid to build its index, and what it occupies.
///
/// @param s - the card being built
/// @param card - the finished score card
fn push_build_cost(s: &mut String, card: &ScoreCard) {
    if !card.build.is_empty() {
        s.push_str("## Build cost and footprint\n\n");
        s.push_str("| engine | chunks | documents | build seconds | graph layers | graph edges | lexical terms | lexical postings | vectors MB | int8 codes MB |\n");
        s.push_str("|---|---|---|---|---|---|---|---|---|---|\n");
        for b in &card.build {
            s.push_str(&format!(
                "| {} | {} | {} | {:.1} | {} | {} | {} | {} | {:.1} | {:.1} |\n",
                b.engine,
                b.chunks,
                b.documents,
                b.build_seconds,
                b.graph_layers,
                b.graph_edges,
                b.lexical_terms,
                b.lexical_postings,
                b.vector_megabytes,
                b.quantized_megabytes
            ));
        }
        s.push('\n');
    }
}

/// What this run was, so a number on the card can be reproduced.
///
/// @param s - the card being built
/// @param card - the finished score card
fn push_provenance(s: &mut String, card: &ScoreCard) {
    if !card.provenance.is_empty() {
        s.push_str("## Provenance\n\n");
        s.push_str("What this run was, so a number on this card can be reproduced rather than only repeated. The per-query file named here holds one line per engine per query, with the ranking, the component scores and the metrics that query contributed, which is what makes the intervals above recomputable without paying for the run again.\n\n");
        s.push_str("| field | value |\n|---|---|\n");
        for (k, v) in &card.provenance {
            s.push_str(&format!("| {k} | {v} |\n"));
        }
        s.push('\n');
    }
}

/// What these numbers do not say.
///
/// @param s - the card being built
/// @param card - the finished score card
fn push_caveats(s: &mut String, card: &ScoreCard) {
    if !card.caveats.is_empty() {
        s.push_str("## What these numbers do not say\n\n");
        for c in &card.caveats {
            s.push_str(&format!("- {c}\n"));
        }
        s.push('\n');
    }
}

/// Prints the verdict counts to standard error.
///
/// **On standard error, so a run that is piping the markdown card to a file
/// still says what it decided.** The detail is in the card; this is the line
/// somebody watching the run reads.
///
/// @param card - the finished score card
pub fn print_summary(card: &ScoreCard) {
    let j = judge(card);
    let count = |v: Verdict| j.iter().filter(|x| x.verdict == v).count();
    eprintln!(
        "\n=== verdict: {} primary comparisons, {} better, {} equivalent, {} inconclusive, {} worse ===",
        j.len(),
        count(Verdict::Better),
        count(Verdict::Equivalent),
        count(Verdict::Inconclusive),
        count(Verdict::Worse)
    );
    for x in j.iter().filter(|x| x.verdict != Verdict::Better) {
        let interval = match &x.paired {
            Some(p) => format!(
                "delta {} [{} .. {}] p={:.4} n={}",
                fmt(p.delta),
                fmt(p.low),
                fmt(p.high),
                p.p_value,
                p.queries
            ),
            None => "not paired".to_string(),
        };
        eprintln!(
            "  {:<12} [{}] {} {} :: inillucent={} baseline={} ({}) {}",
            x.verdict.label(),
            x.scenario,
            x.label,
            x.metric,
            fmt(x.inillucent),
            fmt(x.best_baseline),
            x.best_baseline_engine,
            interval
        );
    }
    eprintln!("\n=== summary ===");
    for sc in &card.scenarios {
        if let Some(g) = &sc.gate {
            eprintln!("{}: {}", sc.name, if g.passed { "pass" } else { "FAIL" });
        }
        for row in &sc.rows {
            let parts: Vec<String> = row
                .measures
                .iter()
                .map(|m| format!("{}={}", m.engine, fmt(m.value)))
                .collect();
            eprintln!(
                "  [{}] {} {} :: {}",
                sc.name,
                row.label,
                row.metric,
                parts.join("  ")
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::PgMode;

    /// A card with one comparable row, and the per-query scores behind it.
    ///
    /// The scores matter: a row without them cannot be judged by a paired test at
    /// all, and half the behaviour being tested here is what the judgement does
    /// with the uncertainty.
    fn card_with(inillucent: &[f64], baseline: &[f64]) -> ScoreCard {
        let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
        ScoreCard {
            corpus_chunks: 100,
            corpus_documents: 10,
            dimensions: 768,
            model_id: "nomic-embed-text-v1.5".into(),
            engines: vec!["inillucent".into(), "pgvector".into()],
            scenarios: vec![Scenario {
                name: "Unfiltered vector accuracy".into(),
                rationale: "why".into(),
                rows: vec![MetricRow::primary(
                    "all sources".into(),
                    "recall@10",
                    vec![
                        Measure {
                            engine: "inillucent".into(),
                            value: mean(inillucent),
                        },
                        Measure {
                            engine: "pgvector".into(),
                            value: mean(baseline),
                        },
                    ],
                    true,
                    vec![
                        Series {
                            engine: "inillucent".into(),
                            values: inillucent.to_vec(),
                        },
                        Series {
                            engine: "pgvector".into(),
                            values: baseline.to_vec(),
                        },
                    ],
                )],
                gate: None,
            }],
            build: vec![],
            caveats: vec!["a caveat".into()],
            generated_at: "now".into(),
            stats_seed: 7,
            provenance: BTreeMap::new(),
            query_counts: BTreeMap::new(),
        }
    }

    /// The historical fixture: point estimates only, no per-query evidence.
    fn card() -> ScoreCard {
        let mut c = card_with(&[0.98; 20], &[0.91; 20]);
        c.scenarios[0].rows[0].series.clear();
        c
    }

    #[test]
    fn renders_a_table_with_one_column_per_engine() {
        let md = render(&card());
        assert!(md.contains("| measurement | metric | inillucent | pgvector |"));
        assert!(md.contains("recall@10"));
    }

    #[test]
    fn marks_the_better_value_in_bold() {
        let md = render(&card());
        assert!(
            md.contains("**0.9800**"),
            "expected the winner marked: {md}"
        );
    }

    #[test]
    fn a_failing_gate_is_rendered_prominently() {
        let mut c = card();
        c.scenarios.push(Scenario {
            name: "Filter correctness".into(),
            rationale: "r".into(),
            rows: vec![],
            gate: Some(GateResult {
                passed: false,
                detail: "3 rows violated the predicate".into(),
            }),
        });
        let md = render(&c);
        assert!(md.contains("**FAIL**"));
        assert!(md.contains("3 rows violated the predicate"));
    }

    #[test]
    fn caveats_are_always_rendered() {
        let md = render(&card());
        assert!(md.contains("What these numbers do not say"));
        assert!(md.contains("a caveat"));
    }

    #[test]
    fn the_verdict_counts_a_better_result_against_the_better_baseline() {
        let mut c = card_with(&[0.95; 40], &[0.90; 40]);
        c.engines = vec![
            "inillucent".into(),
            PgMode::Default.label().into(),
            PgMode::WellConfigured.label().into(),
        ];
        c.scenarios[0].rows[0].measures = vec![
            Measure {
                engine: "inillucent".into(),
                value: 0.95,
            },
            Measure {
                engine: PgMode::Default.label().into(),
                value: 0.10,
            },
            Measure {
                engine: PgMode::WellConfigured.label().into(),
                value: 0.90,
            },
        ];
        c.scenarios[0].rows[0].series = vec![
            Series {
                engine: "inillucent".into(),
                values: vec![0.95; 40],
            },
            Series {
                engine: PgMode::Default.label().into(),
                values: vec![0.10; 40],
            },
            Series {
                engine: PgMode::WellConfigured.label().into(),
                values: vec![0.90; 40],
            },
        ];
        let j = judge(&c);
        assert_eq!(j.len(), 1);
        assert_eq!(j[0].verdict, Verdict::Better);
        // The correctly configured column is the baseline, not the production one.
        assert!((j[0].best_baseline - 0.90).abs() < 1e-9);
        assert_eq!(j[0].best_baseline_engine, PgMode::WellConfigured.label());
    }

    #[test]
    fn a_worse_result_against_the_better_baseline_is_reported() {
        let mut c = card_with(&[0.80; 40], &[0.90; 40]);
        c.engines = vec![
            "inillucent".into(),
            PgMode::Default.label().into(),
            PgMode::WellConfigured.label().into(),
        ];
        c.scenarios[0].rows[0].measures = vec![
            Measure {
                engine: "inillucent".into(),
                value: 0.80,
            },
            Measure {
                engine: PgMode::Default.label().into(),
                value: 0.10,
            },
            Measure {
                engine: PgMode::WellConfigured.label().into(),
                value: 0.90,
            },
        ];
        c.scenarios[0].rows[0].series = vec![
            Series {
                engine: "inillucent".into(),
                values: vec![0.80; 40],
            },
            Series {
                engine: PgMode::Default.label().into(),
                values: vec![0.10; 40],
            },
            Series {
                engine: PgMode::WellConfigured.label().into(),
                values: vec![0.90; 40],
            },
        ];
        let j = judge(&c);
        assert_eq!(j[0].verdict, Verdict::Worse);
        let md = render(&c);
        assert!(md.contains("1 worse"), "{md}");
        assert!(md.contains("Measurements where inillucent is worse"));
    }

    /// The defect that motivated the whole change. One query in ninety changing
    /// its mind moves a mean by about 0.011, a hundred times the old `1e-4` tie
    /// tolerance, and used to be counted as a win.
    #[test]
    fn one_query_of_ninety_is_no_longer_a_win() {
        let baseline = vec![0.5; 90];
        let mut inillucent = baseline.clone();
        inillucent[0] = 1.0;
        let c = card_with(&inillucent, &baseline);
        let j = judge(&c);
        assert!(
            j[0].inillucent > j[0].best_baseline,
            "the point estimate really did move"
        );
        assert_ne!(j[0].verdict, Verdict::Better);
        assert_eq!(j[0].paired.as_ref().unwrap().disagreements, 1);
    }

    /// A difference too small to matter is equivalent, however many queries prove
    /// it is real.
    #[test]
    fn a_difference_below_the_practical_threshold_is_equivalent() {
        let c = card_with(&vec![0.5005; 400], &vec![0.5; 400]);
        assert_eq!(judge(&c)[0].verdict, Verdict::Equivalent);
    }

    /// Two engines that have both found everything there is are not "inconclusive",
    /// which would suggest a longer run could separate them.
    #[test]
    fn both_engines_at_the_ceiling_is_equivalent_rather_than_inconclusive() {
        let c = card_with(&vec![1.0; 30], &vec![1.0; 30]);
        let j = judge(&c);
        assert!(j[0].at_ceiling);
        assert_eq!(j[0].verdict, Verdict::Equivalent);
        assert!(render(&c).contains("at the ceiling"));
    }

    /// A diagnostic is rendered and never voted on. Four correlated views of one
    /// ranking used to become four wins.
    #[test]
    fn a_diagnostic_row_is_rendered_but_not_judged() {
        let mut c = card_with(&[0.95; 40], &[0.50; 40]);
        c.scenarios[0].rows.push(MetricRow::diagnostic(
            "all sources".into(),
            "rows returned of 50",
            vec![
                Measure {
                    engine: "inillucent".into(),
                    value: 50.0,
                },
                Measure {
                    engine: "pgvector".into(),
                    value: 30.0,
                },
            ],
            true,
        ));
        let j = judge(&c);
        assert_eq!(j.len(), 1, "only the primary row is judged");
        assert_eq!(j[0].metric, "recall@10");
        assert!(
            render(&c).contains("rows returned of 50"),
            "the diagnostic is still printed"
        );
    }

    /// The ladder and the sweep compare inillucent settings against each other. If
    /// those counted, every family would report a win against itself.
    #[test]
    fn rows_whose_columns_are_settings_are_excluded_from_the_verdict() {
        let mut c = card();
        c.engines = vec!["inillucent".into(), PgMode::Default.label().into()];
        c.scenarios[0].rows[0].measures = vec![
            Measure {
                engine: "768 dims, f32".into(),
                value: 1.0,
            },
            Measure {
                engine: "64 dims, int8".into(),
                value: 0.34,
            },
        ];
        assert!(judge(&c).is_empty());
    }

    #[test]
    fn a_lower_is_better_row_is_judged_in_the_right_direction() {
        let mut c = card_with(&vec![1.2; 40], &vec![3.1; 40]);
        c.engines = vec!["inillucent".into(), PgMode::Default.label().into()];
        c.scenarios[0].rows[0].metric = "vector search mean ms".into();
        c.scenarios[0].rows[0].higher_is_better = false;
        c.scenarios[0].rows[0].measures = vec![
            Measure {
                engine: "inillucent".into(),
                value: 1.2,
            },
            Measure {
                engine: PgMode::Default.label().into(),
                value: 3.1,
            },
        ];
        c.scenarios[0].rows[0].series = vec![
            Series {
                engine: "inillucent".into(),
                values: vec![1.2; 40],
            },
            Series {
                engine: PgMode::Default.label().into(),
                values: vec![3.1; 40],
            },
        ];
        assert_eq!(judge(&c)[0].verdict, Verdict::Better, "faster should win");
    }

    #[test]
    fn a_failing_gate_is_stated_in_the_verdict() {
        let mut c = card();
        c.scenarios.push(Scenario {
            name: "Filter correctness".into(),
            rationale: "r".into(),
            rows: vec![],
            gate: Some(GateResult {
                passed: false,
                detail: "bad".into(),
            }),
        });
        assert!(render(&c).contains("**A GATE FAILED**"));
    }

    #[test]
    fn lower_is_better_marks_the_smaller_value() {
        let mut c = card();
        c.scenarios[0].rows[0].higher_is_better = false;
        let md = render(&c);
        assert!(md.contains("**0.9100**"));
    }

    #[test]
    fn latency_uses_a_relative_threshold_and_ranking_an_absolute_one() {
        assert!((practical_threshold("nDCG@10", 0.9) - 0.01).abs() < 1e-9);
        assert!((practical_threshold("vector search mean ms", 20.0) - 1.0).abs() < 1e-9);
        // Floored, so a sub-millisecond measurement is not decided by scheduler noise.
        assert!((practical_threshold("vector search mean ms", 0.2) - 0.05).abs() < 1e-9);
    }

    #[test]
    fn provenance_is_rendered_when_the_run_recorded_it() {
        let mut c = card();
        c.provenance.insert("commit".into(), "abc1234".into());
        c.provenance
            .insert("per-query records".into(), "180 lines in runs/x".into());
        let md = render(&c);
        assert!(md.contains("## Provenance"));
        assert!(md.contains("abc1234"));
        assert!(md.contains("180 lines in runs/x"));
    }

    #[test]
    fn the_query_counts_behind_each_family_are_rendered() {
        let mut c = card();
        c.query_counts.insert("passage evidence".into(), 180);
        assert!(render(&c).contains("| passage evidence | 180 |"));
    }

    /// A row with no per-query scores can still be reported, and must not be
    /// allowed to claim more certainty than the point estimate supports.
    #[test]
    fn a_row_without_per_query_scores_falls_back_to_the_point_estimate() {
        let c = card();
        let j = judge(&c);
        assert!(j[0].paired.is_none());
        assert_eq!(
            j[0].verdict,
            Verdict::Better,
            "0.98 against 0.91 clears the threshold"
        );
        assert!(render(&c).contains("not paired"));
    }
}
