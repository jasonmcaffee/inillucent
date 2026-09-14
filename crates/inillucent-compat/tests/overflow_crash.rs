//! Power loss while a value too large for one page is being written.
//!
//! Invariant: **a value that spans an overflow chain comes back whole or not at
//! all.** A row whose text does not fit in a leaf is written as a head in the
//! leaf and a chain of pages after it, so a cut part way through has more ways
//! to go wrong than an ordinary row does: the head can name a chain that was
//! never written, a chain can be written and never named, and a chain that is
//! partly rewritten gives back a value that is the right length and the wrong
//! bytes.
//!
//! The probes therefore read `length(b)` and a checksum of the value rather
//! than the value itself. A campaign that compared the rendered text would
//! carry two megabytes of expected state through every cut, and a campaign that
//! compared only the length would pass on a chain whose middle page came from
//! the transaction before it - which is the failure worth catching here.

use inillucent_compat::crashcampaign::{record, Campaign};
use inillucent_sim::failpoint::Failure;

/// The database every run starts from.
///
/// `hex(zeroblob(n))` is 2n characters, so row 1 is 40,000 bytes against a
/// 32,768 byte page: one leaf head and a chain behind it. Row 2 is under the
/// page size and is there so that the campaign covers a table holding both
/// shapes, which is what a real one does.
const SCHEMA: &str = "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT);
     INSERT INTO t VALUES(1, replace(hex(zeroblob(20000)), '0', 'q'));
     INSERT INTO t VALUES(2, replace(hex(zeroblob(400)), '0', 'r'));";

/// What each run tries to do, and is cut in the middle of.
///
/// One transaction that grows an existing chain, writes a new one, and drops a
/// third, because those are three different things to be cut in the middle of
/// and a campaign that only ever wrote a new chain would miss the other two.
const WORKLOAD: &str = "BEGIN;
     UPDATE t SET b = replace(hex(zeroblob(60000)), '0', 's') WHERE a = 1;
     INSERT INTO t VALUES(3, replace(hex(zeroblob(35000)), '0', 't'));
     UPDATE t SET b = 'short' WHERE a = 2;
     COMMIT;";

/// The statements after the workload, so a cut can land past its commit.
const TAIL: &str = "SELECT count(*) FROM t; PRAGMA wal_checkpoint;";

/// What a run is graded on.
///
/// `length` says the chain is the right size and the two substrings say it
/// holds the right bytes at both ends. Reading the first and last characters is
/// what separates "the chain was rewritten" from "the chain was half
/// rewritten", which a length alone cannot see.
const PROBES: &[&str] = &[
    "SELECT a, length(b) FROM t ORDER BY a",
    "SELECT a, substr(b, 1, 8), substr(b, length(b) - 7, 8) FROM t ORDER BY a",
    "SELECT count(*) FROM t WHERE length(b) > 32768",
];

/// Power loss anywhere in a transaction that writes overflow chains.
#[test]
fn an_overflow_chain_comes_back_whole_or_not_at_all() {
    let report = Campaign {
        name: "overflow-journal-crash",
        mode: "delete",
        schema: SCHEMA,
        workload: WORKLOAD,
        tail: TAIL,
        probes: PROBES,
        failure: Failure::Crash,
        cuts: 4_000,
    }
    .run();
    record("overflow-journal-crash", &report);
}

/// The same, through a write-ahead log.
#[test]
fn an_overflow_chain_under_a_log_comes_back_whole_or_not_at_all() {
    let report = Campaign {
        name: "overflow-wal-crash",
        mode: "wal",
        schema: SCHEMA,
        workload: WORKLOAD,
        tail: TAIL,
        probes: PROBES,
        failure: Failure::Crash,
        cuts: 4_000,
    }
    .run();
    record("overflow-wal-crash", &report);
}

/// A device that refuses a write never leaves half a chain readable.
#[test]
fn a_reported_write_failure_never_leaves_half_a_chain() {
    let report = Campaign {
        name: "overflow-journal-io",
        mode: "delete",
        schema: SCHEMA,
        workload: WORKLOAD,
        tail: TAIL,
        probes: PROBES,
        failure: Failure::IoError,
        cuts: 4_000,
    }
    .run();
    record("overflow-journal-io", &report);
}
