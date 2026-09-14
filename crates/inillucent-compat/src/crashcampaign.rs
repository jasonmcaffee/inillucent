//! Cutting one workload at every injectable call, and grading what came back.
//!
//! Invariant: **a database that comes back from a modelled power loss holds the
//! state from before the transaction or the state from after it, never a
//! mixture, and a transaction that reported success is always the second.**
//!
//! Here rather than copied into each campaign because there are now seven of
//! them. `wal_crash.rs` established the method - number every injectable call
//! of a run, then repeat the run once per number with the failure armed at
//! exactly that call - and `search_crash.rs`, `multi_database_crash.rs` and
//! `free_map_checkpoint_crash.rs` each wrote it out again. The three campaigns
//! task-1932 adds, for `VACUUM`, overflow chains and `REINDEX`, would have been
//! three more copies of four hundred lines whose only differences are a schema,
//! a statement and a list of probes, so those three are the arguments and this
//! is the campaign.
//!
//! The four earlier campaigns are deliberately left where they are. Each has an
//! arm this does not model - `wal_crash.rs` cuts a checkpoint and a recovery as
//! well as a commit, `multi_database_crash.rs` drives two files, and
//! `free_map_checkpoint_crash.rs` reaches inside the free map - and rewriting a
//! passing durability campaign to prove a refactor is a poor trade.
//!
//! ## What the joint state is for
//!
//! A run's state is every probe's rows in one list. Splitting them would let a
//! mixture match one half of a legitimate state and be graded as that state,
//! which is the one failure a campaign like this exists to catch: a database
//! that came back holding the new row and the old index looks right from either
//! side on its own.

use std::path::PathBuf;
use std::sync::Arc;

use inillucent_exec::physical::Params;
use inillucent_sim::failpoint::Failure;
use inillucent_sim::media::MediaModel;
use inillucent_sim::sim_vfs::{CrashSnapshot, SimConfig, SimVfs};
use inillucent_tree::datum::OwnedDatum;
use inillucent_vfs::Vfs;

use crate::newengine::ImportedDatabase;

/// The page size these runs build at, matching `inillucent_engine::connect::PAGE_SIZE`.
pub const PAGE_SIZE: usize = 32_768;

/// How many frames the pool holds, large enough that nothing evicts.
pub const FRAMES: usize = 4_096;

/// One campaign: a schema, a workload, and what to ask the database afterwards.
pub struct Campaign<'a> {
    /// What the report is filed under.
    pub name: &'a str,
    /// The journal mode the run is made in, `delete` or `wal`.
    pub mode: &'a str,
    /// The statements that build the database before any failure is armed.
    pub schema: &'a str,
    /// The statements whose every injectable call is cut, one per run.
    pub workload: &'a str,
    /// The statements run after the workload, so a cut can land past its commit.
    ///
    /// Without them the last injectable call of a run is inside the commit, so
    /// no cut ever observes the committed state and the campaign only proves
    /// the transaction can be abandoned. A `PRAGMA wal_checkpoint` belongs here
    /// for the same reason `search_crash.rs` puts one here: under a rollback
    /// journal it is the only statement that moves pages out of the log, so it
    /// is what puts the journal on the path of a cut.
    pub tail: &'a str,
    /// The queries whose rows, together, are the state a run is graded on.
    pub probes: &'a [&'a str],
    /// What the device does at the call the run is cut at.
    pub failure: Failure,
    /// How many calls to try cutting at before the run stops finding new ones.
    pub cuts: u64,
}

/// What reopening a crashed database produced.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Recovery {
    /// It opened and reported this state.
    Rows(Vec<String>),
    /// It refused to be read, naming the damage.
    Broken(String),
}

