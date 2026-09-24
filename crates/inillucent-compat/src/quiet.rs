//! Whether a gate's pass ran on a quiet machine, read from the reference arm's own speed.
//!
//! Invariant: a gate grades a pass only when the SQLite arm ran within
//! [`DEFAULT_THRESHOLD`] of its recorded quiet speed on this machine, or when
//! no such record exists and the report says the check was not made. A pass the
//! check refuses prints its numbers and no verdict, and the gate exits
//! [`NOT_GRADED`].
//!
//! ## Why the reference arm is the instrument (task-2110, bug 6)
//!
//! A busy machine does not slow the two arms equally. Measured on 2026-09-24
//! with the full gate at medium, 30 rounds, pinned to the performance cores:
//! with other processes holding 16% to 37% of the processor, SQLite's arm was
//! 7.7% to 20.0% slower than in an idle window and this engine's 4.8% to 10.6%,
//! so five consecutive loaded passes read `read.join` at 4.34x to 4.57x against
//! 4.19x to 4.27x idle. Every ratio came out too high, and a family could meet
//! a bar it misses on a quiet machine. Several passes do not fix it, because
//! consecutive passes share the machine's state and their average keeps the
//! bias.
//!
//! `sqlite-bench` is the same program in every pass whatever build is under
//! test, so its time over a workload, against a time recorded when the machine
//! was idle, says how busy the machine was. The pass's **speed index** is the
//! geometric mean over the counted workloads of the pass's median SQLite time
//! over the reference median, minus one. Over twelve idle passes it was at most
//! 1.39%; on every loaded pass seen it was at least 4%. [`DEFAULT_THRESHOLD`]
//! is 3%, between the two.
//!
//! ## What a reference is keyed by
//!
//! The machine: it lives outside the tracked tree, under a directory named for
//! the host, because a time taken on one machine says nothing about another.
//! The fixture scale: one file per scale. And per workload, the things that
//! change what SQLite is asked to do: the repeat count, `cache_size` and the
//! locking mode. A families filtered run is checked against the rows of a full
//! run that match it, and a run whose settings no row matches is reported as
//! unchecked rather than compared with a different amount of work.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::perf::{median, Paired, Plan};

/// Above this speed index a pass is not graded.
pub const DEFAULT_THRESHOLD: f64 = 0.03;

/// The exit code of a gate that measured a pass and would not grade it.
///
/// Distinct from 1, which is a graded miss, and from 2, which is a run that
/// could not measure at all: a caller has to be able to tell "slower than the
/// bar" from "nothing was decided" without reading the report.
pub const NOT_GRADED: u8 = 4;

/// The families whose workloads the index is taken over, when the plan has any of them.
///
/// These are the twelve workloads the bound was measured on in task-2095:
/// `point.*`, `range.*`, `scan.*` and the two `join.*`. `read.correlated` is
/// a read family too, and was not part of that measurement.
const INDEX_FAMILIES: [&str; 4] = ["read.point", "read.range", "read.join", "read.analytical"];

/// The fewest workloads a speed index is taken over. One workload is a noisy
/// instrument on its own; three is the smallest family in the read plan.
const FEWEST_WORKLOADS: usize = 3;

/// What the check decided about one pass.
#[derive(Debug, Clone, PartialEq)]
pub enum Quiet {
    /// The index is at or under the threshold, so the pass is graded.
    Quiet {
        /// The speed index, as a fraction: 0.02 is 2% slower than the reference.
        index: f64,
        /// How many workloads it was taken over.
        workloads: usize,
    },
    /// The index is over the threshold, so the pass is not graded.
    Busy {
        /// The speed index, as a fraction.
        index: f64,
        /// How many workloads it was taken over.
        workloads: usize,
    },
    /// Nothing to compare with, so the pass is graded as it always was and the
    /// report says the check was not made.
    Unchecked {
        /// Why there was nothing to compare with.
        reason: String,
    },
}

impl Quiet {
    /// Whether the gate may print a verdict for this pass.
    pub fn graded(&self) -> bool {
        !matches!(self, Quiet::Busy { .. })
    }
}

