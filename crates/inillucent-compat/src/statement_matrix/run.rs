//! Running cases on both engines, on real files.
//!
//! Invariant: **every case runs on files on both engines, and a case that
//! wrote is reopened, asked its questions again, and checked with
//! `PRAGMA integrity_check` on both engines before it can pass.** A difference
//! is recorded rather than asserted, so one bad case never stops a group: the
//! group runs every case and fails once at the end with every failing id
//! (section 8.3 of the design).
//!
//! **One oracle process per runner, for its whole life.** The part 8 corpus
//! spent most of its 258 seconds starting two shells per case. A [`Runner`] is
//! owned by one test thread and keeps one `sqlite-oracle` process for every
//! case that thread runs, opening and closing files through the protocol. If
//! the process dies, the runner starts another and replays the case once; a
//! second loss is reported as an oracle failure, never as a pass and never as
//! an engine failure.
//!
//! **Setup is built once and copied.** Cases with the same setup share a
//! fixture: the setup runs once per runner on both engines, both databases are
//! closed, reopened and checked with `PRAGMA integrity_check`, and each case
//! starts from a copy of the directory. The copy takes the whole directory,
//! because inillucent's log is a set of numbered segment files beside the main
//! one, which a single file copy would miss.
//!
//! **Cases that only read share one open copy of their fixture.** Measured in
//! phase 0 (section 8.1 of the design), opening and closing an empty database
//! costs this engine about 10 ms of processor time at the default arm in the
//! debug build the tests run in, and a case that writes waits about 70 ms on
//! the disk flushes of its commits and its reopen. A read only case cannot
//! change the file or the connection's counters, so running a run of them
//! against one opened copy asks exactly the questions each would have asked of
//! its own copy, for the cost of the statements alone. A case that writes
//! still gets a copy of its own, its reopen and its integrity check.

use std::collections::HashMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use inillucent_engine::connect::Connection;

use crate::differential::observe_detailed;
use crate::matrix::Arm;
use crate::oracle::{Driver, Observation, Op};
use crate::statement_matrix::case::{is_read_only, split_statements, Case, Record};
use crate::statement_matrix::grade::{
    grade, integrity_ok, Asked, CaseContext, Failure, Kind, Verdict,
};
use crate::statement_matrix::properties;

/// What one runner did, for the phase 0 measurement and the group summary.
#[derive(Clone, Debug, Default)]
pub struct Stats {
    /// Cases run.
    pub cases: usize,
    /// Statements sent to each engine, counting reruns after a reopen.
    pub statements: usize,
    /// Fixtures built.
    pub fixtures_built: usize,
    /// Directories copied from a fixture.
    pub fixture_copies: usize,
    /// Read only cases run inside a shared copy of their fixture.
    pub batched: usize,
    /// Time spent copying fixtures.
    pub copy_time: Duration,
    /// Time spent building fixtures.
    pub fixture_time: Duration,
    /// Time spent in cases, copies included.
    pub case_time: Duration,
    /// Oracle processes started.
    pub oracle_starts: usize,
    /// Processor time the oracle processes used, read from the child's own
    /// accounting when it is retired or the runner finishes.
    pub oracle_cpu: Duration,
    /// The slowest cases, longest first, so a budget overrun names its cause.
    pub slowest: Vec<(Duration, String)>,
}

impl Stats {
    /// Adds another runner's counts into this one.
    ///
    /// @param more - the other runner's stats
    pub fn add(&mut self, more: &Stats) {
        self.cases = self.cases.saturating_add(more.cases);
        self.statements = self.statements.saturating_add(more.statements);
        self.fixtures_built = self.fixtures_built.saturating_add(more.fixtures_built);
        self.fixture_copies = self.fixture_copies.saturating_add(more.fixture_copies);
        self.batched = self.batched.saturating_add(more.batched);
        self.copy_time += more.copy_time;
        self.fixture_time += more.fixture_time;
        self.case_time += more.case_time;
        self.oracle_starts = self.oracle_starts.saturating_add(more.oracle_starts);
        self.oracle_cpu += more.oracle_cpu;
        for (time, id) in &more.slowest {
            self.note_slow(id, *time);
        }
    }

    /// Adds one case's time to the totals and to the slowest list.
    ///
    /// @param id - the case
    /// @param time - how long it took, fixture and copy included
    pub fn record_case(&mut self, id: &str, time: Duration) {
        self.case_time += time;
        self.note_slow(id, time);
    }

    /// Keeps the ten slowest cases.
    pub fn note_slow(&mut self, id: &str, time: Duration) {
        const KEEP: usize = 10;
        if self.slowest.len() < KEEP || self.slowest.last().is_some_and(|(least, _)| time > *least)
        {
            self.slowest.push((time, id.to_string()));
            self.slowest.sort_by(|left, right| right.0.cmp(&left.0));
            self.slowest.truncate(KEEP);
        }
    }
}

/// A lost oracle, which the runner answers by restarting it.
#[derive(Debug)]
pub struct OracleLost(pub String);

/// Runs cases for one test thread.
pub struct Runner {
    arm: Arm,
    root: PathBuf,
    program: Option<PathBuf>,
    oracle: Option<Driver>,
    fixtures: HashMap<String, Result<PathBuf, Failure>>,
    use_fixtures: bool,
    batches: usize,
    /// What this runner did.
    pub stats: Stats,
}

