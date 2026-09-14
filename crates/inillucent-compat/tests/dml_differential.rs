//! inillucent and the pinned SQLite 3.53.4, asked to write the same rows.
//!
//! Invariant: every claim here is a comparison against a live SQLite process,
//! never against a value written into the test. The scripts run against both
//! engines statement by statement, and after each one the reply is compared in
//! full: the rows, their storage classes, `changes`, `total_changes`,
//! `last_insert_rowid`, the autocommit flag, and - when a statement fails - the
//! primary and extended result codes.
//!
//! Comparing the *codes* is what makes this a parity test rather than a smoke
//! test. Any engine can refuse a duplicate key; refusing it as
//! `SQLITE_CONSTRAINT_UNIQUE` rather than `SQLITE_CONSTRAINT_PRIMARYKEY` is
//! what an application's error handling is written against.
//!
//! When the pinned oracle has not been built these tests report what is missing
//! and return, because the compatibility report is driven by recorded results
//! and a run without the oracle should record nothing rather than assume a pass.

use inillucent_compat::differential::{self, Step};

/// Runs a scenario against both engines and compares every reply.
///
/// Returns how many statements were compared, so a scenario that silently
/// stopped early cannot look like one that passed.
///
/// A thin wrapper over `inillucent_compat::differential::compare`, which used
/// to be duplicated here (and in `foreign_keys.rs`) before phase 11 needed the
/// same harness six more times and it was lifted into `src/differential.rs`.
/// Keeping this two-argument `compare(name, steps)` shape, rather than
/// updating every call below to the shared function's `(area, name, steps)`,
/// is the whole reason this wrapper exists.
fn compare(name: &str, steps: &[Step]) -> usize {
    differential::compare("dml_differential", name, steps)
}

/// The plain CRUD path: create, insert, update, delete, read back.
#[test]
fn crud_matches_sqlite() {
    let compared = compare(
        "crud",
        &[
            Step::Exec("CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT, c REAL)"),
            Step::Exec("INSERT INTO t VALUES(1, 'one', 1.5)"),
            Step::Exec("INSERT INTO t VALUES(2, 'two', 2.5)"),
            Step::Exec("INSERT INTO t(b, c) VALUES('three', 3.5)"),
            Step::Exec("INSERT INTO t DEFAULT VALUES"),
            Step::Query("SELECT a, b, c FROM t ORDER BY a"),
            Step::Exec("UPDATE t SET c = c * 2 WHERE a <= 2"),
            Step::Query("SELECT a, c FROM t ORDER BY a"),
            Step::Exec("DELETE FROM t WHERE a = 2"),
            Step::Query("SELECT a FROM t ORDER BY a"),
            Step::Exec("UPDATE t SET b = 'renamed'"),
            Step::Query("SELECT a, b FROM t ORDER BY a"),
            Step::Exec("DELETE FROM t"),
            Step::Query("SELECT count(*) FROM t"),
        ],
    );
    assert!(compared == 0 || compared == 14, "compared {compared} steps");
}

/// Affinity is applied on the way in, exactly as SQLite applies it.
#[test]
fn stored_affinity_matches_sqlite() {
    let compared = compare(
        "affinity",
        &[
            Step::Exec("CREATE TABLE t(i INTEGER, r REAL, t TEXT, b BLOB, n NUMERIC)"),
            Step::Exec("INSERT INTO t VALUES('42', '42', 42, 42, '42')"),
            Step::Exec("INSERT INTO t VALUES('42x', '3.5', 3.5, '3.5', '3.5')"),
            Step::Exec("INSERT INTO t VALUES(NULL, NULL, NULL, NULL, NULL)"),
            Step::Exec("INSERT INTO t VALUES(1.0, 1, x'01', x'01', 1.0)"),
            Step::Query("SELECT i, r, t, b, n FROM t"),
            Step::Query("SELECT typeof(i), typeof(r), typeof(t), typeof(b), typeof(n) FROM t"),
        ],
    );
    assert!(compared == 0 || compared == 7, "compared {compared} steps");
}

/// The rowid rules: allocation, an explicit value, and the largest one.
#[test]
fn rowid_allocation_matches_sqlite() {
    let compared = compare(
        "rowid",
        &[
            Step::Exec("CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)"),
            Step::Exec("INSERT INTO t(b) VALUES('first')"),
            Step::Exec("INSERT INTO t VALUES(100, 'hundred')"),
            Step::Exec("INSERT INTO t(b) VALUES('after')"),
            Step::Exec("INSERT INTO t VALUES(NULL, 'null key')"),
            Step::Query("SELECT a, b FROM t ORDER BY a"),
            Step::Query("SELECT rowid, a FROM t ORDER BY rowid"),
            Step::Exec("CREATE TABLE u(a TEXT)"),
            Step::Exec("INSERT INTO u VALUES('x')"),
            Step::Exec("INSERT INTO u VALUES('y')"),
            Step::Query("SELECT rowid, a FROM u ORDER BY rowid"),
            Step::Exec("DELETE FROM u WHERE rowid = 2"),
            Step::Exec("INSERT INTO u VALUES('z')"),
            Step::Query("SELECT rowid, a FROM u ORDER BY rowid"),
        ],
    );
    assert!(compared == 0 || compared == 14, "compared {compared} steps");
}

/// Every constraint reports the code SQLite reports.
#[test]
fn constraint_codes_match_sqlite() {
    let compared = compare(
        "constraints",
        &[
            Step::Exec(
                "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT NOT NULL, c INTEGER UNIQUE, d INTEGER CHECK (d > 0))",
            ),
            Step::Exec("INSERT INTO t VALUES(1, 'one', 10, 1)"),
            Step::Exec("INSERT INTO t VALUES(1, 'again', 11, 1)"),
            Step::Exec("INSERT INTO t VALUES(2, NULL, 12, 1)"),
            Step::Exec("INSERT INTO t VALUES(3, 'three', 10, 1)"),
            Step::Exec("INSERT INTO t VALUES(4, 'four', 14, 0)"),
            Step::Exec("INSERT INTO t VALUES(5, 'five', NULL, 1)"),
            Step::Exec("INSERT INTO t VALUES(6, 'six', NULL, 1)"),
            Step::Query("SELECT a, b, c, d FROM t ORDER BY a"),
            Step::Exec("UPDATE t SET c = 10 WHERE a = 5"),
            Step::Exec("UPDATE t SET b = NULL WHERE a = 5"),
            Step::Exec("UPDATE t SET d = -1 WHERE a = 5"),
            Step::Query("SELECT a, b, c, d FROM t ORDER BY a"),
        ],
    );
    assert!(compared == 0 || compared == 13, "compared {compared} steps");
}