/// The word a gate prints where a verdict goes.
///
/// One function, so no gate can print MET or MISSED for a pass the check
/// refused: every verdict line in the three gates is built through this.
///
/// @param graded - whether the check let the pass be graded
/// @param met - whether the bar was met
pub fn verdict(graded: bool, met: bool) -> &'static str {
    match (graded, met) {
        (false, _) => "NOT GRADED",
        (true, true) => "MET",
        (true, false) => "MISSED",
    }
}

/// The words a gate's last line prints: whether it was graded, and if so whether it was met.
///
/// @param graded - whether the check let the pass be graded
/// @param passed - whether every bar was met
pub fn gate_line(graded: bool, passed: bool) -> &'static str {
    match (graded, passed) {
        (false, _) => "NOT GRADED - the machine was not quiet",
        (true, true) => "MET",
        (true, false) => "NOT MET",
    }
}

/// What the gate's command line asked of the check.
#[derive(Debug, Clone, PartialEq)]
pub struct Options {
    /// The index above which a pass is not graded.
    pub threshold: f64,
    /// Add this pass's SQLite medians to the reference after reporting it.
    pub record: bool,
}

impl Options {
    /// Reads `--quiet-threshold <percent>` and `--record-quiet-reference`.
    ///
    /// @param arguments - the gate's command line
    pub fn from_arguments(arguments: &[String]) -> Options {
        let threshold = arguments
            .iter()
            .position(|argument| argument == "--quiet-threshold")
            .and_then(|at| arguments.get(at.saturating_add(1)))
            .and_then(|value| value.parse::<f64>().ok())
            .map_or(DEFAULT_THRESHOLD, |percent| percent / 100.0);
        let record = arguments
            .iter()
            .any(|argument| argument == "--record-quiet-reference");
        Options { threshold, record }
    }
}

/// One row of a reference: a workload's median SQLite time in one recorded pass.
#[derive(Debug, Clone, PartialEq)]
struct Row {
    taken: String,
    key: String,
    nanos: f64,
}

/// The key a workload's time is filed under: its name and what SQLite is asked to do.
///
/// @param workload - the workload's name
/// @param repeat - its repeat count in this plan
/// @param plan - the plan both arms read, for `cache_size` and the locking mode
fn key_of(workload: &str, repeat: u32, plan: &Plan) -> String {
    format!(
        "{workload}|repeat={repeat}|cache={}|locking={}",
        plan.cache_size, plan.locking
    )
}

/// The directory references are kept in for this machine.
///
/// `INILLUCENT_QUIET_REFERENCE_DIR` replaces the whole path, for a test and for
/// a person who wants the file somewhere else. Otherwise it is under the
/// platform's per user data directory, never in the checkout: a worktree does
/// not have the main checkout's gitignored files, and a reference is about the
/// machine rather than about a branch.
pub fn reference_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("INILLUCENT_QUIET_REFERENCE_DIR") {
        return PathBuf::from(dir);
    }
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_DATA_HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("inillucent").join("quiet-reference").join(host())
}

/// This machine's name, for the reference directory.
fn host() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "this-machine".to_string())
}

/// The reference file for one fixture scale.
///
/// @param scale - the plan's scale, `small`, `medium` or `large`
pub fn reference_path(scale: &str) -> PathBuf {
    reference_dir().join(format!("{scale}.tsv"))
}

/// Reads a reference file, skipping lines that do not parse.
///
/// @param text - the file's contents
fn parse(text: &str) -> Vec<Row> {
    text.lines()
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let taken = fields.next()?.to_string();
            let key = fields.next()?.to_string();
            let nanos = fields.next()?.parse::<f64>().ok()?;
            Some(Row { taken, key, nanos })
        })
        .collect()
}

