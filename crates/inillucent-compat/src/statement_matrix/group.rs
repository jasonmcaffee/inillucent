//! Which cases a test runs, at which arms, and the one assertion at the end.
//!
//! Invariant: **a group runs every case it owns and then fails once, naming
//! every failing case id and every stale `known.list` line.** One broken case
//! must never hide the next, which is what a first-failure assertion inside a
//! loop does.
//!
//! A family's cases are split between its test functions by a hash of the case
//! id, so libtest's threads share the family and each thread keeps one oracle
//! process for all of its cases. A shard (`INILLUCENT_SHARD=i/N`, set by
//! `inillucent-testrun` for a row with `shards = N`) keeps only the cases whose
//! id hash lands on it, with a different part of the hash from the group
//! split, so a shard's groups stay even.
//!
//! What runs at each cadence is section 8.1 of the design:
//!
//! | cadence | layer 1 | layer 2, every pair | layer 2, every triple | retained |
//! |---|---|---|---|---|
//! | change | default arm | default arm, with layer 3, over the values not in `templates::MERGE_ONLY` | no | every arm |
//! | merge | every arm | every arm | default and `small_pool` | every arm |
//! | nightly | no | no | every arm | every arm |

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::matrix::{arms, Arm, Kind as ArmKind};
use crate::statement_matrix::case::{self, Case};
use crate::statement_matrix::grade::{Failure, Verdict};
use crate::statement_matrix::known::{self, Judged};
use crate::statement_matrix::run::{placement_key, Runner, Stats};
use crate::statement_matrix::templates;

/// When a suite runs, which decides what it runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cadence {
    /// Every change: the `matrix` tier.
    Change,
    /// Every merge: the `matrix_deep` tier.
    Merge,
    /// Every night: the `nightly` tier.
    Nightly,
}

/// Which cases of a family a cadence runs, and at which arms.
#[derive(Clone, Debug)]
pub struct Work {
    /// The cases and, for each, the arms it runs at.
    pub runs: Vec<(Case, Vec<Arm>)>,
}

/// Reads every hand written case of a family.
///
/// Layer 1 lives in `corpora/matrix/<family>/*.slt`; the retained corpus of
/// shrunk failures is the family `retained`.
///
/// @param family - the family, which is also the directory name
pub fn layer_one(family: &str) -> Result<Vec<Case>, String> {
    let directory = known::corpus_root().join(family);
    let mut files: Vec<PathBuf> = match std::fs::read_dir(&directory) {
        Ok(listing) => listing
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|kind| kind == "slt"))
            .collect(),
        Err(_) => return Ok(Vec::new()),
    };
    files.sort();
    let mut cases = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(&file)
            .map_err(|error| format!("cannot read {}: {error}", file.display()))?;
        let origin = relative(&file);
        let parsed = case::parse(&text, family, &origin)?;
        cases.extend(parsed.cases);
    }
    Ok(cases)
}

/// Returns a path relative to the workspace root, for messages.
fn relative(path: &Path) -> String {
    let root = crate::workspace_root();
    path.strip_prefix(&root)
        .unwrap_or(path)
        .display()
        .to_string()
        .replace('\\', "/")
}

/// Every arm, by the name `arms` directives use.
pub fn every_arm() -> Vec<Arm> {
    arms(ArmKind::Full)
}

/// Picks the arms with the given names.
fn named(names: &[&str]) -> Vec<Arm> {
    every_arm()
        .into_iter()
        .filter(|arm| names.contains(&arm.name))
        .collect()
}

/// Builds the work a cadence does for one family.
///
/// @param family - the family
/// @param cadence - when the suite runs
pub fn work(family: &str, cadence: Cadence) -> Result<Work, String> {
    let default = named(&["default"]);
    let every = every_arm();
    let mut runs = Vec::new();
    let hand_written = layer_one(family)?;
    let retained = family == "retained";
    match cadence {
        Cadence::Change => {
            let arms = if retained { &every } else { &default };
            for case in hand_written {
                runs.push((case, arms.clone()));
            }
            for case in templates::generate_change(family)? {
                runs.push((case, default.clone()));
            }
        }
        Cadence::Merge => {
            for case in hand_written {
                runs.push((case, every.clone()));
            }
            for case in templates::generate(family, 2)? {
                runs.push((case, every.clone()));
            }
            for case in templates::generate_only_triples(family)? {
                runs.push((case, named(&["default", "small-pool"])));
            }
        }
        Cadence::Nightly => {
            if retained {
                for case in hand_written {
                    runs.push((case, every.clone()));
                }
            }
            for case in templates::generate(family, 3)? {
                runs.push((case, every.clone()));
            }
        }
    }
    Ok(Work { runs })
}