impl Runner {
    /// A runner at one arm, with its scratch directory under `root`.
    ///
    /// @param arm - the file configuration
    /// @param root - a directory this runner owns alone
    pub fn new(arm: Arm, root: &Path) -> Runner {
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::create_dir_all(root);
        Runner {
            arm,
            root: root.to_path_buf(),
            program: crate::differential::sqlite_oracle(),
            oracle: None,
            fixtures: HashMap::new(),
            use_fixtures: std::env::var("INILLUCENT_MATRIX_FIXTURES").as_deref() != Ok("off"),
            batches: 0,
            stats: Stats::default(),
        }
    }

    /// Whether the pinned SQLite oracle is built on this machine.
    pub fn has_oracle(&self) -> bool {
        self.program.is_some()
    }

    /// The arm this runner runs at.
    pub fn arm(&self) -> &Arm {
        &self.arm
    }

    /// Ends the runner's use of its oracle, so the stats include its cost.
    pub fn finish(&mut self) {
        self.retire_oracle();
    }

    /// Whether a case may run at this runner's arm.
    fn runs_here(&self, case: &Case) -> bool {
        match &case.arms {
            None => true,
            Some(arms) => arms
                .iter()
                .any(|arm| arm.replace('_', "-") == self.arm.name),
        }
    }

    /// Runs every case, sharing a fixture copy between the read only cases
    /// that have the same setup, and returns a verdict per case in order.
    ///
    /// @param cases - the cases
    pub fn run_all(&mut self, cases: &[&Case]) -> Vec<Verdict> {
        let mut verdicts: Vec<Option<Verdict>> = cases.iter().map(|_| None).collect();
        let mut batchable: Vec<usize> = Vec::new();
        for (index, case) in cases.iter().enumerate() {
            if !self.runs_here(case) {
                if let Some(slot) = verdicts.get_mut(index) {
                    *slot = Some(Verdict::Skipped(format!(
                        "not run at the {} arm",
                        self.arm.name
                    )));
                }
            } else if self.use_fixtures && !case.setup.is_empty() && !case.writes() {
                // Only a case that reads shares a merged fixture. A case that
                // writes starts from a copy of its own setup's fixture: a copy
                // of a merged one holds every member's tables, and its reopen
                // and integrity check then read all of them, which measured
                // slower than building the case's own fixture.
                batchable.push(index);
            }
        }
        for union in crate::statement_matrix::union::group(cases, &batchable) {
            self.run_union(cases, &union, &mut verdicts);
        }
        cases
            .iter()
            .zip(verdicts)
            .map(|(case, verdict)| verdict.unwrap_or_else(|| self.run(case)))
            .collect()
    }

    /// Runs the members of one merged fixture: the members that only read as
    /// one batch on one copy of it, and each member that writes on a copy of
    /// its own.
    ///
    /// If the merged setup itself disagrees, each member runs again from a
    /// fixture of its own setup, so one member's broken setup is reported as
    /// that member's failure and not as every member's.
    fn run_union(
        &mut self,
        cases: &[&Case],
        union: &crate::statement_matrix::union::Union,
        verdicts: &mut [Option<Verdict>],
    ) {
        let shared = Setup {
            records: &union.setup,
            oracle: union.oracle,
            capabilities: &union.capabilities,
        };
        let (readers, writers): (Vec<usize>, Vec<usize>) = union
            .members
            .iter()
            .copied()
            .partition(|at| cases.get(*at).is_some_and(|case| !case.writes()));
        let reading: Vec<&Case> = readers
            .iter()
            .filter_map(|at| cases.get(*at).copied())
            .collect();
        let mut results: Vec<(usize, Verdict)> = readers
            .iter()
            .copied()
            .zip(self.run_batch(&reading, &shared))
            .collect();
        for at in &writers {
            if let Some(case) = cases.get(*at) {
                results.push((*at, self.run_on(case, Some(&shared))));
            }
        }
        let shared_failed =
            union.members.len() > 1 && results.iter().all(|(_, verdict)| broken_setup(verdict));
        for (at, verdict) in results {
            let verdict = match (shared_failed, cases.get(at)) {
                (true, Some(case)) => self.run(case),
                _ => verdict,
            };
            if let Some(slot) = verdicts.get_mut(at) {
                *slot = Some(verdict);
            }
        }
    }

    /// Runs one case on a directory of its own and grades it.
    ///
    /// @param case - the case
    pub fn run(&mut self, case: &Case) -> Verdict {
        self.run_on(case, None)
    }

    /// Runs one case on a directory of its own, from a copy of the given
    /// setup's fixture, or of its own setup's when none is given.
    ///
    /// @param case - the case
    /// @param setup - a merged setup the case's own is part of
    fn run_on(&mut self, case: &Case, setup: Option<&Setup<'_>>) -> Verdict {
        if !self.runs_here(case) {
            return Verdict::Skipped(format!("not run at the {} arm", self.arm.name));
        }
        let started = Instant::now();
        self.stats.cases = self.stats.cases.saturating_add(1);
        trace(&case.id);
        let directory = self.root.join(&case.id);
        let verdict = self.with_replay(case, &directory, |runner| runner.attempt(case, setup));
        self.stats.record_case(&case.id, started.elapsed());
        if matches!(verdict, Verdict::Passed) {
            let _ = std::fs::remove_dir_all(&directory);
        }
        verdict
    }

