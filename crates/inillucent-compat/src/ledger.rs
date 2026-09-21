//! A day's worth of transactions against a ledger, checked against a model.
//!
//! Invariant: **after every phase the database and a `BTreeMap` in the test
//! agree, value for value, asked through the table and through the index.** A
//! soak that only checks that nothing crashed is the failure rule 1.2 names -
//! it would have passed through every corruption this project has fixed, because
//! each of those produced wrong answers rather than a panic.
//!
//! ## Why a model rather than a second query
//!
//! The comparison has to come from outside the engine. Asking the engine twice
//! and getting the same answer twice says only that it is consistent with
//! itself, which a corrupted index usually is. The model is a `BTreeMap` the
//! test maintains as it issues each statement, so what it knows is what the
//! statements should have done rather than what the file says they did.
//!
//! ## The phases, and why they are where the work is
//!
//! At most every 200 transactions it checkpoints, at most every 500 it closes
//! and reopens, and at most every 1,000 it runs `VACUUM`, `REINDEX` or
//! `ANALYZE` - the three statements that rewrite storage underneath a schema
//! that is already populated. A shorter run divides those down rather than
//! skipping them; see `Phases`. Reopening is rule 1.4: what reads the rows back
//! is a handle that did not write them.
//!
//! ## The generator is seeded, and the seed is printed
//!
//! Every divergence names the seed and the transaction number it happened at,
//! because a soak whose failure cannot be replayed is a bug report nobody can
//! act on. The same seed produces the same run at every page size.

use std::collections::BTreeMap;
use std::path::Path;

use inillucent_engine::connect::Connection;

use crate::matrix::Arm;
use crate::stories::{ask, open, reopen_and_check, run};

/// How many accounts the ledger holds.
///
/// Eight, so that an index lookup by account reads a fraction of the table
/// rather than nearly all of it, and so that every account has entries by the
/// first phase boundary.
pub const ACCOUNTS: i64 = 8;

/// How many transactions are committed together.
///
/// An application issuing a day of ledger transactions commits a unit of work
/// rather than a row, and a phase boundary below always commits before it does
/// anything - so no comparison ever reads a database with a group still open.
///
/// **It was tried as a speed fix and it is not one.** Three thousand statements
/// cost 16.9 seconds committed one at a time and 16.9 seconds committed in
/// groups of twenty five, measured in the debug build the suite runs. The cost
/// is executing a statement - parse, plan, write, fire the trigger that moves
/// the account's balance - at about 5.6 ms each, not committing one. What
/// brought the story inside the e2e tier's budget was the number of
/// transactions, and `Phases` below records that.
pub const GROUP: usize = 25;

/// The schema: a table with a foreign key, an index a lookup uses, two
/// triggers that maintain a running total, and a view over both.
///
/// The triggers are the part that makes this a ledger rather than a table:
/// `account.balance` is maintained by the database, so a run where the trigger
/// fired once too often or not at all diverges from the model without any
/// statement having been wrong.
pub const SCHEMA: &str = "\
CREATE TABLE account (
  id      INTEGER PRIMARY KEY,
  name    TEXT NOT NULL UNIQUE,
  balance INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE entry (
  id         INTEGER PRIMARY KEY,
  account_id INTEGER NOT NULL REFERENCES account (id),
  amount     INTEGER NOT NULL,
  memo       TEXT NOT NULL,
  at         INTEGER NOT NULL
);
CREATE INDEX entry_account_idx ON entry (account_id, at);
CREATE TRIGGER entry_added AFTER INSERT ON entry BEGIN
  UPDATE account SET balance = balance + NEW.amount WHERE id = NEW.account_id;
END;
CREATE TRIGGER entry_removed AFTER DELETE ON entry BEGIN
  UPDATE account SET balance = balance - OLD.amount WHERE id = OLD.account_id;
END;
CREATE TRIGGER entry_changed AFTER UPDATE ON entry BEGIN
  UPDATE account SET balance = balance - OLD.amount + NEW.amount WHERE id = NEW.account_id;
END;
CREATE VIEW account_total AS
  SELECT a.id AS id,
         a.balance AS balance,
         (SELECT count(*) FROM entry e WHERE e.account_id = a.id) AS entries
    FROM account a;
";