/// The median SQLite time of every workload that should count, keyed as a reference is.
///
/// The workloads of [`INDEX_FAMILIES`] count when the plan has any, because the
/// bound was measured on them and a write's time moves with the disk as well
/// as the processor. A plan with none of them, the write gate's, counts every
/// workload. A workload the two arms disagreed on has no times and does not
/// count.
///
/// @param measured - the pass's paired times
/// @param plan - the plan both arms read
pub fn sqlite_medians(measured: &[Paired], plan: &Plan) -> BTreeMap<String, f64> {
    let counts = |family: &str| INDEX_FAMILIES.contains(&family);
    let any_indexed = measured.iter().any(|entry| counts(&entry.family));
    let mut medians = BTreeMap::new();
    for entry in measured {
        if !entry.agreed || entry.pairs.is_empty() || (any_indexed && !counts(&entry.family)) {
            continue;
        }
        let Some(workload) = plan
            .workloads
            .iter()
            .find(|workload| workload.name == entry.workload)
        else {
            continue;
        };
        let theirs: Vec<f64> = entry.pairs.iter().map(|pair| pair.1).collect();
        medians.insert(
            key_of(&workload.name, workload.repeat, plan),
            median(&theirs),
        );
    }
    medians
}

/// The speed index of a pass against a reference, and how many workloads it is over.
///
/// Each workload's reference time is the median of its rows, so a reference
/// recorded over several passes is not moved by one of them.
///
/// @param reference - the recorded rows
/// @param pass - this pass's medians, from [`sqlite_medians`]
fn speed_index(reference: &[Row], pass: &BTreeMap<String, f64>) -> Option<(f64, usize)> {
    let mut logs = Vec::new();
    for (key, nanos) in pass {
        let recorded: Vec<f64> = reference
            .iter()
            .filter(|row| &row.key == key)
            .map(|row| row.nanos)
            .collect();
        if recorded.is_empty() || *nanos <= 0.0 {
            continue;
        }
        let reference_nanos = median(&recorded);
        if reference_nanos > 0.0 {
            logs.push((nanos / reference_nanos).ln());
        }
    }
    if logs.len() < FEWEST_WORKLOADS {
        return None;
    }
    let mean = logs.iter().sum::<f64>() / logs.len() as f64;
    Some((mean.exp() - 1.0, logs.len()))
}

/// Decides whether a pass is graded, prints the section that says so, and records it when asked.
///
/// @param measured - the pass's paired times
/// @param plan - the plan both arms read
/// @param options - the threshold and whether to record
pub fn check(measured: &[Paired], plan: &Plan, options: &Options) -> Quiet {
    let path = reference_path(&plan.scale);
    let pass = sqlite_medians(measured, plan);
    let rows = std::fs::read_to_string(&path)
        .map(|text| parse(&text))
        .unwrap_or_default();
    let quiet = judge(&rows, &pass, options.threshold, &path);
    println!();
    println!("## was the machine quiet");
    print_reference(&rows, &path);
    match &quiet {
        Quiet::Quiet { index, workloads } => println!(
            "  sqlite speed index {:+.2}% over {workloads} workloads, at or under {:.2}%: graded",
            index * 100.0,
            options.threshold * 100.0
        ),
        Quiet::Busy { index, workloads } => println!(
            "  sqlite speed index {:+.2}% over {workloads} workloads, above {:.2}%: MACHINE NOT \
             QUIET, NOT GRADED. The reference arm ran slower than it does on this machine when \
             it is idle, and a busy machine slows it more than this engine, so every ratio above \
             is too high. Run again when nothing else is loading the processor.",
            index * 100.0,
            options.threshold * 100.0
        ),
        Quiet::Unchecked { reason } => println!("  NOT CHECKED: {reason}"),
    }
    if options.record {
        match record(&path, &pass) {
            Ok(count) => println!(
                "  recorded this pass's {count} SQLite medians into {} (--record-quiet-reference)",
                path.display()
            ),
            Err(reason) => println!("  could not record the reference: {reason}"),
        }
    }
    quiet
}