/// A stable hash of a case id, used to split cases between groups and shards.
pub fn id_hash(id: &str) -> u64 {
    // FNV-1a: stable across runs and platforms, which `DefaultHasher` is not
    // promised to be.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in id.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// Reads `INILLUCENT_SHARD`, as `(index, count)`.
pub fn shard() -> (u64, u64) {
    let Ok(text) = std::env::var("INILLUCENT_SHARD") else {
        return (0, 1);
    };
    let Some((index, count)) = text.split_once('/') else {
        return (0, 1);
    };
    match (index.trim().parse::<u64>(), count.trim().parse::<u64>()) {
        (Ok(index), Ok(count)) if count > 0 && index < count => (index, count),
        _ => (0, 1),
    }
}

/// Whether a case belongs to this group and this shard.
///
/// Called with the case's placement key rather than its id, so every case that
/// shares a fixture lands in one group and the fixture is built once.
///
/// @param id - the case's placement key
/// @param group - this group's index
/// @param groups - how many groups the family has
pub fn owns(id: &str, group: usize, groups: usize) -> bool {
    let hash = id_hash(id);
    let (shard_index, shard_count) = shard();
    let groups = groups.max(1) as u64;
    hash % groups == group as u64 && (hash / groups) % shard_count == shard_index
}

/// Runs one group of a family at a cadence, and fails once if anything failed.
///
/// @param family - the family
/// @param cadence - when the suite runs
/// @param group - this test function's index
/// @param groups - how many test functions the family has
/// @param scratch - the directory scratch files go under
pub fn run_group(family: &str, cadence: Cadence, group: usize, groups: usize, scratch: &Path) {
    let report = run_group_report(family, cadence, group, groups, scratch);
    println!("{}", report.summary);
    assert!(
        report.problems.is_empty(),
        "{}",
        report.problems.join("\n\n")
    );
}

/// What one group did, for its assertion and for the phase 0 measurement.
#[derive(Debug, Default)]
pub struct GroupReport {
    /// One line: cases, statements, times.
    pub summary: String,
    /// Every failure and every stale line, rendered.
    pub problems: Vec<String>,
    /// The ids of the cases that failed and are not listed.
    pub failing: Vec<String>,
    /// What the runners did, added across arms.
    pub stats: Stats,
}

/// Runs one group and returns its report instead of asserting.
///
/// @param family - the family
/// @param cadence - when the suite runs
/// @param group - this test function's index
/// @param groups - how many test functions the family has
/// @param scratch - the directory scratch files go under
pub fn run_group_report(
    family: &str,
    cadence: Cadence,
    group: usize,
    groups: usize,
    scratch: &Path,
) -> GroupReport {
    let root = known::corpus_root();
    let mut report = GroupReport::default();
    let lists = known::read_known(&root.join("known.list")).and_then(|known| {
        Ok((
            known,
            known::read_deliberate(&root.join("deliberate.toml"))?,
        ))
    });
    let (known_list, deliberate) = match lists {
        Ok(lists) => lists,
        Err(problem) => {
            report.problems.push(problem);
            return report;
        }
    };
    let work = match work(family, cadence) {
        Ok(work) => work,
        Err(problem) => {
            report.problems.push(problem);
            return report;
        }
    };
    let mine: Vec<&(Case, Vec<Arm>)> = work
        .runs
        .iter()
        .filter(|(case, _)| owns(&placement_key(case), group, groups))
        .collect();
    let outcomes = run_cases(&mine, family, group, scratch, &mut report);
    judge_all(outcomes, &known_list, &deliberate, &mut report);
    report.summary = format!(
        "matrix {family} group {group}/{groups} ({cadence:?}): {} case run(s), {} statement(s), \
         {} fixture(s) built, {} copied; {:.2}s in cases, {:.2}s building fixtures, {:.3}s \
         copying; {} oracle start(s); {} failing",
        report.stats.cases,
        report.stats.statements,
        report.stats.fixtures_built,
        report.stats.fixture_copies,
        report.stats.case_time.as_secs_f64(),
        report.stats.fixture_time.as_secs_f64(),
        report.stats.copy_time.as_secs_f64(),
        report.stats.oracle_starts,
        report.failing.len()
    );
    report
}

/// The failures of one case at every arm, and whether it ran anywhere.
type CaseOutcome = (Vec<Failure>, bool);

/// Runs the group's cases, one runner per arm.
fn run_cases(
    mine: &[&(Case, Vec<Arm>)],
    family: &str,
    group: usize,
    scratch: &Path,
    report: &mut GroupReport,
) -> BTreeMap<String, CaseOutcome> {
    let mut outcomes: BTreeMap<String, CaseOutcome> = BTreeMap::new();
    let (shard_index, _) = shard();
    let mut announced = false;
    for arm in every_arm() {
        let for_arm: Vec<&Case> = mine
            .iter()
            .filter(|(_, arms)| arms.iter().any(|candidate| candidate.name == arm.name))
            .map(|(case, _)| case)
            .collect();
        if for_arm.is_empty() {
            continue;
        }
        let directory = scratch
            .join(format!("s{shard_index}"))
            .join(format!("{family}-{group}-{}", arm.name));
        let mut runner = Runner::new(arm, &directory);
        if !runner.has_oracle() && !announced {
            crate::differential::announce_skip();
            announced = true;
        }
        let verdicts = runner.run_all(&for_arm);
        for (case, verdict) in for_arm.iter().zip(verdicts) {
            let entry = outcomes.entry(case.id.clone()).or_default();
            match verdict {
                Verdict::Passed => entry.1 = true,
                Verdict::Failed(failures) => {
                    entry.1 = true;
                    entry.0.extend(failures);
                }
                Verdict::Skipped(_) => {}
            }
        }
        runner.finish();
        report.stats.add(&runner.stats);
        let _ = std::fs::remove_dir_all(directory.join("fixtures"));
    }
    outcomes
}

/// Judges every case against the two lists and fills in the report.
fn judge_all(
    outcomes: BTreeMap<String, CaseOutcome>,
    known_list: &BTreeMap<String, known::Known>,
    deliberate: &[known::Deliberate],
    report: &mut GroupReport,
) {
    for (id, (failures, ran)) in outcomes {
        match known::judge(&id, failures, ran, known_list, deliberate) {
            Judged::Pass | Judged::Expected => {}
            Judged::Fail(failures) => {
                report.failing.push(id.clone());
                let rendered: Vec<String> = failures.iter().take(3).map(Failure::render).collect();
                report.problems.push(rendered.join("\n    "));
            }
            Judged::Stale(listed) => report.problems.push(format!(
                "{id} is in known.list as bug {} ({}) and now agrees with SQLite at every arm it \
                 ran at. Take its line off the list; a fixed defect left listed reads as coverage \
                 and is not.",
                listed.bug, listed.reason
            )),
        }
    }
}

/// Declares a family's test module: `groups` test functions, each running its
/// share of the family's cases at a cadence.
///
/// ```ignore
/// inillucent_compat::matrix_family!(select, Change, [g0, g1, g2, g3]);
/// ```
#[macro_export]
macro_rules! matrix_family {
    ($family:ident, $cadence:ident, [$($group:ident),+ $(,)?]) => {
        const GROUP_NAMES: &[&str] = &[$(stringify!($group)),+];

        $(
            #[test]
            fn $group() {
                let index = GROUP_NAMES
                    .iter()
                    .position(|name| *name == stringify!($group))
                    .unwrap_or(0);
                $crate::statement_matrix::group::run_group(
                    stringify!($family),
                    $crate::statement_matrix::group::Cadence::$cadence,
                    index,
                    GROUP_NAMES.len(),
                    &std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("matrix"),
                );
            }
        )+
    };
}