/// One entry as the model holds it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Which account it belongs to.
    pub account: i64,
    /// The signed amount.
    pub amount: i64,
    /// When it was issued, which is the transaction number.
    pub at: i64,
}

/// What the ledger should hold, maintained by the test as it issues statements.
#[derive(Clone, Debug, Default)]
pub struct Model {
    /// Every entry by id, in id order.
    pub entries: BTreeMap<i64, Entry>,
    /// The next entry id to hand out.
    pub next_id: i64,
    /// How many inserts, deletes and updates were issued.
    pub issued: [usize; 3],
}

impl Model {
    // The renderings below join with a comma because `stories::line` does, and
    // what they are compared against is what `stories::ask` returns. A model
    // that rendered its own way would be comparing two formats rather than two
    // sets of values.
    /// Returns what each account's balance should be, in account order.
    pub fn balances(&self) -> BTreeMap<i64, i64> {
        let mut balances: BTreeMap<i64, i64> = (1..=ACCOUNTS).map(|account| (account, 0)).collect();
        for entry in self.entries.values() {
            let running = balances.entry(entry.account).or_insert(0);
            *running = running.saturating_add(entry.amount);
        }
        balances
    }

    /// Renders every entry the way `SELECT id, account_id, amount FROM entry
    /// ORDER BY id` does.
    pub fn every_entry(&self) -> String {
        self.entries
            .iter()
            .map(|(id, entry)| format!("{id},{},{}", entry.account, entry.amount))
            .collect::<Vec<String>>()
            .join("\n")
    }

    /// Renders one account's entries in the order the index holds them.
    ///
    /// @param account - the account to render
    pub fn entries_of(&self, account: i64) -> String {
        let mut theirs: Vec<(i64, i64)> = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.account == account)
            .map(|(id, entry)| (entry.at, *id))
            .collect();
        theirs.sort();
        theirs
            .iter()
            .map(|(at, id)| format!("{id},{at}"))
            .collect::<Vec<String>>()
            .join("\n")
    }

    /// Renders every balance the way `SELECT id, balance FROM account ORDER BY
    /// id` does.
    pub fn every_balance(&self) -> String {
        self.balances()
            .iter()
            .map(|(id, balance)| format!("{id},{balance}"))
            .collect::<Vec<String>>()
            .join("\n")
    }
}

/// The seeded generator: a linear congruential sequence, and nothing else.
///
/// It has to produce the same run on every machine and at every page size, so
/// it cannot be `rand` - which is not a dependency this workspace allows, and
/// whose stream is not promised to be stable across versions anyway.
#[derive(Clone, Copy, Debug)]
pub struct Rolls(u64);

impl Rolls {
    /// Starts the sequence.
    ///
    /// @param seed - what to start from
    pub fn from(seed: u64) -> Rolls {
        Rolls(seed | 1)
    }

    /// Returns the next value below a bound.
    ///
    /// @param bound - one past the largest value wanted
    pub fn below(&mut self, bound: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % bound.max(1)
    }
}

/// Chooses one transaction, moves the model, and returns the statement.
///
/// Seven inserts, two deletes and one update in every ten, which is the mix
/// that keeps the table growing while still exercising both triggers that take
/// a row away. The statement is returned rather than run, so the caller can
/// commit a group of them at once and so a caller replaying the run through
/// another engine issues exactly what this one did.
///
/// @param model - the model to keep in step
/// @param rolls - the generator
/// @param at - the transaction number, which is also the entry's timestamp
pub fn issue(model: &mut Model, rolls: &mut Rolls, at: i64) -> String {
    let choice = rolls.below(10);
    let present: Vec<i64> = if choice >= 7 {
        model.entries.keys().copied().collect()
    } else {
        Vec::new()
    };

    let sql = if choice < 7 || present.is_empty() {
        let account = (rolls.below(ACCOUNTS as u64) as i64).saturating_add(1);
        let amount = (rolls.below(2_000) as i64).saturating_sub(1_000);
        let id = model.next_id.saturating_add(1);
        model.next_id = id;
        model.entries.insert(
            id,
            Entry {
                account,
                amount,
                at,
            },
        );
        model.issued[0] = model.issued[0].saturating_add(1);
        format!(
            "INSERT INTO entry (id, account_id, amount, memo, at) VALUES ({id}, {account}, \
             {amount}, 'entry {id}', {at})"
        )
    } else if choice < 9 {
        let index = rolls.below(present.len() as u64) as usize;
        let id = present.get(index).copied().unwrap_or_default();
        model.entries.remove(&id);
        model.issued[1] = model.issued[1].saturating_add(1);
        format!("DELETE FROM entry WHERE id = {id}")
    } else {
        let index = rolls.below(present.len() as u64) as usize;
        let id = present.get(index).copied().unwrap_or_default();
        let amount = (rolls.below(2_000) as i64).saturating_sub(1_000);
        if let Some(entry) = model.entries.get_mut(&id) {
            entry.amount = amount;
        }
        model.issued[2] = model.issued[2].saturating_add(1);
        format!("UPDATE entry SET amount = {amount} WHERE id = {id}")
    };

    sql
}