/// The decision itself, apart from any printing or file.
///
/// @param rows - the reference
/// @param pass - this pass's medians
/// @param threshold - the index above which the pass is not graded
/// @param path - the reference file, named in the reason when there is nothing to compare
fn judge(
    rows: &[Row],
    pass: &BTreeMap<String, f64>,
    threshold: f64,
    path: &std::path::Path,
) -> Quiet {
    if rows.is_empty() {
        return Quiet::Unchecked {
            reason: format!(
                "there is no quiet reference for this machine at {}. The pass is graded without \
                 the check. Record one by running the gate with --record-quiet-reference while \
                 the machine is idle.",
                path.display()
            ),
        };
    }
    match speed_index(rows, pass) {
        None => Quiet::Unchecked {
            reason: format!(
                "the reference covers fewer than {FEWEST_WORKLOADS} of this pass's workloads with \
                 the same repeat, cache size and locking mode, so the pass is graded without the \
                 check."
            ),
        },
        Some((index, workloads)) if index > threshold => Quiet::Busy { index, workloads },
        Some((index, workloads)) => Quiet::Quiet { index, workloads },
    }
}

/// Prints where the reference is and when it was taken, so a stale one is visible.
///
/// @param rows - the reference
/// @param path - its file
fn print_reference(rows: &[Row], path: &std::path::Path) {
    let mut passes: Vec<&str> = rows.iter().map(|row| row.taken.as_str()).collect();
    passes.sort_unstable();
    passes.dedup();
    match (passes.first(), passes.last()) {
        (Some(first), Some(last)) => println!(
            "  reference   : {} ({} recorded passes, {first} to {last})",
            path.display(),
            passes.len()
        ),
        _ => println!("  reference   : none at {}", path.display()),
    }
}

/// Appends a pass's medians to the reference file, and returns how many were written.
///
/// @param path - the reference file
/// @param pass - this pass's medians
fn record(path: &std::path::Path, pass: &BTreeMap<String, f64>) -> Result<usize, String> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let fresh = !path.exists();
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| error.to_string())?;
    if fresh {
        writeln!(
            file,
            "# The SQLite arm's median time per workload, one pass per timestamp, taken on an idle \
             machine.\n# Written by --record-quiet-reference; see crates/inillucent-compat/src/quiet.rs."
        )
        .map_err(|error| error.to_string())?;
    }
    let taken = timestamp();
    for (key, nanos) in pass {
        writeln!(file, "{taken}\t{key}\t{nanos:.1}").map_err(|error| error.to_string())?;
    }
    Ok(pass.len())
}