impl Campaign<'_> {
    /// Runs the campaign and returns its report, one line per cut.
    ///
    /// Fails through its assertions rather than returning an error, because
    /// every caller is a `#[test]` and a campaign that cannot reach its own
    /// workload is a defect in the campaign.
    pub fn run(&self) -> String {
        let (before, after) = self.expected_states();
        assert_ne!(before, after, "{}: the workload changes nothing", self.name);
        let corruption_allowed = !matches!(self.failure, Failure::Crash);
        let silent = matches!(self.failure, Failure::ShortWrite);
        let mut report = String::new();
        let (mut cuts, mut old, mut new, mut detected) = (0u64, 0u64, 0u64, 0u64);
        for nth in 1..=self.cuts {
            let vfs = self.built(7_000_u64.saturating_add(nth));
            let base = vfs.failpoints().sites_reached();
            vfs.failpoints()
                .fail_nth_call(base.saturating_add(nth), self.failure);
            let mut connection = self.reopen(Arc::clone(&vfs) as Arc<dyn Vfs>);
            let committed = match &mut connection {
                Ok(engine) => {
                    run_script(engine, self.workload).is_ok()
                        && run_script(engine, self.tail).is_ok()
                }
                Err(_) => false,
            };
            let reached = vfs.failpoints().sites_reached().saturating_sub(base);
            let snapshot = vfs.crash();
            drop(connection);
            if reached < nth {
                break;
            }
            cuts = cuts.saturating_add(1);
            let verdict = match &self.recovered(&snapshot, 8_000_u64.saturating_add(nth)) {
                Recovery::Rows(rows) if *rows == before => {
                    old = old.saturating_add(1);
                    "old"
                }
                Recovery::Rows(rows) if *rows == after => {
                    new = new.saturating_add(1);
                    "new"
                }
                Recovery::Broken(detail) => {
                    assert!(
                        corruption_allowed,
                        "{} cut {nth}: the database came back unreadable: {detail}",
                        self.name
                    );
                    detected = detected.saturating_add(1);
                    "reported"
                }
                Recovery::Rows(rows) => {
                    let differs: Vec<&String> = rows
                        .iter()
                        .filter(|line| !before.contains(line) || !after.contains(line))
                        .collect();
                    // Always false here - the two arms above took every state
                    // that is one of the legitimate two - and written as a
                    // condition so that the message carries what came back.
                    assert!(
                        *rows == before || *rows == after,
                        "{} cut {nth}: the database came back a mixture.
  before:                          {before:?}
  after:  {after:?}
  got:    {rows:?}
  differs:                          {differs:?}",
                        self.name
                    );
                    "mixture"
                }
            };
            assert!(
                !committed
                    || verdict == "new"
                    || silent
                    || (corruption_allowed && verdict == "reported"),
                "{} cut {nth}: the commit reported success and the database does not hold it",
                self.name
            );
            report.push_str(&format!("{nth}\t{verdict}\t{committed}\n"));
        }
        // The counts go in every message because what goes wrong here is almost
        // never "the engine answered a third state". It is that the cuts stopped
        // reaching as far into the run as they used to, and a campaign that has
        // stopped covering the commit reports the old state honestly every time.
        let counts = format!("{cuts} cuts, {old} old, {new} new, {detected} reported");
        assert!(
            cuts > 20,
            "{}: only {cuts} cut points were reached",
            self.name
        );
        assert!(
            old > 0,
            "{}: no cut left the old state ({counts})",
            self.name
        );
        assert!(
            new > 0,
            "{}: no cut left the new state ({counts})",
            self.name
        );
        format!("# {}: {counts}\ncut\tstate\tcommitted\n{report}", self.name)
    }

    /// Returns the file every run of this campaign is built in.
    fn path(&self) -> PathBuf {
        PathBuf::from(format!("/sim/{}.db", self.name.replace('-', "_")))
    }

    /// Returns a simulator holding a freshly built database.
    ///
    /// @param seed - what the device model's randomness starts from
    fn built(&self, seed: u64) -> Arc<SimVfs> {
        let vfs = simulator(seed);
        let opened = ImportedDatabase::create_on(
            Arc::clone(&vfs) as Arc<dyn Vfs>,
            self.path(),
            PAGE_SIZE,
            FRAMES,
        );
        assert!(
            opened.is_ok(),
            "{}: the database is not created at {:?}: {:?}",
            self.name,
            self.path(),
            opened.as_ref().err()
        );
        let Ok(mut engine) = opened else {
            return vfs;
        };
        let built = run_script(&mut engine, &format!("PRAGMA journal_mode={}", self.mode))
            .and_then(|()| run_script(&mut engine, self.schema));
        assert!(
            built.is_ok(),
            "{}: the schema does not build: {built:?}",
            self.name
        );
        drop(engine);
        vfs
    }

    /// Reopens a database a prior connection built, reporting rather than
    /// failing - a crash is exactly the case where this refuses.
    ///
    /// @param vfs - the simulator holding the file
    fn reopen(&self, vfs: Arc<dyn Vfs>) -> Result<ImportedDatabase, inillucent_base::DbError> {
        let mut engine = ImportedDatabase::open_on(vfs, self.path(), PAGE_SIZE, FRAMES)?;
        run_script(&mut engine, &format!("PRAGMA journal_mode={}", self.mode))?;
        Ok(engine)
    }

    /// Returns the two states a run may legitimately end in.
    fn expected_states(&self) -> (Vec<String>, Vec<String>) {
        let vfs = self.built(4_242);
        let before = self.state_of(Arc::clone(&vfs) as Arc<dyn Vfs>, "", "before");
        let after = self.state_of(
            Arc::clone(&vfs) as Arc<dyn Vfs>,
            &format!("{}{}", self.workload, self.tail),
            "after",
        );
        (before, after)
    }

    /// Reopens a simulator, runs a script on it, and reports the joint state.
    ///
    /// @param vfs - the simulator holding the file
    /// @param script - what to run before reading the state
    fn state_of(&self, vfs: Arc<dyn Vfs>, script: &str, which: &str) -> Vec<String> {
        let read = self.reopen(vfs).and_then(|mut engine| {
            run_script(&mut engine, script)?;
            self.state(&mut engine)
        });
        assert!(
            read.is_ok(),
            "{}: the {which} state at {:?} is unreadable: {:?}",
            self.name,
            self.path(),
            read.as_ref().err()
        );
        read.unwrap_or_default()
    }

    /// Returns every probe's rows as one list.
    ///
    /// @param engine - the database to read
    fn state(
        &self,
        engine: &mut ImportedDatabase,
    ) -> Result<Vec<String>, inillucent_base::DbError> {
        let mut state = Vec::new();
        for probe in self.probes {
            state.push(format!("-- {probe}"));
            state.extend(query(engine, probe)?);
        }
        Ok(state)
    }

    /// Reopens what a crash left behind.
    ///
    /// @param snapshot - the file as the device had it at the cut
    /// @param seed - what the recovering simulator's randomness starts from
    fn recovered(&self, snapshot: &CrashSnapshot, seed: u64) -> Recovery {
        let vfs = Arc::new(SimVfs::recovered(
            SimConfig {
                seed,
                model: MediaModel::default(),
                ..SimConfig::default()
            },
            snapshot,
        ));
        match self
            .reopen(Arc::clone(&vfs) as Arc<dyn Vfs>)
            .and_then(|mut engine| self.state(&mut engine))
        {
            Ok(rows) => Recovery::Rows(rows),
            Err(failure) => Recovery::Broken(format!("{failure} - {failure:?}")),
        }
    }
}

