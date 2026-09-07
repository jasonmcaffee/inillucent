//! The new engine's write path, graded against pinned SQLite 3.53.4.
//!
//! Invariant: after any sequence of writes, **every index still agrees with its
//! table**. That is the property this file exists for, and it is the one a
//! passing read suite proves least: an index the insert path maintains and the
//! delete path forgets answers a covering query wrongly while every other query
//! about the same row is fine, so the failure is invisible to any test that
//! reads the table.
//!
//! The sweep therefore asks each question **twice**: once in a shape the
//! planner answers from the table, and once in a shape it answers from an
//! index. A difference between those two answers is an index that has drifted,
//! and it is reported as such rather than as a query difference.
//!
//! ## Why the oracle answers over a file it built
//!
//! The write corpus needs constraints the read corpus does not have - a
//! secondary `UNIQUE` index, a `NOT NULL`, an `ON CONFLICT` target - so the
//! fixture is built here rather than checked in. Both engines then get the same
//! file: SQLite opens it, and the new engine imports it.
//!
//! When the pinned oracle has not been built this file says so and returns, the
//! way the rest of the differential suite does. A run without the oracle
//! evidences nothing and must not look like a pass.

use std::path::PathBuf;

use inillucent_compat::newengine::ImportedDatabase;
use inillucent_compat::oracle::{Driver, Op, TaggedValue};
use inillucent_compat::workspace_root;
use inillucent_exec::physical::Params;
use inillucent_tree::datum::OwnedDatum;

/// Returns the pinned SQLite oracle binary, if it has been built.
fn oracle_path() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("INILLUCENT_SQLITE_ORACLE") {
        let path = PathBuf::from(explicit);
        return path.is_file().then_some(path);
    }
    let directory = workspace_root().join(".sqlite-ref/3.53.4");
    let path = directory.join(format!("sqlite-oracle{}", std::env::consts::EXE_SUFFIX));
    path.is_file().then_some(path)
}