/// The current time as an ISO 8601 date and time in UTC.
fn timestamp() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0);
    let (days, rest) = (seconds / 86_400, seconds % 86_400);
    let (hour, minute, second) = (rest / 3_600, (rest % 3_600) / 60, rest % 60);
    let civil = inillucent_scalar::datetime::civil_of_unix_day(days as i64);
    format!(
        "{:04}-{:02}-{:02}T{hour:02}:{minute:02}:{second:02}Z",
        civil.year, civil.month, civil.day
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reference of one pass, with each named workload at 100 ns.
    ///
    /// @param keys - the workload keys
    fn reference(keys: &[&str]) -> Vec<Row> {
        keys.iter()
            .map(|key| Row {
                taken: "2026-09-24T00:00:00Z".to_string(),
                key: (*key).to_string(),
                nanos: 100.0,
            })
            .collect()
    }

    /// A pass with each named workload at `nanos`.
    ///
    /// @param keys - the workload keys
    /// @param nanos - the time every one of them took
    fn pass(keys: &[&str], nanos: f64) -> BTreeMap<String, f64> {
        keys.iter().map(|key| ((*key).to_string(), nanos)).collect()
    }

    const KEYS: [&str; 3] = ["a|r", "b|r", "c|r"];

    /// Both sides of the bound: 2% slower is graded and 4% slower is not, which
    /// are the idle maximum and the loaded minimum task-2095 measured.
    #[test]
    fn a_pass_is_graded_under_the_bound_and_not_over_it() {
        let rows = reference(&KEYS);
        let path = std::path::Path::new("reference.tsv");
        let quiet = judge(&rows, &pass(&KEYS, 102.0), DEFAULT_THRESHOLD, path);
        assert!(
            matches!(quiet, Quiet::Quiet { workloads: 3, .. }),
            "{quiet:?}"
        );
        assert!(quiet.graded());
        let busy = judge(&rows, &pass(&KEYS, 104.0), DEFAULT_THRESHOLD, path);
        match busy {
            Quiet::Busy { index, workloads } => {
                assert!((index - 0.04).abs() < 1e-9, "{index}");
                assert_eq!(workloads, 3);
            }
            other => panic!("a pass 4% slow was graded: {other:?}"),
        }
        assert!(!judge(&rows, &pass(&KEYS, 104.0), DEFAULT_THRESHOLD, path).graded());
    }

    /// No reference, or one that matches too little of the plan, grades the
    /// pass and says the check was not made, rather than refusing every run on
    /// a machine nobody has recorded.
    #[test]
    fn without_a_matching_reference_the_pass_is_graded_and_says_so() {
        let path = std::path::Path::new("reference.tsv");
        let none = judge(&[], &pass(&KEYS, 500.0), DEFAULT_THRESHOLD, path);
        assert!(matches!(none, Quiet::Unchecked { .. }) && none.graded());
        let other = judge(
            &reference(&["x|r", "y|r", "z|r"]),
            &pass(&KEYS, 500.0),
            DEFAULT_THRESHOLD,
            path,
        );
        assert!(matches!(other, Quiet::Unchecked { .. }) && other.graded());
    }

    /// A reference recorded over several passes uses each workload's median, so
    /// one slow recorded pass does not move it.
    #[test]
    fn one_slow_recorded_pass_does_not_move_the_reference() {
        let mut rows = reference(&KEYS);
        rows.extend(reference(&KEYS));
        rows.extend(KEYS.iter().map(|key| Row {
            taken: "2026-09-24T01:00:00Z".to_string(),
            key: (*key).to_string(),
            nanos: 150.0,
        }));
        let (index, _) = speed_index(&rows, &pass(&KEYS, 100.0)).expect("an index");
        assert!(index.abs() < 1e-9, "{index}");
    }

    /// Over the full plan, the index counts the point, range, join and
    /// analytical workloads and nothing else, and a plan with none of them
    /// counts every workload it has.
    #[test]
    fn the_index_counts_the_families_the_bound_was_measured_on() {
        let plan = crate::perf::plan_for("small");
        let paired = |workload: &crate::perf::Workload| Paired {
            workload: workload.name.clone(),
            family: workload.family.clone(),
            pairs: vec![(1.0, 2.0)],
            agreed: true,
            disagreement: String::new(),
        };
        let measured: Vec<Paired> = plan.workloads.iter().map(paired).collect();
        let medians = sqlite_medians(&measured, &plan);
        let expected = plan
            .workloads
            .iter()
            .filter(|workload| INDEX_FAMILIES.contains(&workload.family.as_str()))
            .count();
        assert!(
            expected >= 12,
            "the plan has the twelve measured read workloads"
        );
        assert_eq!(medians.len(), expected);
        assert!(medians.keys().all(|key| !key.starts_with("correlated.")));
        let writes: Vec<Paired> = measured
            .into_iter()
            .filter(|entry| entry.family == "write")
            .collect();
        assert_eq!(sqlite_medians(&writes, &plan).len(), writes.len());
        assert!(!writes.is_empty());
    }

    /// What is recorded is what is read back.
    #[test]
    fn a_recorded_pass_reads_back() {
        let dir = std::env::temp_dir().join(format!("inillucent-quiet-{}", std::process::id()));
        let path = dir.join("medium.tsv");
        let _ = std::fs::remove_file(&path);
        let written = pass(&KEYS, 123.4);
        assert_eq!(record(&path, &written).expect("recorded"), 3);
        let rows = parse(&std::fs::read_to_string(&path).expect("read back"));
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|row| (row.nanos - 123.4).abs() < 1e-9));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }
}
