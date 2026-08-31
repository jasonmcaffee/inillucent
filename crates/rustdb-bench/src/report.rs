//! The score card structures and their rendering.

use serde::Serialize;

/// One measured number for one engine.
#[derive(Serialize, Clone)]
pub struct Measure {
    pub engine: String,
    pub value: f64,
}

/// One row of a scenario: a metric measured across engines.
#[derive(Serialize, Clone)]
pub struct MetricRow {
    pub label: String,
    pub metric: String,
    pub measures: Vec<Measure>,
    /// Higher is better for accuracy, lower is better for latency.
    pub higher_is_better: bool,
}

/// Whether a row is a comparison between the engines at all. A row whose columns
/// are rust-db configurations, such as the quantization ladder or the `ef_search`
/// sweep, must not count towards the verdict: rust-db would be compared against
/// itself and every family would report a win.
fn is_engine_comparison(row: &MetricRow, engines: &[String]) -> bool {
    let named: usize = row
        .measures
        .iter()
        .filter(|m| engines.contains(&m.engine))
        .count();
    named >= 2
}

#[derive(Serialize, Clone, PartialEq)]
pub enum Outcome {
    Win,
    Tie,
    Loss,
}

#[derive(Serialize, Clone)]
pub struct Judgement {
    pub scenario: String,
    pub metric: String,
    pub label: String,
    pub rustdb: f64,
    pub best_baseline: f64,
    pub best_baseline_engine: String,
    pub outcome: Outcome,
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
    pub engines: Vec<String>,
    pub scenarios: Vec<Scenario>,
    pub build: Vec<BuildFacts>,
    pub caveats: Vec<String>,
    pub generated_at: String,
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

/// Compare rust-db against the better of the two pgvector configurations on every
/// row that is genuinely a comparison between engines.
///
/// The pass condition from the ticket is parity across all scenarios, so the
/// baseline is deliberately the *best* pgvector column rather than the production
/// one. Comparing against the misconfiguration would be easy and would prove
/// nothing.
pub fn judge(card: &ScoreCard) -> Vec<Judgement> {
    // Within one part in ten thousand is a tie: below that the difference is
    // smaller than the query sampling noise these figures rest on.
    const TIE: f64 = 1e-4;
    let mut out = Vec::new();

    for sc in &card.scenarios {
        for row in &sc.rows {
            if !is_engine_comparison(row, &card.engines) {
                continue;
            }
            let Some(rustdb) = row.measures.iter().find(|m| m.engine == "rust-db") else {
                continue;
            };
            let baselines: Vec<&Measure> = row
                .measures
                .iter()
                .filter(|m| m.engine != "rust-db" && card.engines.contains(&m.engine))
                .collect();
            if baselines.is_empty() {
                continue;
            }
            let best = baselines
                .iter()
                .copied()
                .reduce(|a, b| {
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
                })
                .unwrap();

            let delta = if row.higher_is_better {
                rustdb.value - best.value
            } else {
                best.value - rustdb.value
            };
            let outcome = if delta.abs() <= TIE {
                Outcome::Tie
            } else if delta > 0.0 {
                Outcome::Win
            } else {
                Outcome::Loss
            };

            out.push(Judgement {
                scenario: sc.name.clone(),
                metric: row.metric.clone(),
                label: row.label.clone(),
                rustdb: rustdb.value,
                best_baseline: best.value,
                best_baseline_engine: best.engine.clone(),
                outcome,
            });
        }
    }
    out
}

/// Render the score card as markdown.
pub fn render(card: &ScoreCard) -> String {
    let mut s = String::new();
    s.push_str("# rust-db Score Card\n\n");
    s.push_str(&format!(
        "Generated {}. Corpus: {} chunks across {} documents, {} dimensional embeddings from `nomic-embed-text-v1.5` run in process at full precision.\n\n",
        card.generated_at, card.corpus_chunks, card.corpus_documents, card.dimensions
    ));

    s.push_str("The corpus is assembled from public data by this repository and embedded once. The identical vectors are written to the cache rust-db reads and to the PostgreSQL column pgvector reads, and every query is embedded once and handed to both engines, so the embedding model cancels out of the comparison entirely. A score difference is therefore attributable to indexing and ranking.\n\n");

    s.push_str("Engines graded:\n\n");
    for e in &card.engines {
        s.push_str(&format!("- {e}\n"));
    }
    s.push('\n');

    // The verdict, so a reader gets the answer before the evidence.
    let judgements = judge(card);
    let gates_pass = card
        .scenarios
        .iter()
        .filter_map(|sc| sc.gate.as_ref())
        .all(|g| g.passed);
    let wins = judgements.iter().filter(|j| j.outcome == Outcome::Win).count();
    let ties = judgements.iter().filter(|j| j.outcome == Outcome::Tie).count();
    let losses: Vec<&Judgement> = judgements
        .iter()
        .filter(|j| j.outcome == Outcome::Loss)
        .collect();

    s.push_str("## Verdict\n\n");
    s.push_str("Each comparable measurement below is scored against the better of the two pgvector configurations, not against the production one, because comparing against a misconfiguration would prove nothing. Rows whose columns are rust-db settings rather than engines, the quantization ladder and the `ef_search` sweep, are excluded: rust-db cannot beat itself.\n\n");
    s.push_str(&format!(
        "**{} comparable measurements: {} won, {} tied, {} lost. Correctness gates: {}.**\n\n",
        judgements.len(),
        wins,
        ties,
        losses.len(),
        if gates_pass { "all pass" } else { "**A GATE FAILED**" }
    ));

    if losses.is_empty() {
        s.push_str("No measurement was worse than the best the configured PostgreSQL baseline can do.\n\n");
    } else {
        s.push_str("Measurements where rust-db is worse than the best pgvector configuration, stated because a score card that cannot report a loss is not measuring anything:\n\n");
        s.push_str("| scenario | measurement | metric | rust-db | best baseline | which baseline |\n|---|---|---|---|---|---|\n");
        for j in &losses {
            s.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} |\n",
                j.scenario,
                j.label,
                j.metric,
                fmt(j.rustdb),
                fmt(j.best_baseline),
                j.best_baseline_engine
            ));
        }
        s.push('\n');
    }

    // Gates first: a correctness failure changes how every other number should be read.
    let gates: Vec<&Scenario> = card.scenarios.iter().filter(|sc| sc.gate.is_some()).collect();
    if !gates.is_empty() {
        s.push_str("## Correctness gates\n\n");
        s.push_str("These pass or fail rather than scoring. An engine that returns rows it was told to exclude is not a faster engine, it is a wrong one, so a failure here caps the result regardless of any accuracy number.\n\n");
        s.push_str("| gate | result | detail |\n|---|---|---|\n");
        for sc in gates {
            let g = sc.gate.as_ref().unwrap();
            s.push_str(&format!(
                "| {} | {} | {} |\n",
                sc.name,
                if g.passed { "pass" } else { "**FAIL**" },
                g.detail
            ));
        }
        s.push('\n');
    }

    for sc in &card.scenarios {
        if sc.gate.is_some() && sc.rows.is_empty() {
            continue;
        }
        s.push_str(&format!("## {}\n\n{}\n\n", sc.name, sc.rationale));
        if sc.rows.is_empty() {
            s.push_str("No measurements in this family.\n\n");
            continue;
        }
        // One column per engine, in the order the card declares.
        s.push_str("| measurement | metric |");
        for e in &card.engines {
            s.push_str(&format!(" {e} |"));
        }
        s.push_str("\n|---|---|");
        for _ in &card.engines {
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
            for engine in &card.engines {
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

    if !card.caveats.is_empty() {
        s.push_str("## What these numbers do not say\n\n");
        for c in &card.caveats {
            s.push_str(&format!("- {c}\n"));
        }
        s.push('\n');
    }

    s
}

pub fn print_summary(card: &ScoreCard) {
    let j = judge(card);
    let wins = j.iter().filter(|x| x.outcome == Outcome::Win).count();
    let ties = j.iter().filter(|x| x.outcome == Outcome::Tie).count();
    let losses = j.iter().filter(|x| x.outcome == Outcome::Loss).count();
    eprintln!("\n=== verdict: {} comparable measurements, {wins} won, {ties} tied, {losses} lost ===", j.len());
    for x in j.iter().filter(|x| x.outcome == Outcome::Loss) {
        eprintln!(
            "  LOSS  [{}] {} {} :: rust-db={} best_baseline={} ({})",
            x.scenario, x.label, x.metric, fmt(x.rustdb), fmt(x.best_baseline), x.best_baseline_engine
        );
    }
    eprintln!("\n=== summary ===");
    for sc in &card.scenarios {
        if let Some(g) = &sc.gate {
            eprintln!(
                "{}: {}",
                sc.name,
                if g.passed { "pass" } else { "FAIL" }
            );
        }
        for row in &sc.rows {
            let parts: Vec<String> = row
                .measures
                .iter()
                .map(|m| format!("{}={}", m.engine, fmt(m.value)))
                .collect();
            eprintln!("  [{}] {} {} :: {}", sc.name, row.label, row.metric, parts.join("  "));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::PgMode;

    fn card() -> ScoreCard {
        ScoreCard {
            corpus_chunks: 100,
            corpus_documents: 10,
            dimensions: 768,
            engines: vec!["rust-db".into(), "pgvector".into()],
            scenarios: vec![Scenario {
                name: "Unfiltered vector accuracy".into(),
                rationale: "why".into(),
                rows: vec![MetricRow {
                    label: "all sources".into(),
                    metric: "recall@10".into(),
                    measures: vec![
                        Measure { engine: "rust-db".into(), value: 0.98 },
                        Measure { engine: "pgvector".into(), value: 0.91 },
                    ],
                    higher_is_better: true,
                }],
                gate: None,
            }],
            build: vec![],
            caveats: vec!["a caveat".into()],
            generated_at: "now".into(),
        }
    }

    #[test]
    fn renders_a_table_with_one_column_per_engine() {
        let md = render(&card());
        assert!(md.contains("| measurement | metric | rust-db | pgvector |"));
        assert!(md.contains("recall@10"));
    }

    #[test]
    fn marks_the_better_value_in_bold() {
        let md = render(&card());
        assert!(md.contains("**0.9800**"), "expected the winner marked: {md}");
    }

    #[test]
    fn a_failing_gate_is_rendered_prominently() {
        let mut c = card();
        c.scenarios.push(Scenario {
            name: "Filter correctness".into(),
            rationale: "r".into(),
            rows: vec![],
            gate: Some(GateResult { passed: false, detail: "3 rows violated the predicate".into() }),
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
    fn the_verdict_counts_a_win_against_the_better_baseline() {
        let mut c = card();
        c.engines = vec![
            "rust-db".into(),
            PgMode::Default.label().into(),
            PgMode::WellConfigured.label().into(),
        ];
        c.scenarios[0].rows[0].measures = vec![
            Measure { engine: "rust-db".into(), value: 0.95 },
            Measure { engine: PgMode::Default.label().into(), value: 0.10 },
            Measure { engine: PgMode::WellConfigured.label().into(), value: 0.90 },
        ];
        let j = judge(&c);
        assert_eq!(j.len(), 1);
        assert!(j[0].outcome == Outcome::Win);
        // The correctly configured column is the baseline, not the production one.
        assert!((j[0].best_baseline - 0.90).abs() < 1e-9);
        assert_eq!(j[0].best_baseline_engine, PgMode::WellConfigured.label());
    }

    #[test]
    fn a_loss_against_the_better_baseline_is_reported_even_when_production_is_worse() {
        let mut c = card();
        c.engines = vec![
            "rust-db".into(),
            PgMode::Default.label().into(),
            PgMode::WellConfigured.label().into(),
        ];
        c.scenarios[0].rows[0].measures = vec![
            Measure { engine: "rust-db".into(), value: 0.80 },
            Measure { engine: PgMode::Default.label().into(), value: 0.10 },
            Measure { engine: PgMode::WellConfigured.label().into(), value: 0.90 },
        ];
        let j = judge(&c);
        assert!(j[0].outcome == Outcome::Loss);
        let md = render(&c);
        assert!(md.contains("1 lost"), "{md}");
        assert!(md.contains("Measurements where rust-db is worse"));
    }

    /// The ladder and the sweep compare rust-db settings against each other. If
    /// those counted, every family would report a win against itself.
    #[test]
    fn rows_whose_columns_are_settings_are_excluded_from_the_verdict() {
        let mut c = card();
        c.engines = vec!["rust-db".into(), PgMode::Default.label().into()];
        c.scenarios[0].rows[0].measures = vec![
            Measure { engine: "768 dims, f32".into(), value: 1.0 },
            Measure { engine: "64 dims, int8".into(), value: 0.34 },
        ];
        assert!(judge(&c).is_empty());
    }

    #[test]
    fn a_lower_is_better_row_is_judged_in_the_right_direction() {
        let mut c = card();
        c.engines = vec!["rust-db".into(), PgMode::Default.label().into()];
        c.scenarios[0].rows[0].higher_is_better = false;
        c.scenarios[0].rows[0].measures = vec![
            Measure { engine: "rust-db".into(), value: 1.2 },
            Measure { engine: PgMode::Default.label().into(), value: 3.1 },
        ];
        assert!(judge(&c)[0].outcome == Outcome::Win, "faster should win");
    }

    #[test]
    fn a_failing_gate_is_stated_in_the_verdict() {
        let mut c = card();
        c.scenarios.push(Scenario {
            name: "Filter correctness".into(),
            rationale: "r".into(),
            rows: vec![],
            gate: Some(GateResult { passed: false, detail: "bad".into() }),
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
}
