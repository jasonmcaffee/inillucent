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

use std::collections::{BTreeMap, HashMap};
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
        let mut batches: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (index, case) in cases.iter().enumerate() {
            if !self.runs_here(case) {
                if let Some(slot) = verdicts.get_mut(index) {
                    *slot = Some(Verdict::Skipped(format!(
                        "not run at the {} arm",
                        self.arm.name
                    )));
                }
            } else if self.use_fixtures && !case.setup.is_empty() && !case.writes() {
                batches.entry(fixture_key(case)).or_default().push(index);
            }
        }
        for members in batches.values() {
            let batch: Vec<&Case> = members
                .iter()
                .filter_map(|at| cases.get(*at).copied())
                .collect();
            for (at, verdict) in members.iter().zip(self.run_batch(&batch)) {
                if let Some(slot) = verdicts.get_mut(*at) {
                    *slot = Some(verdict);
                }
            }
        }
        cases
            .iter()
            .zip(verdicts)
            .map(|(case, verdict)| verdict.unwrap_or_else(|| self.run(case)))
            .collect()
    }

    /// Runs one case on a directory of its own and grades it.
    ///
    /// @param case - the case
    pub fn run(&mut self, case: &Case) -> Verdict {
        if !self.runs_here(case) {
            return Verdict::Skipped(format!("not run at the {} arm", self.arm.name));
        }
        let started = Instant::now();
        self.stats.cases = self.stats.cases.saturating_add(1);
        let directory = self.root.join(&case.id);
        let verdict = self.with_replay(case, &directory, |runner| runner.attempt(case));
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
    fn attempt(&mut self, case: &Case) -> Result<Vec<Failure>, OracleLost> {
        let directory = self.root.join(&case.id);
        let _ = std::fs::remove_dir_all(&directory);
        let _ = std::fs::create_dir_all(&directory);
        let mut setup_in_case: &[Record] = &case.setup;
        if !case.setup.is_empty() && self.use_fixtures {
            match self.fixture(case)? {
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
            failures: &mut failures,
        };
        self.run_script(&script, &mut context)?;
        if case.writes() || context.setup_count > 0 {
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
    fn run_batch(&mut self, cases: &[&Case]) -> Vec<Verdict> {
        let Some(first) = cases.first() else {
            return Vec::new();
        };
        self.batches = self.batches.saturating_add(1);
        let directory = self.root.join(format!("batch-{}", self.batches));
        let _ = std::fs::remove_dir_all(&directory);
        let _ = std::fs::create_dir_all(&directory);
        let fixture = match self.fixture(first) {
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
    }

    /// Runs one read only case on a batch's open connection.
    fn batched_case(
        &mut self,
        connection: &Connection<'_>,
        case: &Case,
        directory: &Path,
        oracle_open: &mut bool,
    ) -> Result<Vec<Failure>, OracleLost> {
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
            failures: &mut failures,
        };
        for (index, record) in case.records.iter().enumerate() {
            self.step(connection, record, index, &mut context)?;
        }
        check_properties(connection, &mut context);
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
        let Some(sql) = record.sql() else {
            return Ok(());
        };
        let asked = self.ask_both(connection, sql, context)?;
        grade(record, &asked, index, context, false);
        Ok(())
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
                    let asked = self.ask_both(&connection, sql, context)?;
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
        let key = fixture_key(case);
        if let Some(found) = self.fixtures.get(&key) {
            return Ok(found.clone());
        }
        let built = Instant::now();
        let directory = self.root.join("fixtures").join(&key);
        let _ = std::fs::remove_dir_all(&directory);
        let _ = std::fs::create_dir_all(&directory);
        let mut setup_case = Case::new(&case.family, &case.origin);
        setup_case.id = format!("fixture {key}");
        setup_case.oracle = case.oracle;
        setup_case.capabilities = case.capabilities.clone();
        setup_case.records = in_one_transaction(&case.setup);
        let script: Vec<&Record> = setup_case.records.iter().collect();
        let mut failures = Vec::new();
        let mut context = CaseContext {
            case: &setup_case,
            directory: &directory,
            graded: case.oracle && self.program.is_some(),
            counters: !mentions_a_module(&setup_case),
            setup_count: script.len(),
            module_pending: false,
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
    let case = context.case;
    for violation in properties::check(connection, &case.properties) {
        if violation.unsupported.is_some() && context.gap_allowed() {
            continue;
        }
        let detail = format!("{}: {}", violation.name, violation.detail);
        context.fail(Kind::Property, None, "", detail);
    }
}

/// The key a case's fixture is stored under: its setup and whether the oracle
/// builds it too.
///
/// @param case - the case
pub fn fixture_key(case: &Case) -> String {
    let mut text = String::new();
    for record in &case.setup {
        match record.sql() {
            Some(sql) => text.push_str(sql),
            None => text.push_str("\u{2}reopen"),
        }
        text.push('\u{1}');
    }
    text.push_str(if case.oracle { "oracle" } else { "alone" });
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
        ["CREATE", second, ..] => {
            matches!(*second, "TABLE" | "INDEX" | "UNIQUE" | "VIEW" | "TRIGGER")
        }
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