/// The five conflict algorithms behave identically.
#[test]
fn conflict_algorithms_match_sqlite() {
    let compared = compare(
        "conflicts",
        &[
            Step::Exec("CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT UNIQUE)"),
            Step::Exec("INSERT INTO t VALUES(1, 'one')"),
            Step::Exec("INSERT INTO t VALUES(2, 'two')"),
            Step::Exec("INSERT OR IGNORE INTO t VALUES(1, 'ignored')"),
            Step::Query("SELECT a, b FROM t ORDER BY a"),
            Step::Exec("INSERT OR REPLACE INTO t VALUES(1, 'replaced')"),
            Step::Query("SELECT a, b FROM t ORDER BY a"),
            Step::Exec("INSERT OR REPLACE INTO t VALUES(3, 'two')"),
            Step::Query("SELECT a, b FROM t ORDER BY a"),
            Step::Exec("INSERT OR ABORT INTO t VALUES(1, 'nope')"),
            Step::Query("SELECT a, b FROM t ORDER BY a"),
            Step::Exec("INSERT OR FAIL INTO t VALUES(4, 'four'), (1, 'nope'), (5, 'five')"),
            Step::Query("SELECT a, b FROM t ORDER BY a"),
            Step::Exec("INSERT OR IGNORE INTO t VALUES(6, 'six'), (1, 'nope'), (7, 'seven')"),
            Step::Query("SELECT a, b FROM t ORDER BY a"),
        ],
    );
    assert!(compared == 0 || compared == 15, "compared {compared} steps");
}

/// Transactions and savepoints report the same state and keep the same rows.
#[test]
fn transactions_match_sqlite() {
    let compared = compare(
        "transactions",
        &[
            Step::Exec("CREATE TABLE t(a INTEGER PRIMARY KEY)"),
            Step::Exec("INSERT INTO t VALUES(1)"),
            Step::Exec("BEGIN"),
            Step::Exec("INSERT INTO t VALUES(2)"),
            Step::Query("SELECT a FROM t ORDER BY a"),
            Step::Exec("ROLLBACK"),
            Step::Query("SELECT a FROM t ORDER BY a"),
            Step::Exec("BEGIN"),
            Step::Exec("INSERT INTO t VALUES(3)"),
            Step::Exec("SAVEPOINT s"),
            Step::Exec("INSERT INTO t VALUES(4)"),
            Step::Exec("ROLLBACK TO s"),
            Step::Query("SELECT a FROM t ORDER BY a"),
            Step::Exec("RELEASE s"),
            Step::Exec("COMMIT"),
            Step::Query("SELECT a FROM t ORDER BY a"),
            Step::Exec("SAVEPOINT outer_level"),
            Step::Exec("INSERT INTO t VALUES(5)"),
            Step::Exec("RELEASE outer_level"),
            Step::Query("SELECT a FROM t ORDER BY a"),
        ],
    );
    assert!(compared == 0 || compared == 20, "compared {compared} steps");
}

/// Index maintenance keeps a query answering the same rows as SQLite's.
#[test]
fn index_maintenance_matches_sqlite() {
    let compared = compare(
        "indexes",
        &[
            Step::Exec("CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT, c INTEGER)"),
            Step::Exec("INSERT INTO t VALUES(1, 'bbb', 30)"),
            Step::Exec("INSERT INTO t VALUES(2, 'aaa', 20)"),
            Step::Exec("INSERT INTO t VALUES(3, 'ccc', 10)"),
            Step::Exec("CREATE INDEX t_b ON t(b)"),
            Step::Exec("CREATE INDEX t_c ON t(c)"),
            Step::Exec("INSERT INTO t VALUES(4, 'aab', 40)"),
            Step::Query("SELECT a FROM t WHERE b = 'aaa'"),
            Step::Query("SELECT a FROM t WHERE b > 'aab' ORDER BY a"),
            Step::Exec("UPDATE t SET b = 'zzz' WHERE a = 2"),
            Step::Query("SELECT a FROM t WHERE b = 'aaa'"),
            Step::Query("SELECT a FROM t WHERE b = 'zzz'"),
            Step::Exec("DELETE FROM t WHERE c = 10"),
            Step::Query("SELECT a, b, c FROM t ORDER BY a"),
            Step::Exec("DROP INDEX t_c"),
            Step::Query("SELECT a FROM t WHERE c = 40"),
            Step::Exec("DROP TABLE t"),
            Step::Query("SELECT count(*) FROM sqlite_master WHERE name = 't'"),
        ],
    );
    assert!(compared == 0 || compared == 18, "compared {compared} steps");
}

/// The `sqlite_schema` rows inillucent writes are the ones SQLite writes.
#[test]
fn the_schema_table_matches_sqlite() {
    let compared = compare(
        "schema",
        &[
            Step::Exec("CREATE TABLE people(id INTEGER PRIMARY KEY, name TEXT UNIQUE, note TEXT)"),
            Step::Exec("CREATE INDEX people_note ON people(note)"),
            Step::Exec("CREATE TABLE plain(a, b)"),
            Step::Exec("CREATE TABLE IF NOT EXISTS plain(a, b, c)"),
            Step::Query("SELECT type, name, tbl_name, sql FROM sqlite_master ORDER BY name"),
            Step::Exec("DROP TABLE plain"),
            Step::Query("SELECT type, name, tbl_name, sql FROM sqlite_master ORDER BY name"),
        ],
    );
    assert!(compared == 0 || compared == 7, "compared {compared} steps");
}

/// RETURNING reports the row each statement wrote.
#[test]
fn returning_matches_sqlite() {
    let compared = compare(
        "returning",
        &[
            Step::Exec("CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)"),
            Step::Query("INSERT INTO t(b) VALUES('one') RETURNING a, b"),
            Step::Query("INSERT INTO t(b) VALUES('two') RETURNING a"),
            Step::Query("UPDATE t SET b = b || '!' RETURNING a, b"),
            Step::Query("DELETE FROM t WHERE a = 1 RETURNING a, b"),
            Step::Query("SELECT a, b FROM t ORDER BY a"),
        ],
    );
    assert!(compared == 0 || compared == 6, "compared {compared} steps");
}