    /// Runs an attempt, replaying it once on a new oracle process if the
    /// oracle is lost, and turning a panic into a failure.
    ///
    /// A second loss is the oracle's failure, reported under its own kind so
    /// nobody reads it as the engine's.
    fn with_replay(
        &mut self,
        case: &Case,
        directory: &Path,
        mut attempt: impl FnMut(&mut Runner) -> Result<Vec<Failure>, OracleLost>,
    ) -> Verdict {
        let mut lost = Vec::new();
        for _ in 0..2 {
            let outcome = catch_unwind(AssertUnwindSafe(|| attempt(self)));
            match outcome {
                Ok(Ok(failures)) if failures.is_empty() => return Verdict::Passed,
                Ok(Ok(failures)) => return Verdict::Failed(failures),
                Ok(Err(OracleLost(reason))) => {
                    self.retire_oracle();
                    lost.push(reason);
                }
                Err(payload) => {
                    // The oracle may be in the middle of the case; retire it so
                    // the next case starts on a clean process.
                    self.retire_oracle();
                    return Verdict::Failed(vec![Failure {
                        case: case.id.clone(),
                        kind: Kind::Panic,
                        statement: None,
                        sql: String::new(),
                        detail: panic_message(&payload),
                        directory: directory.to_path_buf(),
                    }]);
                }
            }
        }
        Verdict::Failed(vec![Failure {
            case: case.id.clone(),
            kind: Kind::Oracle,
            statement: None,
            sql: String::new(),
            detail: format!("the oracle was lost twice: {}", lost.join("; then ")),
            directory: directory.to_path_buf(),
        }])
    }

    /// One attempt at a case on a directory of its own.
    fn attempt(
        &mut self,
        case: &Case,
        setup: Option<&Setup<'_>>,
    ) -> Result<Vec<Failure>, OracleLost> {
        let _budget = case_budget();
        let directory = self.root.join(&case.id);
        let _ = std::fs::remove_dir_all(&directory);
        let _ = std::fs::create_dir_all(&directory);
        let mut setup_in_case: &[Record] = &case.setup;
        if !case.setup.is_empty() && self.use_fixtures {
            let fixture = match setup {
                Some(shared) => self.fixture_for(shared, case)?,
                None => self.fixture(case)?,
            };
            match fixture {
                Ok(fixture) => {
                    self.copy_fixture(&fixture, &directory);
                    setup_in_case = &[];
                }
                Err(failure) => {
                    return Ok(vec![Failure {
                        case: case.id.clone(),
                        ..failure
                    }])
                }
            }
        }
        let mut script: Vec<&Record> = setup_in_case.iter().collect();
        script.extend(case.records.iter());
        let mut failures = Vec::new();
        let mut context = CaseContext {
            case,
            directory: &directory,
            graded: case.oracle && self.program.is_some(),
            counters: !mentions_a_module(case),
            setup_count: setup_in_case.len(),
            module_pending: false,
            halted: false,
            failures: &mut failures,
        };
        self.run_script(&script, &mut context)?;
        if (case.writes() || context.setup_count > 0) && !context.halted {
            self.after_reopen(&script, &mut context)?;
        }
        if context.graded {
            self.close_oracle()?;
        }
        Ok(failures)
    }

    /// Runs a script's records, reopening both engines at each `reopen`, and
    /// checks the case's properties at the end.
    fn run_script(
        &mut self,
        script: &[&Record],
        context: &mut CaseContext<'_>,
    ) -> Result<(), OracleLost> {
        let sqlite = context.directory.join("sqlite.db");
        let ours = context.directory.join("inillucent.rdb");
        if context.graded {
            self.open_oracle(&sqlite)?;
        }
        let mut position = 0usize;
        loop {
            let database = match self.arm.open(&ours) {
                Ok(database) => database,
                Err(error) => {
                    context.fail(
                        Kind::Outcome,
                        None,
                        "",
                        format!("inillucent did not open: {error}"),
                    );
                    return Ok(());
                }
            };
            let connection = database.session();
            let mut reopen = false;
            while let Some(record) = script.get(position) {
                position = position.saturating_add(1);
                if matches!(record, Record::Reopen) {
                    reopen = true;
                    break;
                }
                self.step(&connection, record, position.saturating_sub(1), context)?;
            }
            if !reopen {
                check_properties(&connection, context);
                return Ok(());
            }
            drop(connection);
            drop(database);
            if context.graded {
                self.reopen_oracle(&sqlite)?;
            }
        }
    }