/// Returns a simulator with the pessimistic device model.
///
/// @param seed - what its randomness starts from
fn simulator(seed: u64) -> Arc<SimVfs> {
    Arc::new(SimVfs::new(SimConfig {
        seed,
        model: MediaModel::default(),
        ..SimConfig::default()
    }))
}

/// Runs a script of one or more statements, stopping at the first failure.
///
/// @param engine - the database to run them on
/// @param sql - the statements, separated by semicolons
fn run_script(engine: &mut ImportedDatabase, sql: &str) -> Result<(), inillucent_base::DbError> {
    let mut rest = sql;
    loop {
        let trimmed = rest.trim_start();
        if trimmed.is_empty() {
            return Ok(());
        }
        let consumed = engine.statement_length(trimmed)?;
        let Some(head) = trimmed.get(..consumed) else {
            return Ok(());
        };
        if head.trim().is_empty() {
            return Ok(());
        }
        engine.execute_any(head, &Params::new())?;
        rest = trimmed.get(consumed..).unwrap_or("");
    }
}

/// Returns one query's rows, each rendered as text.
///
/// @param engine - the database to read
/// @param sql - the query
fn query(
    engine: &mut ImportedDatabase,
    sql: &str,
) -> Result<Vec<String>, inillucent_base::DbError> {
    let outcome = engine.execute_any(sql, &Params::new())?;
    let mut rows = Vec::new();
    for row in outcome.rows {
        let parts: Vec<String> = row
            .iter()
            .map(|value| match value {
                OwnedDatum::Null => "NULL".to_string(),
                OwnedDatum::Int(number) => number.to_string(),
                OwnedDatum::Real(number) => format!("{number:.4}"),
                OwnedDatum::Text(bytes) => String::from_utf8_lossy(bytes).into_owned(),
                OwnedDatum::Blob(bytes) => format!("blob:{}", bytes.len()),
            })
            .collect();
        rows.push(parts.join("|"));
    }
    Ok(rows)
}

/// Writes one campaign's report where the others are kept.
///
/// @param name - the campaign's name, which becomes the file name
/// @param report - what `Campaign::run` returned
pub fn record(name: &str, report: &str) {
    let directory = crate::workspace_root().join("_agent_output/crash-campaigns");
    let _ = std::fs::create_dir_all(&directory);
    let _ = std::fs::write(directory.join(format!("{name}.tsv")), report);
}