/// UPSERT matches SQLite, in both its forms and with `excluded`.
#[test]
fn upsert_matches_sqlite() {
    let compared = compare(
        "upsert",
        &[
            Step::Exec("CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT UNIQUE, hits INTEGER)"),
            Step::Exec("INSERT INTO t VALUES(1, 'one', 1)"),
            Step::Exec("INSERT INTO t VALUES(1, 'uno', 5) ON CONFLICT DO NOTHING"),
            Step::Query("SELECT a, b, hits FROM t ORDER BY a"),
            Step::Exec(
                "INSERT INTO t VALUES(1, 'uno', 5) ON CONFLICT(a) DO UPDATE SET hits = hits + excluded.hits",
            ),
            Step::Query("SELECT a, b, hits FROM t ORDER BY a"),
            Step::Exec(
                "INSERT INTO t VALUES(2, 'two', 7) ON CONFLICT(a) DO UPDATE SET hits = hits + excluded.hits",
            ),
            Step::Query("SELECT a, b, hits FROM t ORDER BY a"),
            Step::Exec(
                "INSERT INTO t VALUES(3, 'one', 9) ON CONFLICT(b) DO UPDATE SET hits = excluded.hits",
            ),
            Step::Query("SELECT a, b, hits FROM t ORDER BY a"),
            Step::Exec(
                "INSERT INTO t VALUES(1, 'one', 100) ON CONFLICT(a) DO UPDATE SET hits = excluded.hits WHERE hits < 0",
            ),
            Step::Query("SELECT a, b, hits FROM t ORDER BY a"),
            Step::Exec("INSERT INTO t VALUES(9, 'nine', 1) ON CONFLICT DO NOTHING"),
            Step::Query("SELECT a, b, hits FROM t ORDER BY a"),
        ],
    );
    assert!(compared == 0 || compared == 14, "compared {compared} steps");
}

/// A `STORED` generated column is recomputed by every statement that rewrites
/// the row, not only by the `INSERT` that created it.
///
/// **The value is on the disk, which is what makes this worse than a wrong
/// answer (task-1913).** `UPDATE g SET a = 5` left `c GENERATED ALWAYS AS
/// (a + 1) STORED` holding the 2 the insert computed, so the record itself was
/// wrong: a later read of the same file read 2, and an index over `c` indexed
/// 2. The `DO UPDATE` arm of an upsert had it too, because that also rewrites
/// a row without naming the column. The `VIRTUAL` column beside it was always
/// right - it has no slot and is computed when it is read - and it is here so
/// that a fix which recomputed everything into the record would fail rather
/// than pass.
///
/// The index is here for the same reason: a generated column that is indexed
/// is the case where a stale value stops being merely wrong and starts
/// answering the wrong rows.
#[test]
fn a_stored_generated_column_is_recomputed_by_every_write() {
    let compared = compare(
        "generated",
        &[
            Step::Exec(
                "CREATE TABLE g (a INTEGER PRIMARY KEY, n INTEGER, \
                 v INTEGER GENERATED ALWAYS AS (n * 2) VIRTUAL, \
                 c INTEGER GENERATED ALWAYS AS (n + 1) STORED, \
                 u TEXT GENERATED ALWAYS AS (upper(CAST(n AS TEXT))) STORED)",
            ),
            Step::Exec("CREATE INDEX g_c ON g (c)"),
            Step::Exec("INSERT INTO g (a, n) VALUES (1, 1), (2, 2)"),
            Step::Query("SELECT a, n, v, c, u FROM g ORDER BY a"),
            Step::Exec("UPDATE g SET n = 5 WHERE a = 1"),
            Step::Query("SELECT a, n, v, c, u FROM g ORDER BY a"),
            Step::Query("SELECT a FROM g WHERE c = 6"),
            Step::Query("SELECT a FROM g WHERE c = 2"),
            Step::Exec("UPDATE g SET n = n + 1"),
            Step::Query("SELECT a, n, c FROM g ORDER BY a"),
            // The upsert arm rewrites a row too.
            Step::Exec(
                "INSERT INTO g (a, n) VALUES (1, 20) ON CONFLICT (a) DO UPDATE SET n = excluded.n",
            ),
            Step::Query("SELECT a, n, v, c, u FROM g ORDER BY a"),
            Step::Query("SELECT a FROM g WHERE c = 21"),
            // A write that does not touch the generating column leaves the
            // generated one where it was.
            Step::Exec("UPDATE g SET a = a + 10 WHERE a = 2"),
            Step::Query("SELECT a, n, c FROM g ORDER BY a"),
            // Writing a generated column directly is refused, on both engines.
            Step::Query("UPDATE g SET c = 99 WHERE a = 1"),
            Step::Query("INSERT INTO g (a, n, c) VALUES (7, 7, 7)"),
            Step::Query("SELECT a, n, c FROM g ORDER BY a"),
        ],
    );
    assert!(compared == 0 || compared == 18, "compared {compared} steps");
}