    /// Runs read only cases that share a fixture against one copy of it.
    ///
    /// A panic in one case is that case's failure; both engines are then
    /// opened again for the rest, so a case after it does not run on a
    /// connection the panic left in an unknown state.
    fn run_batch(&mut self, cases: &[&Case], setup: &Setup<'_>) -> Vec<Verdict> {
        let Some(first) = cases.first() else {
            return Vec::new();
        };
        for case in cases {
            trace(&case.id);
        }
        // The fixture's statements get a case's budget; each case then arms its own.
        let fixture_budget = case_budget();
        self.batches = self.batches.saturating_add(1);
        let directory = self.root.join(format!("batch-{}", self.batches));
        let _ = std::fs::remove_dir_all(&directory);
        let _ = std::fs::create_dir_all(&directory);
        let fixture = match self.fixture_for(setup, first) {
            Ok(Ok(fixture)) => fixture,
            Ok(Err(failure)) => return fixture_failures(cases, &failure),
            Err(OracleLost(reason)) => {
                self.retire_oracle();
                return cases
                    .iter()
                    .map(|case| oracle_failure(case, &reason, &directory))
                    .collect();
            }
        };
        drop(fixture_budget);
        self.copy_fixture(&fixture, &directory);
        let mut verdicts = Vec::with_capacity(cases.len());
        while verdicts.len() < cases.len() {
            let before = verdicts.len();
            self.run_batch_session(cases, &directory, &mut verdicts);
            if verdicts.len() == before {
                break;
            }
        }
        if verdicts
            .iter()
            .all(|verdict| matches!(verdict, Verdict::Passed))
        {
            let _ = std::fs::remove_dir_all(&directory);
        }
        verdicts
    }

    /// Opens both engines on a batch's copy and runs cases from where the
    /// batch has got to, until the end or a panic.
    fn run_batch_session(
        &mut self,
        cases: &[&Case],
        directory: &Path,
        verdicts: &mut Vec<Verdict>,
    ) {
        let ours = directory.join("inillucent.rdb");
        let database = match self.arm.open(&ours) {
            Ok(database) => database,
            Err(error) => {
                while verdicts.len() < cases.len() {
                    let case = cases.get(verdicts.len()).copied();
                    verdicts.push(Verdict::Failed(vec![Failure {
                        case: case.map(|case| case.id.clone()).unwrap_or_default(),
                        kind: Kind::Outcome,
                        statement: None,
                        sql: String::new(),
                        detail: format!("inillucent did not open the batch's copy: {error}"),
                        directory: directory.to_path_buf(),
                    }]));
                }
                return;
            }
        };
        let connection = database.session();
        let mut oracle_open = false;
        let first = verdicts.len();
        while let Some(case) = cases.get(verdicts.len()).copied() {
            let started = Instant::now();
            self.stats.cases = self.stats.cases.saturating_add(1);
            self.stats.batched = self.stats.batched.saturating_add(1);
            let verdict = self.with_replay(case, directory, |runner| {
                runner.batched_case(&connection, case, directory, &mut oracle_open)
            });
            self.stats.record_case(&case.id, started.elapsed());
            let panicked = matches!(&verdict, Verdict::Failed(failures)
                if failures.iter().any(|failure| failure.kind == Kind::Panic));
            verdicts.push(verdict);
            if panicked {
                return;
            }
        }
        if oracle_open {
            let _ = self.close_oracle();
        }
        self.batch_properties(&connection, cases, directory, first, verdicts);
    }

    /// Checks the properties of every case a batch session ran, after all of
    /// its comparisons.
    ///
    /// **After, and not case by case, because this engine counts `changes()`
    /// per database rather than per session.** A property's writes are rolled
    /// back, but they move the counter, and on this engine they move it for
    /// every session of the database, so the next case in the batch compared a
    /// counter the check had set. The rows are the same before and after a
    /// rolled back check, so checking at the end asks the same questions.
    fn batch_properties(
        &mut self,
        connection: &Connection<'_>,
        cases: &[&Case],
        directory: &Path,
        first: usize,
        verdicts: &mut [Verdict],
    ) {
        for (at, case) in cases.iter().enumerate().skip(first) {
            let _budget = case_budget();
            let mut failures = Vec::new();
            let mut context = CaseContext {
                case,
                directory,
                graded: false,
                counters: false,
                setup_count: 0,
                module_pending: false,
                halted: false,
                failures: &mut failures,
            };
            let checked = catch_unwind(AssertUnwindSafe(|| {
                check_properties(connection, &mut context)
            }));
            if checked.is_err() {
                failures.push(Failure {
                    case: case.id.clone(),
                    kind: Kind::Panic,
                    statement: None,
                    sql: String::new(),
                    detail: "inillucent panicked checking a property".to_string(),
                    directory: directory.to_path_buf(),
                });
            }
            if failures.is_empty() {
                continue;
            }
            if let Some(verdict) = verdicts.get_mut(at) {
                *verdict = match std::mem::replace(verdict, Verdict::Passed) {
                    Verdict::Failed(mut earlier) => {
                        earlier.extend(failures);
                        Verdict::Failed(earlier)
                    }
                    _ => Verdict::Failed(failures),
                };
            }
        }
    }