/// Compares the database against the model, three ways.
///
/// Rule 1.6, and the three are deliberately different reads: the whole table in
/// id order, one account's entries through `entry_account_idx`, and the
/// balances the triggers maintained. A corrupted index answers the first
/// correctly and the second wrongly; a trigger that fired twice answers both
/// correctly and the third wrongly.
///
/// @param connection - the open connection
/// @param model - what the database should hold
/// @param rolls - the generator, for choosing which account to ask about
/// @param at - the transaction number, for the failure message
/// @param seed - the run's seed, for the failure message
pub fn compare(connection: &Connection<'_>, model: &Model, rolls: &mut Rolls, at: i64, seed: u64) {
    let whole = ask(
        connection,
        "SELECT id, account_id, amount FROM entry ORDER BY id",
    );
    assert_eq!(
        whole,
        model.every_entry(),
        "seed {seed:#x}, transaction {at}: the table does not hold what the model says"
    );

    let account = (rolls.below(ACCOUNTS as u64) as i64).saturating_add(1);
    let through_the_index = ask(
        connection,
        &format!("SELECT id, at FROM entry WHERE account_id = {account} ORDER BY at, id"),
    );
    assert_eq!(
        through_the_index,
        model.entries_of(account),
        "seed {seed:#x}, transaction {at}: account {account} reads differently through \
         entry_account_idx than the model says"
    );

    let balances = ask(connection, "SELECT id, balance FROM account ORDER BY id");
    assert_eq!(
        balances,
        model.every_balance(),
        "seed {seed:#x}, transaction {at}: the balances the triggers maintained are not the \
         sum of the entries"
    );

    let through_the_view = ask(
        connection,
        &format!("SELECT balance FROM account_total WHERE id = {account}"),
    );
    let wanted = model.balances().get(&account).copied().unwrap_or_default();
    assert_eq!(
        through_the_view,
        wanted.to_string(),
        "seed {seed:#x}, transaction {at}: account_total disagrees with the table it is over"
    );
}

/// When a run does each of the three things that are not a transaction.
///
/// **The cadence scales with the run, so a short run still crosses every
/// boundary.** The design's numbers - a checkpoint every 200, a reopen every
/// 500, maintenance every 1,000 - are the ceilings, and a thousand transaction
/// run divides them down so it still checkpoints ten times, reopens four times
/// and runs `VACUUM`, `REINDEX` or `ANALYZE` twice. A fixed 1,000 would have
/// meant the quick form never ran maintenance at all, which is the phase the
/// story most wants: those three statements rewrite storage underneath a
/// schema that is already populated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Phases {
    /// How often the log is checkpointed and the answers compared.
    pub checkpoint: usize,
    /// How often the database is closed and opened again.
    pub reopen: usize,
    /// How often one of `VACUUM`, `REINDEX` or `ANALYZE` runs.
    pub maintenance: usize,
}

impl Phases {
    /// Returns the cadence for a run of a given length.
    ///
    /// @param transactions - how many the run issues
    pub fn for_run(transactions: usize) -> Phases {
        Phases {
            checkpoint: 200.min(transactions / 10).max(1),
            reopen: 500.min(transactions / 4).max(1),
            maintenance: 1_000.min(transactions / 2).max(1),
        }
    }
}

/// What one maintenance statement a phase boundary runs.
///
/// @param rolls - the generator
fn maintenance(rolls: &mut Rolls) -> &'static str {
    match rolls.below(3) {
        0 => "VACUUM",
        1 => "REINDEX",
        _ => "ANALYZE",
    }
}