/// Row triggers: every event, both times, OLD and NEW, WHEN, and UPDATE OF.
///
/// The counters are what make this worth running in lockstep rather than
/// against written-out rows. `changes()` reports the *statement's* row count and
/// not the rows its triggers wrote, and `last_insert_rowid()` moves while a
/// trigger body is inserting - both are easy to get wrong in a way no SELECT
/// would show, and both are compared after every step here.
#[test]
fn row_triggers_match_sqlite() {
    let compared = compare(
        "triggers",
        &[
            Step::Exec("CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT, score INTEGER)"),
            Step::Exec("CREATE TABLE audit(seq INTEGER PRIMARY KEY, what TEXT, detail TEXT)"),
            Step::Exec(
                "CREATE TRIGGER t_ai AFTER INSERT ON t BEGIN
                   INSERT INTO audit(what, detail) VALUES('after-insert', new.name);
                 END",
            ),
            Step::Exec(
                "CREATE TRIGGER t_bi BEFORE INSERT ON t BEGIN
                   INSERT INTO audit(what, detail) VALUES('before-insert', new.name);
                 END",
            ),
            Step::Exec(
                "CREATE TRIGGER t_ad AFTER DELETE ON t BEGIN
                   INSERT INTO audit(what, detail) VALUES('after-delete', old.name);
                 END",
            ),
            Step::Exec(
                "CREATE TRIGGER t_au AFTER UPDATE ON t BEGIN
                   INSERT INTO audit(what, detail) VALUES('after-update', old.name || '->' || new.name);
                 END",
            ),
            // Narrowed to one column: it must not fire when only `score` moves.
            Step::Exec(
                "CREATE TRIGGER t_auname AFTER UPDATE OF name ON t BEGIN
                   INSERT INTO audit(what, detail) VALUES('name-changed', new.name);
                 END",
            ),
            // A guard, and the rowid read through NEW.
            Step::Exec(
                "CREATE TRIGGER t_high AFTER INSERT ON t WHEN new.score > 100 BEGIN
                   INSERT INTO audit(what, detail) VALUES('high', new.rowid);
                 END",
            ),
            Step::Query("SELECT type, name, tbl_name FROM sqlite_schema WHERE type = 'trigger' ORDER BY name"),
            Step::Exec("INSERT INTO t VALUES(1, 'ada', 10)"),
            Step::Query("SELECT seq, what, detail FROM audit ORDER BY seq"),
            Step::Exec("INSERT INTO t VALUES(2, 'bob', 500)"),
            Step::Query("SELECT seq, what, detail FROM audit ORDER BY seq"),
            Step::Exec("UPDATE t SET score = score + 1 WHERE id = 1"),
            Step::Query("SELECT seq, what, detail FROM audit ORDER BY seq"),
            Step::Exec("UPDATE t SET name = 'ada2' WHERE id = 1"),
            Step::Query("SELECT seq, what, detail FROM audit ORDER BY seq"),
            Step::Exec("DELETE FROM t WHERE id = 2"),
            Step::Query("SELECT seq, what, detail FROM audit ORDER BY seq"),
            // A multi-row write fires the trigger once per row.
            Step::Exec("INSERT INTO t VALUES(7, 'g', 1), (8, 'h', 2), (9, 'i', 3)"),
            Step::Query("SELECT count(*) FROM audit"),
            Step::Exec("UPDATE t SET name = name || '!'"),
            Step::Query("SELECT id, name FROM t ORDER BY id"),
            Step::Query("SELECT what, count(*) FROM audit GROUP BY what ORDER BY what"),
            Step::Exec("DELETE FROM t"),
            Step::Query("SELECT count(*) FROM t"),
            Step::Query("SELECT what, count(*) FROM audit GROUP BY what ORDER BY what"),
            // Dropping one leaves the others.
            Step::Exec("DROP TRIGGER t_bi"),
            Step::Query("SELECT name FROM sqlite_schema WHERE type = 'trigger' ORDER BY name"),
            Step::Exec("INSERT INTO t VALUES(20, 'z', 1)"),
            Step::Query("SELECT what FROM audit ORDER BY seq DESC LIMIT 2"),
            Step::Exec("DROP TRIGGER IF EXISTS nosuchtrigger"),
            Step::Exec("DROP TRIGGER nosuchtrigger"),
            // A dropped table takes its triggers with it.
            Step::Exec("DROP TABLE t"),
            Step::Query("SELECT count(*) FROM sqlite_schema WHERE type = 'trigger'"),
        ],
    );
    assert!(compared == 0 || compared == 35, "compared {compared} steps");
}

/// `RAISE()`: the three that stop the statement and the one that skips the row.
#[test]
fn trigger_raise_matches_sqlite() {
    let compared = compare(
        "trigger-raise",
        &[
            Step::Exec("CREATE TABLE t(id INTEGER PRIMARY KEY, score INTEGER)"),
            Step::Exec(
                "CREATE TRIGGER no_negative BEFORE INSERT ON t WHEN new.score < 0 BEGIN
                   SELECT RAISE(ABORT, 'score must not be negative');
                 END",
            ),
            Step::Exec("INSERT INTO t VALUES(1, 5)"),
            Step::Exec("INSERT INTO t VALUES(2, -1)"),
            Step::Query("SELECT id, score FROM t ORDER BY id"),
            // ABORT undoes the whole statement, so neither row of this one lands.
            Step::Exec("INSERT INTO t VALUES(3, 7), (4, -7)"),
            Step::Query("SELECT id, score FROM t ORDER BY id"),
            // A guard written as a WHERE inside the body rather than as WHEN.
            Step::Exec("CREATE TABLE u(id INTEGER PRIMARY KEY, score INTEGER)"),
            Step::Exec(
                "CREATE TRIGGER u_check BEFORE INSERT ON u BEGIN
                   SELECT RAISE(FAIL, 'too big') WHERE new.score > 100;
                 END",
            ),
            Step::Exec("INSERT INTO u VALUES(1, 5)"),
            Step::Exec("INSERT INTO u VALUES(2, 500)"),
            Step::Query("SELECT id, score FROM u ORDER BY id"),
            // FAIL keeps the rows already written by the same statement.
            Step::Exec("INSERT INTO u VALUES(3, 6), (4, 900), (5, 7)"),
            Step::Query("SELECT id, score FROM u ORDER BY id"),
            // IGNORE abandons the row and lets the statement carry on.
            Step::Exec("CREATE TABLE v(id INTEGER PRIMARY KEY, score INTEGER)"),
            Step::Exec(
                "CREATE TRIGGER v_skip BEFORE INSERT ON v WHEN new.score < 0 BEGIN
                   SELECT RAISE(IGNORE);
                 END",
            ),
            Step::Exec("INSERT INTO v VALUES(1, 5), (2, -1), (3, 9)"),
            Step::Query("SELECT id, score FROM v ORDER BY id"),
            // RAISE outside a trigger body is not a statement at all.
            Step::Exec("SELECT RAISE(ABORT, 'nope')"),
            Step::Exec("INSERT INTO v VALUES(4, RAISE(IGNORE))"),
        ],
    );
    assert!(compared == 0 || compared == 20, "compared {compared} steps");
}

/// A trigger whose body writes a table that has triggers of its own.
///
/// With SQLite's default `recursive_triggers = off` a trigger already on the
/// stack is skipped rather than fired again, so a trigger that writes its own
/// table terminates instead of recursing. That is the rule this checks, and it
/// is also the reason the compiler can inline trigger bodies at all.
#[test]
fn trigger_chains_match_sqlite() {
    let compared = compare(
        "trigger-chains",
        &[
            Step::Exec("CREATE TABLE a(id INTEGER PRIMARY KEY, n INTEGER)"),
            Step::Exec("CREATE TABLE b(id INTEGER PRIMARY KEY, n INTEGER)"),
            Step::Exec("CREATE TABLE c(id INTEGER PRIMARY KEY, n INTEGER)"),
            Step::Exec(
                "CREATE TRIGGER a_to_b AFTER INSERT ON a BEGIN
                   INSERT INTO b(n) VALUES(new.n * 10);
                 END",
            ),
            Step::Exec(
                "CREATE TRIGGER b_to_c AFTER INSERT ON b BEGIN
                   INSERT INTO c(n) VALUES(new.n * 10);
                 END",
            ),
            Step::Exec("INSERT INTO a(n) VALUES(1)"),
            Step::Query("SELECT n FROM a ORDER BY n"),
            Step::Query("SELECT n FROM b ORDER BY n"),
            Step::Query("SELECT n FROM c ORDER BY n"),
            // Self-recursion: the trigger writes its own table.
            Step::Exec("CREATE TABLE r(id INTEGER PRIMARY KEY, depth INTEGER)"),
            Step::Exec(
                "CREATE TRIGGER r_deeper AFTER INSERT ON r WHEN new.depth < 5 BEGIN
                   INSERT INTO r(depth) VALUES(new.depth + 1);
                 END",
            ),
            Step::Exec("INSERT INTO r(depth) VALUES(0)"),
            Step::Query("SELECT depth FROM r ORDER BY depth"),
            // A trigger that deletes from the table it fires for.
            Step::Exec("CREATE TABLE cap(id INTEGER PRIMARY KEY, n INTEGER)"),
            Step::Exec(
                "CREATE TRIGGER cap_trim AFTER INSERT ON cap BEGIN
                   DELETE FROM cap WHERE n < new.n - 1;
                 END",
            ),
            Step::Exec("INSERT INTO cap(n) VALUES(1), (2), (3), (4)"),
            Step::Query("SELECT n FROM cap ORDER BY n"),
            // An UPDATE trigger that updates a second table.
            Step::Exec("CREATE TABLE total(id INTEGER PRIMARY KEY, sum INTEGER)"),
            Step::Exec("INSERT INTO total VALUES(1, 0)"),
            Step::Exec(
                "CREATE TRIGGER a_sum AFTER UPDATE ON a BEGIN
                   UPDATE total SET sum = sum + new.n - old.n WHERE id = 1;
                 END",
            ),
            Step::Exec("UPDATE a SET n = n + 4"),
            Step::Query("SELECT sum FROM total"),
        ],
    );
    assert!(compared == 0 || compared == 22, "compared {compared} steps");
}