    /// Runs one read only case on a batch's open connection.
    fn batched_case(
        &mut self,
        connection: &Connection<'_>,
        case: &Case,
        directory: &Path,
        oracle_open: &mut bool,
    ) -> Result<Vec<Failure>, OracleLost> {
        let _budget = case_budget();
        let graded = case.oracle && self.program.is_some();
        if graded && (!*oracle_open || self.oracle.is_none()) {
            self.open_oracle(&directory.join("sqlite.db"))?;
            *oracle_open = true;
        }
        let mut failures = Vec::new();
        let mut context = CaseContext {
            case,
            directory,
            graded,
            counters: !mentions_a_module(case),
            setup_count: 0,
            module_pending: false,
            halted: false,
            failures: &mut failures,
        };
        for (index, record) in case.records.iter().enumerate() {
            self.step(connection, record, index, &mut context)?;
        }
        // The properties are checked by `batch_properties`, after every case
        // in the session has been compared.
        Ok(failures)
    }

    /// Runs one record on both engines and grades it.
    fn step(
        &mut self,
        connection: &Connection<'_>,
        record: &Record,
        index: usize,
        context: &mut CaseContext<'_>,
    ) -> Result<(), OracleLost> {
        if record.sql().is_none() || context.halted {
            return Ok(());
        }
        let asked = self.ask_record(connection, record, context)?;
        grade(record, &asked, index, context, false);
        Ok(())
    }

    /// Asks both engines one record: with its bound values when it has
    /// them (see `bind.rs`), and as written otherwise.
    fn ask_record(
        &mut self,
        connection: &Connection<'_>,
        record: &Record,
        context: &CaseContext<'_>,
    ) -> Result<Asked, OracleLost> {
        let sql = record.sql().unwrap_or("");
        match record {
            Record::Query { binds, .. } if !binds.is_empty() => {
                self.ask_bound(connection, sql, binds, context)
            }
            _ => self.ask_both(connection, sql, context),
        }
    }

    /// Runs a statement with bound values on inillucent, one prepared
    /// statement for every run, and on the oracle with the values written in
    /// as literals, one statement per run. The rows of every run are kept in
    /// run order; the counters are the last run's.
    fn ask_bound(
        &mut self,
        connection: &Connection<'_>,
        sql: &str,
        runs: &[Vec<String>],
        context: &CaseContext<'_>,
    ) -> Result<Asked, OracleLost> {
        self.stats.statements = self.stats.statements.saturating_add(runs.len());
        let ours = localize(sql, context.directory, "inillucent");
        let (candidate, error) =
            crate::statement_matrix::bind::observe_bound(connection, &ours, runs);
        let unsupported = error
            .as_ref()
            .and_then(|error| error.unsupported().map(str::to_string));
        let status = error.as_ref().map(status_name);
        let reference = if context.graded {
            let theirs = localize(sql, context.directory, "sqlite");
            let mut merged: Option<Observation> = None;
            for run in runs {
                let literal = crate::statement_matrix::bind::substitute(&theirs, run);
                let literal =
                    crate::statement_matrix::limited::rewrite(&literal).unwrap_or(literal);
                let answer = self.oracle_send(&Op::Query(literal))?;
                merged = Some(match merged {
                    None => answer,
                    Some(mut earlier) if earlier.ok && answer.ok => {
                        earlier.rows.extend(answer.rows);
                        Observation {
                            rows: earlier.rows,
                            ..answer
                        }
                    }
                    Some(earlier) if !earlier.ok => earlier,
                    Some(_) => answer,
                });
                if merged.as_ref().is_some_and(|answer| !answer.ok) {
                    break;
                }
            }
            merged
        } else {
            None
        };
        Ok(Asked {
            candidate,
            unsupported,
            status,
            reference,
        })
    }

    /// Runs one record's SQL on inillucent and, when the case is graded, on
    /// the oracle.
    ///
    /// Text holding several statements goes to the oracle as `exec`, which
    /// runs them all, because `query` prepares only the first; one statement
    /// goes as `query`, which reports rows and column names for anything that
    /// returns them. `%SCRATCH%` in the text becomes a directory of each
    /// engine's own inside the case's directory, so a `VACUUM INTO` or an
    /// `ATTACH` of a named file never has the two engines, or two cases,
    /// writing the same file.
    fn ask_both(
        &mut self,
        connection: &Connection<'_>,
        sql: &str,
        context: &CaseContext<'_>,
    ) -> Result<Asked, OracleLost> {
        self.stats.statements = self.stats.statements.saturating_add(1);
        let single = split_statements(sql).len() == 1;
        let ours = localize(sql, context.directory, "inillucent");
        let (candidate, error) = observe_detailed(connection, &ours, single);
        let unsupported = error
            .as_ref()
            .and_then(|error| error.unsupported().map(str::to_string));
        let status = error.as_ref().map(status_name);
        let reference = if context.graded {
            let theirs = localize(sql, context.directory, "sqlite");
            let op = if single {
                // The pinned build has no `DELETE ... LIMIT`; see `limited.rs`.
                Op::Query(crate::statement_matrix::limited::rewrite(&theirs).unwrap_or(theirs))
            } else {
                Op::Exec(theirs)
            };
            Some(self.oracle_send(&op)?)
        } else {
            None
        };
        Ok(Asked {
            candidate,
            unsupported,
            status,
            reference,
        })
    }