/// Runs a day of transactions and returns the model they produced.
///
/// The phases are the design: a checkpoint every 200, a close and reopen every
/// 500, and one of `VACUUM`, `REINDEX` or `ANALYZE` every 1,000. Every one of
/// them is followed by the full comparison, so a statement that rewrites
/// storage under a populated schema is the last thing that happened before the
/// answers are checked.
///
/// @param arm - the configuration to run at
/// @param path - where to build the ledger
/// @param transactions - how many to issue
/// @param seed - the generator's seed
/// @param script - a file to record every statement in, for replaying elsewhere
pub fn play(
    arm: &Arm,
    path: &Path,
    transactions: usize,
    seed: u64,
    script: Option<&Path>,
) -> Model {
    play_with(
        arm,
        path,
        transactions,
        seed,
        script,
        Phases::for_run(transactions),
    )
}

/// Runs a day of transactions at a cadence the caller chooses.
///
/// **The long form needs its own cadence, and the reason is a measurement.** A
/// `VACUUM` over a populated ledger costs 3.8 seconds at a thousand
/// transactions in the debug build the suite runs, and it grows with the table
/// - so a hundred thousand transaction run at the default cadence would spend
/// its night on `VACUUM` rather than on transactions.
///
/// @param arm - the configuration to run at
/// @param path - where to build the ledger
/// @param transactions - how many to issue
/// @param seed - the generator's seed
/// @param script - a file to record every statement in, for replaying elsewhere
/// @param phases - how often to checkpoint, reopen and run maintenance
pub fn play_with(
    arm: &Arm,
    path: &Path,
    transactions: usize,
    seed: u64,
    script: Option<&Path>,
    phases: Phases,
) -> Model {
    let mut model = Model::default();
    let mut rolls = Rolls::from(seed);
    let mut recorded: Vec<String> = Vec::new();

    let mut database = open(arm, path);
    {
        let connection = database.session();
        run(&connection, SCHEMA);
        for account in 1..=ACCOUNTS {
            run(
                &connection,
                &format!("INSERT INTO account (id, name) VALUES ({account}, 'account {account}')"),
            );
        }
    }
    if script.is_some() {
        recorded.push(SCHEMA.to_string());
        for account in 1..=ACCOUNTS {
            recorded.push(format!(
                "INSERT INTO account (id, name) VALUES ({account}, 'account {account}');"
            ));
        }
    }

    let mut pending: Vec<String> = Vec::with_capacity(GROUP);
    for number in 1..=transactions {
        let at = number as i64;
        let sql = issue(&mut model, &mut rolls, at);
        if script.is_some() {
            recorded.push(format!("{sql};"));
        }
        pending.push(format!("{sql};"));

        let boundary = number % GROUP == 0
            || number % phases.checkpoint == 0
            || number % phases.reopen == 0
            || number % phases.maintenance == 0
            || number == transactions;
        if !boundary {
            continue;
        }

        {
            let connection = database.session();
            run(
                &connection,
                &pending.join(
                    "
",
                ),
            );
            pending.clear();

            if number % phases.maintenance == 0 {
                let statement = maintenance(&mut rolls);
                run(&connection, statement);
                if script.is_some() && statement != "VACUUM" {
                    // VACUUM is not recorded for the replay: it changes how the
                    // other engine stores the rows and nothing about what they
                    // are. Leaving it out keeps the script a list of changes
                    // rather than of maintenance.
                    recorded.push(format!("{statement};"));
                }
            }
            if number % phases.checkpoint == 0 {
                run(&connection, "PRAGMA wal_checkpoint(TRUNCATE)");
                compare(&connection, &model, &mut rolls, at, seed);
            }
        }

        if number % phases.reopen == 0 {
            drop(database);
            database = reopen_and_check(arm, path);
            let connection = database.session();
            compare(&connection, &model, &mut rolls, at, seed);
        }
    }

    {
        let connection = database.session();
        compare(&connection, &model, &mut rolls, transactions as i64, seed);
    }
    drop(database);

    // And once more through a handle that wrote none of it, which is rule 1.4
    // and the only read in this function that a warm pool cannot answer.
    let reopened = reopen_and_check(arm, path);
    {
        let connection = reopened.session();
        compare(&connection, &model, &mut rolls, transactions as i64, seed);
    }

    if let Some(file) = script {
        let _ = std::fs::write(file, recorded.join("\n"));
    }
    model
}