/// `INSTEAD OF` triggers, which are what makes a view writable.
#[test]
fn instead_of_triggers_match_sqlite() {
    let compared = compare(
        "instead-of",
        &[
            Step::Exec("CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT, hidden TEXT)"),
            Step::Exec("INSERT INTO t VALUES(1, 'ada', 'x'), (2, 'bob', 'y')"),
            Step::Exec("CREATE VIEW v AS SELECT id, name FROM t"),
            // Without a trigger a view refuses every write.
            Step::Exec("INSERT INTO v VALUES(3, 'cai')"),
            Step::Exec("UPDATE v SET name = 'z'"),
            Step::Exec("DELETE FROM v"),
            Step::Exec(
                "CREATE TRIGGER v_ins INSTEAD OF INSERT ON v BEGIN
                   INSERT INTO t(id, name, hidden) VALUES(new.id, new.name, 'from-view');
                 END",
            ),
            Step::Exec(
                "CREATE TRIGGER v_upd INSTEAD OF UPDATE ON v BEGIN
                   UPDATE t SET name = new.name WHERE id = old.id;
                 END",
            ),
            Step::Exec(
                "CREATE TRIGGER v_del INSTEAD OF DELETE ON v BEGIN
                   DELETE FROM t WHERE id = old.id;
                 END",
            ),
            Step::Exec("INSERT INTO v VALUES(3, 'cai')"),
            Step::Query("SELECT id, name, hidden FROM t ORDER BY id"),
            Step::Exec("UPDATE v SET name = 'robert' WHERE id = 2"),
            Step::Query("SELECT id, name FROM t ORDER BY id"),
            Step::Exec("DELETE FROM v WHERE id = 1"),
            Step::Query("SELECT id, name FROM t ORDER BY id"),
            // The shapes SQLite refuses.
            Step::Exec("CREATE TRIGGER v_before BEFORE INSERT ON v BEGIN SELECT 1; END"),
            Step::Exec("CREATE TRIGGER v_after AFTER INSERT ON v BEGIN SELECT 1; END"),
            Step::Exec("CREATE TRIGGER t_instead INSTEAD OF INSERT ON t BEGIN SELECT 1; END"),
            Step::Exec("CREATE TRIGGER t_stmt AFTER INSERT ON t FOR EACH STATEMENT BEGIN SELECT 1; END"),
            Step::Exec("CREATE TRIGGER t_empty AFTER INSERT ON t BEGIN END"),
            Step::Exec("CREATE TRIGGER t_nosuch AFTER INSERT ON nosuchtable BEGIN SELECT 1; END"),
            Step::Exec("CREATE TRIGGER t_badcol AFTER UPDATE OF nosuchcolumn ON t BEGIN SELECT 1; END"),
            Step::Exec("CREATE TRIGGER t_badbody AFTER INSERT ON t BEGIN INSERT INTO t(nosuch) VALUES(1); END"),
            Step::Exec("CREATE TRIGGER t_badnew AFTER DELETE ON t BEGIN INSERT INTO t(name) VALUES(new.name); END"),
            Step::Exec("CREATE TRIGGER t_badold AFTER INSERT ON t BEGIN INSERT INTO t(name) VALUES(old.name); END"),
            Step::Exec("CREATE TRIGGER v_ins INSTEAD OF INSERT ON v BEGIN SELECT 1; END"),
            Step::Exec("CREATE TRIGGER IF NOT EXISTS v_ins INSTEAD OF INSERT ON v BEGIN SELECT 1; END"),
            Step::Query("SELECT name FROM sqlite_schema WHERE type = 'trigger' ORDER BY name"),
        ],
    );
    assert!(compared == 0 || compared == 28, "compared {compared} steps");
}