    /// After a case that wrote: reopen both engines, ask every read again,
    /// then check integrity on both.
    fn after_reopen(
        &mut self,
        script: &[&Record],
        context: &mut CaseContext<'_>,
    ) -> Result<(), OracleLost> {
        if context.graded {
            self.reopen_oracle(&context.directory.join("sqlite.db"))?;
        }
        let database = match self.arm.open(&context.directory.join("inillucent.rdb")) {
            Ok(database) => database,
            Err(error) => {
                context.fail(
                    Kind::Reopen,
                    None,
                    "",
                    format!("inillucent did not reopen: {error}"),
                );
                return Ok(());
            }
        };
        let connection = database.session();
        if context.graded {
            for (index, record) in script.iter().enumerate() {
                let Record::Query { sql, .. } = record else {
                    continue;
                };
                if is_read_only(sql) {
                    let asked = self.ask_record(&connection, record, context)?;
                    grade(record, &asked, index, context, true);
                }
            }
        }
        self.check_integrity(&connection, context)
    }

    /// Runs `PRAGMA integrity_check` on both engines; each must answer `ok`.
    fn check_integrity(
        &mut self,
        connection: &Connection<'_>,
        context: &mut CaseContext<'_>,
    ) -> Result<(), OracleLost> {
        let ours = observe_detailed(connection, "PRAGMA integrity_check", true).0;
        if !integrity_ok(&ours) {
            let detail = format!("inillucent answered {:?} {}", ours.rows, ours.message);
            context.fail(Kind::Integrity, None, "PRAGMA integrity_check", detail);
        }
        if context.graded {
            let theirs = self.oracle_send(&Op::Query("PRAGMA integrity_check".to_string()))?;
            if !integrity_ok(&theirs) {
                let detail = format!("SQLite answered {:?} {}", theirs.rows, theirs.message);
                context.fail(Kind::Integrity, None, "PRAGMA integrity_check", detail);
            }
        }
        Ok(())
    }

    /// Copies a fixture into a case's directory, counting the cost.
    fn copy_fixture(&mut self, fixture: &Path, directory: &Path) {
        let copied = Instant::now();
        copy_directory(fixture, directory);
        self.stats.copy_time += copied.elapsed();
        self.stats.fixture_copies = self.stats.fixture_copies.saturating_add(1);
    }

    /// Returns the fixture directory for a case's setup, building it once.
    ///
    /// The outer `Result` is the oracle; the inner one is whether the setup
    /// agreed, which is remembered so a broken setup is reported once per case
    /// without being rebuilt for each. A fixture is reopened and checked with
    /// `PRAGMA integrity_check` once, when it is built, because the cases that
    /// share it read it without writing and so are not checked themselves.
    fn fixture(&mut self, case: &Case) -> Result<Result<PathBuf, Failure>, OracleLost> {
        let setup = Setup {
            records: &case.setup,
            oracle: case.oracle,
            capabilities: &case.capabilities,
        };
        self.fixture_for(&setup, case)
    }

    /// Returns the fixture directory for a setup, building it once.
    ///
    /// @param setup - the setup, with its oracle flag and capability rows
    /// @param case - the case the fixture is first built for, named in a
    ///   failure and in the slow list
    fn fixture_for(
        &mut self,
        setup: &Setup<'_>,
        case: &Case,
    ) -> Result<Result<PathBuf, Failure>, OracleLost> {
        let key = setup_key(setup.records, setup.oracle);
        if let Some(found) = self.fixtures.get(&key) {
            return Ok(found.clone());
        }
        let built = Instant::now();
        let directory = self.root.join("fixtures").join(&key);
        let _ = std::fs::remove_dir_all(&directory);
        let _ = std::fs::create_dir_all(&directory);
        let mut setup_case = Case::new(&case.family, &case.origin);
        setup_case.id = format!("fixture {key}");
        setup_case.oracle = setup.oracle;
        setup_case.capabilities = setup.capabilities.to_vec();
        setup_case.records = in_one_transaction(setup.records);
        let script: Vec<&Record> = setup_case.records.iter().collect();
        let mut failures = Vec::new();
        let mut context = CaseContext {
            case: &setup_case,
            directory: &directory,
            graded: setup.oracle && self.program.is_some(),
            counters: !mentions_a_module(&setup_case),
            setup_count: script.len(),
            module_pending: false,
            halted: false,
            failures: &mut failures,
        };
        self.run_script(&script, &mut context)?;
        self.after_reopen(&[], &mut context)?;
        if context.graded {
            self.close_oracle()?;
        }
        self.stats.fixtures_built = self.stats.fixtures_built.saturating_add(1);
        self.stats.fixture_time += built.elapsed();
        self.stats
            .note_slow(&format!("fixture for {}", case.id), built.elapsed());
        let result = match failures.into_iter().next() {
            None => Ok(directory),
            Some(first) => Err(Failure {
                kind: Kind::Fixture,
                detail: format!("the shared setup disagreed: {}", first.render()),
                ..first
            }),
        };
        self.fixtures.insert(key, result.clone());
        Ok(result)
    }

    /// Stops using the oracle process, adding what it cost to the stats.
    fn retire_oracle(&mut self) {
        if let Some(driver) = self.oracle.take() {
            self.stats.oracle_cpu += Duration::from_nanos(driver.cost().cpu_nanos());
        }
    }

