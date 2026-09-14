//! Power loss while `REINDEX` is rebuilding an index.
//!
//! Invariant: **an index that was being rebuilt when the power went comes back
//! answering the same rows as a scan of the table.** `REINDEX` empties an index
//! and fills it again from the table, so the window in the middle is the one
//! state a database must never be readable in: every row still there, and a
//! query that uses the index answering a subset of them. Nothing about that
//! looks wrong from the outside, which is why it is graded here by asking the
//! same question twice, once through the index and once through a scan.
//!
//! `REINDEX` is also the second of the two multi write directives behind H3's
//! undo floor - `ALTER TABLE` is the other - so this campaign is what says the
//! floor holds under a cut rather than only under a refusal.

use inillucent_compat::crashcampaign::{record, Campaign};
use inillucent_sim::failpoint::Failure;

/// The database every run starts from.
///
/// Two indexes, because `REINDEX` on a table rebuilds all of them and a
/// campaign with one index cannot tell a rebuild that stopped after the first
/// from one that finished.
const SCHEMA: &str = "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT, c INTEGER);
     CREATE INDEX t_b ON t(b);
     CREATE INDEX t_c ON t(c, b);
     WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 150)
     INSERT INTO t SELECT i, 'k' || (i * 37 % 150), i % 11 FROM n;";

/// What each run tries to do, and is cut in the middle of.
///
/// The update is what makes the two states differ; the `REINDEX` after it is
/// what this campaign is about. A cut inside the update leaves the rows it
/// started with, and a cut anywhere in the rebuild leaves the updated rows with
/// an index that has to agree with them.
const WORKLOAD: &str = "UPDATE t SET b = 'j' || (a * 53 % 150) WHERE c = 3; REINDEX t;";

/// The statements after the workload, so a cut can land past its commit.
const TAIL: &str = "SELECT count(*) FROM t; PRAGMA wal_checkpoint;";

/// What a run is graded on: the same question through each index and a scan.
///
/// `+b` and `+c` defeat the index on the last two, because an expression is not
/// an indexed column, so those two rows are the scan's answer and the two above
/// them are the index's. A rebuild that lost entries shows up as the pair
/// disagreeing rather than as a missing row, which no single query would see.
const PROBES: &[&str] = &[
    "SELECT a, b, c FROM t ORDER BY a",
    "SELECT a FROM t WHERE b >= 'k100' AND b < 'k140' ORDER BY a",
    "SELECT a FROM t WHERE +b >= 'k100' AND +b < 'k140' ORDER BY a",
    "SELECT count(*), sum(a) FROM t WHERE c = 3",
    "SELECT count(*), sum(a) FROM t WHERE +c = 3",
];

/// Power loss anywhere in a `REINDEX` under a rollback journal.
#[test]
fn an_interrupted_reindex_leaves_an_index_that_agrees_with_the_table() {
    let report = Campaign {
        name: "reindex-journal-crash",
        mode: "delete",
        schema: SCHEMA,
        workload: WORKLOAD,
        tail: TAIL,
        probes: PROBES,
        failure: Failure::Crash,
        cuts: 4_000,
    }
    .run();
    record("reindex-journal-crash", &report);
}

/// The same, through a write-ahead log.
#[test]
fn an_interrupted_reindex_under_a_log_agrees_with_the_table() {
    let report = Campaign {
        name: "reindex-wal-crash",
        mode: "wal",
        schema: SCHEMA,
        workload: WORKLOAD,
        tail: TAIL,
        probes: PROBES,
        failure: Failure::Crash,
        cuts: 4_000,
    }
    .run();
    record("reindex-wal-crash", &report);
}

/// A device that refuses a write never leaves an index short of the table.
#[test]
fn a_reported_write_failure_during_a_reindex_never_shortens_an_index() {
    let report = Campaign {
        name: "reindex-journal-io",
        mode: "delete",
        schema: SCHEMA,
        workload: WORKLOAD,
        tail: TAIL,
        probes: PROBES,
        failure: Failure::IoError,
        cuts: 4_000,
    }
    .run();
    record("reindex-journal-io", &report);
}