/// `INSERT ... SELECT`, including the case where the query reads the target.
#[test]
fn insert_from_select_matches_sqlite() {
    let compared = compare(
        "insert-select",
        &[
            Step::Exec("CREATE TABLE src(id INTEGER PRIMARY KEY, n TEXT, v REAL)"),
            Step::Exec("CREATE TABLE dst(id INTEGER PRIMARY KEY, n TEXT, v REAL)"),
            Step::Exec("INSERT INTO src VALUES(1, 'a', 1.5), (2, 'b', 2.5), (3, NULL, NULL)"),
            Step::Exec("INSERT INTO dst SELECT id, n, v FROM src"),
            Step::Query("SELECT id, n, v FROM dst ORDER BY id"),
            Step::Exec("DELETE FROM dst"),
            Step::Exec("INSERT INTO dst(n) SELECT n FROM src WHERE n IS NOT NULL ORDER BY n DESC"),
            Step::Query("SELECT id, n, v FROM dst ORDER BY id"),
            // The query reads the table being written: it must see the rows that
            // were there when it started and no more, or it never terminates.
            Step::Exec("INSERT INTO dst(n, v) SELECT n, v FROM dst"),
            Step::Query("SELECT count(*) FROM dst"),
            Step::Exec("INSERT INTO dst(n, v) SELECT n, v FROM src JOIN dst USING (n)"),
            Step::Query("SELECT count(*) FROM dst"),
            // An aggregate, a compound and a CTE as the source.
            Step::Exec("CREATE TABLE agg(k TEXT, total REAL)"),
            Step::Exec("INSERT INTO agg SELECT n, sum(v) FROM src GROUP BY n"),
            Step::Query("SELECT k, total FROM agg ORDER BY k"),
            Step::Exec("INSERT INTO agg SELECT 'u', 1.0 UNION SELECT 'u', 1.0"),
            Step::Query("SELECT k, total FROM agg ORDER BY k, total"),
            Step::Exec("INSERT INTO agg WITH w AS (SELECT 'w' AS k, 9.0 AS t) SELECT k, t FROM w"),
            Step::Query("SELECT k, total FROM agg ORDER BY k, total"),
            // A width mismatch, and a constraint the query's rows break.
            Step::Exec("INSERT INTO dst SELECT id FROM src"),
            Step::Exec("INSERT INTO dst(id, n) SELECT id, n FROM src"),
            Step::Query("SELECT count(*) FROM dst"),
            // Fired triggers see rows the query produced.
            Step::Exec("CREATE TABLE seen(seq INTEGER PRIMARY KEY, n TEXT)"),
            Step::Exec("CREATE TRIGGER dst_ai AFTER INSERT ON dst BEGIN INSERT INTO seen(n) VALUES(new.n); END"),
            Step::Exec("DELETE FROM dst"),
            Step::Exec("INSERT INTO dst(n, v) SELECT n, v FROM src"),
            Step::Query("SELECT n FROM seen ORDER BY seq"),
        ],
    );
    assert!(compared == 0 || compared == 27, "compared {compared} steps");
}

/// `WITHOUT ROWID` tables, whose b-tree is an index and whose rows have no rowid.
///
/// The record is permuted - primary key first, then the rest - so a reader that
/// got the order wrong would answer every column with its neighbour's value and
/// still look like it worked. Comparing against the reference row by row is the
/// only way to see it.
#[test]
fn without_rowid_matches_sqlite() {
    let compared = compare(
        "without-rowid",
        &[
            Step::Exec(
                "CREATE TABLE w(a TEXT, b INTEGER, c TEXT, PRIMARY KEY(b, a)) WITHOUT ROWID",
            ),
            Step::Exec("CREATE TABLE nokey(a) WITHOUT ROWID"),
            Step::Exec("INSERT INTO w VALUES('x', 2, 'cx')"),
            Step::Exec("INSERT INTO w VALUES('y', 1, 'cy')"),
            Step::Exec("INSERT INTO w VALUES('z', 1, 'cz')"),
            Step::Query("SELECT a, b, c FROM w"),
            Step::Query("SELECT a, b, c FROM w ORDER BY b, a"),
            Step::Query("SELECT a, b, c FROM w ORDER BY b DESC, a DESC"),
            // The key is unique and NOT NULL, both implicitly.
            Step::Exec("INSERT INTO w VALUES('x', 2, 'again')"),
            Step::Exec("INSERT INTO w VALUES(NULL, 3, 'nullkey')"),
            Step::Exec("INSERT INTO w(b, c) VALUES(4, 'nokeycol')"),
            Step::Query("SELECT a, b, c FROM w ORDER BY b, a"),
            // A seek on the key, and on a prefix of it.
            Step::Query("SELECT c FROM w WHERE b = 2 AND a = 'x'"),
            Step::Query("SELECT a, c FROM w WHERE b = 1 ORDER BY a"),
            Step::Query("SELECT a, b FROM w WHERE b > 1 ORDER BY b, a"),
            // The rowid is not a column of such a table, in any spelling.
            Step::Exec("SELECT rowid FROM w"),
            Step::Exec("SELECT _rowid_ FROM w"),
            Step::Exec("SELECT oid FROM w"),
            // Writes.
            Step::Exec("UPDATE w SET c = 'updated' WHERE b = 1 AND a = 'y'"),
            Step::Query("SELECT a, b, c FROM w ORDER BY b, a"),
            Step::Exec("UPDATE w SET b = 7 WHERE a = 'z'"),
            Step::Query("SELECT a, b, c FROM w ORDER BY b, a"),
            Step::Exec("UPDATE w SET b = 7 WHERE a = 'x'"),
            Step::Query("SELECT a, b, c FROM w ORDER BY b, a"),
            Step::Exec("DELETE FROM w WHERE b = 7 AND a = 'z'"),
            Step::Query("SELECT a, b, c FROM w ORDER BY b, a"),
            // A secondary index, which locates a row by the primary key at the
            // end of each entry rather than by a rowid.
            Step::Exec("CREATE INDEX w_c ON w (c)"),
            Step::Exec("INSERT INTO w VALUES('q', 9, 'cq')"),
            Step::Query("SELECT a, b FROM w WHERE c = 'cq'"),
            Step::Query("SELECT a, b, c FROM w WHERE c > 'cq' ORDER BY c"),
            Step::Exec("UPDATE w SET c = 'cq2' WHERE a = 'q'"),
            Step::Query("SELECT a, b FROM w WHERE c = 'cq2'"),
            Step::Query("SELECT a, b FROM w WHERE c = 'cq'"),
            Step::Exec("DELETE FROM w WHERE c = 'cq2'"),
            Step::Query("SELECT a, b, c FROM w ORDER BY b, a"),
            Step::Query("SELECT count(*) FROM w"),
            // A unique secondary index, and the constraint it enforces.
            Step::Exec("CREATE UNIQUE INDEX w_c2 ON w (c)"),
            Step::Exec("INSERT INTO w VALUES('dup', 20, 'cx')"),
            Step::Query("SELECT a, b, c FROM w ORDER BY b, a"),
            // A single-column key, and a TEXT one - not an INTEGER PRIMARY KEY.
            Step::Exec("CREATE TABLE k(id TEXT PRIMARY KEY, v INTEGER) WITHOUT ROWID"),
            Step::Exec("INSERT INTO k VALUES('b', 2), ('a', 1), ('c', 3)"),
            Step::Query("SELECT id, v FROM k"),
            Step::Exec("UPDATE k SET v = v * 10 WHERE id = 'b'"),
            Step::Query("SELECT id, v FROM k ORDER BY id"),
            Step::Exec("DELETE FROM k WHERE id = 'a'"),
            Step::Query("SELECT id, v FROM k ORDER BY id"),
            // An INTEGER PRIMARY KEY on a WITHOUT ROWID table is *not* a rowid
            // alias, so it stores an integer in the record like any column.
            Step::Exec("CREATE TABLE i(id INTEGER PRIMARY KEY, v TEXT) WITHOUT ROWID"),
            Step::Exec("INSERT INTO i VALUES(3, 'c'), (1, 'a')"),
            Step::Query("SELECT id, v, typeof(id) FROM i ORDER BY id"),
            Step::Exec("INSERT INTO i(v) VALUES('nokey')"),
            Step::Query("SELECT id, v FROM i ORDER BY id"),
            // A join with a rowid table, both directions.
            Step::Exec("CREATE TABLE r(id INTEGER PRIMARY KEY, k TEXT)"),
            Step::Exec("INSERT INTO r VALUES(1, 'b'), (2, 'c')"),
            Step::Query("SELECT r.id, k.v FROM r JOIN k ON r.k = k.id ORDER BY r.id"),
            Step::Query("SELECT k.id, r.id FROM k LEFT JOIN r ON r.k = k.id ORDER BY k.id"),
            Step::Exec("DROP TABLE k"),
            Step::Query("SELECT count(*) FROM sqlite_schema WHERE tbl_name = 'k'"),
        ],
    );
    assert!(compared == 0 || compared == 57, "compared {compared} steps");
}