/// Returns a fresh directory for one test's files.
///
/// A directory per test rather than a file per test, so two tests running in
/// parallel never open each other's database - and so the whole of a failed
/// run's evidence is in one place.
///
/// @param tag - what to name it after
fn scratch(tag: &str) -> PathBuf {
    let directory = std::env::temp_dir().join(format!(
        "inillucent-writes-{}-{tag}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("a scratch directory");
    directory
}

/// The schema the write corpus is asked about.
///
/// Every constraint here is one the write path has to enforce, and each is
/// distinct: `id` is the rowid, `email` is a secondary `UNIQUE` index, `team`
/// carries a plain index, and `alias` carries a `UNIQUE` index that is
/// nullable - which is the case where SQL's "every NULL is distinct" rule
/// applies and a naive uniqueness check reports a collision that is not one.
const SCHEMA: &[&str] = &[
    "CREATE TABLE members (id INTEGER PRIMARY KEY, email TEXT NOT NULL, \
     team TEXT, alias TEXT, score INTEGER)",
    "CREATE UNIQUE INDEX members_email ON members (email)",
    "CREATE INDEX members_team ON members (team)",
    "CREATE UNIQUE INDEX members_alias ON members (alias)",
    "CREATE INDEX members_score ON members (score DESC, email)",
];

/// The rows the fixture starts with.
const SEED: &[&str] = &[
    "INSERT INTO members VALUES (1, 'ana@x', 'red', 'ana', 30)",
    "INSERT INTO members VALUES (2, 'bo@x', 'blue', NULL, 20)",
    "INSERT INTO members VALUES (3, 'cy@x', 'red', 'cy', 40)",
    "INSERT INTO members VALUES (4, 'di@x', 'green', NULL, 20)",
    "INSERT INTO members VALUES (5, 'ed@x', 'blue', 'ed', 10)",
];

/// A fixture both engines have opened, with the oracle already positioned on it.
struct Pair {
    engine: ImportedDatabase,
    oracle: Driver,
    /// Kept so the directory outlives the test.
    _directory: PathBuf,
}

/// Builds the fixture with the oracle and imports it into the new engine.
///
/// Returns `None` when the oracle has not been built, which every test here
/// reports rather than passing quietly.
///
/// @param tag - what to name the scratch directory after
fn pair(tag: &str) -> Option<Pair> {
    let program = oracle_path()?;
    let directory = scratch(tag);
    let path = directory.join("members.db");
    let mut oracle = Driver::start("sqlite", &program).expect("the oracle starts");
    oracle
        .send(&Op::Open(path.to_string_lossy().into_owned()))
        .expect("the oracle opens the fixture");
    for sql in SCHEMA.iter().chain(SEED.iter()) {
        let observed = oracle
            .send(&Op::Exec((*sql).to_string()))
            .expect("the exec runs");
        assert!(
            observed.ok,
            "the fixture did not build: {sql}: {}",
            observed.message
        );
    }
    let engine = ImportedDatabase::import(path, 4_096)
        .unwrap_or_else(|error| panic!("the fixture did not import: {:?}", error.detail()));
    Some(Pair {
        engine,
        oracle,
        _directory: directory,
    })
}

/// The questions every write is graded by.
///
/// Each pair asks the same thing twice: once in a shape the planner answers
/// from the table, and once in a shape it answers from an index. An index that
/// has drifted from its table answers the second differently from the first,
/// and no query that only reads the table can see it.
fn probes() -> Vec<String> {
    let mut queries = vec![
        // The table, whole and in key order.
        "SELECT id, email, team, alias, score FROM members ORDER BY id".to_string(),
        "SELECT count(*) FROM members".to_string(),
        // `members_email`: a covering scan and a point lookup through it.
        "SELECT email FROM members ORDER BY email".to_string(),
        "SELECT id FROM members WHERE email = 'cy@x'".to_string(),
        "SELECT count(*) FROM members WHERE email > 'b' AND email < 'e'".to_string(),
        // `members_team`, which is not unique and holds NULLs.
        "SELECT team FROM members ORDER BY team".to_string(),
        "SELECT id FROM members WHERE team = 'red' ORDER BY id".to_string(),
        "SELECT count(*) FROM members WHERE team IS NULL".to_string(),
        // `members_alias`, unique and nullable - the case where SQL's rule that
        // every NULL is distinct means several rows share the "same" key.
        "SELECT alias FROM members ORDER BY alias".to_string(),
        "SELECT id FROM members WHERE alias = 'ed'".to_string(),
        "SELECT count(*) FROM members WHERE alias IS NULL".to_string(),
        // `members_score`, whose leading column is stored descending.
        "SELECT score, email FROM members ORDER BY score DESC, email".to_string(),
        "SELECT id FROM members WHERE score = 20 ORDER BY id".to_string(),
        "SELECT count(*) FROM members WHERE score >= 20".to_string(),
        // The aggregate every index has to agree about.
        "SELECT team, count(*), sum(score) FROM members GROUP BY team ORDER BY team".to_string(),
    ];
    queries.sort();
    queries.dedup();
    queries
}

/// Renders one of the new engine's rows the way the oracle renders its own.
///
/// @param row - the row
fn render(row: &[OwnedDatum]) -> Vec<TaggedValue> {
    row.iter()
        .map(|value| match value {
            OwnedDatum::Null => TaggedValue::Null,
            OwnedDatum::Int(number) => TaggedValue::Integer(*number),
            OwnedDatum::Real(number) => TaggedValue::Real(*number),
            OwnedDatum::Text(bytes) => TaggedValue::Text(bytes.clone()),
            OwnedDatum::Blob(bytes) => TaggedValue::Blob(bytes.clone()),
        })
        .collect()
}

/// Asks both engines every probe and returns the differences.
///
/// @param pair - the two engines over the same data
fn differences(pair: &mut Pair) -> Vec<String> {
    let mut found = Vec::new();
    for sql in probes() {
        let reference = pair
            .oracle
            .send(&Op::Query(sql.clone()))
            .expect("the oracle answers");
        if !reference.ok {
            continue;
        }
        let answered = match pair.engine.execute_any(&sql, &Params::new()) {
            Ok(outcome) => outcome.rows,
            Err(error) => {
                found.push(format!(
                    "{sql}\n  the new engine refused: {}",
                    error.detail().unwrap_or("no detail")
                ));
                continue;
            }
        };
        let ours: Vec<Vec<TaggedValue>> = answered.iter().map(|row| render(row)).collect();
        if !same(&reference.rows, &ours) {
            found.push(format!(
                "{sql}\n  sqlite {:?}\n  ours   {:?}",
                reference.rows, ours
            ));
        }
    }
    found
}

/// Compares two row sequences by value and by type.
///
/// @param reference - what SQLite answered
/// @param ours - what the new engine answered
fn same(reference: &[Vec<TaggedValue>], ours: &[Vec<TaggedValue>]) -> bool {
    reference.len() == ours.len()
        && reference.iter().zip(ours.iter()).all(|(left, right)| {
            left.len() == right.len() && left.iter().zip(right.iter()).all(|(a, b)| a.identical(b))
        })
}

/// Applies one statement to both engines and returns what each said.
///
/// The answer is the error message, or `None` for a statement that worked. Both
/// halves matter: a statement one engine refuses and the other accepts is a
/// difference even when the resulting rows happen to agree.
///
/// @param pair - the two engines over the same data
/// @param sql - the statement
fn apply(pair: &mut Pair, sql: &str) -> (Option<String>, Option<String>) {
    let reference = pair
        .oracle
        .send(&Op::Exec(sql.to_string()))
        .expect("the oracle runs the statement");
    let ours = pair
        .engine
        .execute_any(sql, &Params::new())
        .err()
        .map(|error| error.message().to_string());
    let theirs = (!reference.ok).then(|| reference.message.clone());
    (theirs, ours)
}

/// Every index still agrees with its table after a campaign of writes.
///
/// The campaign is written out rather than generated because each statement is
/// here for a reason the next one is not: a plain insert, an insert that lands
/// between two existing keys, an update that moves a row in one index and not
/// another, an update that moves it in all of them, a delete of a row several
/// indexes name, and a re-insert of the key that was just freed.
#[test]
fn every_index_still_agrees_with_its_table_after_a_campaign() {
    let Some(mut pair) = pair("campaign") else {
        eprintln!("the pinned SQLite oracle is not built; nothing was compared");
        return;
    };

    let campaign = [
        "INSERT INTO members VALUES (6, 'fi@x', 'red', 'fi', 50)",
        "INSERT INTO members VALUES (7, 'gu@x', NULL, NULL, 20)",
        // An update that moves the row in `members_team` and nowhere else.
        "UPDATE members SET team = 'blue' WHERE id = 1",
        // An update that moves it in `members_email`, `members_alias` and
        // `members_score` at once.
        "UPDATE members SET email = 'zed@x', alias = 'zed', score = 5 WHERE id = 3",
        // An update to the value it already holds, which must remove and re-add
        // one entry rather than leave two.
        "UPDATE members SET team = 'blue' WHERE id = 2",
        // A delete of a row three indexes name.
        "DELETE FROM members WHERE id = 5",
        // The key that was just freed, taken again.
        "INSERT INTO members VALUES (5, 'new@x', 'green', 'new', 15)",
        // A delete that removes nothing, which must leave every index alone.
        "DELETE FROM members WHERE id = 999",
        // An update over a range rather than a key.
        "UPDATE members SET score = score + 1 WHERE team = 'blue'",
        // A delete over an index rather than the table.
        "DELETE FROM members WHERE alias IS NULL",
    ];

    let mut failures = Vec::new();
    for sql in campaign {
        let (reference, ours) = apply(&mut pair, sql);
        if reference.is_some() != ours.is_some() {
            failures.push(format!("{sql}\n  sqlite {reference:?}\n  ours   {ours:?}"));
            continue;
        }
        // The structure, after every statement rather than at the end. A tree
        // whose separators no longer bound its children answers correctly for a
        // long time, so the check has to be a check - and running it per
        // statement is what names the statement that broke it.
        if let Err(error) = pair.engine.check_trees() {
            failures.push(format!(
                "after {sql}\n  a tree is structurally wrong: {}",
                error.detail().unwrap_or("no detail")
            ));
        }
        for difference in differences(&mut pair) {
            failures.push(format!("after {sql}\n{difference}"));
        }
    }
    assert!(
        failures.is_empty(),
        "the write path and SQLite disagree:\n{}",
        failures.join("\n\n")
    );
}

/// A duplicate key reports SQLite's own text, for every kind of key.
///
/// The acceptance asks for SQLite's `UNIQUE` error rather than merely an error,
/// which means the *text*: a caller matching on `UNIQUE constraint failed:
/// members.email` must not have to know which engine answered it.
#[test]
fn a_duplicate_key_reports_the_text_sqlite_reports() {
    let Some(mut pair) = pair("unique") else {
        eprintln!("the pinned SQLite oracle is not built; nothing was compared");
        return;
    };

    let collisions = [
        // The rowid, aliased by an `INTEGER PRIMARY KEY`.
        "INSERT INTO members VALUES (1, 'zz@x', 'red', 'zz', 1)",
        // A secondary `UNIQUE` index.
        "INSERT INTO members VALUES (80, 'ana@x', 'red', 'zz', 1)",
        // A second secondary `UNIQUE` index, so the message names the right one.
        "INSERT INTO members VALUES (81, 'zz@x', 'red', 'ana', 1)",
    ];

    let mut failures = Vec::new();
    for sql in collisions {
        let (reference, ours) = apply(&mut pair, sql);
        let Some(reference) = reference else {
            failures.push(format!(
                "{sql}\n  sqlite accepted it and we expected a refusal"
            ));
            continue;
        };
        match ours {
            Some(message) if message == reference => {}
            other => failures.push(format!("{sql}\n  sqlite {reference:?}\n  ours   {other:?}")),
        }
    }
    assert!(
        failures.is_empty(),
        "the constraint messages differ from SQLite's:\n{}",
        failures.join("\n\n")
    );
}

/// A nullable `UNIQUE` column holds any number of NULLs.
///
/// SQL's rule is that a NULL is distinct from every other NULL, so
/// `members_alias` may name four rows with no alias and none of them collides.
/// A uniqueness check that probed the index without excluding NULLs would
/// refuse the second one - and the fixture already has two, so the bug would
/// show up as an insert that cannot happen rather than as a wrong answer.
#[test]
fn a_null_never_collides_in_a_unique_index() {
    let Some(mut pair) = pair("nulls") else {
        eprintln!("the pinned SQLite oracle is not built; nothing was compared");
        return;
    };

    let mut failures = Vec::new();
    for sql in [
        "INSERT INTO members VALUES (20, 'p@x', 'red', NULL, 1)",
        "INSERT INTO members VALUES (21, 'q@x', 'red', NULL, 1)",
        "UPDATE members SET alias = NULL WHERE id = 1",
    ] {
        let (reference, ours) = apply(&mut pair, sql);
        if reference.is_some() || ours.is_some() {
            failures.push(format!("{sql}\n  sqlite {reference:?}\n  ours   {ours:?}"));
        }
    }
    for difference in differences(&mut pair) {
        failures.push(difference);
    }
    assert!(
        failures.is_empty(),
        "a NULL was treated as a value in a unique index:\n{}",
        failures.join("\n\n")
    );
}

/// An `UPDATE` is refused by a secondary `UNIQUE` index, and only by a real one.
///
/// **This is the silent wrong answer task-1849 was filed for**, and it is worth
/// stating what "silent" bought: `UPDATE members SET email = 'ana@x' WHERE id =
/// 2` was performed, answered success, and left `members_email` holding two
/// entries under one key. Nothing failed. The next reader asking
/// `WHERE email = 'ana@x'` got two rows from the index and one from the table,
/// which is why `differences()` runs after every statement here rather than a
/// `SELECT` at the end: the table alone cannot see it.
///
/// The other half is as important and is the reason this is not a one-line fix.
/// `UPDATE members SET id = 50 WHERE id = 2` is legal - the row moves and every
/// unique key it holds moves with it - and adding the probe without teaching it
/// which row is asking refuses it, because the probe finds *this* row's own
/// entry. That statement was already being refused before the check existed,
/// for the same reason, whenever the rowid moved.
///
/// Each statement is applied to both engines, so the two stay on the same data
/// and a later case is asked about the state the earlier ones actually left.
#[test]
fn an_update_onto_a_taken_unique_key_is_refused_the_way_sqlite_refuses_it() {
    let Some(mut pair) = pair("update-unique") else {
        eprintln!("the pinned SQLite oracle is not built; nothing was compared");
        return;
    };

    let mut failures = Vec::new();
    for sql in [
        // The ticket's own shape: the rowid does not move, `email` does, and
        // another row already holds the value it moves onto.
        "UPDATE members SET email = 'ana@x' WHERE id = 2",
        // The *second* unique index, so the refusal names the index that
        // collided rather than the first one probed.
        "UPDATE members SET alias = 'cy' WHERE id = 1",
        // Both at once, and a `WHERE` that selects the row holding neither.
        "UPDATE members SET email = 'cy@x', alias = 'cy' WHERE id = 4",
        // Assigning a row its own value is not a collision with itself.
        "UPDATE members SET email = 'ana@x', score = 99 WHERE id = 1",
        // The rowid moves and no indexed value does. Legal, and refused before
        // this test existed.
        "UPDATE members SET id = 50 WHERE id = 2",
        // The rowid moves onto a key another row holds, which is not legal.
        "UPDATE members SET id = 3 WHERE id = 50",
        // No unique index holds `team`, so nothing is probed and nothing fails.
        "UPDATE members SET team = 'gold' WHERE id = 3",
        // A NULL is distinct from every other NULL, including under an update.
        "UPDATE members SET alias = NULL WHERE id = 3",
        // Every row at once, each onto a value only it holds.
        "UPDATE members SET email = email || '!'",
        // Every row at once, onto a value another row holds.
        "UPDATE members SET email = 'ana@x!'",
        // `OR IGNORE` skips the row; `OR REPLACE` deletes the one in the way.
        "UPDATE OR IGNORE members SET email = 'ana@x!' WHERE id = 5",
        "UPDATE OR REPLACE members SET email = 'ana@x!' WHERE id = 5",
        // The upsert's update arm has the same reach: it collides on
        // `members_alias` while resolving a conflict on `members_email`.
        "INSERT INTO members VALUES (60, 'n60@x', 'red', 'n60', 1)",
        "INSERT INTO members VALUES (61, 'n60@x', 'red', 'zz', 2) \
         ON CONFLICT(email) DO UPDATE SET alias = 'cy'",
        // And the same arm assigning a value the conflicting row already holds,
        // which is the row finding itself again.
        "INSERT INTO members VALUES (62, 'n60@x', 'red', 'zz', 3) \
         ON CONFLICT(email) DO UPDATE SET alias = 'n60', score = 7",
    ] {
        let (reference, ours) = apply(&mut pair, sql);
        match (&reference, &ours) {
            (Some(theirs), Some(mine)) if theirs == mine => {}
            (None, None) => {}
            _ => failures.push(format!("{sql}\n  sqlite {reference:?}\n  ours   {ours:?}")),
        }
        for difference in differences(&mut pair) {
            failures.push(format!("after {sql}\n{difference}"));
        }
    }
    assert!(
        failures.is_empty(),
        "an update resolved a unique key differently from SQLite:\n{}",
        failures.join("\n\n")
    );
}

/// `ON CONFLICT` resolves a collision the way SQLite resolves it.
#[test]
fn on_conflict_does_what_sqlite_does() {
    let Some(mut pair) = pair("conflict") else {
        eprintln!("the pinned SQLite oracle is not built; nothing was compared");
        return;
    };

    let mut failures = Vec::new();
    for sql in [
        // `DO NOTHING`: the row already there is left exactly as it was.
        "INSERT INTO members VALUES (1, 'zz@x', 'gold', 'zz', 99) ON CONFLICT DO NOTHING",
        // `DO UPDATE`, reading `excluded`.
        "INSERT INTO members VALUES (2, 'bo@x', 'gold', 'bo2', 77) \
         ON CONFLICT(id) DO UPDATE SET score = excluded.score, team = excluded.team",
        // `OR IGNORE`, the statement-level form.
        "INSERT OR IGNORE INTO members VALUES (3, 'zz@x', 'gold', 'zz', 99)",
        // `OR REPLACE`, which deletes the row in the way and takes its place.
        "INSERT OR REPLACE INTO members VALUES (4, 'zz@x', 'gold', 'zz', 99)",
        // An upsert that has to find the conflict through a secondary index.
        "INSERT INTO members VALUES (90, 'ana@x', 'gold', 'a90', 5) \
         ON CONFLICT(email) DO UPDATE SET score = excluded.score",
    ] {
        let (reference, ours) = apply(&mut pair, sql);
        if reference.is_some() != ours.is_some() {
            failures.push(format!("{sql}\n  sqlite {reference:?}\n  ours   {ours:?}"));
            continue;
        }
        for difference in differences(&mut pair) {
            failures.push(format!("after {sql}\n{difference}"));
        }
    }
    assert!(
        failures.is_empty(),
        "`ON CONFLICT` resolved differently from SQLite:\n{}",
        failures.join("\n\n")
    );
}

/// A rowid the statement leaves out is the next one after the largest.
#[test]
fn an_omitted_rowid_is_allocated_the_way_sqlite_allocates_it() {
    let Some(mut pair) = pair("rowid") else {
        eprintln!("the pinned SQLite oracle is not built; nothing was compared");
        return;
    };

    let mut failures = Vec::new();
    for sql in [
        "INSERT INTO members (email, team, alias, score) VALUES ('n1@x', 'red', 'n1', 1)",
        "INSERT INTO members (email, team, alias, score) VALUES ('n2@x', 'red', 'n2', 2)",
        // The largest key is deleted, so the next allocation reuses its number -
        // which is SQLite's rule for a table that is not `AUTOINCREMENT`. The
        // key is written out rather than found with `max(id)`, because a scalar
        // subquery in a `WHERE` is a read-path gap this phase leaves named and
        // open, and a test that met it here would be grading the write path on
        // somebody else's refusal.
        "DELETE FROM members WHERE id = 7",
        "INSERT INTO members (email, team, alias, score) VALUES ('n3@x', 'red', 'n3', 3)",
        // And once more, so the reused number is followed by a fresh one.
        "INSERT INTO members (email, team, alias, score) VALUES ('n4@x', 'red', 'n4', 4)",
    ] {
        let (reference, ours) = apply(&mut pair, sql);
        if reference.is_some() != ours.is_some() {
            failures.push(format!("{sql}\n  sqlite {reference:?}\n  ours   {ours:?}"));
            continue;
        }
        for difference in differences(&mut pair) {
            failures.push(format!("after {sql}\n{difference}"));
        }
    }
    assert!(
        failures.is_empty(),
        "rowids were allocated differently from SQLite:\n{}",
        failures.join("\n\n")
    );
}

/// `RETURNING` names the row as it ended up, not as it arrived.
#[test]
fn returning_names_the_row_that_was_written() {
    let Some(mut pair) = pair("returning") else {
        eprintln!("the pinned SQLite oracle is not built; nothing was compared");
        return;
    };

    let mut failures = Vec::new();
    for sql in [
        "INSERT INTO members VALUES (30, 'r1@x', 'red', 'r1', 3) RETURNING id, email",
        "UPDATE members SET score = score * 2 WHERE id = 30 RETURNING id, score",
        "DELETE FROM members WHERE id = 30 RETURNING id, email",
    ] {
        let reference = pair
            .oracle
            .send(&Op::Query(sql.to_string()))
            .expect("the oracle answers");
        let ours = match pair.engine.execute_any(sql, &Params::new()) {
            Ok(outcome) => outcome.rows,
            Err(error) => {
                failures.push(format!(
                    "{sql}\n  the new engine refused: {}",
                    error.detail().unwrap_or("no detail")
                ));
                continue;
            }
        };
        let rendered: Vec<Vec<TaggedValue>> = ours.iter().map(|row| render(row)).collect();
        if !same(&reference.rows, &rendered) {
            failures.push(format!(
                "{sql}\n  sqlite {:?}\n  ours   {rendered:?}",
                reference.rows
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "`RETURNING` answered differently from SQLite:\n{}",
        failures.join("\n\n")
    );
}

/// Every imported tree is in its own key order.
///
/// A descending index is stored descending by SQLite, and the import walks it
/// in that physical order. A tree whose leaves are not in the order its own
/// comparisons define answers a *seek* wrongly while a scan of it looks fine -
/// so this is checked directly rather than inferred from a query.
#[test]
fn every_imported_tree_is_in_its_own_key_order() {
    let Some(pair) = pair("order") else {
        eprintln!("the pinned SQLite oracle is not built; nothing was compared");
        return;
    };
    pair.engine
        .check_trees()
        .unwrap_or_else(|error| panic!("a tree is out of order: {:?}", error.detail()));
}

/// The two engines agree about the fixture before anything is written to it.
///
/// The baseline every other test in this file rests on. A difference that is
/// already there before the first write is an import or a read difference, and
/// grading a write against it would blame the write path for something it did
/// not do.
#[test]
fn the_two_engines_agree_before_anything_is_written() {
    let Some(mut pair) = pair("baseline") else {
        eprintln!("the pinned SQLite oracle is not built; nothing was compared");
        return;
    };
    let found = differences(&mut pair);
    assert!(
        found.is_empty(),
        "the engines disagree about the fixture as imported:\n{}",
        found.join("\n\n")
    );
}

/// A subquery in a `WHERE` changes the same rows SQLite changes.
///
/// It was a **read**-path gap rather than a write one - nothing in the physical
/// pass translated a `BoundExpr::Subquery` in an expression position, so a
/// query carrying one was refused whether or not a write was attached. This
/// test was that refusal; it is now the answer, because the fold in
/// `inillucent-exec`'s `subquery` module evaluates an uncorrelated block once
/// per execution and hands the operator chain a literal.
///
/// It stays on the write path because that is where the gap was met, and
/// because a write is the sharper question: the two engines have to agree about
/// *which rows changed*, not just about what a `SELECT` answered. The three
/// statements run in sequence on both sides, so the `DELETE` moves the maximum
/// the `UPDATE` then looks up.
#[test]
fn a_subquery_in_a_where_changes_the_rows_sqlite_changes() {
    let Some(mut pair) = pair("subquery") else {
        eprintln!("the pinned SQLite oracle is not built; nothing was compared");
        return;
    };
    let mut failures = Vec::new();

    compare(
        &mut pair,
        "SELECT id FROM members WHERE id = (SELECT max(id) FROM members)",
        &mut failures,
    );
    compare(
        &mut pair,
        "SELECT id FROM members WHERE id IN (SELECT id FROM members WHERE score >= 20)          ORDER BY id",
        &mut failures,
    );

    for statement in [
        "DELETE FROM members WHERE id = (SELECT max(id) FROM members)",
        "UPDATE members SET score = 0 WHERE id = (SELECT max(id) FROM members)",
    ] {
        let reference = pair
            .oracle
            .send(&Op::Query(statement.to_string()))
            .expect("the oracle answers");
        assert!(reference.ok, "{statement}: sqlite refused it");
        if let Err(error) = pair.engine.execute_any(statement, &Params::new()) {
            failures.push(format!(
                "{statement}
  the new engine refused: {}",
                error.detail().unwrap_or("no detail")
            ));
            continue;
        }
        // What the write actually did, which is the part a row count would not
        // catch: a subquery read against the wrong snapshot changes the wrong
        // row and still reports one change.
        compare(
            &mut pair,
            "SELECT id, score FROM members ORDER BY id",
            &mut failures,
        );
    }

    assert!(
        failures.is_empty(),
        "the engines disagree about a subquery in a WHERE:
{}",
        failures.join(
            "

"
        )
    );
}

/// Compares one query's answer against SQLite's, in order.
///
/// @param pair - the two engines over the same data
/// @param sql - the query
/// @param failures - where a difference is recorded
fn compare(pair: &mut Pair, sql: &str, failures: &mut Vec<String>) {
    let reference = pair
        .oracle
        .send(&Op::Query(sql.to_string()))
        .expect("the oracle answers");
    if !reference.ok {
        failures.push(format!("{sql}\n  sqlite refused it: {}", reference.message));
        return;
    }
    let ours = match pair.engine.execute_any(sql, &Params::new()) {
        Ok(outcome) => outcome.rows,
        Err(error) => {
            failures.push(format!(
                "{sql}\n  the new engine refused: {}",
                error.detail().unwrap_or("no detail")
            ));
            return;
        }
    };
    let rendered: Vec<Vec<TaggedValue>> = ours.iter().map(|row| render(row)).collect();
    if !same(&reference.rows, &rendered) {
        failures.push(format!(
            "{sql}\n  sqlite {:?}\n  ours   {rendered:?}",
            reference.rows
        ));
    }
}

/// The four set operators answer what SQLite answers.
///
/// A compound is the last of the three Phase 2 gaps this ticket names. It is
/// swept rather than sampled: every operator against every operator, because
/// `EXCEPT` and `INTERSECT` have to see their right arm before they can judge a
/// left row and the two unions do not, and a chain mixes the two rules.
///
/// Every query carries a total `ORDER BY`. A compound's row order is otherwise
/// unspecified, and a strict comparison of an unspecified order reports
/// differences that are not differences.
#[test]
fn the_four_set_operators_answer_what_sqlite_answers() {
    let Some(mut pair) = pair("compound") else {
        eprintln!("the pinned SQLite oracle is not built; nothing was compared");
        return;
    };

    let arms = [
        "SELECT team FROM members WHERE score >= 20",
        "SELECT team FROM members WHERE alias IS NOT NULL",
        "SELECT team FROM members WHERE id < 3",
    ];
    let operators = ["UNION", "UNION ALL", "EXCEPT", "INTERSECT"];

    let mut failures = Vec::new();
    for left in arms {
        for right in arms {
            for op in operators {
                compare(
                    &mut pair,
                    &format!("{left} {op} {right} ORDER BY 1"),
                    &mut failures,
                );
            }
        }
    }
    // A chain of three arms, so the fold is exercised rather than one step of
    // it - and mixing the operators is what makes the fold's order matter.
    for first in operators {
        for second in operators {
            compare(
                &mut pair,
                &format!(
                    "{} {first} {} {second} {} ORDER BY 1",
                    arms[0], arms[1], arms[2]
                ),
                &mut failures,
            );
        }
    }
    // The compound's own `ORDER BY`, `LIMIT` and `OFFSET`, which belong to the
    // whole and not to the last arm.
    for tail in [
        "ORDER BY 1 LIMIT 2",
        "ORDER BY 1 DESC LIMIT 2",
        "ORDER BY 1 LIMIT 2 OFFSET 1",
        "ORDER BY 1 LIMIT -1",
    ] {
        compare(
            &mut pair,
            &format!("{} UNION {} {tail}", arms[0], arms[1]),
            &mut failures,
        );
        compare(
            &mut pair,
            &format!("{} UNION ALL {} {tail}", arms[0], arms[1]),
            &mut failures,
        );
    }
    // Multi-column arms, so the set comparison is over whole rows rather than
    // over one value.
    for op in operators {
        compare(
            &mut pair,
            &format!(
                "SELECT team, score FROM members WHERE id <= 3 {op} \
                 SELECT team, score FROM members WHERE id >= 3 ORDER BY 1, 2"
            ),
            &mut failures,
        );
    }
    assert!(
        failures.is_empty(),
        "a compound query answered differently from SQLite:\n{}",
        failures.join("\n\n")
    );
}

/// The window functions answer what SQLite answers.
///
/// The last of the three Phase 2 gaps this ticket names. It is swept across the
/// eleven functions that only exist in a window, the aggregates over a frame,
/// and the frame specifications themselves - because a frame is where the peer
/// rules live, and `RANGE` and `ROWS` differ exactly when there are peers.
///
/// Every query carries a total `ORDER BY` on its output. A window's *input*
/// order is fixed by its own `ORDER BY`, but the order the rows come *out* in
/// is not, and comparing an unspecified order reports differences that are not
/// differences.
#[test]
fn the_window_functions_answer_what_sqlite_answers() {
    let Some(mut pair) = pair("window") else {
        eprintln!("the pinned SQLite oracle is not built; nothing was compared");
        return;
    };

    let mut failures = Vec::new();

    // The eleven that only exist in a window, over one window.
    for call in [
        "row_number()",
        "rank()",
        "dense_rank()",
        "percent_rank()",
        "cume_dist()",
        "ntile(2)",
        "lag(score)",
        "lead(score)",
        "first_value(score)",
        "last_value(score)",
        "nth_value(score, 2)",
    ] {
        compare(
            &mut pair,
            &format!("SELECT id, {call} OVER (ORDER BY score, id) AS w FROM members ORDER BY id"),
            &mut failures,
        );
        compare(
            &mut pair,
            &format!(
                "SELECT id, {call} OVER (PARTITION BY team ORDER BY score, id) AS w \
                 FROM members ORDER BY id"
            ),
            &mut failures,
        );
    }

    // The aggregates over a frame.
    for call in [
        "count(*)",
        "count(score)",
        "sum(score)",
        "total(score)",
        "avg(score)",
        "min(score)",
        "max(score)",
        "group_concat(email)",
    ] {
        compare(
            &mut pair,
            &format!(
                "SELECT id, {call} OVER (PARTITION BY team ORDER BY id) AS w \
                 FROM members ORDER BY id"
            ),
            &mut failures,
        );
    }

    // The frames, in two groups, because the two groups need opposite orderings
    // to be *questions with one right answer*.
    //
    // A `ROWS` frame counts rows, so which of two peers is the earlier row
    // decides its answer - and which that is, is unspecified. These are asked
    // over `(score, id)`, a total order, so the frame is graded and the tie is
    // not.
    for frame in [
        "ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW",
        "ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING",
        "ROWS BETWEEN CURRENT ROW AND UNBOUNDED FOLLOWING",
        "ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING",
        "ROWS BETWEEN 2 PRECEDING AND CURRENT ROW",
    ] {
        compare(
            &mut pair,
            &format!(
                "SELECT id, sum(score) OVER (ORDER BY score, id {frame}) AS w \
                 FROM members ORDER BY id"
            ),
            &mut failures,
        );
    }
    // A `RANGE` or `GROUPS` frame takes whole peer groups, so its answer is
    // decided even when two rows tie - and a tie is exactly what makes it
    // different from `ROWS`. These are asked over `score` alone, deliberately,
    // because the two rows at 20 are the case being graded.
    for frame in [
        "RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW",
        "RANGE BETWEEN CURRENT ROW AND UNBOUNDED FOLLOWING",
        "RANGE BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING",
        "GROUPS BETWEEN 1 PRECEDING AND 1 FOLLOWING",
        "GROUPS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW",
    ] {
        compare(
            &mut pair,
            &format!(
                "SELECT id, sum(score) OVER (ORDER BY score {frame}) AS w \
                 FROM members ORDER BY id"
            ),
            &mut failures,
        );
    }

    // Two calls sharing one window, an expression over a window value, a
    // descending window ordering, and the statement's own ORDER BY and LIMIT
    // over the result.
    for sql in [
        "SELECT id, row_number() OVER (ORDER BY score, id), rank() OVER (ORDER BY score, id) \
         FROM members ORDER BY id",
        "SELECT id, row_number() OVER (ORDER BY score, id) * 10 FROM members ORDER BY id",
        "SELECT id, row_number() OVER (ORDER BY score DESC, id) FROM members ORDER BY id",
        "SELECT id, row_number() OVER (PARTITION BY team) FROM members ORDER BY id",
        "SELECT id, row_number() OVER (ORDER BY score, id) AS w FROM members ORDER BY w DESC",
        "SELECT id, row_number() OVER (ORDER BY score, id) AS w FROM members \
         ORDER BY id LIMIT 3",
        "SELECT team, row_number() OVER (PARTITION BY team ORDER BY id) FROM members \
         WHERE score >= 20 ORDER BY team, 2",
    ] {
        compare(&mut pair, sql, &mut failures);
    }

    assert!(
        failures.is_empty(),
        "a window function answered differently from SQLite:\n{}",
        failures.join("\n\n")
    );
}

/// Two different windows in one statement are two passes, and both are right.
///
/// One buffer can only be sorted one way, and the operator computes each call's
/// peer groups over a sequence it assumes is sorted by that call's ordering -
/// so two windows with different frames were refused by name until task-1838,
/// which groups the calls by frame and runs one pass per group. The answers are
/// scattered back into the slot the binder numbered each call, which is what
/// keeps a two-pass statement's projection reading the same columns a one-pass
/// statement's does.
///
/// Graded against the pinned shell, because the interesting half is that the
/// *second* pass is right: a first pass that answered both calls would produce
/// plausible numbers in the wrong order.
#[test]
fn two_different_windows_in_one_statement_are_both_computed() {
    let Some(mut pair) = pair("twowindows") else {
        eprintln!("the pinned SQLite oracle is not built; nothing was compared");
        return;
    };
    let mut failures = Vec::new();
    compare(
        &mut pair,
        "SELECT id, row_number() OVER (ORDER BY id), row_number() OVER (ORDER BY score, id)          FROM members ORDER BY id",
        &mut failures,
    );
    compare(
        &mut pair,
        "SELECT id, count(*) OVER (PARTITION BY team), sum(score) OVER (ORDER BY id)          FROM members ORDER BY id",
        &mut failures,
    );
    assert!(
        failures.is_empty(),
        "{}",
        failures.join(
            "

"
        )
    );
}

/// An `ORDER BY` over a descending index comes back in the right order.
///
/// The third face of the same root cause as the import order and the range
/// bounds. The planner reads the *catalog*, where `members_score` is
/// `(score DESC, email)`, and concludes that scanning that index forwards gives
/// `ORDER BY score DESC` and backwards gives `ORDER BY score`. Our tree stores
/// it ascending, so both conclusions are inverted - and the query comes back in
/// exactly the wrong order, with no error anywhere.
///
/// It is a **read**-path wrong answer, reachable from a plain `SELECT` with no
/// write involved. It was found by the window sweep, whose inner query is
/// `SELECT score, id ... ORDER BY score` - a projection the descending index
/// covers, which is what made the planner reach for it.
#[test]
fn an_order_by_over_a_descending_index_is_not_reversed() {
    let Some(mut pair) = pair("descorder") else {
        eprintln!("the pinned SQLite oracle is not built; nothing was compared");
        return;
    };
    // Every ordering is total. Two rows share a score, and which of them comes
    // first under `ORDER BY score` alone is unspecified - so a query without a
    // tiebreak would report a difference that is not one, and would say nothing
    // about the direction, which is what this test is for.
    let mut failures = Vec::new();
    for sql in [
        "SELECT score, id FROM members ORDER BY score, id",
        "SELECT score, id FROM members ORDER BY score DESC, id",
        "SELECT score, id FROM members ORDER BY score, id DESC",
        "SELECT score, id FROM members ORDER BY score DESC, id DESC",
        "SELECT score, email FROM members ORDER BY score, email",
        "SELECT score, email FROM members ORDER BY score DESC, email",
        "SELECT score, email FROM members ORDER BY score, email DESC",
        "SELECT score, email FROM members ORDER BY score DESC, email DESC",
        "SELECT score, id FROM members WHERE score >= 20 ORDER BY score, id",
        "SELECT score, id FROM members WHERE score < 30 ORDER BY score, id",
        "SELECT score, id FROM members ORDER BY score, id LIMIT 2",
        "SELECT score, id FROM members ORDER BY score DESC, id LIMIT 2",
        // The count is direction-blind, so it grades the *bound* on its own:
        // this is the query that returned three where SQLite returned four.
        "SELECT count(*) FROM members WHERE score >= 20",
        "SELECT count(*) FROM members WHERE score > 20",
        "SELECT count(*) FROM members WHERE score <= 20",
        "SELECT count(*) FROM members WHERE score BETWEEN 15 AND 35",
    ] {
        compare(&mut pair, sql, &mut failures);
    }
    assert!(
        failures.is_empty(),
        "an ordering over a descending index came back wrong:\n{}",
        failures.join("\n\n")
    );
}

/// Runs a script through both engines, grading every statement and every probe.
///
/// **The grading is per statement, not at the end**, for the reason the file's
/// campaign test gives: a statement that leaves the two engines on different
/// data is the statement to name, and a comparison only at the end names the
/// last one. Each step compares three things - whether both engines failed, the
/// message when they did, and every probe query - plus `check_trees`, so a row
/// put back by an undo that missed an index entry is caught here rather than by
/// a covering query months later.
///
/// The probes are the caller's because these tables are the caller's: the
/// fixture's `members` schema is not what the task-1850 repros are written
/// over, and each of them wants its own shape.
///
/// @param pair - the two engines over the same data
/// @param script - the statements, in order
/// @param probes - the queries asked after every statement
fn walk(pair: &mut Pair, script: &[&str], probes: &[&str]) -> Vec<String> {
    let mut failures = Vec::new();
    for sql in script {
        let (reference, ours) = apply(pair, sql);
        if reference.is_some() != ours.is_some() {
            failures.push(format!("{sql}\n  sqlite {reference:?}\n  ours   {ours:?}"));
            continue;
        }
        if let (Some(theirs), Some(mine)) = (&reference, &ours) {
            if theirs != mine {
                failures.push(format!(
                    "{sql}\n  sqlite refused with {theirs:?}\n  ours   refused with {mine:?}"
                ));
            }
        }
        if let Err(error) = pair.engine.check_trees() {
            failures.push(format!(
                "after {sql}\n  a tree is structurally wrong: {}",
                error.detail().unwrap_or("no detail")
            ));
        }
        for probe in probes {
            probe_both(pair, probe, &mut failures);
        }
    }
    failures
}

/// Compares one probe, counting a refusal both engines agree on as agreement.
///
/// [`compare`] treats a query the oracle refuses as evidence of nothing and
/// records it, which is right where the probe is the point. Here the probes run
/// after *every* statement of a script that builds its own tables, so a probe
/// over the second table is asked before the second table exists - and "no such
/// table" from both engines is a real answer they agree on. A refusal from only
/// one of them is still a difference, and that is the case worth keeping.
///
/// @param pair - the two engines over the same data
/// @param sql - the query
/// @param failures - where a difference is recorded
fn probe_both(pair: &mut Pair, sql: &str, failures: &mut Vec<String>) {
    let reference = pair
        .oracle
        .send(&Op::Query(sql.to_string()))
        .expect("the oracle answers");
    if !reference.ok {
        match pair.engine.execute_any(sql, &Params::new()) {
            Ok(_) => failures.push(format!(
                "{sql}\n  sqlite refused it: {}\n  ours   answered it",
                reference.message
            )),
            Err(_) => {}
        }
        return;
    }
    compare(pair, sql, failures);
}

/// A statement that fails partway puts back everything it had written.
///
/// **This is task-1850's defect, and it is one missing thing rather than a list
/// of cases.** SQLite's default conflict algorithm is `ABORT`, which undoes the
/// *statement* and keeps the transaction; this engine undid nothing, so a
/// four-row `INSERT` that collided on its third row kept the first two and
/// committed them - half a statement, durably, behind a diagnostic that said it
/// failed.
///
/// The cases below are deliberately not all about uniqueness. The engine had no
/// statement boundary at all, so every kind of failure leaked the same way, and
/// each of these was measured differing against the pinned oracle before the
/// boundary existed: a `NOT NULL`, a `STRICT` type class, an `INSERT ... SELECT`
/// rather than `VALUES`, an `UPDATE OR REPLACE` that **deleted** the row in its
/// way and then failed a `CHECK` - which is the worst shape of it, because the
/// failure destroys a row rather than half-writing one - and a `DELETE` stopped
/// by a `BEFORE DELETE` trigger, which had already removed the rows before it.
#[test]
fn a_statement_that_fails_partway_puts_back_what_it_wrote() {
    let Some(mut pair) = pair("partial") else {
        eprintln!("the pinned SQLite oracle is not built; nothing was compared");
        return;
    };

    let mut failures = walk(
        &mut pair,
        &[
            "CREATE TABLE k(id INTEGER PRIMARY KEY, b INTEGER)",
            "INSERT INTO k VALUES (1,1),(2,2),(12,3)",
            // The ticket's first repro: row 1 moves to 11, row 2 collides with
            // 12, and the 11 stayed.
            "UPDATE k SET id = id + 10 WHERE id <= 2",
            // A multi-row INSERT whose second row collides.
            "INSERT INTO k VALUES (9,0)",
            "INSERT INTO k VALUES (7,1),(9,2)",
            // The rows a `SELECT` produced rather than a `VALUES` list.
            "INSERT INTO k SELECT id + 1, b FROM k ORDER BY id",
        ],
        &["SELECT id, b FROM k ORDER BY id", "SELECT count(*) FROM k"],
    );

    failures.extend(walk(
        &mut pair,
        &[
            "CREATE TABLE n(a TEXT, b INTEGER NOT NULL)",
            "INSERT INTO n VALUES ('p',1),('q',2)",
            // No index anywhere: the row that fails is the second, and the
            // first had already been rewritten.
            "UPDATE n SET a = 'r', b = CASE WHEN a = 'q' THEN NULL ELSE 5 END",
        ],
        &["SELECT a, b FROM n ORDER BY rowid"],
    ));

    failures.extend(walk(
        &mut pair,
        &[
            "CREATE TABLE s(a INTEGER) STRICT",
            // Not a constraint at all - a type class - and it leaked the same
            // way, which is what the untagged-error default is for.
            "INSERT INTO s VALUES (1),('x'),(3)",
        ],
        &["SELECT a FROM s ORDER BY rowid", "SELECT count(*) FROM s"],
    ));

    failures.extend(walk(
        &mut pair,
        &[
            "CREATE TABLE r(a TEXT, b INTEGER CHECK(b < 9))",
            "CREATE UNIQUE INDEX ru ON r(a)",
            "INSERT INTO r VALUES ('x',1),('y',2)",
            // `OR REPLACE` deletes the row in the way and *then* fails the
            // `CHECK`, so the failure destroyed a row rather than half-writing
            // one - and reported an error, so nothing suggested looking.
            "UPDATE OR REPLACE r SET a = 'x', b = 20 WHERE b = 2",
        ],
        &[
            "SELECT a, b FROM r ORDER BY b",
            "SELECT a FROM r ORDER BY a",
        ],
    ));

    failures.extend(walk(
        &mut pair,
        &[
            "CREATE TABLE d(id INTEGER PRIMARY KEY, b INTEGER)",
            "INSERT INTO d VALUES (1,1),(2,2),(3,3)",
            "CREATE TRIGGER dg BEFORE DELETE ON d WHEN OLD.id = 3 \
             BEGIN SELECT RAISE(ABORT,'no'); END",
            // A `DELETE`, which has no `OR` clause at all and so can only get
            // the default.
            "DELETE FROM d",
        ],
        &["SELECT id FROM d ORDER BY id", "SELECT count(*) FROM d"],
    ));

    assert!(
        failures.is_empty(),
        "a failed statement did not put back what it wrote:\n{}",
        failures.join("\n\n")
    );
}

/// `FAIL` keeps the rows written before the failure and `ABORT` keeps none.
///
/// The two were indistinguishable: both raised and neither undid anything, so
/// `FAIL` was right by accident and `ABORT` - which every unqualified statement
/// gets - was wrong. The same statement is run three ways over the same data so
/// the difference is the clause and nothing else.
#[test]
fn or_fail_keeps_the_rows_or_abort_does_not() {
    let Some(mut pair) = pair("failabort") else {
        eprintln!("the pinned SQLite oracle is not built; nothing was compared");
        return;
    };

    let probes = ["SELECT id, b FROM f ORDER BY id", "SELECT count(*) FROM f"];
    let mut failures = walk(
        &mut pair,
        &[
            "CREATE TABLE f(id INTEGER PRIMARY KEY, b INTEGER)",
            "INSERT INTO f VALUES (30,0)",
            // Four rows, the third colliding. `OR FAIL` keeps 10 and 20.
            "INSERT OR FAIL INTO f VALUES (10,1),(20,2),(30,3),(40,4)",
            "DELETE FROM f WHERE id <> 30",
            // The same statement, defaulting to `ABORT`: nothing is kept.
            "INSERT INTO f VALUES (10,1),(20,2),(30,3),(40,4)",
            // And written out, which has to mean the same thing.
            "INSERT OR ABORT INTO f VALUES (10,1),(20,2),(30,3),(40,4)",
            // `IGNORE` and `REPLACE` resolve rather than raise, and are here so
            // that a change to the raising arms cannot quietly move them.
            "INSERT OR IGNORE INTO f VALUES (10,1),(20,2),(30,3),(40,4)",
            "DELETE FROM f WHERE id <> 30",
            "INSERT OR REPLACE INTO f VALUES (10,1),(20,2),(30,3),(40,4)",
        ],
        &probes,
    );

    failures.extend(walk(
        &mut pair,
        &[
            "CREATE TABLE g(id INTEGER PRIMARY KEY, b INTEGER)",
            "INSERT INTO g VALUES (1,1),(2,2),(12,3)",
            // An `UPDATE` rather than an `INSERT`: the first row moved and
            // `OR FAIL` keeps the move.
            "UPDATE OR FAIL g SET id = id + 10 WHERE id <= 2",
        ],
        &["SELECT id, b FROM g ORDER BY b"],
    ));

    failures.extend(walk(
        &mut pair,
        &[
            "CREATE TABLE h(a TEXT, b INTEGER NOT NULL)",
            "INSERT INTO h VALUES ('p',1),('q',2)",
            "UPDATE OR FAIL h SET a = 'r', b = CASE WHEN a = 'q' THEN NULL ELSE 5 END",
        ],
        &["SELECT a, b FROM h ORDER BY rowid"],
    ));

    assert!(
        failures.is_empty(),
        "`OR FAIL` and `OR ABORT` did not differ the way SQLite's do:\n{}",
        failures.join("\n\n")
    );
}

/// `OR ROLLBACK` discards the whole open transaction, not just the statement.
///
/// Three things go, and the engine used to lose all three: the rows the
/// transaction had already written, the savepoints inside it, and the
/// transaction itself - so the `COMMIT` that follows has nothing to commit and
/// says so. That last one is how a caller *finds out*, which is why the scripts
/// here end in a `COMMIT` and a statement after it.
#[test]
fn or_rollback_discards_the_transaction() {
    let Some(mut pair) = pair("rollback") else {
        eprintln!("the pinned SQLite oracle is not built; nothing was compared");
        return;
    };

    let probes = ["SELECT id, b FROM w ORDER BY id", "SELECT count(*) FROM w"];
    let mut failures = walk(
        &mut pair,
        &[
            "CREATE TABLE w(id INTEGER PRIMARY KEY, b INTEGER)",
            "INSERT INTO w VALUES (30,0)",
            "BEGIN",
            "INSERT INTO w VALUES (99,9)",
            // The 99 goes with the transaction, not just the row that failed.
            "INSERT OR ROLLBACK INTO w VALUES (10,1),(30,3)",
            // Nothing is open, so this refuses - which is the observable.
            "COMMIT",
            // And the connection still works afterwards.
            "INSERT INTO w VALUES (98,8)",
        ],
        &probes,
    );

    failures.extend(walk(
        &mut pair,
        &[
            "BEGIN",
            "INSERT INTO w VALUES (97,7)",
            "SAVEPOINT s1",
            "INSERT INTO w VALUES (96,6)",
            // The savepoints go with the transaction: `RELEASE` then has no
            // such savepoint to release.
            "UPDATE OR ROLLBACK w SET id = 30 WHERE id = 97",
            "RELEASE s1",
            "COMMIT",
        ],
        &probes,
    ));

    failures.extend(walk(
        &mut pair,
        &[
            // Outside a transaction `ROLLBACK` and `ABORT` are the same thing,
            // because the statement *is* the transaction - and the `COMMIT`
            // after it still has nothing to commit.
            "INSERT OR ROLLBACK INTO w VALUES (11,1),(30,3)",
            "COMMIT",
        ],
        &probes,
    ));

    failures.extend(walk(
        &mut pair,
        &[
            "CREATE TABLE c(a TEXT, b INTEGER NOT NULL ON CONFLICT ROLLBACK)",
            "INSERT INTO c VALUES ('p',1)",
            "BEGIN",
            "INSERT INTO c VALUES ('w',9)",
            // Written on the constraint rather than on the statement, which is
            // the same algorithm reached a different way.
            "INSERT INTO c VALUES ('x',NULL)",
            "COMMIT",
        ],
        &["SELECT a, b FROM c ORDER BY rowid"],
    ));

    assert!(
        failures.is_empty(),
        "`OR ROLLBACK` did not discard the transaction:\n{}",
        failures.join("\n\n")
    );
}

/// A statement failing inside a `SAVEPOINT` leaves the savepoint standing.
///
/// The other side of the `OR ROLLBACK` test, and the one that says the undo is
/// *scoped*: `ABORT` puts back the statement and stops, so everything the
/// transaction wrote before it - including the savepoint - is still there, and
/// `ROLLBACK TO` then means what it meant.
#[test]
fn a_failed_statement_leaves_the_savepoint_around_it() {
    let Some(mut pair) = pair("savepoint") else {
        eprintln!("the pinned SQLite oracle is not built; nothing was compared");
        return;
    };

    let failures = walk(
        &mut pair,
        &[
            "CREATE TABLE p(id INTEGER PRIMARY KEY, b INTEGER)",
            "INSERT INTO p VALUES (30,0)",
            "BEGIN",
            "INSERT INTO p VALUES (99,9)",
            "SAVEPOINT s1",
            "INSERT INTO p VALUES (98,8)",
            // Fails on its second row: the 97 goes, the 98 and the 99 stay.
            "INSERT INTO p VALUES (97,7),(30,3)",
            // The savepoint is still there, so this takes the 98 with it.
            "ROLLBACK TO s1",
            "COMMIT",
        ],
        &["SELECT id, b FROM p ORDER BY id", "SELECT count(*) FROM p"],
    );

    assert!(
        failures.is_empty(),
        "a failed statement disturbed the savepoint around it:\n{}",
        failures.join("\n\n")
    );
}

/// A trigger's `RAISE` action decides what the statement undoes.
///
/// `RAISE(ABORT)`, `RAISE(FAIL)` and `RAISE(ROLLBACK)` report the same code and
/// the same message and differ **only** in this, which is why all three behaved
/// as one before the action was carried past the compiler. `RAISE(IGNORE)` is
/// here as the control: it abandons the row rather than failing, and the firing
/// point rather than the unwind is what does it.
///
/// The second half is the one a caller notices: a trigger's writes to *another*
/// table are the failing statement's writes too, and they go back with it.
#[test]
fn a_triggers_raise_action_decides_what_goes_back() {
    let Some(mut pair) = pair("raise") else {
        eprintln!("the pinned SQLite oracle is not built; nothing was compared");
        return;
    };

    let probes = [
        "SELECT id, b FROM t1 ORDER BY id",
        "SELECT count(*) FROM t1",
    ];
    let mut failures = walk(
        &mut pair,
        &[
            "CREATE TABLE t1(id INTEGER PRIMARY KEY, b INTEGER)",
            "CREATE TRIGGER t1a BEFORE INSERT ON t1 WHEN NEW.b = 2 \
             BEGIN SELECT RAISE(ABORT,'no'); END",
            "INSERT INTO t1 VALUES (1,1),(2,2),(3,3)",
            "DROP TRIGGER t1a",
            "CREATE TRIGGER t1f BEFORE INSERT ON t1 WHEN NEW.b = 2 \
             BEGIN SELECT RAISE(FAIL,'no'); END",
            "INSERT INTO t1 VALUES (1,1),(2,2),(3,3)",
            "DROP TRIGGER t1f",
            "DELETE FROM t1",
            "CREATE TRIGGER t1i BEFORE INSERT ON t1 WHEN NEW.b = 2 \
             BEGIN SELECT RAISE(IGNORE); END",
            "INSERT INTO t1 VALUES (1,1),(2,2),(3,3)",
            "DROP TRIGGER t1i",
        ],
        &probes,
    );

    failures.extend(walk(
        &mut pair,
        &[
            "DELETE FROM t1",
            "CREATE TRIGGER t1r BEFORE INSERT ON t1 WHEN NEW.b = 2 \
             BEGIN SELECT RAISE(ROLLBACK,'no'); END",
            "BEGIN",
            "INSERT INTO t1 VALUES (9,9)",
            // The 9 goes with the transaction, and the `COMMIT` after it has
            // nothing to commit.
            "INSERT INTO t1 VALUES (1,1),(2,2),(3,3)",
            "COMMIT",
            "DROP TRIGGER t1r",
        ],
        &probes,
    ));

    failures.extend(walk(
        &mut pair,
        &[
            "CREATE TABLE t2(id INTEGER PRIMARY KEY, b INTEGER)",
            "CREATE TABLE t2log(x INTEGER)",
            "CREATE TRIGGER t2a AFTER INSERT ON t2 BEGIN INSERT INTO t2log VALUES (NEW.id); END",
            "INSERT INTO t2 VALUES (5,0)",
            // The trigger wrote a log row per inserted row, and the statement
            // then failed: the log rows are the statement's writes too.
            "INSERT INTO t2 VALUES (1,1),(2,2),(5,5)",
            // Under `OR FAIL` they stay, along with the rows that produced them.
            "INSERT OR FAIL INTO t2 VALUES (1,1),(2,2),(5,5)",
        ],
        &[
            "SELECT id, b FROM t2 ORDER BY id",
            "SELECT x FROM t2log ORDER BY x",
            "SELECT count(*) FROM t2log",
        ],
    ));

    assert!(
        failures.is_empty(),
        "a trigger's `RAISE` action did not decide what went back:\n{}",
        failures.join("\n\n")
    );
}

/// A foreign key stopping a statement partway puts the rest back.
///
/// A foreign-key violation is a `RAISE(ABORT)` in a body the binder synthesises,
/// so it never had to hear of conflict algorithms to be fixed - it gets the
/// default like any untagged error. It is graded anyway, in both directions,
/// because it is the shape that leaves *two* tables disagreeing: a `DELETE` a
/// child restricts had already deleted the parents before it.
#[test]
fn a_foreign_key_that_stops_a_statement_puts_the_rest_back() {
    let Some(mut pair) = pair("foreignkey") else {
        eprintln!("the pinned SQLite oracle is not built; nothing was compared");
        return;
    };

    let failures = walk(
        &mut pair,
        &[
            "PRAGMA foreign_keys = ON",
            "CREATE TABLE fp(id INTEGER PRIMARY KEY)",
            "CREATE TABLE fc(id INTEGER PRIMARY KEY, p INTEGER REFERENCES fp(id))",
            "INSERT INTO fp VALUES (1),(2),(3)",
            "INSERT INTO fc VALUES (10,3)",
            // The third child has no parent: the first two must not be there.
            "INSERT INTO fc VALUES (11,1),(12,2),(13,7)",
            // And the delete the child restricts must leave every parent.
            "DELETE FROM fp",
        ],
        &[
            "SELECT id FROM fp ORDER BY id",
            "SELECT id, p FROM fc ORDER BY id",
            "SELECT count(*) FROM fp",
            "SELECT count(*) FROM fc",
        ],
    );

    assert!(
        failures.is_empty(),
        "a foreign key left a statement half-applied:\n{}",
        failures.join("\n\n")
    );
}

/// A transaction statement refuses what SQLite refuses.
///
/// `COMMIT` and `ROLLBACK` with nothing open, and `BEGIN` with something open,
/// all succeeded silently. The engine's own `commit_batch` and `rollback` are
/// deliberately tolerant - they are called at boundaries by code that does not
/// know whether a transaction is open - and the *statements* must not be, or a
/// caller has no way to learn that an `OR ROLLBACK` ended the transaction
/// underneath it.
#[test]
fn a_transaction_statement_refuses_what_sqlite_refuses() {
    let Some(mut pair) = pair("txnstate") else {
        eprintln!("the pinned SQLite oracle is not built; nothing was compared");
        return;
    };

    let failures = walk(
        &mut pair,
        &[
            "COMMIT",
            "ROLLBACK",
            "BEGIN",
            "BEGIN",
            "COMMIT",
            "COMMIT",
            "ROLLBACK",
            // Still usable after all of that.
            "INSERT INTO members VALUES (60, 'x60@x', 'red', 'x60', 1)",
        ],
        &["SELECT count(*) FROM members"],
    );

    assert!(
        failures.is_empty(),
        "a transaction statement did not refuse what SQLite refuses:\n{}",
        failures.join("\n\n")
    );
}