    /// Starts the oracle if it is not running.
    fn ensure_oracle(&mut self) -> Result<&mut Driver, OracleLost> {
        if self.oracle.is_none() {
            let program = self
                .program
                .clone()
                .ok_or_else(|| OracleLost("the oracle is not built".to_string()))?;
            let mut driver = Driver::start("sqlite", &program).map_err(OracleLost)?;
            let hello = driver.send(&Op::Hello).map_err(OracleLost)?;
            if !hello.ok {
                return Err(OracleLost("the oracle did not answer hello".to_string()));
            }
            self.stats.oracle_starts = self.stats.oracle_starts.saturating_add(1);
            self.oracle = Some(driver);
        }
        self.oracle
            .as_mut()
            .ok_or_else(|| OracleLost("the oracle is not running".to_string()))
    }

    /// Sends one command, treating a broken pipe as a lost oracle.
    fn oracle_send(&mut self, op: &Op) -> Result<Observation, OracleLost> {
        let driver = self.ensure_oracle()?;
        match driver.send(op) {
            Ok(observation) => Ok(observation),
            Err(error) => {
                self.retire_oracle();
                Err(OracleLost(error))
            }
        }
    }

    /// Opens the oracle's database at a path.
    fn open_oracle(&mut self, path: &Path) -> Result<(), OracleLost> {
        let opened = self.oracle_send(&Op::Open(path.display().to_string()))?;
        if !opened.ok {
            return Err(OracleLost(format!(
                "the oracle could not open {}: {}",
                path.display(),
                opened.message
            )));
        }
        Ok(())
    }

    /// Closes the oracle's database.
    fn close_oracle(&mut self) -> Result<(), OracleLost> {
        let closed = self.oracle_send(&Op::Close)?;
        if !closed.ok {
            return Err(OracleLost(
                "the oracle would not close its database".to_string(),
            ));
        }
        Ok(())
    }

    /// Closes the oracle's database and opens it again.
    fn reopen_oracle(&mut self, path: &Path) -> Result<(), OracleLost> {
        self.close_oracle()?;
        self.open_oracle(path)
    }
}

/// Checks a case's properties on inillucent and records each violation.
fn check_properties(connection: &Connection<'_>, context: &mut CaseContext<'_>) {
    if context.halted {
        return;
    }
    let case = context.case;
    for violation in properties::check(connection, &case.properties) {
        if violation.unsupported.is_some() && context.gap_allowed() {
            continue;
        }
        let detail = format!("{}: {}", violation.name, violation.detail);
        context.fail(Kind::Property, None, "", detail);
    }
}

/// A setup to build a fixture from: its records, whether the oracle builds
/// it too, and the capability rows the cases it serves name.
pub struct Setup<'a> {
    /// The setup statements.
    pub records: &'a [Record],
    /// Whether the oracle builds it too.
    pub oracle: bool,
    /// The capability rows the cases it serves name.
    pub capabilities: &'a [String],
}

/// The key a case's fixture is stored under: its setup and whether the oracle
/// builds it too.
///
/// @param case - the case
pub fn fixture_key(case: &Case) -> String {
    setup_key(&case.setup, case.oracle)
}

/// The key a setup's fixture is stored under.
///
/// @param records - the setup
/// @param oracle - whether the oracle builds it too
pub fn setup_key(records: &[Record], oracle: bool) -> String {
    let mut text = String::new();
    for record in records {
        match record.sql() {
            Some(sql) => text.push_str(sql),
            None => text.push_str("\u{2}reopen"),
        }
        text.push('\u{1}');
    }
    text.push_str(if oracle { "oracle" } else { "alone" });
    let key = crate::hash::sha3_256_hex(text.as_bytes());
    key.get(..16).unwrap_or(&key).to_string()
}

/// Wraps a fixture's setup in one transaction when every statement in it only
/// creates schema or inserts rows.
///
/// A commit waits on the disk, and on a machine running twenty four test
/// threads that wait was most of the matrix's time: phase 0 measured a four
/// statement fixture at about 110 ms alone and about 4 s with every thread
/// committing at once. A transaction makes the whole setup one commit. The
/// file it leaves is the same, which is all a fixture is for, and the case
/// reads it only after the reopen that follows. A setup holding anything else,
/// such as a `PRAGMA` or its own `BEGIN`, runs as written.
///
/// @param setup - the setup records
fn in_one_transaction(setup: &[Record]) -> Vec<Record> {
    let wrappable = setup.len() > 1
        && setup.iter().all(|record| match record {
            Record::Statement {
                expect: crate::statement_matrix::case::Expect::Ok,
                sql,
            } => creates_or_inserts(sql),
            _ => false,
        });
    if !wrappable {
        return setup.to_vec();
    }
    let mut wrapped = Vec::with_capacity(setup.len().saturating_add(2));
    wrapped.push(Record::ok("BEGIN"));
    wrapped.extend(setup.iter().cloned());
    wrapped.push(Record::ok("COMMIT"));
    wrapped
}