/// `VACUUM` and `VACUUM INTO`, answering exactly what the reference answers.
///
/// The counters matter as much as the rows: `VACUUM` is not a row change, so it
/// must leave `changes()` reading whatever the last write left, and it must not
/// move `total_changes()` at all.
#[test]
fn vacuum_matches_sqlite() {
    let compared = compare(
        "vacuum",
        &[
            Step::Exec("CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT, pad TEXT)"),
            Step::Exec("CREATE INDEX t_name ON t (name)"),
            Step::Exec("CREATE VIEW v AS SELECT id, name FROM t"),
            Step::Exec("CREATE TABLE w(k TEXT PRIMARY KEY, n INTEGER) WITHOUT ROWID"),
            Step::Exec("INSERT INTO t VALUES(1,'a','p'),(2,'b','p'),(3,'c','p'),(4,'d','p')"),
            Step::Exec("INSERT INTO w VALUES('a',1),('b',2),('c',3)"),
            Step::Exec("DELETE FROM t WHERE id % 2 = 0"),
            Step::Exec("PRAGMA user_version = 7"),
            Step::Query("SELECT id, name FROM t ORDER BY id"),
            Step::Exec("VACUUM"),
            Step::Query("SELECT id, name, pad FROM t ORDER BY id"),
            Step::Query("SELECT id FROM t WHERE name = 'c'"),
            Step::Query("SELECT k, n FROM w ORDER BY k"),
            Step::Query("SELECT n FROM w WHERE k = 'b'"),
            Step::Query("SELECT id, name FROM v ORDER BY id"),
            Step::Query("PRAGMA user_version"),
            Step::Query("SELECT type, name FROM sqlite_schema ORDER BY name"),
            // Writes still work against the rebuilt roots.
            Step::Exec("INSERT INTO t VALUES(9,'i','p')"),
            Step::Exec("INSERT INTO w VALUES('z',9)"),
            Step::Exec("UPDATE t SET name = 'renamed' WHERE id = 1"),
            Step::Exec("DELETE FROM w WHERE k = 'a'"),
            Step::Query("SELECT id, name FROM t ORDER BY id"),
            Step::Query("SELECT k, n FROM w ORDER BY k"),
            Step::Query("SELECT id FROM t WHERE name = 'renamed'"),
            // And it cannot run inside a transaction.
            Step::Exec("BEGIN"),
            Step::Exec("VACUUM"),
            Step::Exec("COMMIT"),
            Step::Query("SELECT count(*) FROM t"),
            // An empty database vacuums to an empty database.
            Step::Exec("DELETE FROM t"),
            Step::Exec("DELETE FROM w"),
            Step::Exec("VACUUM"),
            Step::Query("SELECT count(*) FROM t"),
            Step::Query("SELECT count(*) FROM w"),
            Step::Query("SELECT count(*) FROM sqlite_schema"),
            // `PRAGMA foreign_keys` is a connection setting, not a fact about
            // the file `VACUUM` rewrites - SQLite's own `VACUUM` never closes
            // the connection, so nothing about it resets, and this engine's
            // reopen has to put it back rather than default it.
            Step::Exec("PRAGMA foreign_keys = ON"),
            Step::Exec("VACUUM"),
            Step::Query("PRAGMA foreign_keys"),
            // `journal_mode` is the sharper version of the same question,
            // because `wal` is also written into the file's own header - a
            // `VACUUM` that lost it would leave every later opener of the
            // file reading `delete` for what was set to `wal`, not just this
            // connection.
            Step::Exec("PRAGMA journal_mode = wal"),
            Step::Exec("VACUUM"),
            Step::Query("PRAGMA journal_mode"),
            // A `TEMP` table is not a fact about `main`'s file either, and
            // SQLite's `VACUUM` never closes the connection it lives on -
            // checked against the pinned shell, which answers `1` for
            // `CREATE TEMP TABLE t(x); INSERT INTO t VALUES(1); VACUUM;
            // SELECT * FROM t;`. This engine's `VACUUM` does close the
            // connection, twice, so it has to carry the temporary table
            // across the reopen rather than simply never touching it.
            Step::Exec("CREATE TEMP TABLE tt(v)"),
            Step::Exec("INSERT INTO tt VALUES(42)"),
            Step::Exec("VACUUM"),
            Step::Query("SELECT v FROM tt"),
        ],
    );
    assert!(compared == 0 || compared == 44, "compared {compared} steps");
}

/// `VACUUM` of `main` carries an attached database across, real file or
/// `:memory:`, matching SQLite.
///
/// **Not a differential case** - `compare()` runs one file per engine with no
/// second path either can attach, so this drives the engine directly rather
/// than through the oracle harness. What it checks was itself checked against
/// the pinned shell first: `ATTACH DATABASE '<path>' AS aux; CREATE TABLE
/// aux.u(x); INSERT INTO aux.u VALUES(5); CREATE TABLE main.t(y); INSERT INTO
/// t VALUES(7); VACUUM; SELECT * FROM aux.u; SELECT * FROM main.t;` answers
/// `5` then `7`, and substituting `ATTACH DATABASE ':memory:' AS aux` answers
/// the same - because SQLite's `VACUUM` of `main` never closes the connection
/// an attachment lives on, real file or not. This engine's `VACUUM` does
/// close the connection, twice, so `vacuum_in_place` carries `main`'s
/// attachments across the reopen through `crate::rebuild::AttachedSchemas`
/// rather than leaving them for a reopen that would never see them.
#[test]
fn vacuum_carries_an_attached_database_across_like_sqlite() {
    use inillucent_compat::newengine::ImportedDatabase;
    use inillucent_exec::physical::Params;

    let main_path = std::env::temp_dir().join(format!(
        "inillucent-vacuum-attach-main-{}.rdb",
        std::process::id()
    ));
    let file_aux_path = std::env::temp_dir().join(format!(
        "inillucent-vacuum-attach-aux-{}.rdb",
        std::process::id()
    ));
    let mut engine =
        ImportedDatabase::create(main_path.clone(), 4096, 256).expect("main is created");
    engine
        .execute_any(
            &format!(
                "ATTACH DATABASE '{}' AS file_aux",
                file_aux_path.to_string_lossy().replace('\\', "/")
            ),
            &Params::new(),
        )
        .expect("the real-file database attaches");
    engine
        .execute_any("ATTACH DATABASE ':memory:' AS mem_aux", &Params::new())
        .expect("the :memory: database attaches");
    for sql in [
        "CREATE TABLE main.t(y)",
        "INSERT INTO t VALUES(7)",
        "CREATE TABLE file_aux.u(x)",
        // The real-file attachment gets its own index too, so the carry-over
        // is checked against `covering` (a table root's index roots) as well
        // as against the row data.
        "CREATE INDEX file_aux.u_x ON u(x)",
        "INSERT INTO file_aux.u VALUES(5)",
        "CREATE TABLE mem_aux.w(z)",
        "INSERT INTO mem_aux.w VALUES(9)",
    ] {
        engine
            .execute_any(sql, &Params::new())
            .expect("the fixture builds");
    }
    engine
        .execute_any("VACUUM", &Params::new())
        .expect("VACUUM carries the attachments across rather than refusing");

    let (main_rows, _) = engine.run("SELECT y FROM t").expect("main still reads");
    let (file_rows, _) = engine
        .run("SELECT x FROM file_aux.u WHERE x = 5")
        .expect("the real-file attachment's index still reads");
    let (mem_rows, _) = engine
        .run("SELECT z FROM mem_aux.w")
        .expect("the :memory: attachment still reads");
    assert_eq!(main_rows.len(), 1, "main's own row must survive VACUUM");
    assert_eq!(
        file_rows.len(),
        1,
        "the real-file attachment's indexed row must survive VACUUM of main"
    );
    assert_eq!(
        mem_rows.len(),
        1,
        "the :memory: attachment's row must survive VACUUM of main"
    );
    drop(engine);
    let _ = std::fs::remove_file(&main_path);
    let _ = std::fs::remove_file(&file_aux_path);
}

/// `AUTOINCREMENT`, and the `sqlite_sequence` table that makes it work.
///
/// The difference from an ordinary `INTEGER PRIMARY KEY` shows only after a
/// delete: a plain table hands the freed number out again, and an
/// `AUTOINCREMENT` one never does, because it remembers the largest it has
/// issued in a table of its own.
#[test]
fn autoincrement_matches_sqlite() {
    let compared = compare(
        "autoincrement",
        &[
            Step::Exec("CREATE TABLE t(id INTEGER PRIMARY KEY AUTOINCREMENT, v TEXT)"),
            Step::Query("SELECT type, name FROM sqlite_schema ORDER BY name"),
            Step::Exec("INSERT INTO t(v) VALUES('a'),('b'),('c')"),
            Step::Query("SELECT id, v FROM t ORDER BY id"),
            Step::Query("SELECT name, seq FROM sqlite_sequence"),
            // The freed number is not handed out again.
            Step::Exec("DELETE FROM t WHERE id = 3"),
            Step::Exec("INSERT INTO t(v) VALUES('d')"),
            Step::Query("SELECT id, v FROM t ORDER BY id"),
            Step::Query("SELECT name, seq FROM sqlite_sequence"),
            // Nor after the whole table is emptied.
            Step::Exec("DELETE FROM t"),
            Step::Query("SELECT name, seq FROM sqlite_sequence"),
            Step::Exec("INSERT INTO t(v) VALUES('e')"),
            Step::Query("SELECT id, v FROM t"),
            // An explicit key past the counter raises it.
            Step::Exec("INSERT INTO t VALUES(100, 'f')"),
            Step::Query("SELECT name, seq FROM sqlite_sequence"),
            Step::Exec("INSERT INTO t(v) VALUES('g')"),
            Step::Query("SELECT id, v FROM t ORDER BY id"),
            // An explicit key below it does not lower it.
            Step::Exec("INSERT INTO t VALUES(4, 'h')"),
            Step::Query("SELECT name, seq FROM sqlite_sequence"),
            Step::Exec("INSERT INTO t(v) VALUES('i')"),
            Step::Query("SELECT id, v FROM t ORDER BY id"),
            Step::Query("SELECT last_insert_rowid()"),
            // A NULL key means "give me one", as it does anywhere else.
            Step::Exec("INSERT INTO t VALUES(NULL, 'j')"),
            Step::Query("SELECT id, v FROM t ORDER BY id"),
            // A second such table gets its own counter.
            Step::Exec("CREATE TABLE u(id INTEGER PRIMARY KEY AUTOINCREMENT, v TEXT)"),
            Step::Exec("INSERT INTO u(v) VALUES('x')"),
            Step::Query("SELECT name, seq FROM sqlite_sequence ORDER BY name"),
            // A plain INTEGER PRIMARY KEY reuses what it freed, which is the
            // whole point of the difference.
            Step::Exec("CREATE TABLE p(id INTEGER PRIMARY KEY, v TEXT)"),
            Step::Exec("INSERT INTO p(v) VALUES('a'),('b')"),
            Step::Exec("DELETE FROM p WHERE id = 2"),
            Step::Exec("INSERT INTO p(v) VALUES('c')"),
            Step::Query("SELECT id, v FROM p ORDER BY id"),
            // A dropped table takes its counter with it.
            Step::Exec("DROP TABLE u"),
            Step::Query("SELECT name, seq FROM sqlite_sequence ORDER BY name"),
            // The declarations SQLite refuses.
            Step::Exec("CREATE TABLE a(x TEXT PRIMARY KEY AUTOINCREMENT)"),
            Step::Exec("CREATE TABLE b(x INT PRIMARY KEY AUTOINCREMENT)"),
            Step::Exec("CREATE TABLE c(x, y, PRIMARY KEY(x, y) AUTOINCREMENT)"),
            Step::Exec("CREATE TABLE d(x INTEGER PRIMARY KEY AUTOINCREMENT) WITHOUT ROWID"),
            Step::Query("SELECT count(*) FROM sqlite_schema"),
        ],
    );
    assert!(compared == 0 || compared == 39, "compared {compared} steps");
}