/// Whether a statement creates a table, index, view or trigger, or inserts.
fn creates_or_inserts(sql: &str) -> bool {
    let upper = sql.trim_start().to_ascii_uppercase();
    let words: Vec<&str> = upper.split_whitespace().take(3).collect();
    match words.as_slice() {
        ["INSERT", "INTO", ..] => true,
        ["CREATE", second, ..] => matches!(
            *second,
            "TABLE" | "INDEX" | "UNIQUE" | "VIEW" | "TRIGGER" | "VIRTUAL"
        ),
        ["ANALYZE", ..] | ["ALTER", "TABLE", ..] => true,
        _ => false,
    }
}

/// The key that decides which group and shard runs a case.
///
/// A read only case with a setup goes by its fixture's key, so every case that
/// shares one open copy of a fixture is run by the one runner that builds it.
/// Every other case goes by its own id: it copies its fixture and reopens its
/// own files, and placing a whole file of writing cases on one runner, which
/// the fixture key did before this, left one thread doing most of the work
/// while the rest waited.
///
/// @param case - the case
pub fn placement_key(case: &Case) -> String {
    if case.setup.is_empty() || case.writes() {
        case.id.clone()
    } else {
        fixture_key(case)
    }
}

/// Whether a verdict is a failure of the setup the case was run from.
fn broken_setup(verdict: &Verdict) -> bool {
    matches!(verdict, Verdict::Failed(failures)
        if failures.iter().any(|failure| failure.kind == Kind::Fixture))
}

/// The same fixture failure for every case of a batch.
fn fixture_failures(cases: &[&Case], failure: &Failure) -> Vec<Verdict> {
    cases
        .iter()
        .map(|case| {
            Verdict::Failed(vec![Failure {
                case: case.id.clone(),
                ..failure.clone()
            }])
        })
        .collect()
}

/// An oracle failure for one case.
fn oracle_failure(case: &Case, reason: &str, directory: &Path) -> Verdict {
    Verdict::Failed(vec![Failure {
        case: case.id.clone(),
        kind: Kind::Oracle,
        statement: None,
        sql: String::new(),
        detail: format!("the oracle was lost building the fixture: {reason}"),
        directory: directory.to_path_buf(),
    }])
}

/// Replaces `%SCRATCH%` with a directory of one engine's own inside the case's
/// directory, making it on first use.
///
/// @param sql - the statement
/// @param directory - the case's directory
/// @param engine - `sqlite` or `inillucent`
pub fn localize(sql: &str, directory: &Path, engine: &str) -> String {
    if !sql.contains("%SCRATCH%") {
        return sql.to_string();
    }
    let place = directory.join(format!("{engine}-scratch"));
    let _ = std::fs::create_dir_all(&place);
    sql.replace("%SCRATCH%", &place.display().to_string().replace('\\', "/"))
}

/// The driver's status name for an engine error.
fn status_name(error: &inillucent_base::DbError) -> String {
    inillucent_driver::Error::from_engine(error, false)
        .status
        .name()
        .to_string()
}

/// Whether a case creates or uses a virtual table, which stops the cumulative
/// counters being comparable: they then count the module's own statements.
fn mentions_a_module(case: &Case) -> bool {
    case.setup
        .iter()
        .chain(case.records.iter())
        .filter_map(Record::sql)
        .any(|sql| sql.to_ascii_uppercase().contains("VIRTUAL TABLE"))
}

/// Copies every file in one directory into another.
fn copy_directory(from: &Path, to: &Path) {
    let _ = std::fs::create_dir_all(to);
    let Ok(listing) = std::fs::read_dir(from) else {
        return;
    };
    for entry in listing.flatten() {
        let source = entry.path();
        if source.is_file() {
            let _ = std::fs::copy(&source, to.join(entry.file_name()));
        }
    }
}

/// The text of a panic's payload.
fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        return format!("inillucent panicked: {text}");
    }
    if let Some(text) = payload.downcast_ref::<String>() {
        return format!("inillucent panicked: {text}");
    }
    "inillucent panicked with a payload that is not text".to_string()
}

/// Prints a case id to standard error before it runs, when
/// `INILLUCENT_MATRIX_TRACE` is set, so a case that never finishes can be
/// named: the runner's own report comes only at the end of a group.
///
/// @param id - the case about to run
fn trace(id: &str) {
    if std::env::var_os("INILLUCENT_MATRIX_TRACE").is_some() {
        eprintln!("matrix: running {id}");
    }
}

/// What one case may spend on inillucent: five million rows, 1 GiB of row
/// data and two minutes, counted across every statement the case runs.
///
/// **A case that runs away has to fail, not grow.** Run against an older
/// engine for section 11.1 of the design, one matrix process reached 66 GB and
/// another 34 GB before they were stopped by hand, with 1.6 GB of the machine
/// left. The largest case here reads a few thousand rows, so these bounds are
/// three orders of magnitude above any honest answer. A statement past one
/// fails with the engine's budget error, and the case reports it as a
/// difference. `inillucent-testrun` also caps each test process's memory, for
/// the growth this cannot see.
fn case_budget() -> inillucent_base::budget::Guard {
    let limits = inillucent_base::budget::Limits {
        rows: Some(5_000_000),
        bytes: Some(1024 * 1024 * 1024),
        time: Some(Duration::from_secs(120)),
    };
    inillucent_base::budget::arm(
        limits,
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )
}
