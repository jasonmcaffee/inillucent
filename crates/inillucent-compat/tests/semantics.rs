//! Every construct the second review probed, answered by both shells.
//!
//! Invariant: **each case declares whether it agrees with SQLite, and a case
//! that changes its mind fails** - in either direction, so a construct that
//! starts agreeing fails until its row is moved, and the recorded difference
//! below fails if it is ever fixed without the table being told.
//!
//! ## The one construct that still differs
//!
//! **`alias.limit`**: `... LIMIT x` where `x` is a result alias. Both refuse it
//! and each says something different: SQLite does not resolve a result alias in
//! a `LIMIT` at all and reports `no such column`, while this engine resolves it
//! and then refuses the statement for having a `LIMIT` that is not a constant.
//! A caller gets a refusal either way and a sentence about the wrong thing
//! here.
//!
//! There were two. `alias.having` was the other: `SELECT count(*) AS n FROM t
//! HAVING n > 0` was a syntax error here and an answer in SQLite. task-2026
//! found it and recorded it as a difference, task-2040 fixed it, and its row
//! moved to `Agrees` in the same change - which is the whole of what this
//! table is for.
//!
//! ## Why a case that starts agreeing is a failure too
//!
//! It is the same discipline `new_engine_surface.rs` applies to *acceptance*,
//! applied here to *answers*. Without it, a fix is invisible: the probe was a
//! one-off script, so the nine wrong answers the review found could each have
//! been repaired and then silently regressed with nothing to notice. A row has
//! to move for the file to go green again, which is where the next reader
//! learns that the construct changed.
//!
//! ## Why whole scripts through the shells
//!
//! Because a difference in an *error* is a difference. Half of what the review
//! found was a refusal where SQLite answers, or a message that names something
//! else, and a harness comparing rows would have discarded exactly that. So the
//! comparison is the same one `cli.rs` makes - a whole script on standard
//! input, every byte of standard output and standard error - and the case
//! declares the outcome rather than the rows.
//!
//! ## The five cases a partial index needs, and why three of them look like
//! nothing
//!
//! An `UPDATE` that moves a row across a partial index's predicate exists only
//! where partial indexes and the `UPDATE` conflict check meet: neither could
//! be tested alone, because until partial indexes existed the index cannot be
//! created, and until the conflict check existed the check is never reached.
//!
//! `crossing.into` and `two.indexes` are the ones that were wrong: a row moving
//! into the predicate onto a key already there must clash, and it did not. The
//! other three **agreed before either fix, because nothing was checked at
//! all** - and they have to go on agreeing for the opposite reason, now that
//! the check is reached and declines to fire. That is what makes them worth
//! writing down rather than assuming, because a test that passes because
//! nothing ran is not evidence:
//!
//! - `crossing.outof` goes red if the predicate guard is asked of the row's
//!   **old** image, because a row leaving the index would be probed as though
//!   it were still in it, and refused a write SQLite allows;
//! - `crossing.staying` goes red if the conflict check cannot recognise the
//!   row's **own** entry, because it would report a collision with itself;
//! - `maintenance.crossing` goes red if the write path skips an index whose
//!   entry bytes did not change without also asking whether the *predicate*
//!   changed - which would leave a crossing row with no entry, or with one it
//!   should have lost. That is an index silently disagreeing with its table
//!   rather than a refused write, and it reads correctly until the query that
//!   uses the index.
//!
//! `two.indexes` carries a second job: with one plain unique index and one
//! partial one on the same table, it fails if the predicate is looked up by a
//! position that does not survive the order `unique_indexes` yields them in.
//! That would consult **another index's** predicate, silently, and only on a
//! table with more than one unique index.
//!
//! ## The two cases about building an index over a written-to table
//!
//! `index.after.writes` and its `WITHOUT ROWID` twin insert, update and delete
//! *before* the `CREATE INDEX`, so the leaves the build reads carry tombstones
//! and a delta area. That is a different code path from a freshly imported
//! table - the merge, rather than the vectorised mini-column read - and it was
//! given a projected implementation of its own to stop it reading every
//! column of every row. Two implementations of one merge is exactly the
//! shape that drifts, so the answer is compared against the reference here.
//!
//! ## The three `HAVING` with no `GROUP BY` cases
//!
//! `SELECT count(*) AS n FROM t HAVING n > 0` answered
//! `near "HAVING": syntax error` here and `3` at the reference, because the
//! grammar read `HAVING` only inside the `GROUP BY` arm (task-2040). A syntax
//! error is the worst refusal available for it: `AGENTS.md` tells a caller that
//! exit code 3 and the `unsupported` status mean "this engine has not built
//! that", so a *syntax* error says the SQL is wrong and sends them rewording a
//! statement that is already correct.
//!
//! - `alias.having` is the statement from the ticket, in both directions - the
//!   `HAVING` that keeps the group and the one that drops it - and with `max`
//!   as well as `count`, because a bare column follows a single `min` or `max`
//!   by a rule of its own and a `HAVING` must not disturb it.
//! - `having.whole.table` is the surrounding surface: the aggregate beside a
//!   `WHERE`, a bare column in the `HAVING`, `DISTINCT`, an `ORDER BY` with a
//!   `LIMIT`, a subquery in the `HAVING`, a `HAVING` that is `NULL` or a
//!   string, and an empty table - where the group still exists and the count
//!   is zero.
//! - `having.non.aggregate` is the half that stays refused, and it is the case
//!   that says the grammar and the binder agree. SQLite makes a statement an
//!   aggregating one by finding an aggregate **among the result columns** and
//!   by nothing else, so `SELECT 1 FROM t HAVING count(*) > 0` and
//!   `SELECT 1 FROM t HAVING 1 ORDER BY count(*)` are both
//!   `HAVING clause on a non-aggregate query` there. A parser that accepts the
//!   clause and a binder that then accepts every statement carrying it would
//!   answer rows for all five of these, which is a wrong answer where the
//!   syntax error was at least a refusal. The last three statements are the
//!   ones that go on working, so a run cannot pass by refusing everything.
//!
//! All three arrive as `Agrees`, in the same commit as the fix. The ticket that
//! asked for this expected to find `alias.having` already here as `Differs`,
//! recording the syntax error so that fixing it would fail this file until the
//! row moved - and no such row was ever written, so there was nothing to move.
//! A case that is added at the same time as the behaviour it describes cannot
//! have that history, and what it is worth is the other direction: it fails the
//! day the grammar or the binder changes its mind again.
//!
//! The refusal is `ParseErrorKind::Refused` with a **default span**: the
//! reference reports it with no offset, so its shell prints the sentence and no
//! caret art, and a span here would have made the two transcripts differ on two
//! lines of drawing rather than on an answer.
//!
//! ## Every construct agrees; seven measured differences do not, and say so
//!
//! `index.partial`, `index.expr` and `without.rowid.index` - the three
//! `CREATE INDEX` forms that were once left refused - are now built, and their
//! rows moved from `Differs` to `Agrees`. No *construct* differs.
//!
//! The seven `Differs` rows at the end of the table are a different thing, and
//! they arrived in task-2036. `docs/feature-comparison.md` measured them, gave
//! each a reason and published them as "the seven rows that are not the same",
//! and nothing in this suite asserted any of them - so `Expect::Differs` was
//! constructed zero times while seven known differences went unchecked. Each of
//! the seven is this engine's page size, a number that describes SQLite's own C
//! structures, or the two pinned reference artifacts disagreeing with one
//! another; none of them is a defect, and every one of them is a thing a change
//! could close or widen with nothing going red.
//!
//! The check below that a case which starts agreeing fails until its row is
//! moved is what reports the next one either way.
//!
//! ## The seven `DROP COLUMN` cases, and why one of them is the control
//!
//! `ALTER TABLE t(a,b,c,d) DROP COLUMN b` left `c` holding `b`'s numbers and
//! `d` holding `c`'s, and `d`'s numbers were gone (task-2057). The rebuild that
//! fills the new tree indexed the *old* layout with a position from the *new*
//! declaration, and those agree only while nothing moved - so dropping the last
//! column was right and dropping any other column was not. The statement
//! committed, so the file held the wrong rows.
//!
//! `alter.drop.last` is the case that passed before the fix and has to go on
//! passing. It is the control the other six are read against: a mapping that
//! shifted every position rather than only those at and after the dropped one
//! would answer the first six and fail this one, and without it a run could
//! report the family green while the arithmetic was wrong in the other
//! direction. It is the shape task-2051's own reproduction had, which is why
//! nothing caught the defect for as long as it existed.
//!
//! `alter.drop.reopen` reads the table back through a second connection over
//! the same file. That is what separates a connection holding a wrong derived
//! view from a file holding wrong rows, and this one was the file.
//!
//! `alter.drop.index` is the second defect, found by the check task-2057 asked
//! for and fixed in the same change. An index's layout is derived once and
//! `refresh_catalog` never derives it again, so after the drop an index on `d`
//! still recorded `d` at the position it had in the old declaration: the
//! planner offered the index, the physical pass then asked it for a column its
//! layout said it did not carry, and `SELECT d FROM t WHERE d = 40` failed with
//! `the tree read for FROM term 0 does not carry column 2` where SQLite answers
//! `40`. Reopening answered it, because the layouts are derived afresh on open
//! - the mirror image of `alter.drop.reopen`, and the reason both are here.
//!
//! `alter.drop.withoutrowid` drops a middle column of a table whose primary key
//! *is* the tree. It grades the rebuild over a keyed layout, where a column's
//! tree position is not its declared position at all, and it is also what
//! caught the index refresh replacing that table's own layout with an index's:
//! such a primary key is listed among the table's indexes and carries the
//! table's root, so re-deriving it broke reads that had been correct.
//!
//! `alter.add.default.wide` is item 4 of the same ticket. `ADD COLUMN` fills
//! from the same loop, and its added column is the one thing in that loop with
//! no old column behind it, so a mapping written without a case for it would
//! fill the new column from the last old one instead of from its `DEFAULT`.

//! ## The ten `ALTER TABLE` cases on a database that is not `main`
//!
//! `ALTER TABLE` worked on `main` and on nothing else (task-2061). Three
//! separate faults produced that, and the cases below are grouped by which one
//! each grades, because a fix for one of them leaves the other two answering
//! wrongly.
//!
//! **The tree was rebuilt into the right file and then read out of `main`'s.**
//! `ALTER TABLE ... ADD COLUMN` and `DROP COLUMN` release the old tree and
//! build a new one under the same handle, and `release_tree` takes that handle
//! out of the map recording which file it belongs to. Nothing put it back, so
//! `schema_of` fell to its "not attached, so `main`" answer and the next read
//! of the table went to `main`'s file at a page number belonging to another
//! one: `read 0 of 32768 bytes at 163840` where SQLite answers the row.
//! `alter.attach.add`, `alter.attach.drop`, `alter.temp.actions` and
//! `alter.temp.qualified` are that failure, on both kinds of schema.
//!
//! **The rebuild found its table by name with no schema, and wrote the new
//! root page into `main`'s catalog rows.** `alter.attach.shadow` is the case
//! that matters most, because it answered rather than failing: with a table
//! called `t` in both schemas, `ALTER TABLE side.t DROP COLUMN b` rebuilt
//! `main.t`'s tree and left `side.t`'s catalog row naming a page that had been
//! given back to the free map. One statement about `side.t` changed what
//! `main.t` answered. `alter.temp.shadow` is the same shape with a temporary
//! table over a permanent one.
//!
//! `alter.attach.reopen` is what separates the two halves of that fault. The
//! rebuilt tree's root page is written back into a catalog row, and the search
//! for that row read `main`'s rows whatever schema the statement named - so a
//! connection could answer correctly from its own derived view while the
//! attached file on disk still pointed at the released page. It reopens
//! `side.db` as `main` through a second connection, which reads the file and
//! nothing else.
//!
//! `alter.attach.index` and `alter.attach.withoutrowid` carry task-2057's two
//! hardest cases onto an attached database: an index whose layout has to be
//! re-derived after the drop, and a rebuild over a keyed layout where a
//! column's tree position is not its declared position. Both go through the
//! same unfiltered lookup, so both were wrong for the same reason.
//!
//! **An unqualified `ALTER TABLE` searched `main` alone.** The binder resolved
//! a name with no schema qualifier to `main` before searching, rather than
//! searching in SQLite's order, so `CREATE TEMP TABLE t (a,b);
//! ALTER TABLE t ADD COLUMN c` was `no such table: t`. `alter.temp.actions`
//! grades the refusal and `alter.temp.shadow` grades the order - SQLite
//! searches `temp` first, so with a `t` in both, the temporary one is the one
//! altered and `main.t` must come back untouched.
//! `alter.unqualified.attached` is the far end of that order: neither `temp`
//! nor `main` holds a `t`, so the attached database's is the one found.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use inillucent_compat::workspace_root;

/// Whether a case is expected to agree with the reference.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Expect {
    /// Byte-for-byte the same output.
    Agrees,
    /// Still different, and the row says what the difference measures.
    ///
    /// **Constructed by nine cases: two of the engine's own and the seven
    /// `docs/feature-comparison.md` measured.** For a long stretch it was
    /// constructed by none - the constructs that once differed were fixed one by
    /// one and their rows moved to `Agrees` - and the variant was kept anyway,
    /// because the header this file carries is a list of the constructs that once
    /// differed and a variant that went with the last of them would take the
    /// vocabulary too. `alias.having` and `alias.limit` are the engine's two.
    ///
    /// The other seven arrived with task-2036. Rule 1.3 says a known difference is
    /// recorded as a test that asserts it, and the comparison document carried
    /// seven measured, argued differences that nothing in the suite asserted - a
    /// difference only a document knows about can be closed, or widened, with
    /// nothing going red.
    Differs,
}

use Expect::{Agrees, Differs};

/// One probed construct.
struct Case {
    /// What it is called, in the review's own naming.
    name: &'static str,
    /// Which group of behaviour it belongs to.
    kind: &'static str,
    /// The whole script, fed to both shells on standard input.
    script: &'static str,
    /// Whether the two are expected to agree.
    expect: Expect,
}

/// The cases: the review's 61, the agreeing shapes it did not record, the
/// seven `UNIQUE`-under-`UPDATE` shapes from fixing the update conflict
/// check, and the index-forms work's own - two over a table that has been
/// written to, and five where the two meet.
const CASES: &[Case] = &[
    // The silent differences: every one of these answered, and answered
    // something else.
    //
    // A `CHECK` containing a subquery is *not* here, and the reason is the
    // shell rather than the engine: both refuse it, in the same words, and the
    // reference's shell then draws a caret under the offending token in a
    // spelling this one does not use. The refusal is what the ticket is about
    // and the probe records it as agreement; a case here would be asserting on
    // the reference's caret art. `docs/feature-comparison.md` names it.
    Case {
        name: "upsert.arm.where",
        kind: "write",
        script: "CREATE TABLE t(a INTEGER PRIMARY KEY, n INTEGER);\nINSERT INTO t VALUES(1,100),(2,600);\nINSERT INTO t VALUES(1,7) ON CONFLICT(a) DO UPDATE SET n=999 WHERE t.n > 500;\nINSERT INTO t VALUES(2,7) ON CONFLICT(a) DO UPDATE SET n=999 WHERE t.n > 500;\nSELECT * FROM t ORDER BY a;\nSELECT changes();",
        expect: Agrees,
    },
    // A later QA pass: the two shapes the 416-case probe caught reporting a
    // refusal in the wrong words.
    //
    // The pinned reference is not compiled with
    // `SQLITE_ENABLE_UPDATE_DELETE_LIMIT`, so it has no grammar for the clause
    // and answers `near "ORDER": syntax error`. This engine parses the form -
    // it is a published production and the syntax register requires it - and
    // refuses it at bind time in the reference's words. A later change moved
    // `bind::refused` from `Unexpected` to `Refused`, which was right for the
    // forty-seven sentence-shaped refusals it was aimed at and wrong for this
    // one: the message became a bare `ORDER`. Both engines still refused, so
    // nothing that only checks for failure could see it.
    Case {
        name: "dml.delete.limit",
        kind: "write",
        script: "CREATE TABLE t(id INTEGER PRIMARY KEY, a INTEGER, b TEXT);
INSERT INTO t VALUES (1,10,'p'),(2,20,'q'),(3,30,'r'),(4,20,'s'),(5,50,'t');
DELETE FROM t ORDER BY a DESC LIMIT 2;
SELECT count(*) FROM t;",
        expect: Agrees,
    },
    Case {
        name: "dml.update.limit",
        kind: "write",
        script: "CREATE TABLE t(id INTEGER PRIMARY KEY, a INTEGER, b TEXT);
INSERT INTO t VALUES (1,10,'p'),(2,20,'q'),(3,30,'r'),(4,20,'s'),(5,50,'t');
UPDATE t SET b='z' ORDER BY a DESC LIMIT 1;
SELECT group_concat(b) FROM (SELECT b FROM t ORDER BY id);",
        expect: Agrees,
    },
    Case {
        name: "trigger.recursive",
        kind: "trigger",
        script: "PRAGMA recursive_triggers=ON;\nCREATE TABLE t(a INTEGER);\nCREATE TRIGGER r AFTER INSERT ON t WHEN new.a < 5 BEGIN INSERT INTO t VALUES(new.a+1); END;\nINSERT INTO t VALUES(1);\nSELECT group_concat(a) FROM (SELECT a FROM t ORDER BY a);",
        expect: Agrees,
    },
    // And with the pragma off, which is the default: the trigger fires once.
    Case {
        name: "trigger.recursive.off",
        kind: "trigger",
        script: "CREATE TABLE t(a INTEGER);\nCREATE TRIGGER r AFTER INSERT ON t WHEN new.a < 5 BEGIN INSERT INTO t VALUES(new.a+1); END;\nINSERT INTO t VALUES(1);\nSELECT group_concat(a) FROM (SELECT a FROM t ORDER BY a);",
        expect: Agrees,
    },
    Case {
        name: "like.case.sensitive",
        kind: "operator",
        script: "SELECT 'ABC' LIKE 'a%';\nPRAGMA case_sensitive_like=ON;\nSELECT 'ABC' LIKE 'a%', 'ABC' LIKE 'A%', like('a%','ABC');\nPRAGMA case_sensitive_like=OFF;\nSELECT 'ABC' LIKE 'a%';",
        expect: Agrees,
    },
    Case {
        name: "is.distinct.from",
        kind: "operator",
        script: "SELECT 1 IS DISTINCT FROM NULL, 1 IS NOT DISTINCT FROM 1, NULL IS NOT DISTINCT FROM NULL, NULL IS DISTINCT FROM NULL, 1 IS 1, 1 IS NOT 2, 1 IS NULL;",
        expect: Agrees,
    },
    Case {
        name: "types.min.integer",
        kind: "types",
        script: "SELECT typeof(-9223372036854775808), -9223372036854775808, typeof(-1), -1, -0x10, typeof(9223372036854775808), -9223372036854775809;",
        expect: Agrees,
    },
    Case {
        name: "agg.sum.overflow",
        kind: "read",
        script: "CREATE TABLE b(x);\nINSERT INTO b VALUES(9223372036854775807),(9223372036854775807);\nSELECT total(x) FROM b;\nSELECT avg(x) FROM b;\nSELECT sum(x) FROM b;",
        expect: Agrees,
    },
    // A real anywhere in the column makes the sum a real, which has no
    // overflow to report.
    Case {
        name: "agg.sum.overflow.real",
        kind: "read",
        script: "CREATE TABLE b(x);\nINSERT INTO b VALUES(9223372036854775807),(9223372036854775807),(0.5);\nSELECT sum(x) FROM b;",
        expect: Agrees,
    },
    Case {
        name: "types.nan",
        kind: "types",
        script: "SELECT 1e999, -1e999, 1e999-1e999, typeof(1e999), 0.0/0.0;",
        expect: Agrees,
    },
    Case {
        name: "fn.jsonb.extract",
        kind: "json",
        script: "SELECT jsonb_extract(jsonb('{\"a\":2}'), '$.a'), json_extract(jsonb('{\"a\":2}'), '$.a'), typeof(jsonb_extract(jsonb('{\"b\":[1,2]}'), '$.b')), hex(jsonb_extract(jsonb('{\"b\":[1,2]}'), '$.b')), typeof(jsonb_extract(jsonb('{\"s\":\"x\"}'),'$.s'));",
        expect: Agrees,
    },
    // **The whole specifier family, not the three that were reported.** That
    // is the lesson from a bug where fixing one member of a family and
    // assuming the rest left the next three hidden.
    Case {
        name: "fn.strftime.family",
        kind: "time",
        script: "SELECT strftime('%d %e %f %F %H %I %j %k %l %m %M %p %P %R %s %S %u %U %V %w %W %G %g %Y %%','2024-03-01 09:05:07');",
        expect: Agrees,
    },
    Case {
        name: "fn.strftime.unknown",
        kind: "time",
        script: "SELECT quote(strftime('%y','2024-03-01')), quote(strftime('%Z','2024-03-01')), quote(strftime('%J','2024-03-01 09:05:07'));",
        expect: Agrees,
    },
    Case {
        name: "fn.time.subsec",
        kind: "time",
        script: "SELECT datetime('2024-03-01 12:00:00','subsec'), datetime('2024-03-01 12:00:00.123','subsec'), time('2024-03-01 12:00:00','subsec'), unixepoch('2024-03-01 09:05:07'), strftime('%s','2024-03-01 09:05:07');",
        expect: Agrees,
    },
    Case {
        name: "fn.round.huge",
        kind: "read",
        script: "SELECT round(1e308,2), round(2.5), round(-2.5), round(1.005,2);",
        expect: Agrees,
    },
    Case {
        name: "fn.length.nul",
        kind: "read",
        script: "SELECT length(char(0)), length(char(65,0,66)), hex(char(0)), hex(char(65,0,66)), length('abc');",
        expect: Agrees,
    },
    Case {
        name: "ddl.pk.desc.index",
        kind: "ddl",
        script: "CREATE TABLE pd(a INTEGER PRIMARY KEY DESC, b);\nINSERT INTO pd VALUES(1,'x'),(2,'y');\nSELECT count(*) FROM sqlite_schema WHERE type='index' AND tbl_name='pd';\nSELECT a,b FROM pd ORDER BY a;\nSELECT typeof(a) FROM pd LIMIT 1;\nCREATE TABLE pa(a INTEGER PRIMARY KEY, b);\nSELECT count(*) FROM sqlite_schema WHERE type='index' AND tbl_name='pa';",
        expect: Agrees,
    },
    // The stored declaration is the *affinity*, in the reference's own layout -
    // including its under-fifty-characters-one-line rule.
    Case {
        name: "ddl.ctas.declared",
        kind: "ddl",
        script: "CREATE TABLE s(a INTEGER, b INT, c BIGINT, d TEXT, e VARCHAR(3), f CHAR(5), g BLOB, h REAL, i DOUBLE, j NUMERIC, k DECIMAL(4,2), l, m DATETIME);\nCREATE TABLE d1 AS SELECT a,b,c,d,e,f,g,h,i,j,k,l,m FROM s;\nSELECT sql FROM sqlite_schema WHERE name='d1';\nCREATE TABLE d4 AS SELECT g FROM s;\nSELECT sql FROM sqlite_schema WHERE name='d4';",
        expect: Agrees,
    },
    // The refusals an ordinary query hits.
    Case {
        name: "select.bare.column",
        kind: "read",
        script: "CREATE TABLE t(id INTEGER PRIMARY KEY, a INTEGER, b TEXT);\nINSERT INTO t VALUES (1,10,'p'),(2,30,'q'),(3,20,'r'),(4,30,'s');\nSELECT id, max(a) FROM t;\nSELECT id, min(a) FROM t;\nSELECT id, b, max(a) FROM t;\nSELECT id, count(*) FROM t;\nSELECT id+100, max(a) FROM t;",
        expect: Agrees,
    },
    Case {
        name: "select.bare.column.grouped",
        kind: "read",
        script: "CREATE TABLE t(id INTEGER PRIMARY KEY, a INTEGER, b TEXT);\nINSERT INTO t VALUES (1,10,'p'),(2,30,'q'),(3,20,'r'),(4,30,'s');\nSELECT b, id, max(a) FROM t GROUP BY (a>15) ORDER BY 1;",
        expect: Agrees,
    },
    Case {
        name: "select.distinct.carried",
        kind: "read",
        script: "CREATE TABLE t(a INTEGER, b TEXT);\nINSERT INTO t VALUES(10,'z'),(20,'y'),(30,'x'),(10,'w');\nSELECT DISTINCT a FROM t ORDER BY b;\nSELECT DISTINCT a FROM t ORDER BY a;\nSELECT DISTINCT a, b FROM t ORDER BY b;",
        expect: Agrees,
    },
    Case {
        name: "agg.filter",
        kind: "read",
        script: "CREATE TABLE t(id INTEGER PRIMARY KEY, a INTEGER, b TEXT);\nINSERT INTO t VALUES (1,10,'p'),(2,30,'q'),(3,20,'r'),(4,30,'s');\nSELECT count(*) FILTER (WHERE a>15) FROM t;\nSELECT count(*) FILTER (WHERE a>15), count(*), sum(a) FILTER (WHERE a<25) FROM t;\nSELECT b, count(*) FILTER (WHERE a>15) FROM t GROUP BY b ORDER BY b;",
        expect: Agrees,
    },
    Case {
        name: "agg.order.by",
        kind: "read",
        script: "CREATE TABLE t(id INTEGER PRIMARY KEY, a INTEGER, b TEXT);\nINSERT INTO t VALUES (1,10,'p'),(2,30,'q'),(3,20,'r'),(4,30,'s');\nSELECT group_concat(b ORDER BY a DESC) FROM t;\nSELECT group_concat(b ORDER BY a) FROM t;\nSELECT group_concat(b, '-' ORDER BY a DESC) FROM t;\nSELECT json_group_array(a ORDER BY a DESC) FROM t;",
        expect: Agrees,
    },
    Case {
        name: "agg.json.group",
        kind: "json",
        script: "CREATE TABLE t(id INTEGER PRIMARY KEY, a INTEGER, b TEXT);\nINSERT INTO t VALUES(1,10,'p'),(2,20,'q'),(3,NULL,'r');\nSELECT json_group_array(a) FROM t;\nSELECT json_group_object(b, a) FROM t;\nSELECT b, json_group_array(a) FROM t GROUP BY b ORDER BY b;\nSELECT typeof(jsonb_group_array(a)) FROM t;",
        expect: Agrees,
    },
    Case {
        name: "select.row.value.query",
        kind: "read",
        script: "CREATE TABLE t(id INTEGER PRIMARY KEY, a INTEGER, b TEXT);\nINSERT INTO t VALUES (1,10,'p'),(2,30,'q'),(3,20,'r'),(4,30,'s');\nSELECT id FROM t WHERE (a,b) = (SELECT a,b FROM t WHERE id=3);\nSELECT id FROM t WHERE (a,b) <> (SELECT a,b FROM t WHERE id=3) ORDER BY id;\nSELECT id FROM t WHERE (a,b) < (SELECT a,b FROM t WHERE id=3) ORDER BY id;\nSELECT id FROM t WHERE (a,b) = (SELECT a,b FROM t WHERE id=99);",
        expect: Agrees,
    },
    Case {
        name: "cte.recursive.limit",
        kind: "read",
        script: "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n) SELECT count(*), max(x) FROM (SELECT x FROM n LIMIT 1000);\nWITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM c WHERE n < 5) SELECT sum(n) FROM c;",
        expect: Agrees,
    },
    Case {
        name: "syntax.trailing.comment",
        kind: "syntax",
        script: "SELECT 1;\n-- trailing",
        expect: Agrees,
    },
    // ----------------------------------------------------------------------
    //
    // A round of constructs closed together, in the order they were fixed.
    // A row here is the guard on a whole feature: `geopoly.overlap` is the
    // sweep, `fts5.external` is the shadow-table grant, and `pgvector.ops` is
    // the three-byte operator lexing that `<=>` needs.
    Case {
        name: "join.lateral",
        kind: "join",
        script: "CREATE TABLE t(a INTEGER);\nINSERT INTO t VALUES (2),(3);\nSELECT t.a, s.value FROM t, generate_series(1, t.a) AS s ORDER BY t.a, s.value;",
        expect: Agrees,
    },
    Case {
        name: "upsert.two.clauses",
        kind: "dml",
        script: "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT UNIQUE, c INTEGER);\nINSERT INTO t VALUES (1,'x',10);\nINSERT INTO t VALUES (1,'y',20) ON CONFLICT(a) DO UPDATE SET c=99 ON CONFLICT(b) DO NOTHING;\nSELECT * FROM t ORDER BY a;\nINSERT INTO t VALUES (2,'x',30) ON CONFLICT(a) DO UPDATE SET c=98 ON CONFLICT(b) DO UPDATE SET c=97;\nSELECT * FROM t ORDER BY a;",
        expect: Agrees,
    },
    Case {
        name: "fts5.highlight",
        kind: "fts5",
        script: "CREATE VIRTUAL TABLE f USING fts5(body);\nINSERT INTO f VALUES ('the quick brown fox');\nSELECT highlight(f, 0, '[', ']') FROM f WHERE f MATCH 'quick brown';\nSELECT highlight(f, 0, '[', ']') FROM f WHERE f MATCH '\"quick brown\"';\nSELECT snippet(f, 0, '<', '>', '...', 3) FROM f WHERE f MATCH 'brown';",
        expect: Agrees,
    },
    Case {
        name: "fts5.vocab",
        kind: "fts5",
        script: "CREATE VIRTUAL TABLE f USING fts5(body);\nINSERT INTO f VALUES ('one two two'),('two three');\nCREATE VIRTUAL TABLE v USING fts5vocab(f, 'row');\nSELECT term, doc, cnt FROM v ORDER BY term;\nCREATE VIRTUAL TABLE vc USING fts5vocab(f, 'col');\nSELECT term, col, doc, cnt FROM vc ORDER BY term;\nCREATE VIRTUAL TABLE vi USING fts5vocab(f, 'instance');\nSELECT term, doc, col, offset FROM vi ORDER BY term, doc, offset;",
        expect: Agrees,
    },
    Case {
        name: "fts5.external",
        kind: "fts5",
        script: "CREATE TABLE c(id INTEGER PRIMARY KEY, body TEXT);\nINSERT INTO c VALUES (1,'quick fox'),(2,'lazy dog');\nCREATE VIRTUAL TABLE f USING fts5(body, content='c', content_rowid='id');\nINSERT INTO f(f) VALUES ('rebuild');\nSELECT count(*) FROM f WHERE f MATCH 'fox';\nSELECT rowid, body FROM f WHERE f MATCH 'dog';\nSELECT count(*) FROM f;",
        expect: Agrees,
    },
    Case {
        name: "fts4.basic",
        kind: "fts5",
        script: "CREATE VIRTUAL TABLE f USING fts4(body, title);\nINSERT INTO f VALUES ('quick fox','one'),('lazy dog','two');\nSELECT count(*) FROM f WHERE f MATCH 'fox';\nSELECT docid, body FROM f WHERE f MATCH 'dog';\nSELECT snippet(f) FROM f WHERE f MATCH 'quick';\nSELECT offsets(f) FROM f WHERE f MATCH 'quick';\nDELETE FROM f WHERE docid=1;\nSELECT count(*) FROM f;",
        expect: Agrees,
    },
    Case {
        name: "fts3.basic",
        kind: "fts5",
        script: "CREATE VIRTUAL TABLE g USING fts3(a);\nINSERT INTO g VALUES ('hello world');\nSELECT count(*) FROM g WHERE g MATCH 'world';\nSELECT count(*) FROM g WHERE g MATCH 'a:hello';",
        expect: Agrees,
    },
    Case {
        name: "geopoly.shapes",
        kind: "ext",
        script: "SELECT geopoly_json('[[0,0],[3,0],[3,3],[0,3],[0,0]]');\nSELECT hex(geopoly_blob('[[0,0],[3,0],[3,3],[0,3],[0,0]]'));\nSELECT geopoly_area('[[0,0],[3,0],[3,3],[0,3],[0,0]]'), geopoly_area('[[0,0],[0,3],[3,3],[3,0],[0,0]]');\nSELECT geopoly_json(geopoly_ccw('[[0,0],[0,3],[3,3],[3,0],[0,0]]'));\nSELECT geopoly_json(geopoly_bbox('[[1,2],[5,2],[3,7],[1,2]]'));\nSELECT geopoly_json(geopoly_xform('[[0,0],[3,0],[3,3],[0,3],[0,0]]',1,0,0,1,10,20));\nSELECT geopoly_json(geopoly_regular(0,0,10,4));\nSELECT geopoly_svg('[[0,0],[3,0],[3,3],[0,0]]','fill=\"red\"');",
        expect: Agrees,
    },
    Case {
        name: "geopoly.overlap",
        kind: "ext",
        script: "SELECT geopoly_contains_point('[[0,0],[3,0],[3,3],[0,3],[0,0]]',1,1), geopoly_contains_point('[[0,0],[3,0],[3,3],[0,3],[0,0]]',0,0), geopoly_contains_point('[[0,0],[3,0],[3,3],[0,3],[0,0]]',9,9);\nSELECT geopoly_overlap('[[0,0],[3,0],[3,3],[0,3],[0,0]]','[[1,1],[2,1],[2,2],[1,2],[1,1]]');\nSELECT geopoly_overlap('[[0,0],[3,0],[3,3],[0,3],[0,0]]','[[9,9],[10,9],[10,10],[9,9]]');\nSELECT geopoly_overlap('[[0,0],[3,0],[3,3],[0,3],[0,0]]','[[0,0],[3,0],[3,3],[0,3],[0,0]]');\nSELECT geopoly_overlap('[[0,0],[3,0],[3,3],[0,3],[0,0]]','[[2,2],[5,2],[5,5],[2,5],[2,2]]');\nSELECT geopoly_within('[[0,0],[3,0],[3,3],[0,3],[0,0]]','[[1,1],[2,1],[2,2],[1,2],[1,1]]');",
        expect: Agrees,
    },
    Case {
        name: "geopoly.table",
        kind: "ext",
        script: "CREATE VIRTUAL TABLE g USING geopoly(name);\nINSERT INTO g(_shape,name) VALUES('[[0,0],[3,0],[3,3],[0,3],[0,0]]','a'),('[[10,10],[13,10],[13,13],[10,10]]','b');\nSELECT rowid, name, geopoly_json(_shape) FROM g ORDER BY rowid;\nSELECT count(*) FROM g WHERE geopoly_overlap(_shape,'[[1,1],[2,1],[2,2],[1,1]]');\nSELECT name FROM g WHERE geopoly_contains_point(_shape, 11, 11);\nUPDATE g SET name='c' WHERE rowid=1;\nSELECT rowid,name FROM g ORDER BY rowid;\nDELETE FROM g WHERE rowid=2;\nSELECT count(*) FROM g;\nPRAGMA integrity_check;",
        expect: Agrees,
    },
    Case {
        name: "geopoly.invalid",
        kind: "ext",
        script: "CREATE VIRTUAL TABLE g USING geopoly(name);\nINSERT INTO g(_shape,name) VALUES ('not a polygon','t1');\nINSERT INTO g(_shape,name) VALUES ('[[0,0],[3,0]]','t2');\nINSERT INTO g(_shape,name) VALUES (x'0102','t3');\nINSERT INTO g(_shape,name) VALUES (42,'t4');\nSELECT rowid, name, typeof(_shape) FROM g ORDER BY rowid;",
        expect: Agrees,
    },
    Case {
        name: "geopoly.group.bbox",
        kind: "ext",
        script: "CREATE TABLE p(x TEXT);\nINSERT INTO p VALUES ('[[0,0],[1,0],[1,1],[0,0]]'),('[[5,5],[6,5],[6,6],[5,5]]');\nSELECT geopoly_json(geopoly_group_bbox(x)) FROM p;",
        expect: Agrees,
    },
    Case {
        name: "rtree.helpers",
        kind: "ext",
        script: "CREATE VIRTUAL TABLE r USING rtree(id, x0, x1, y0, y1);\nINSERT INTO r VALUES (1, 0,1, 0,1),(2, 5,6, 5,6);\nSELECT rtreecheck('r');\nSELECT rtreedepth(data) FROM r_node WHERE nodeno=1;\nSELECT rtreenode(2, data) FROM r_node WHERE nodeno=1;",
        expect: Agrees,
    },
    Case {
        name: "json.array.insert",
        kind: "json",
        script: "SELECT json_array_insert('[1,2]','$[0]',9);\nSELECT json_array_insert('[1,2]','$[2]',9);\nSELECT json_array_insert('[1,2]','$[#]',9);\nSELECT json_array_insert('[1,2]','$[#-1]',9);\nSELECT json_array_insert('{\"a\":[1,2]}','$.a[1]',9);\nSELECT json_array_insert('[1,2]','$',9);\nSELECT json_array_insert('[1,2]','$[0]',9,'$[0]',8);\nSELECT json_array_insert('[1,2]','$[5]',9);\nSELECT json_array_insert(NULL,'$[0]',1), json_array_insert('[1]','$[0]',NULL);\nSELECT hex(jsonb_array_insert('[1]','$[0]',7));",
        expect: Agrees,
    },
    Case {
        name: "json.subtype",
        kind: "json",
        script: "SELECT subtype(json('[1]')), subtype(json_array(1)), subtype(json_object('a',1)), subtype(json_quote(1)), subtype(json_extract('[1]','$[0]')), subtype(json_extract('[[1]]','$[0]')), subtype(json_insert('[1]','$[1]',2)), subtype('[1]'), subtype(1), subtype(NULL), subtype(jsonb('[1]'));\nSELECT subtype(json_group_array(1)), subtype(json_patch('{}','{}')), subtype(json_remove('[1]','$[0]'));",
        expect: Agrees,
    },
    Case {
        name: "shell.help.topic",
        kind: "shell",
        script: ".help .mode",
        expect: Agrees,
    },
    Case {
        name: "introspect.completion",
        kind: "shell",
        script: "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT);\nSELECT phase, count(*) FROM completion('') GROUP BY phase ORDER BY phase;\nSELECT candidate FROM completion('sel');",
        expect: Agrees,
    },
    Case {
        name: "ext.sqlite.offset",
        kind: "ext",
        script: "CREATE TABLE t(id INTEGER PRIMARY KEY, a INTEGER, b TEXT);
INSERT INTO t VALUES (1,10,'p'),(2,20,'q'),(3,30,'r'),(4,20,'s'),(5,50,'t');
SELECT sqlite_offset(a) IS NOT NULL FROM t LIMIT 1;
SELECT id, sqlite_offset(a) > 0 FROM t ORDER BY id;
SELECT sqlite_offset(1);",
        expect: Agrees,
    },
    Case {
        name: "introspect.tables.used",
        kind: "shell",
        script: "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT);\nSELECT * FROM tables_used('SELECT * FROM t');",
        expect: Agrees,
    },
    // FTS5 update-in-place and the porter tokenizer, plus VACUUM's shell forms.
    Case {
        name: "fts5.update",
        kind: "fts5",
        script: "CREATE VIRTUAL TABLE f USING fts5(body, tag);\nINSERT INTO f(body, tag) VALUES('the quick brown fox','a'),('jumps over','b');\nUPDATE f SET body='the quick red fox' WHERE rowid=1;\nSELECT rowid, body, tag FROM f ORDER BY rowid;\nSELECT rowid FROM f WHERE f MATCH 'red';\nSELECT rowid FROM f WHERE f MATCH 'brown';",
        expect: Agrees,
    },
    Case {
        name: "fts5.porter",
        kind: "fts5",
        script: "CREATE VIRTUAL TABLE fp USING fts5(body, tokenize='porter unicode61');\nINSERT INTO fp(body) VALUES('running quickly');\nSELECT body FROM fp WHERE fp MATCH 'run';\nCREATE VIRTUAL TABLE fq USING fts5(body);\nINSERT INTO fq(body) VALUES('running quickly');\nSELECT body FROM fq WHERE fq MATCH 'run';\nSELECT body FROM fq WHERE fq MATCH 'running';",
        expect: Agrees,
    },
    Case {
        name: "vacuum.into",
        kind: "shell",
        script: "CREATE TABLE t(a);\nINSERT INTO t VALUES(1),(2);\nVACUUM;\nSELECT count(*) FROM t;\nVACUUM INTO 'copy.db';\nVACUUM INTO 'copy.db';",
        expect: Agrees,
    },
    Case {
        name: "vacuum.in.transaction",
        kind: "shell",
        script: "CREATE TABLE t(a);\nBEGIN;\nVACUUM;\nCOMMIT;\nSELECT 'after';",
        expect: Agrees,
    },
    Case {
        name: "shell.parameter",
        kind: "shell",
        script: ".parameter init\n.parameter set $x 42\n.parameter set @y 'hi'\n.parameter list\nSELECT $x, @y;\n.parameter unset $x\nSELECT $x;\n.parameter clear\n.parameter list",
        expect: Agrees,
    },
    Case {
        name: "shell.plan.tree",
        kind: "shell",
        script: "CREATE TABLE t(id INTEGER PRIMARY KEY, a);\nCREATE TABLE u(id INTEGER PRIMARY KEY, tid);\nCREATE INDEX ia ON t(a);\nEXPLAIN QUERY PLAN SELECT t.id, u.id FROM t JOIN u ON u.tid=t.id ORDER BY t.a;\nEXPLAIN QUERY PLAN SELECT id FROM t WHERE a = 1;",
        expect: Agrees,
    },
    // The pragmas. Every one of these answered nothing at all before - no
    // value and no error, which a caller cannot tell from an empty result.
    Case {
        name: "prag.user.version",
        kind: "pragma",
        script: "PRAGMA user_version;\nPRAGMA user_version = 12;\nPRAGMA user_version;",
        expect: Agrees,
    },
    Case {
        name: "prag.application.id",
        kind: "pragma",
        script: "PRAGMA application_id;\nPRAGMA application_id = 99;\nPRAGMA application_id;",
        expect: Agrees,
    },
    Case {
        name: "prag.schema.version",
        kind: "pragma",
        script: "PRAGMA schema_version;\nCREATE TABLE t(a);\nPRAGMA schema_version;\nCREATE INDEX ia ON t(a);\nPRAGMA schema_version;\nPRAGMA data_version;",
        expect: Agrees,
    },
    // The cookie moves once per schema change and not once per catalog
    // refresh, so a statement that changes nothing leaves it where it was.
    Case {
        name: "prag.schema.version.steady",
        kind: "pragma",
        script: "CREATE TABLE t(a);\nINSERT INTO t VALUES (1);\nSELECT count(*) FROM t;\nPRAGMA schema_version;",
        expect: Agrees,
    },
    Case {
        name: "prag.table.info.view",
        kind: "pragma",
        script: "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT, c INTEGER GENERATED ALWAYS AS (a*2) VIRTUAL, d TEXT COLLATE NOCASE);
CREATE INDEX it ON t(b DESC, d COLLATE NOCASE);
CREATE VIEW v AS SELECT a AS q, b AS r FROM t;
PRAGMA table_info(v);",
        expect: Agrees,
    },
    Case {
        name: "prag.table.xinfo.generated",
        kind: "pragma",
        script: "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT, c INTEGER GENERATED ALWAYS AS (a*2) VIRTUAL, d TEXT COLLATE NOCASE);
CREATE INDEX it ON t(b DESC, d COLLATE NOCASE);
CREATE VIEW v AS SELECT a AS q, b AS r FROM t;
PRAGMA table_xinfo(t);",
        expect: Agrees,
    },
    // The plain form hides the generated column *and renumbers* what is left,
    // which is the part that was wrong once the two forms were separated.
    Case {
        name: "prag.table.info.renumbered",
        kind: "pragma",
        script: "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT, c INTEGER GENERATED ALWAYS AS (a*2) VIRTUAL, d TEXT COLLATE NOCASE);
CREATE INDEX it ON t(b DESC, d COLLATE NOCASE);
CREATE VIEW v AS SELECT a AS q, b AS r FROM t;
PRAGMA table_info(t);",
        expect: Agrees,
    },
    Case {
        name: "prag.index.xinfo",
        kind: "pragma",
        script: "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT, c INTEGER GENERATED ALWAYS AS (a*2) VIRTUAL, d TEXT COLLATE NOCASE);
CREATE INDEX it ON t(b DESC, d COLLATE NOCASE);
CREATE VIEW v AS SELECT a AS q, b AS r FROM t;
PRAGMA index_xinfo(it);\nPRAGMA index_info(it);",
        expect: Agrees,
    },
    Case {
        name: "prag.table.list.view",
        kind: "pragma",
        script: "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT, c INTEGER GENERATED ALWAYS AS (a*2) VIRTUAL, d TEXT COLLATE NOCASE);
CREATE INDEX it ON t(b DESC, d COLLATE NOCASE);
CREATE VIEW v AS SELECT a AS q, b AS r FROM t;
PRAGMA table_list;",
        expect: Agrees,
    },
    Case {
        name: "prag.max.page.count",
        kind: "pragma",
        script: "PRAGMA max_page_count;\nPRAGMA max_page_count=1000;\nPRAGMA max_page_count;",
        expect: Agrees,
    },
    Case {
        name: "prag.query.only",
        kind: "pragma",
        script: "CREATE TABLE t(a);\nPRAGMA query_only;\nPRAGMA query_only=ON;\nPRAGMA query_only;\nINSERT INTO t VALUES (1);",
        expect: Agrees,
    },
    Case {
        name: "prag.recursive.triggers.value",
        kind: "pragma",
        script: "PRAGMA recursive_triggers;\nPRAGMA recursive_triggers=ON;\nPRAGMA recursive_triggers;",
        expect: Agrees,
    },
    Case {
        name: "prag.introspection",
        kind: "pragma",
        script: "SELECT count(*)>0 FROM pragma_pragma_list;\nSELECT count(*)>0 FROM pragma_function_list;\nSELECT count(*)>0 FROM pragma_module_list;\nSELECT count(*)>0 FROM pragma_compile_options;",
        expect: Agrees,
    },
    // Free means a page the file has and nothing is using. It used to mean
    // every bit the free map had room for, which is six figures on a five-page
    // database and does not move when rows are deleted.
    Case {
        name: "prag.freelist.count",
        kind: "pragma",
        script: "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT);\nINSERT INTO t VALUES (1,'x'),(2,'y');\nPRAGMA freelist_count;\nDELETE FROM t;\nPRAGMA freelist_count;",
        expect: Agrees,
    },
    // A pragma neither engine has heard of is silent in both, which is the
    // parity the refusal rule must not break.
    Case {
        name: "prag.unknown",
        kind: "pragma",
        script: "PRAGMA nonesuch;\nPRAGMA nonesuch = 4;\nSELECT 1;",
        expect: Agrees,
    },
    // `USING` and `NATURAL` coalesce the named column, so the join has one
    // `k` and an unqualified reference to it is not ambiguous. All five of
    // these answered `ambiguous column name: k` on the ORDER BY.
    Case {
        name: "join.using",
        kind: "join",
        script: "CREATE TABLE a(k INTEGER, x TEXT);
CREATE TABLE b(k INTEGER, y TEXT);
INSERT INTO a VALUES (1,'p'),(2,'q');
INSERT INTO b VALUES (1,'m'),(3,'n');
SELECT * FROM a JOIN b USING (k) ORDER BY k;",
        expect: Agrees,
    },
    Case {
        name: "join.left.using",
        kind: "join",
        script: "CREATE TABLE a(k INTEGER, x TEXT);
CREATE TABLE b(k INTEGER, y TEXT);
INSERT INTO a VALUES (1,'p'),(2,'q');
INSERT INTO b VALUES (1,'m'),(3,'n');
SELECT k, x, y FROM a LEFT JOIN b USING (k) ORDER BY k;",
        expect: Agrees,
    },
    Case {
        name: "join.natural",
        kind: "join",
        script: "CREATE TABLE a(k INTEGER, x TEXT);
CREATE TABLE b(k INTEGER, y TEXT);
INSERT INTO a VALUES (1,'p'),(2,'q');
INSERT INTO b VALUES (1,'m'),(3,'n');
SELECT * FROM a NATURAL JOIN b ORDER BY k;",
        expect: Agrees,
    },
    Case {
        name: "join.natural.left",
        kind: "join",
        script: "CREATE TABLE a(k INTEGER, x TEXT);
CREATE TABLE b(k INTEGER, y TEXT);
INSERT INTO a VALUES (1,'p'),(2,'q');
INSERT INTO b VALUES (1,'m'),(3,'n');
SELECT * FROM a NATURAL LEFT JOIN b ORDER BY k;",
        expect: Agrees,
    },
    Case {
        name: "join.using.chain",
        kind: "join",
        script: "CREATE TABLE a(k INTEGER, x TEXT);
CREATE TABLE b(k INTEGER, y TEXT);
INSERT INTO a VALUES (1,'p'),(2,'q');
INSERT INTO b VALUES (1,'m'),(3,'n');
CREATE TABLE c(k INTEGER, z TEXT);
INSERT INTO c VALUES (1,'r'),(2,'s');
SELECT * FROM a JOIN b USING (k) JOIN c USING (k) ORDER BY k;",
        expect: Agrees,
    },
    // The right-hand copy is suppressed from an *unqualified* reference only; a
    // qualified one still reaches it, and in a LEFT JOIN it is NULL where the
    // coalesced column carries the left value.
    Case {
        name: "join.using.qualified",
        kind: "join",
        script: "CREATE TABLE a(k INTEGER, x TEXT);
CREATE TABLE b(k INTEGER, y TEXT);
INSERT INTO a VALUES (1,'p'),(2,'q');
INSERT INTO b VALUES (1,'m'),(3,'n');
SELECT k, a.k, b.k FROM a LEFT JOIN b USING (k) ORDER BY k;",
        expect: Agrees,
    },
    // A self join whose inner term carries a rowid bound. The planner used to
    // choose a rowid range for it and the physical pass then refused the plan.
    Case {
        name: "join.self",
        kind: "join",
        script: "CREATE TABLE t(id INTEGER PRIMARY KEY, a INTEGER);
INSERT INTO t VALUES (1,5),(2,5),(3,7),(4,7);
SELECT x.id, y.id FROM t x JOIN t y ON y.a = x.a AND y.id > x.id ORDER BY x.id;",
        expect: Agrees,
    },
    Case {
        name: "select.basic",
        kind: "read",
        script: "CREATE TABLE t(a INTEGER, b TEXT);\nINSERT INTO t VALUES (1,'x'),(2,'y');\nSELECT * FROM t ORDER BY a;",
        expect: Agrees,
    },
    Case {
        name: "select.where",
        kind: "read",
        script: "CREATE TABLE t(a INTEGER, b TEXT);\nINSERT INTO t VALUES (1,'x'),(2,'y'),(3,'z');\nSELECT b FROM t WHERE a > 1 ORDER BY a;",
        expect: Agrees,
    },
    Case {
        name: "select.group",
        kind: "read",
        script: "CREATE TABLE t(a INTEGER, b TEXT);\nINSERT INTO t VALUES (1,'x'),(1,'y'),(2,'z');\nSELECT a, count(*), group_concat(b,'-') FROM t GROUP BY a ORDER BY a;",
        expect: Agrees,
    },
    Case {
        name: "select.join",
        kind: "read",
        script: "CREATE TABLE t(a INTEGER PRIMARY KEY, v TEXT);\nCREATE TABLE u(a INTEGER, w TEXT);\nINSERT INTO t VALUES (1,'x'),(2,'y');\nINSERT INTO u VALUES (1,'p'),(3,'q');\nSELECT t.a, t.v, u.w FROM t LEFT JOIN u ON u.a = t.a ORDER BY t.a;",
        expect: Agrees,
    },
    Case {
        name: "select.subquery",
        kind: "read",
        script: "CREATE TABLE t(a INTEGER);\nINSERT INTO t VALUES (1),(2),(3);\nSELECT a FROM t WHERE a IN (SELECT a FROM t WHERE a > 1) ORDER BY a;",
        expect: Agrees,
    },
    Case {
        name: "select.cte",
        kind: "read",
        script: "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM c WHERE n < 5) SELECT sum(n) FROM c;",
        expect: Agrees,
    },
    // `select.window` was retired here and is back (task-1932, H1). It was
    // removed because the shipping engine refused every window function -
    // but the refusal came from `compiled::try_compile`, which checked
    // `plan.compounds` and not `plan.select.windows`, so the cached path that
    // every application entry point uses never reached the evaluator that
    // answers them. One `Ok(None)` reconnects the two.
    Case {
        name: "select.window",
        kind: "read",
        script: "CREATE TABLE t(a INTEGER);
INSERT INTO t VALUES (3),(1),(2);
SELECT a, row_number() OVER (ORDER BY a) FROM t ORDER BY a;",
        expect: Agrees,
    },
    Case {
        name: "select.distinct",
        kind: "read",
        script: "CREATE TABLE t(a INTEGER);\nINSERT INTO t VALUES (1),(1),(2);\nSELECT DISTINCT a FROM t ORDER BY a;",
        expect: Agrees,
    },
    Case {
        name: "select.union",
        kind: "read",
        script: "CREATE TABLE t(a);\nCREATE TABLE u(a);\nINSERT INTO t VALUES (1),(2);\nINSERT INTO u VALUES (2),(3);\nSELECT a FROM t UNION SELECT a FROM u ORDER BY a;",
        expect: Agrees,
    },
    Case {
        name: "select.limit",
        kind: "read",
        script: "CREATE TABLE t(a INTEGER);\nINSERT INTO t VALUES (1),(2),(3),(4);\nSELECT a FROM t ORDER BY a LIMIT 2 OFFSET 1;",
        expect: Agrees,
    },
    Case {
        name: "select.view",
        kind: "read",
        script: "CREATE TABLE t(a INTEGER);\nINSERT INTO t VALUES (1),(2);\nCREATE VIEW v AS SELECT a*10 AS b FROM t;\nSELECT b FROM v ORDER BY b;",
        expect: Agrees,
    },
    Case {
        name: "select.collate",
        kind: "read",
        script: "CREATE TABLE t(a TEXT);\nINSERT INTO t VALUES ('B'),('a');\nSELECT a FROM t ORDER BY a COLLATE NOCASE;",
        expect: Agrees,
    },
    Case {
        name: "select.null.order",
        kind: "read",
        script: "CREATE TABLE t(a INTEGER);\nINSERT INTO t VALUES (2),(NULL),(1);\nSELECT ifnull(a,-1) FROM t ORDER BY a;",
        expect: Agrees,
    },
    Case {
        name: "write.update",
        kind: "write",
        script: "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT);\nINSERT INTO t VALUES (1,'x'),(2,'y');\nUPDATE t SET b = 'z' WHERE a = 2;\nSELECT * FROM t ORDER BY a;",
        expect: Agrees,
    },
    Case {
        name: "write.delete",
        kind: "write",
        script: "CREATE TABLE t(a INTEGER);\nINSERT INTO t VALUES (1),(2),(3);\nDELETE FROM t WHERE a = 2;\nSELECT a FROM t ORDER BY a;",
        expect: Agrees,
    },
    Case {
        name: "write.upsert",
        kind: "write",
        script: "CREATE TABLE t(a INTEGER PRIMARY KEY, n INTEGER);\nINSERT INTO t VALUES (1,1);\nINSERT INTO t VALUES (1,5) ON CONFLICT(a) DO UPDATE SET n = n + excluded.n;\nSELECT * FROM t;",
        expect: Agrees,
    },
    Case {
        name: "write.replace",
        kind: "write",
        script: "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT);\nINSERT INTO t VALUES (1,'x');\nINSERT OR REPLACE INTO t VALUES (1,'y');\nSELECT * FROM t;",
        expect: Agrees,
    },
    Case {
        name: "write.returning",
        kind: "write",
        script: "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT);\nINSERT INTO t VALUES (1,'x') RETURNING a, b;",
        expect: Agrees,
    },
    Case {
        name: "write.insert.select",
        kind: "write",
        script: "CREATE TABLE t(a INTEGER);\nCREATE TABLE u(a INTEGER);\nINSERT INTO t VALUES (1),(2);\nINSERT INTO u SELECT a*3 FROM t;\nSELECT a FROM u ORDER BY a;",
        expect: Agrees,
    },
    Case {
        name: "constraint.notnull",
        kind: "constraint",
        script: "CREATE TABLE t(a INTEGER NOT NULL);\nINSERT INTO t VALUES (NULL);\nSELECT count(*) FROM t;",
        expect: Agrees,
    },
    Case {
        name: "constraint.unique",
        kind: "constraint",
        script: "CREATE TABLE t(a INTEGER UNIQUE);\nINSERT INTO t VALUES (1);\nINSERT INTO t VALUES (1);\nSELECT count(*) FROM t;",
        expect: Agrees,
    },
    Case {
        name: "constraint.primarykey",
        kind: "constraint",
        script: "CREATE TABLE t(a INTEGER PRIMARY KEY);\nINSERT INTO t VALUES (1);\nINSERT INTO t VALUES (1);\nSELECT count(*) FROM t;",
        expect: Agrees,
    },
    // An `UPDATE` onto another row's key in a secondary `UNIQUE` index was
    // performed and answered success, leaving the index with two entries
    // under one key: the first case is the original bug report's own script.
    // The second is the other half of the same defect - the row must not
    // collide with *itself*, and moving the rowid moves an index entry whose
    // key did not change, which was already being refused. The rest are the paths a
    // check that only looked at the table's key never reached.
    Case {
        name: "constraint.unique.update",
        kind: "constraint",
        script: "CREATE TABLE t(a TEXT, b INTEGER);\nCREATE UNIQUE INDEX u ON t(a);\nINSERT INTO t VALUES ('x',1),('y',2);\nUPDATE t SET a='x' WHERE b=2;\nSELECT a,b FROM t ORDER BY b;\nSELECT count(*) FROM t WHERE a='x';",
        expect: Agrees,
    },
    Case {
        name: "constraint.unique.update.self",
        kind: "constraint",
        script: "CREATE TABLE t(id INTEGER PRIMARY KEY, a TEXT);\nCREATE UNIQUE INDEX u ON t(a);\nINSERT INTO t VALUES (1,'x'),(2,'y');\nUPDATE t SET id=5 WHERE id=2;\nSELECT id,a FROM t ORDER BY id;\nSELECT id FROM t WHERE a='y';",
        expect: Agrees,
    },
    Case {
        name: "constraint.unique.update.replace",
        kind: "constraint",
        script: "CREATE TABLE t(a TEXT, b TEXT, c INTEGER);\nCREATE UNIQUE INDEX u1 ON t(a);\nCREATE UNIQUE INDEX u2 ON t(b);\nINSERT INTO t VALUES ('x','p',1),('y','q',2),('z','r',3);\nUPDATE OR REPLACE t SET a='x', b='q' WHERE c=3;\nSELECT a,b,c FROM t ORDER BY c;",
        expect: Agrees,
    },
    Case {
        name: "constraint.unique.update.ignore",
        kind: "constraint",
        script: "CREATE TABLE t(a INTEGER, b INTEGER);\nCREATE UNIQUE INDEX u ON t(a);\nINSERT INTO t VALUES (1,1),(2,2),(3,3);\nUPDATE OR IGNORE t SET a=a+1;\nSELECT a,b FROM t ORDER BY b;",
        expect: Agrees,
    },
    Case {
        name: "constraint.unique.upsert.arm",
        kind: "constraint",
        script: "CREATE TABLE t(a TEXT, b TEXT, c INTEGER);\nCREATE UNIQUE INDEX u1 ON t(a);\nCREATE UNIQUE INDEX u2 ON t(b);\nINSERT INTO t VALUES ('x','p',1),('y','q',2);\nINSERT INTO t VALUES ('y','z',5) ON CONFLICT(a) DO UPDATE SET b='p';\nSELECT a,b,c FROM t ORDER BY c;",
        expect: Agrees,
    },
    Case {
        name: "constraint.unique.newest.named",
        kind: "constraint",
        script: "CREATE TABLE t(a TEXT, b TEXT, c INTEGER);\nCREATE UNIQUE INDEX u1 ON t(a);\nCREATE UNIQUE INDEX u2 ON t(b);\nINSERT INTO t VALUES ('x','p',1),('z','r',3);\nUPDATE t SET a='x', b='p' WHERE c=3;\nINSERT INTO t VALUES ('x','p',9);",
        expect: Agrees,
    },
    Case {
        name: "constraint.without.rowid.key",
        kind: "constraint",
        script: "CREATE TABLE t(a TEXT, b TEXT, c INTEGER, PRIMARY KEY(a,b)) WITHOUT ROWID;\nINSERT INTO t VALUES ('x','1',1),('y','2',2);\nINSERT INTO t VALUES ('x','1',3);\nUPDATE t SET a='x', b='1' WHERE c=2;\nSELECT a,b,c FROM t ORDER BY c;",
        expect: Agrees,
    },
    Case {
        name: "constraint.foreignkey",
        kind: "constraint",
        script: "PRAGMA foreign_keys = ON;\nCREATE TABLE p(id INTEGER PRIMARY KEY);\nCREATE TABLE c(id INTEGER, p INTEGER REFERENCES p(id));\nINSERT INTO c VALUES (1, 99);\nSELECT count(*) FROM c;",
        expect: Agrees,
    },
    Case {
        name: "check.column",
        kind: "constraint",
        script: "CREATE TABLE t(a INTEGER, b INTEGER CHECK (b > 0));\nINSERT INTO t VALUES (1,-5);\nSELECT * FROM t;",
        expect: Agrees,
    },
    Case {
        name: "check.table",
        kind: "constraint",
        script: "CREATE TABLE t(a INTEGER, b INTEGER, CHECK (a < b));\nINSERT INTO t VALUES (9,1);\nSELECT * FROM t;",
        expect: Agrees,
    },
    Case {
        name: "check.update",
        kind: "constraint",
        script: "CREATE TABLE t(a INTEGER CHECK (a > 0));\nINSERT INTO t VALUES (5);\nUPDATE t SET a = -1;\nSELECT * FROM t;",
        expect: Agrees,
    },
    Case {
        name: "check.named",
        kind: "constraint",
        script: "CREATE TABLE t(a INTEGER, CONSTRAINT positive CHECK (a > 0));\nINSERT INTO t VALUES (-1);\nSELECT count(*) FROM t;",
        expect: Agrees,
    },
    Case {
        name: "check.null.passes",
        kind: "constraint",
        script: "CREATE TABLE t(a INTEGER CHECK (a > 0));\nINSERT INTO t VALUES (NULL);\nSELECT typeof(a) FROM t;",
        expect: Agrees,
    },
    Case {
        name: "check.or.ignore",
        kind: "constraint",
        script: "CREATE TABLE t(a INTEGER CHECK (a > 0));\nINSERT OR IGNORE INTO t VALUES (-1),(5);\nSELECT a FROM t;",
        expect: Agrees,
    },
    Case {
        name: "check.order.notnull.first",
        kind: "constraint",
        script: "CREATE TABLE t(a INTEGER NOT NULL CHECK (a > 0));\nINSERT INTO t(a) VALUES (NULL);\nSELECT count(*) FROM t;",
        expect: Agrees,
    },
    Case {
        name: "check.order.before.unique",
        kind: "constraint",
        script: "CREATE TABLE t(a INTEGER UNIQUE, b INTEGER CHECK (b > 0));\nINSERT INTO t VALUES (1,1);\nINSERT INTO t VALUES (1,-1);\nSELECT count(*) FROM t;",
        expect: Agrees,
    },
    Case {
        name: "strict.int",
        kind: "constraint",
        script: "CREATE TABLE s(x INTEGER) STRICT;\nINSERT INTO s VALUES ('abc');\nSELECT typeof(x) FROM s;",
        expect: Agrees,
    },
    Case {
        name: "strict.text",
        kind: "constraint",
        script: "CREATE TABLE s(x TEXT) STRICT;\nINSERT INTO s VALUES (x'00');\nSELECT count(*) FROM s;",
        expect: Agrees,
    },
    Case {
        name: "strict.real",
        kind: "constraint",
        script: "CREATE TABLE s(x REAL) STRICT;\nINSERT INTO s VALUES (3);\nSELECT typeof(x), x FROM s;",
        expect: Agrees,
    },
    Case {
        name: "strict.any",
        kind: "constraint",
        script: "CREATE TABLE s(x ANY) STRICT;\nINSERT INTO s VALUES ('a'),(1),(1.5),(x'01');\nSELECT typeof(x) FROM s;",
        expect: Agrees,
    },
    Case {
        name: "strict.int.from.real",
        kind: "constraint",
        script: "CREATE TABLE s(x INT) STRICT;\nINSERT INTO s VALUES (3.0);\nSELECT typeof(x), x FROM s;\nINSERT INTO s VALUES (1.5);",
        expect: Agrees,
    },
    Case {
        name: "strict.update",
        kind: "constraint",
        script: "CREATE TABLE s(x INT) STRICT;\nINSERT INTO s VALUES (1);\nUPDATE s SET x = 'q';\nSELECT x FROM s;",
        expect: Agrees,
    },
    Case {
        name: "strict.notnull.first",
        kind: "constraint",
        script: "CREATE TABLE s(a INT, b TEXT NOT NULL) STRICT;\nINSERT INTO s(a) VALUES ('x');\nSELECT count(*) FROM s;",
        expect: Agrees,
    },
    Case {
        name: "strict.declared.name",
        kind: "constraint",
        script: "CREATE TABLE s(a INT) STRICT;\nINSERT INTO s VALUES ('x');\nSELECT count(*) FROM s;",
        expect: Agrees,
    },
    Case {
        name: "affinity.int",
        kind: "affinity",
        script: "CREATE TABLE t(a INTEGER);\nINSERT INTO t VALUES ('42');\nSELECT typeof(a), a FROM t;",
        expect: Agrees,
    },
    Case {
        name: "affinity.text",
        kind: "affinity",
        script: "CREATE TABLE t(a TEXT);\nINSERT INTO t VALUES (42);\nSELECT typeof(a), a FROM t;",
        expect: Agrees,
    },
    Case {
        name: "affinity.real",
        kind: "affinity",
        script: "CREATE TABLE t(a REAL);\nINSERT INTO t VALUES (1);\nSELECT typeof(a), a FROM t;",
        expect: Agrees,
    },
    Case {
        name: "affinity.numeric",
        kind: "affinity",
        script: "CREATE TABLE t(a NUMERIC);\nINSERT INTO t VALUES ('12'),('12.5'),('abc'),(x'01');\nSELECT typeof(a), a FROM t;",
        expect: Agrees,
    },
    Case {
        name: "affinity.blob.column",
        kind: "affinity",
        script: "CREATE TABLE t(a);\nINSERT INTO t VALUES ('12'),(12),(12.0);\nSELECT typeof(a), a FROM t;",
        expect: Agrees,
    },
    Case {
        name: "affinity.blob.declared",
        kind: "affinity",
        script: "CREATE TABLE t(a BLOB);\nINSERT INTO t VALUES ('12');\nSELECT typeof(a), a FROM t;",
        expect: Agrees,
    },
    Case {
        name: "affinity.text.leaves.blob",
        kind: "affinity",
        script: "CREATE TABLE t(a TEXT);\nINSERT INTO t VALUES (x'4142');\nSELECT typeof(a) FROM t;",
        expect: Agrees,
    },
    Case {
        name: "affinity.nonnumeric.text",
        kind: "affinity",
        script: "CREATE TABLE t(a INTEGER);\nINSERT INTO t VALUES ('12abc');\nSELECT typeof(a), a FROM t;",
        expect: Agrees,
    },
    Case {
        name: "affinity.update",
        kind: "affinity",
        script: "CREATE TABLE t(a INTEGER);\nINSERT INTO t VALUES (1);\nUPDATE t SET a = '77';\nSELECT typeof(a), a FROM t;",
        expect: Agrees,
    },
    Case {
        name: "affinity.order",
        kind: "affinity",
        script: "CREATE TABLE t(a INTEGER);\nCREATE INDEX ix ON t(a);\nINSERT INTO t VALUES ('42'),(5),('abc');\nSELECT typeof(a), a FROM t ORDER BY a;",
        expect: Agrees,
    },
    Case {
        name: "affinity.where",
        kind: "affinity",
        script: "CREATE TABLE t(a INTEGER);\nINSERT INTO t VALUES ('42');\nSELECT count(*) FROM t WHERE a = 42;",
        expect: Agrees,
    },
    Case {
        name: "affinity.rowid.alias",
        kind: "affinity",
        script: "CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT);\nINSERT INTO t VALUES ('42','x');\nSELECT typeof(id), id FROM t;",
        expect: Agrees,
    },
    Case {
        name: "affinity.real.big.int",
        kind: "affinity",
        script: "CREATE TABLE t(a REAL);\nINSERT INTO t VALUES (9007199254740993);\nSELECT typeof(a), a FROM t;",
        expect: Agrees,
    },
    Case {
        name: "affinity.insert.select",
        kind: "affinity",
        script: "CREATE TABLE t(a TEXT);\nCREATE TABLE u(a INTEGER);\nINSERT INTO t VALUES ('7');\nINSERT INTO u SELECT a FROM t;\nSELECT typeof(a), a FROM u;",
        expect: Agrees,
    },
    Case {
        name: "ctas",
        kind: "surface",
        script: "CREATE TABLE t(a);\nINSERT INTO t VALUES (1),(2);\nCREATE TABLE u AS SELECT a*2 AS b FROM t;\nSELECT * FROM u;",
        expect: Agrees,
    },
    Case {
        name: "update.from",
        kind: "surface",
        script: "CREATE TABLE t(a INTEGER PRIMARY KEY, v INTEGER);\nCREATE TABLE s(a INTEGER, v INTEGER);\nINSERT INTO t VALUES (1,0),(2,0);\nINSERT INTO s VALUES (1,9),(2,8);\nUPDATE t SET v = s.v FROM s WHERE s.a = t.a;\nSELECT * FROM t;",
        expect: Agrees,
    },
    Case {
        name: "with.dml",
        kind: "surface",
        script: "CREATE TABLE t(a);\nWITH q(x) AS (VALUES (1),(2)) INSERT INTO t SELECT x FROM q;\nSELECT * FROM t;",
        expect: Agrees,
    },
    Case {
        name: "with.delete",
        kind: "surface",
        script: "CREATE TABLE t(a);\nINSERT INTO t VALUES (1),(2),(3);\nWITH q(x) AS (VALUES (2)) DELETE FROM t WHERE a IN (SELECT x FROM q);\nSELECT a FROM t ORDER BY a;",
        expect: Agrees,
    },
    Case {
        name: "index.partial",
        kind: "surface",
        script: "CREATE TABLE t(a,b);\nCREATE INDEX ix ON t(a) WHERE b > 5;\nINSERT INTO t VALUES (1,9),(2,1);\nSELECT a FROM t WHERE a=1 AND b>5;",
        expect: Agrees,
    },
    Case {
        name: "index.expr",
        kind: "surface",
        script: "CREATE TABLE t(a TEXT);\nCREATE INDEX ix ON t(lower(a));\nINSERT INTO t VALUES ('AB');\nSELECT a FROM t WHERE lower(a)='ab';",
        expect: Agrees,
    },
    Case {
        name: "rowvalue",
        kind: "surface",
        script: "CREATE TABLE t(a,b);\nINSERT INTO t VALUES (1,2);\nSELECT * FROM t WHERE (a,b) = (1,2);",
        expect: Agrees,
    },
    Case {
        name: "rowvalue.in",
        kind: "surface",
        script: "CREATE TABLE t(a,b);\nINSERT INTO t VALUES (1,2),(3,4);\nSELECT a FROM t WHERE (a,b) IN (VALUES (1,2)) ORDER BY a;",
        expect: Agrees,
    },
    Case {
        name: "without.rowid.index",
        kind: "surface",
        script: "CREATE TABLE t(a TEXT PRIMARY KEY, b) WITHOUT ROWID;\nCREATE INDEX ix ON t(b);\nINSERT INTO t VALUES ('k',5);\nSELECT * FROM t WHERE b=5;",
        expect: Agrees,
    },
    Case {
        name: "index.after.writes",
        kind: "surface",
        script: "CREATE TABLE t(a INTEGER, b TEXT);\nINSERT INTO t VALUES (1,'a'),(2,'b'),(3,'c'),(4,'d'),(5,'e');\nUPDATE t SET b='B' WHERE a=2;\nDELETE FROM t WHERE a=4;\nINSERT INTO t VALUES (6,'f');\nCREATE INDEX ix ON t(b);\nSELECT b, a FROM t ORDER BY b;\nSELECT count(*) FROM t;\nSELECT a FROM t WHERE b='B';",
        expect: Agrees,
    },
    Case {
        name: "index.after.writes.without.rowid",
        kind: "surface",
        script: "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT) WITHOUT ROWID;\nINSERT INTO t VALUES (1,'a'),(2,'b'),(3,'c'),(4,'d'),(5,'e');\nUPDATE t SET b='B' WHERE a=2;\nDELETE FROM t WHERE a=4;\nINSERT INTO t VALUES (6,'f');\nCREATE INDEX ix ON t(b);\nSELECT b, a FROM t ORDER BY b;\nSELECT a FROM t WHERE b='B';",
        expect: Agrees,
    },
    Case {
        name: "index.partial.unique.crossing.into",
        kind: "surface",
        script: "CREATE TABLE t(a TEXT, b INTEGER);\nCREATE UNIQUE INDEX up ON t(a) WHERE b > 5;\nINSERT INTO t VALUES ('x', 9), ('x', 1);\nUPDATE t SET b = 7 WHERE b = 1;\nSELECT a, b FROM t ORDER BY b;",
        expect: Agrees,
    },
    Case {
        name: "index.partial.unique.crossing.outof",
        kind: "surface",
        script: "CREATE TABLE t(a TEXT, b INTEGER);\nCREATE UNIQUE INDEX up ON t(a) WHERE b > 5;\nINSERT INTO t VALUES ('x', 9), ('y', 1);\nUPDATE t SET b = 2 WHERE b = 9;\nSELECT a, b FROM t ORDER BY a;",
        expect: Agrees,
    },
    Case {
        name: "index.partial.unique.crossing.staying",
        kind: "surface",
        script: "CREATE TABLE t(a TEXT, b INTEGER, c TEXT);\nCREATE UNIQUE INDEX up ON t(a) WHERE b > 5;\nINSERT INTO t VALUES ('x', 9, 'first'), ('y', 7, 'second');\nUPDATE t SET c = 'changed' WHERE a = 'x';\nUPDATE t SET b = 8 WHERE a = 'x';\nSELECT a, b, c FROM t ORDER BY a;",
        expect: Agrees,
    },
    Case {
        name: "index.partial.maintenance.crossing",
        kind: "surface",
        script: "CREATE TABLE t(a INTEGER, b INTEGER);\nCREATE INDEX ixp ON t(a) WHERE b > 5;\nINSERT INTO t VALUES (1,9),(2,1),(3,7);\nUPDATE t SET b = 20 WHERE a = 2;\nSELECT 'in', a, b FROM t WHERE a>0 AND b>5 ORDER BY a;\nUPDATE t SET b = 0 WHERE a = 1;\nSELECT 'out', a, b FROM t WHERE a>0 AND b>5 ORDER BY a;\nUPDATE t SET a = 9 WHERE a = 3;\nSELECT 'moved', a, b FROM t WHERE a>0 AND b>5 ORDER BY a;\nDELETE FROM t WHERE a = 2;\nSELECT 'left', a, b FROM t WHERE a>0 AND b>5 ORDER BY a;",
        expect: Agrees,
    },
    Case {
        name: "index.partial.unique.two.indexes",
        kind: "surface",
        script: "CREATE TABLE t(a TEXT, b TEXT, n INTEGER);\nCREATE UNIQUE INDEX ua ON t(a);\nCREATE UNIQUE INDEX ub ON t(b) WHERE n > 5;\nINSERT INTO t VALUES ('a1','b1',9);\nINSERT INTO t VALUES ('a2','b1',1);\nINSERT INTO t VALUES ('a3','b1',7);\nINSERT INTO t VALUES ('a1','b9',1);\nUPDATE t SET n = 8 WHERE a = 'a2';\nSELECT a, b, n FROM t ORDER BY a;",
        expect: Agrees,
    },
    Case {
        name: "pragma.tablevalued",
        kind: "surface",
        script: "CREATE TABLE t(a);\nSELECT name FROM pragma_table_info('t');",
        expect: Agrees,
    },
    Case {
        name: "instead.of",
        kind: "surface",
        script: "CREATE TABLE t(a);\nCREATE VIEW v AS SELECT a FROM t;\nCREATE TRIGGER tr INSTEAD OF INSERT ON v BEGIN INSERT INTO t VALUES (NEW.a); END;\nINSERT INTO v VALUES (3);\nSELECT * FROM t;",
        expect: Agrees,
    },
    Case {
        name: "instead.of.update",
        kind: "surface",
        script: "CREATE TABLE t(a);\nINSERT INTO t VALUES (1);\nCREATE VIEW v AS SELECT a FROM t;\nCREATE TRIGGER tr INSTEAD OF UPDATE ON v BEGIN UPDATE t SET a = NEW.a; END;\nUPDATE v SET a = 9;\nSELECT * FROM t;",
        expect: Agrees,
    },
    Case {
        name: "trigger.after",
        kind: "surface",
        script: "CREATE TABLE t(a);\nCREATE TABLE log(a);\nCREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO log VALUES (NEW.a); END;\nINSERT INTO t VALUES (4);\nSELECT * FROM log;",
        expect: Agrees,
    },
    Case {
        name: "alter.rename",
        kind: "surface",
        script: "CREATE TABLE t(a);\nINSERT INTO t VALUES (1);\nALTER TABLE t RENAME TO u;\nSELECT * FROM u;",
        expect: Agrees,
    },
    Case {
        name: "alter.addcolumn",
        kind: "surface",
        script: "CREATE TABLE t(a);\nINSERT INTO t VALUES (1);\nALTER TABLE t ADD COLUMN b DEFAULT 7;\nSELECT a, b FROM t;",
        expect: Agrees,
    },
    Case {
        name: "alter.drop.middle",
        kind: "surface",
        script: "CREATE TABLE t (a INTEGER, b INTEGER, c INTEGER, d INTEGER);\nINSERT INTO t VALUES (1,2,3,4),(10,20,30,40);\nALTER TABLE t DROP COLUMN b;\nSELECT a, c, d FROM t ORDER BY a;",
        expect: Agrees,
    },
    Case {
        name: "alter.drop.first",
        kind: "surface",
        script: "CREATE TABLE t (a INTEGER, b INTEGER, c INTEGER, d INTEGER);\nINSERT INTO t VALUES (1,2,3,4),(10,20,30,40);\nALTER TABLE t DROP COLUMN a;\nSELECT b, c, d FROM t ORDER BY b;",
        expect: Agrees,
    },
    Case {
        name: "alter.drop.last",
        kind: "surface",
        script: "CREATE TABLE t (a INTEGER, b INTEGER, c INTEGER, d INTEGER);\nINSERT INTO t VALUES (1,2,3,4),(10,20,30,40);\nALTER TABLE t DROP COLUMN d;\nSELECT a, b, c FROM t ORDER BY a;",
        expect: Agrees,
    },
    Case {
        name: "alter.drop.reopen",
        kind: "surface",
        script: "CREATE TABLE t (a INTEGER, b TEXT, c REAL, d BLOB);\nINSERT INTO t VALUES (1,'two',3.5,x'04'),(10,'twenty',30.5,x'28');\nALTER TABLE t DROP COLUMN b;\n.open probe.db\nSELECT a, c, d FROM t ORDER BY a;\nSELECT typeof(a), typeof(c), typeof(d) FROM t ORDER BY a;",
        expect: Agrees,
    },
    Case {
        name: "alter.drop.index",
        kind: "surface",
        script: "CREATE TABLE t (a INTEGER, b INTEGER, c INTEGER, d INTEGER);\nCREATE INDEX ixd ON t(d);\nCREATE INDEX ixca ON t(c,a);\nINSERT INTO t VALUES (1,2,3,4),(10,20,30,40),(100,200,300,400);\nALTER TABLE t DROP COLUMN b;\nSELECT d FROM t WHERE d = 40;\nSELECT c, a FROM t WHERE c = 300;\nSELECT a, c, d FROM t WHERE d > 4 ORDER BY d;\nPRAGMA integrity_check;",
        expect: Agrees,
    },
    Case {
        name: "alter.drop.withoutrowid",
        kind: "surface",
        script: "CREATE TABLE t (k TEXT PRIMARY KEY, a INTEGER, b INTEGER, c INTEGER) WITHOUT ROWID;\nINSERT INTO t VALUES ('x',10,20,30),('y',100,200,300);\nALTER TABLE t DROP COLUMN b;\nSELECT k, a, c FROM t ORDER BY k;\nSELECT * FROM t ORDER BY k;",
        expect: Agrees,
    },
    Case {
        name: "alter.add.default.wide",
        kind: "surface",
        script: "CREATE TABLE t (a INTEGER, b INTEGER, c INTEGER);\nINSERT INTO t VALUES (1,2,3),(10,20,30);\nALTER TABLE t ADD COLUMN d INTEGER DEFAULT 9;\nALTER TABLE t ADD COLUMN e TEXT DEFAULT 'x';\nSELECT a, b, c, d, e FROM t ORDER BY a;\nALTER TABLE t DROP COLUMN b;\nSELECT * FROM t ORDER BY a;",
        expect: Agrees,
    },
    // -----------------------------------------------------------------------
    // `ALTER TABLE` on a database that is not `main` (task-2061). Every one of
    // these was an error or a wrong answer before the fix; the section of the
    // module comment above says which fault each of them grades.
    Case {
        name: "alter.attach.add",
        kind: "surface",
        script: "ATTACH DATABASE 'side.db' AS side;
CREATE TABLE side.t (a,b);
INSERT INTO side.t VALUES (1,2);
ALTER TABLE side.t ADD COLUMN c DEFAULT 9;
SELECT * FROM side.t;",
        expect: Agrees,
    },
    Case {
        name: "alter.attach.drop",
        kind: "surface",
        script: "ATTACH DATABASE 'side.db' AS side;
CREATE TABLE side.t (a,b,c);
INSERT INTO side.t VALUES (1,2,3),(10,20,30);
ALTER TABLE side.t DROP COLUMN b;
SELECT * FROM side.t ORDER BY a;",
        expect: Agrees,
    },
    Case {
        name: "alter.attach.shadow",
        kind: "surface",
        script: "ATTACH DATABASE 'side.db' AS side;
CREATE TABLE t (a,b,c,d);
CREATE TABLE side.t (a,b,c,d);
INSERT INTO t VALUES (1,2,3,4);
INSERT INTO side.t VALUES (10,20,30,40);
ALTER TABLE side.t DROP COLUMN b;
SELECT 'main', * FROM main.t;
SELECT 'side', * FROM side.t;",
        expect: Agrees,
    },
    Case {
        name: "alter.attach.reopen",
        kind: "surface",
        script: "ATTACH DATABASE 'side.db' AS side;
CREATE TABLE side.t (a,b,c);
INSERT INTO side.t VALUES (1,2,3),(10,20,30);
ALTER TABLE side.t DROP COLUMN b;
ALTER TABLE side.t ADD COLUMN e DEFAULT 7;
.open side.db
SELECT * FROM t ORDER BY a;
SELECT sql FROM sqlite_master WHERE name = 't';",
        expect: Agrees,
    },
    Case {
        name: "alter.attach.index",
        kind: "surface",
        script: "ATTACH DATABASE 'side.db' AS side;
CREATE TABLE side.t (a,b,c,d);
CREATE INDEX side.ixd ON t(d);
INSERT INTO side.t VALUES (1,2,3,4),(10,20,30,40);
ALTER TABLE side.t DROP COLUMN b;
SELECT d FROM side.t WHERE d = 40;
SELECT a, c, d FROM side.t WHERE d > 4 ORDER BY d;",
        expect: Agrees,
    },
    Case {
        name: "alter.attach.withoutrowid",
        kind: "surface",
        script: "ATTACH DATABASE 'side.db' AS side;
CREATE TABLE side.t (k TEXT PRIMARY KEY, a, b, c) WITHOUT ROWID;
INSERT INTO side.t VALUES ('x',10,20,30),('y',100,200,300);
ALTER TABLE side.t DROP COLUMN b;
SELECT k, a, c FROM side.t ORDER BY k;
SELECT * FROM side.t ORDER BY k;",
        expect: Agrees,
    },
    Case {
        name: "alter.temp.actions",
        kind: "surface",
        script: "CREATE TEMP TABLE t (a,b);
INSERT INTO t VALUES (1,2);
ALTER TABLE t ADD COLUMN c DEFAULT 9;
SELECT * FROM t;
ALTER TABLE t DROP COLUMN b;
SELECT * FROM t;
ALTER TABLE t RENAME TO u;
SELECT * FROM u;",
        expect: Agrees,
    },
    Case {
        name: "alter.temp.qualified",
        kind: "surface",
        script: "CREATE TEMP TABLE t (a,b);
INSERT INTO t VALUES (1,2);
ALTER TABLE temp.t ADD COLUMN c DEFAULT 9;
SELECT * FROM temp.t;
ALTER TABLE temp.t DROP COLUMN b;
SELECT * FROM temp.t;",
        expect: Agrees,
    },
    Case {
        name: "alter.temp.shadow",
        kind: "surface",
        script: "CREATE TABLE t (a,b,c);
CREATE TEMP TABLE t (x,y);
INSERT INTO main.t VALUES (1,2,3);
INSERT INTO temp.t VALUES (10,20);
ALTER TABLE t ADD COLUMN z DEFAULT 7;
SELECT 'main', * FROM main.t;
SELECT 'temp', * FROM temp.t;
ALTER TABLE temp.t DROP COLUMN y;
SELECT 'temp2', * FROM temp.t;",
        expect: Agrees,
    },
    Case {
        name: "alter.unqualified.attached",
        kind: "surface",
        script: "ATTACH DATABASE 'side.db' AS side;
CREATE TABLE side.t (a,b,c);
INSERT INTO side.t VALUES (1,2,3);
ALTER TABLE t DROP COLUMN b;
SELECT * FROM side.t;
ALTER TABLE t ADD COLUMN d DEFAULT 4;
SELECT * FROM side.t;",
        expect: Agrees,
    },
    Case {
        name: "attach",
        kind: "surface",
        script: "ATTACH DATABASE 'side.db' AS side;\nCREATE TABLE side.t(a);\nINSERT INTO side.t VALUES (1);\nSELECT * FROM side.t;\nDETACH DATABASE side;",
        expect: Agrees,
    },
    Case {
        name: "temp.table",
        kind: "surface",
        script: "CREATE TEMP TABLE t(a);\nINSERT INTO t VALUES (5);\nSELECT * FROM t;",
        expect: Agrees,
    },
    Case {
        name: "temp.view",
        kind: "surface",
        script: "CREATE TABLE t(a);\nINSERT INTO t VALUES (2);\nCREATE TEMP VIEW v AS SELECT a*4 FROM t;\nSELECT * FROM v;",
        expect: Agrees,
    },
    Case {
        name: "json.ops",
        kind: "semantics",
        script: "SELECT json_extract('{\"a\":[1,2,3]}','$.a[1]'), json_array_length('[1,2,3]'), json_valid('{}');",
        expect: Agrees,
    },
    Case {
        name: "json.valid.forms",
        kind: "semantics",
        script: "SELECT json_valid('{}'), json_valid('[]'), json_valid('null'), json_valid('{\"a\":1}'), json_valid('nope'), json_valid('{');",
        expect: Agrees,
    },
    Case {
        name: "date.fns",
        kind: "semantics",
        script: "SELECT date('2024-02-29','+1 day'), strftime('%Y-%W','2024-03-01'), julianday('2000-01-01');",
        expect: Agrees,
    },
    Case {
        name: "date.weeks",
        kind: "semantics",
        script: "SELECT strftime('%W','2024-01-01'), strftime('%W','2024-03-01'), strftime('%W','2023-12-31'), strftime('%j','2024-03-01');",
        expect: Agrees,
    },
    Case {
        name: "string.fns",
        kind: "semantics",
        script: "SELECT printf('%05.2f|%s',3.14159,'x'), substr('hello',-3), replace('aaa','a','bb'), instr('abc','c');",
        expect: Agrees,
    },
    Case {
        name: "printf.widths",
        kind: "semantics",
        script: "SELECT printf('%05.2f',3.14159), printf('%8.3f',2.5), printf('%-6d|',42), printf('%+d',7), printf('%05d',42), printf('%x',255);",
        expect: Agrees,
    },
    Case {
        name: "math.fns",
        kind: "semantics",
        script: "SELECT abs(-3), round(2.567,2), max(1,9,3), min(1,9,3), length('abc'), typeof(1/2);",
        expect: Agrees,
    },
    Case {
        name: "autoincrement",
        kind: "semantics",
        script: "CREATE TABLE t(a INTEGER PRIMARY KEY AUTOINCREMENT, b);\nINSERT INTO t(b) VALUES (1);\nDELETE FROM t;\nINSERT INTO t(b) VALUES (2);\nSELECT a FROM t;",
        expect: Agrees,
    },
    Case {
        name: "autoincrement.sequence",
        kind: "semantics",
        script: "CREATE TABLE t(a INTEGER PRIMARY KEY AUTOINCREMENT, b);\nINSERT INTO t(b) VALUES (1),(2),(3);\nDELETE FROM t WHERE a = 3;\nSELECT seq FROM sqlite_sequence WHERE name = 't';",
        expect: Agrees,
    },
    Case {
        name: "autoincrement.rollback",
        kind: "semantics",
        script: "CREATE TABLE t(a INTEGER PRIMARY KEY AUTOINCREMENT, b);\nINSERT INTO t(b) VALUES (1);\nBEGIN;\nINSERT INTO t(b) VALUES (2);\nROLLBACK;\nINSERT INTO t(b) VALUES (3);\nSELECT a FROM t ORDER BY a;",
        expect: Agrees,
    },
    Case {
        name: "rowid.reuse",
        kind: "semantics",
        script: "CREATE TABLE t(a INTEGER PRIMARY KEY, b);\nINSERT INTO t(b) VALUES (1);\nDELETE FROM t;\nINSERT INTO t(b) VALUES (2);\nSELECT a FROM t;",
        expect: Agrees,
    },
    Case {
        name: "json.each",
        kind: "extension",
        script: "SELECT key, value FROM json_each('[10,20]');",
        expect: Agrees,
    },
    Case {
        name: "json.tree",
        kind: "extension",
        script: "SELECT count(*) FROM json_tree('{\"a\":[1,2]}');",
        expect: Agrees,
    },
    Case {
        name: "generate.series",
        kind: "extension",
        script: "SELECT count(*), sum(value) FROM generate_series(1,10);",
        expect: Agrees,
    },
    Case {
        name: "generate.series.limit",
        kind: "extension",
        script: "SELECT value FROM generate_series(1,10) LIMIT 3;",
        expect: Agrees,
    },
    Case {
        name: "generate.series.step",
        kind: "extension",
        script: "SELECT value FROM generate_series(0,10,5);",
        expect: Agrees,
    },
    Case {
        name: "txn.commit",
        kind: "txn",
        script: "CREATE TABLE t(a);\nBEGIN;\nINSERT INTO t VALUES (1);\nCOMMIT;\nSELECT count(*) FROM t;",
        expect: Agrees,
    },
    Case {
        name: "txn.rollback",
        kind: "txn",
        script: "CREATE TABLE t(a);\nINSERT INTO t VALUES (1);\nBEGIN;\nINSERT INTO t VALUES (2);\nROLLBACK;\nSELECT count(*) FROM t;",
        expect: Agrees,
    },
    Case {
        name: "txn.savepoint",
        kind: "txn",
        script: "CREATE TABLE t(a);\nBEGIN;\nINSERT INTO t VALUES (1);\nSAVEPOINT s;\nINSERT INTO t VALUES (2);\nROLLBACK TO s;\nCOMMIT;\nSELECT count(*) FROM t;",
        expect: Agrees,
    },
    Case {
        name: "drop.rollback",
        kind: "txn",
        script: "CREATE TABLE t(a);\nINSERT INTO t VALUES (1);\nBEGIN;\nDROP TABLE t;\nROLLBACK;\nSELECT count(*) FROM t;",
        expect: Agrees,
    },
    Case {
        name: "create.rollback",
        kind: "txn",
        script: "CREATE TABLE t(a);\nBEGIN;\nCREATE TABLE u(a);\nROLLBACK;\nSELECT count(*) FROM sqlite_schema WHERE name = 'u';",
        expect: Agrees,
    },
    // Four cases, one per conclusion the planner drew from a `DESC`
    // key column, plus the write path that maintains one. Nine rows rather
    // than three, because with three rows an inverted bound selects the same
    // count by coincidence - which is what kept this hidden.
    Case {
        name: "desc.index.range",
        kind: "desc",
        script: "CREATE TABLE t(id INTEGER PRIMARY KEY, c INTEGER);\nCREATE INDEX ic ON t(c DESC);\nINSERT INTO t VALUES (1,10),(2,20),(3,30),(4,40),(5,50),(6,60),(7,70),(8,80),(9,90);\nSELECT count(*) FROM t WHERE c >= 10;\nSELECT count(*) FROM t WHERE c > 40;\nSELECT count(*) FROM t WHERE c <= 30;\nSELECT count(*) FROM t WHERE c < 90;\nSELECT group_concat(c) FROM (SELECT c FROM t WHERE c > 40 AND c <= 70 ORDER BY c);",
        expect: Agrees,
    },
    Case {
        name: "desc.index.order",
        kind: "desc",
        script: "CREATE TABLE t(id INTEGER PRIMARY KEY, c INTEGER);\nCREATE INDEX ic ON t(c DESC);\nINSERT INTO t VALUES (1,10),(2,20),(3,30),(4,40),(5,50),(6,60),(7,70),(8,80),(9,90);\nSELECT group_concat(c) FROM (SELECT c FROM t ORDER BY c);\nSELECT group_concat(c) FROM (SELECT c FROM t ORDER BY c DESC);\nSELECT group_concat(c) FROM (SELECT c FROM t WHERE c >= 30 ORDER BY c DESC);",
        expect: Agrees,
    },
    Case {
        name: "desc.index.write",
        kind: "desc",
        script: "CREATE TABLE t(id INTEGER PRIMARY KEY, c INTEGER);\nCREATE UNIQUE INDEX ic ON t(c DESC);\nINSERT INTO t VALUES (1,10),(2,20),(3,30),(4,40),(5,50);\nUPDATE t SET c = 35 WHERE id = 3;\nDELETE FROM t WHERE id = 1;\nINSERT INTO t VALUES (6,10);\nSELECT group_concat(c) FROM (SELECT c FROM t WHERE c >= 20 ORDER BY c);\nINSERT INTO t VALUES (7,35);\nSELECT group_concat(id) FROM (SELECT id FROM t ORDER BY id);",
        expect: Agrees,
    },
    Case {
        name: "desc.index.compound",
        kind: "desc",
        script: "CREATE TABLE t(id INTEGER PRIMARY KEY, a INTEGER, b TEXT);\nCREATE INDEX ic ON t(a DESC, b);\nINSERT INTO t VALUES (1,1,'p'),(2,1,'q'),(3,2,'r'),(4,2,'s'),(5,3,'t'),(6,3,'u');\nSELECT count(*) FROM t WHERE a >= 2;\nSELECT group_concat(b) FROM (SELECT b FROM t WHERE a = 2 ORDER BY b);\nSELECT group_concat(a) FROM (SELECT a FROM t WHERE a > 1 ORDER BY a, b);",
        expect: Agrees,
    },
    Case {
        name: "desc.index.bulk",
        kind: "desc",
        script: "CREATE TABLE t(id INTEGER PRIMARY KEY, c INTEGER);
INSERT INTO t VALUES (1,10),(2,20),(3,30),(4,40),(5,50),(6,60),(7,70),(8,80),(9,90);
CREATE INDEX ic ON t(c DESC);
SELECT count(*) FROM t WHERE c > 40;
SELECT count(*) FROM t WHERE c <= 30;
SELECT group_concat(c) FROM (SELECT c FROM t ORDER BY c);
SELECT group_concat(c) FROM (SELECT c FROM t WHERE c > 40 AND c <= 70 ORDER BY c);
PRAGMA integrity_check;",
        expect: Agrees,
    },
    Case {
        name: "desc.index.nulls",
        kind: "desc",
        script: "CREATE TABLE t(id INTEGER PRIMARY KEY, c INTEGER);
CREATE INDEX ic ON t(c DESC);
INSERT INTO t VALUES (1,10),(2,NULL),(3,30),(4,NULL),(5,50);
SELECT count(*) FROM t WHERE c >= 10;
SELECT count(*) FROM t WHERE c <= 30;
SELECT count(*) FROM t WHERE c IS NULL;
SELECT group_concat(coalesce(c,'-')) FROM (SELECT c FROM t ORDER BY c);
SELECT group_concat(coalesce(c,'-')) FROM (SELECT c FROM t ORDER BY c DESC);",
        expect: Agrees,
    },
    Case {
        name: "reindex.forms",
        kind: "ddl",
        script: "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, team TEXT COLLATE NOCASE);
CREATE INDEX t_name ON t (name);
CREATE INDEX t_team ON t (team);
CREATE UNIQUE INDEX t_unique ON t (name, team);
INSERT INTO t VALUES (1,'ada','Blue'),(2,'bob','red'),(3,'cai','BLUE');
DELETE FROM t WHERE id = 2;
INSERT INTO t VALUES (4,'dee','green');
REINDEX t_name;
REINDEX t;
REINDEX NOCASE;
REINDEX;
SELECT id FROM t WHERE name = 'cai';
SELECT group_concat(id) FROM (SELECT id FROM t WHERE team = 'blue' ORDER BY id);
SELECT count(*) FROM t;
PRAGMA integrity_check;",
        expect: Agrees,
    },
    Case {
        name: "reindex.desc",
        kind: "ddl",
        script: "CREATE TABLE t(id INTEGER PRIMARY KEY, c INTEGER);
CREATE INDEX ic ON t(c DESC);
INSERT INTO t VALUES (1,10),(2,20),(3,30);
REINDEX ic;
SELECT group_concat(c) FROM (SELECT c FROM t ORDER BY c);
PRAGMA integrity_check;",
        expect: Agrees,
    },
    Case {
        name: "dump.generated",
        kind: "shell",
        script: "CREATE TABLE t (a INTEGER PRIMARY KEY, b INTEGER, c INTEGER GENERATED ALWAYS AS (b*2) VIRTUAL, d TEXT AS (b || '!') STORED);
INSERT INTO t (a,b) VALUES (1,10),(2,20);
.dump",
        expect: Agrees,
    },
    Case {
        name: "dump.sequence",
        kind: "shell",
        script: "CREATE TABLE q(id INTEGER PRIMARY KEY AUTOINCREMENT, v);
INSERT INTO q(v) VALUES('a'),('b');
DELETE FROM q WHERE id = 2;
.dump",
        expect: Agrees,
    },
    Case {
        name: "dump.quoted",
        kind: "shell",
        script: "CREATE TABLE \"odd name\"(x, \"y z\");
INSERT INTO \"odd name\" VALUES(1,2);
CREATE INDEX \"ix odd\" ON \"odd name\"(x);
.dump",
        expect: Agrees,
    },
    Case {
        name: "dump.order",
        kind: "shell",
        script: "CREATE TABLE g(a INTEGER PRIMARY KEY, b);
CREATE VIEW v1 AS SELECT a FROM g;
CREATE INDEX i1 ON g(b);
CREATE TRIGGER t1 AFTER INSERT ON g BEGIN UPDATE g SET b=1; END;
CREATE INDEX i2 ON g(a,b);
CREATE VIEW v2 AS SELECT b FROM g;
.dump",
        expect: Agrees,
    },
    Case {
        name: "analyze.subjects",
        kind: "ddl",
        script: "CREATE TABLE p(a,b);
CREATE INDEX pi ON p(a);
INSERT INTO p VALUES(1,2),(3,4);
CREATE VIEW pv AS SELECT a FROM p;
CREATE TABLE q(id INTEGER PRIMARY KEY AUTOINCREMENT, v);
INSERT INTO q(v) VALUES('a');
ANALYZE;
SELECT tbl, idx, stat FROM sqlite_stat1 ORDER BY tbl, idx;",
        expect: Agrees,
    },
    Case {
        name: "journal.default",
        kind: "pragma",
        script: "PRAGMA journal_mode;
PRAGMA journal_mode=WAL;
PRAGMA journal_mode=DELETE;
PRAGMA journal_mode=MEMORY;
PRAGMA journal_mode=TRUNCATE;
PRAGMA journal_mode=PERSIST;
PRAGMA journal_mode=OFF;",
        expect: Agrees,
    },
    Case {
        name: "journal.rollback.writes",
        kind: "pragma",
        script: "CREATE TABLE t(a);
INSERT INTO t VALUES(1),(2),(3);
BEGIN;
INSERT INTO t VALUES(4);
ROLLBACK;
SELECT group_concat(a) FROM t;
UPDATE t SET a = a * 10;
SELECT group_concat(a) FROM t;
PRAGMA integrity_check;",
        expect: Agrees,
    },
    Case {
        name: "explain.bytecode.layout",
        kind: "explain",
        script: "CREATE TABLE t(id INTEGER PRIMARY KEY, a INTEGER);
EXPLAIN QUERY PLAN SELECT a FROM t WHERE id = 1;",
        expect: Agrees,
    },
    Case {
        name: "shell.dbconfig",
        kind: "shell",
        script: ".dbconfig
.dbconfig defensive off
.dbconfig defensive on
.dbconfig enable_fkey on
PRAGMA foreign_keys;
.dbconfig writable_schema
.dbconfig nosuch on",
        expect: Agrees,
    },
    Case {
        name: "shell.dbconfig.defensive",
        kind: "shell",
        script: "PRAGMA journal_mode=OFF;
PRAGMA journal_mode;
.dbconfig defensive off
PRAGMA journal_mode=OFF;
PRAGMA journal_mode;",
        expect: Agrees,
    },
    Case {
        name: "shell.usage.lines",
        kind: "shell",
        script: ".cd
.nonce
.scanstats
.system
.shell
.auth",
        expect: Agrees,
    },
    Case {
        name: "shell.crlf",
        kind: "shell",
        script: ".crlf
.crlf on
.crlf off
.crlf",
        expect: Agrees,
    },
    Case {
        name: "shell.testcase",
        kind: "shell",
        script: ".check
.testcase one
SELECT 1;
.check 1
.testcase two
SELECT 2;
.check 9",
        expect: Agrees,
    },
    Case {
        name: "shell.filectrl",
        kind: "shell",
        script: ".filectrl
.filectrl bogus
.filectrl psow
.filectrl reserve_bytes",
        expect: Agrees,
    },
    Case {
        name: "shell.connection",
        kind: "shell",
        script: "CREATE TABLE t(a);
.connection 1
CREATE TABLE u(b);
.tables
.connection 0
.tables
.connection close 1
.connection 1
.tables
.connection 9
.connection 0
.tables",
        expect: Agrees,
    },
    Case {
        name: "shell.auth",
        kind: "shell",
        script: "CREATE TABLE t(a, b);
INSERT INTO t VALUES(1,'x');
.auth on
SELECT a FROM t;
.auth off
SELECT b FROM t;",
        expect: Agrees,
    },
    Case {
        name: "arith.text.class",
        kind: "types",
        script: "SELECT x'00' + x'00', typeof(x'00' + x'00');
SELECT x'41' + 1;
SELECT 'abc' + 1, typeof('abc' + 1);
SELECT '3' + 1, typeof('3' + 1);
SELECT '3.5' + 1, typeof('3.5' + 1);
SELECT '3e2' + 1, typeof('3e2' + 1);
SELECT '  7 apples' + 1;
SELECT '-4' * 2;
SELECT 9223372036854775807 + 1;
SELECT '9223372036854775808' + 0, typeof('9223372036854775808' + 0);
SELECT 1.5 + 1, typeof(1.5 + 1);
SELECT NULL + 1;
SELECT '' + 1, typeof('' + 1);",
        expect: Agrees,
    },
    Case {
        name: "arith.text.column",
        kind: "types",
        script: "CREATE TABLE t(a TEXT, b BLOB, c INTEGER);
INSERT INTO t VALUES ('7','abc',2),('3.5',x'00',1);
SELECT a + c, typeof(a + c) FROM t ORDER BY rowid;
SELECT b + c, typeof(b + c) FROM t ORDER BY rowid;
SELECT a * c, a - c FROM t ORDER BY rowid;",
        expect: Agrees,
    },
    Case {
        name: "shell.imposter",
        kind: "shell",
        script: "CREATE TABLE t(id INTEGER PRIMARY KEY, a INTEGER, b TEXT);
INSERT INTO t VALUES(1,10,'x'),(2,20,'y');
CREATE INDEX ia ON t(a);
CREATE INDEX iab ON t(a,b);
.imposter ia im
SELECT * FROM im;
.imposter iab im2
SELECT * FROM im2;
.imposter t im3
.imposter
.imposter off
SELECT * FROM im;",
        expect: Agrees,
    },
    // **The five clauses that may name a result column by its alias
    // (task-2026).** The binder keeps a list of `(alias, expression)` for the
    // block it is binding and consults it in exactly one place, after a real
    // column has failed to match. Filling that list costs an allocation per
    // result column, so it is filled only when one of these clauses is present
    // - and the argument that made that safe is the claim that these are all of
    // them.
    //
    // Four of the five had no case anywhere in the suite. `ORDER BY` naming an
    // alias was covered, through a view and through a compound; `GROUP BY`,
    // `HAVING` and `LIMIT` were not, and neither was the statement with none of
    // them, which is the one the change actually alters. A wrong enumeration
    // would have answered `no such column` on a query SQLite answers, and
    // nothing in the suite would have said so.
    //
    // `alias.having` was one of them and is now task-2040's: it was a syntax
    // error here and an answer in SQLite, this file recorded that, and that
    // ticket fixed it. The three `HAVING` cases below are its.
    //
    // Each case is a single row or an explicit order, so neither shell is being
    // asked about an ordering that is unspecified.
    Case {
        name: "alias.order.by",
        kind: "read",
        script: "CREATE TABLE t(a INTEGER PRIMARY KEY, b INTEGER);
INSERT INTO t VALUES(1,30),(2,10),(3,20);
SELECT b AS x FROM t ORDER BY x;",
        expect: Agrees,
    },
    Case {
        // No `ORDER BY`, so the alias list is reached through the `GROUP BY`
        // alone. One group, so there is no order to disagree about.
        name: "alias.group.by",
        kind: "read",
        script: "CREATE TABLE t(a INTEGER PRIMARY KEY, b INTEGER);
INSERT INTO t VALUES(1,30),(2,10),(3,20);
SELECT b AS x, count(*) FROM t WHERE a = 1 GROUP BY x;",
        expect: Agrees,
    },
    // The three `HAVING` with no `GROUP BY` cases (task-2040). This file's
    // header says what each one is for.
    Case {
        name: "alias.having",
        kind: "read",
        script: "CREATE TABLE t(a INTEGER PRIMARY KEY, b INTEGER);
INSERT INTO t VALUES(1,30),(2,10),(3,20);
SELECT count(*) AS n FROM t HAVING n > 0;
SELECT count(*) AS n FROM t HAVING n > 9;
SELECT max(b) AS m FROM t HAVING m > 25;
SELECT max(b) AS m FROM t HAVING m > 35;",
        expect: Agrees,
    },
    Case {
        name: "having.whole.table",
        kind: "read",
        script: "CREATE TABLE t(id INTEGER PRIMARY KEY, a INTEGER, b TEXT);
INSERT INTO t VALUES (1,10,'p'),(2,20,'q'),(3,30,'r'),(4,20,'s'),(5,50,'t');
SELECT count(*) FROM t HAVING count(*) > 4;
SELECT sum(a), avg(a) FROM t HAVING sum(a) > 100;
SELECT max(a) AS m, b FROM t HAVING m > 25;
SELECT min(a) AS m, b FROM t HAVING m < 25;
SELECT count(*) FROM t WHERE a > 15 HAVING count(*) = 3;
SELECT count(*) FROM t HAVING a > 5;
SELECT DISTINCT count(*) FROM t HAVING count(*) > 0;
SELECT count(*) FROM t HAVING count(*) > 0 ORDER BY 1 LIMIT 1;
SELECT count(*) FROM t HAVING (SELECT count(*) FROM t) > 1;
SELECT count(*) FROM t HAVING NULL;
SELECT count(*) FROM t HAVING 'x';
CREATE TABLE e(x);
SELECT count(*) FROM e HAVING count(*) > 0;
SELECT count(*) FROM e HAVING count(*) = 0;",
        expect: Agrees,
    },
    Case {
        name: "having.non.aggregate",
        kind: "read",
        script: "CREATE TABLE t(id INTEGER PRIMARY KEY, a INTEGER, b TEXT);
INSERT INTO t VALUES (1,10,'p'),(2,20,'q'),(3,30,'r'),(4,20,'s'),(5,50,'t');
SELECT a FROM t HAVING a > 15;
SELECT 1 FROM t HAVING count(*) > 0;
SELECT 1 FROM t HAVING 1 ORDER BY count(*);
SELECT * FROM t HAVING 1;
SELECT 1 HAVING 1;
SELECT a FROM t GROUP BY a HAVING a > 15;
SELECT 1 FROM t GROUP BY a HAVING count(*) > 1;
SELECT count(*) FROM t;",
        expect: Agrees,
    },
    Case {
        // **Both refuse an alias in a `LIMIT`, and they refuse it for different
        // reasons.** SQLite does not resolve a result alias there at all and
        // says `no such column: x`. This engine resolves it, and then the
        // physical pass refuses the statement because the `LIMIT` is not a
        // constant. A caller gets a refusal either way, and gets a sentence
        // about the wrong thing here.
        //
        // The alias list is filled when a `LIMIT` is present for exactly this
        // reason: whatever the refusal turns out to be, it has to come from
        // resolving the name rather than from never having recorded it.
        name: "alias.limit",
        kind: "read",
        script: "CREATE TABLE t(a INTEGER PRIMARY KEY, b INTEGER);
INSERT INTO t VALUES(1,30),(2,10),(3,20);
SELECT b AS x FROM t ORDER BY a LIMIT x;",
        expect: Differs,
    },
    Case {
        // The case the change alters: nothing here can name an alias, so the
        // list is not filled at all. The alias must still name the column in
        // the answer, and a reference to it must still fail the way it always
        // did.
        name: "alias.unread",
        kind: "read",
        script: "CREATE TABLE t(a INTEGER PRIMARY KEY, b INTEGER);
INSERT INTO t VALUES(1,30),(2,10),(3,20);
.headers on
SELECT b AS x FROM t WHERE a = 2;
SELECT b AS x, x + 1 FROM t WHERE a = 2;",
        expect: Agrees,
    },
    // The three shapes task-2042 fixed: an aggregate, a grouped aggregate and
    // a window function, each in a compound arm that is not the first.
    //
    // The binder bound every arm after the head in a block whose `aggregates`
    // and `windows` were thrown away when the block was left, so the arm
    // reached the planner claiming to compute nothing while its result column
    // was still a `BoundExpr::Aggregate` or a `BoundExpr::WindowRef`. The last
    // statement of `compound.arm.aggregate` is the one that narrows it: with
    // the aggregate in the *head* arm the same query always answered, because
    // the head's aggregates are copied off the binder and the arms' were not.
    //
    // `compound.arm.grouped` is the half that answered rather than refusing,
    // and it is the reason these cases compare the whole script. A `GROUP BY`
    // on such
    // an arm survived into the plan when the aggregates did not, so
    // `AggregationMode::Grouped` was chosen, an aggregate operator was built
    // with no accumulators in it, and the projection read one column past the
    // end of the group key: `SELECT 1 UNION ALL SELECT count(*) FROM t GROUP
    // BY a` answered `1` and then one **blank** row per group where SQLite
    // answers `1` four times. A harness comparing only whether the statement
    // failed would call that agreement.
    Case {
        name: "compound.arm.aggregate",
        kind: "read",
        script: "CREATE TABLE t(id INTEGER PRIMARY KEY, a INTEGER);
INSERT INTO t VALUES (1,10),(2,20),(3,30);
SELECT 1 UNION ALL SELECT count(*) FROM t;
SELECT 1 UNION SELECT count(*) FROM t;
SELECT 1 EXCEPT SELECT count(*) FROM t;
SELECT 3 INTERSECT SELECT count(*) FROM t;
SELECT 1 UNION ALL SELECT sum(a) FROM t;
SELECT 1 UNION ALL SELECT max(a) FROM t UNION ALL SELECT min(a) FROM t;
SELECT 1 UNION ALL SELECT count(DISTINCT a) FROM t;
SELECT 1 UNION ALL SELECT group_concat(a) FROM t;
SELECT 1 UNION ALL SELECT count(*) FILTER (WHERE a > 15) FROM t;
SELECT count(*) FROM t UNION ALL SELECT 2;",
        expect: Agrees,
    },
    Case {
        name: "compound.arm.grouped",
        kind: "read",
        script: "CREATE TABLE t(id INTEGER PRIMARY KEY, a INTEGER, g TEXT);
INSERT INTO t VALUES (1,10,'x'),(2,20,'y'),(3,30,'x'),(4,20,'z');
SELECT 1 UNION ALL SELECT count(*) FROM t GROUP BY a;
SELECT 1 UNION ALL SELECT count(*) FROM t GROUP BY g ORDER BY 1;
SELECT 0 UNION ALL SELECT count(*) FROM t GROUP BY g HAVING count(*) > 1;
SELECT g, count(*) FROM t GROUP BY g UNION ALL SELECT g, sum(a) FROM t GROUP BY g ORDER BY 1, 2;",
        expect: Agrees,
    },
    // `windows` is taken off the binder in the same place `aggregates` is, so
    // a window function in a later arm was refused by the same root cause and
    // in the same release - `the expression a WindowRef expression` on 0.1.2 -
    // and is fixed by the same line. Without this case the next change to that
    // line could restore the aggregates and lose the windows again.
    Case {
        name: "compound.arm.window",
        kind: "read",
        script: "CREATE TABLE t(id INTEGER PRIMARY KEY, a INTEGER);
INSERT INTO t VALUES (1,10),(2,20),(3,30);
SELECT 1 UNION ALL SELECT row_number() OVER (ORDER BY a) FROM t;
SELECT 1 UNION ALL SELECT sum(a) OVER (ORDER BY a) FROM t;",
        expect: Agrees,
    },
    // -----------------------------------------------------------------------
    // The seven rows `docs/feature-comparison.md` records as answering
    // differently, as cases that assert the difference (task-2036, TDD 5.8).
    //
    // **`Expect::Differs` existed and was constructed by nothing.** The variant
    // was kept so the vocabulary would survive the last construct being fixed,
    // and it left rule 1.3 - "a known difference is recorded as a test that
    // asserts it" - applying to nothing at all, while the comparison document
    // carried seven differences it had measured and argued for. A difference
    // that only a document knows about is one a change can close, or widen,
    // with nothing going red either way.
    //
    // Each of these is a difference with a reason, not a defect: this engine's
    // page size, a number that describes SQLite's own C structures, or the two
    // pinned reference artifacts disagreeing with one another. The reason is on
    // the row. `every_probed_construct_answers_as_the_table_says` fails if one
    // of them starts agreeing, which is what would happen if somebody changed
    // the default page size without reading the measurement behind it.
    Case {
        name: "pragma.page.size",
        kind: "differs",
        script: "PRAGMA page_size;",
        // 4096 there, 32768 here. Measured both ways on the medium gate: 4096
        // with a cache-matched pool put the `schema` family at 0.94x, under the
        // 1.00x floor the performance contract requires.
        expect: Differs,
    },
    Case {
        name: "shell.recover.page.size",
        kind: "differs",
        script: "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT);
INSERT INTO t VALUES(1,'x');
.recover",
        // Nineteen statements, identical but for `PRAGMA page_size = '4096'`
        // against `'32768'` - which is the row above, seen from the shell.
        expect: Differs,
    },
    Case {
        name: "shell.limit.trigger.depth",
        kind: "differs",
        script: ".limit",
        // Twelve of the thirteen lines agree. `trigger_depth` is 100 in the
        // downloaded `sqlite3.exe`, which was built with
        // SQLITE_MAX_TRIGGER_DEPTH=100 and says so in its own
        // `PRAGMA compile_options`, and 1000 in the amalgamation this engine
        // matches - which `compat/limits.toml` names as authoritative. No value
        // closes this row: whichever artifact is agreed with, the other one
        // disagrees.
        expect: Differs,
    },
    Case {
        name: "explain.bytecode",
        kind: "differs",
        script: "EXPLAIN SELECT 1;",
        // The same eight columns under the same widths and the same header
        // rule, holding this engine's operator chain. SQLite lists the opcodes
        // of a bytecode program and this engine compiles none, so the rows are
        // what the statement actually runs.
        expect: Differs,
    },
    Case {
        name: "shell.vfslist",
        kind: "differs",
        script: ".vfslist",
        // Four lines per file system in the reference's format, over the two
        // this build has rather than the six SQLite's registry holds.
        // `szOsFile` is the size of a C struct in a library that is not linked
        // here.
        expect: Differs,
    },
    Case {
        name: "shell.stats",
        kind: "differs",
        script: ".stats on
SELECT 1;",
        // The same two-column shape over the counters this engine keeps - page
        // cache fetches, hits, misses and rewarms, frames cooled and evicted.
        // Lookaside slots and pcache overflow bytes are facts about SQLite's
        // allocator rather than about the query.
        expect: Differs,
    },
    Case {
        name: "functions.sqlite.offset",
        kind: "differs",
        script: "CREATE TABLE t(a);
INSERT INTO t VALUES(1),(2),(3);
SELECT sqlite_offset(a) FROM t;",
        // The offset of the *page* the row is read from rather than of the
        // record, because a PAX leaf stores each column as its own run of bytes
        // and one row therefore occupies several places on its page. SQLite's
        // own documentation says its value may name the table or an index
        // depending on the plan, so it is opaque to a caller either way; what
        // both engines agree on is that a column of a real table has an offset
        // and a literal does not.
        expect: Differs,
    },
];

/// Returns the pinned SQLite shell, if it has been downloaded.
fn reference() -> Option<PathBuf> {
    let directory = workspace_root().join(".sqlite-ref/3.53.4/shell");
    let path = directory.join(format!("sqlite3{}", std::env::consts::EXE_SUFFIX));
    path.is_file().then_some(path)
}

/// Runs one script through one shell over a fresh database.
///
/// Each case gets its own directory, because several of them attach a second
/// file or write one, and a case that read another's leftovers would be a case
/// about the order the table happens to be in.
///
/// @param program - the shell to run
/// @param area - where its database goes
/// @param script - the whole script
fn run(program: &PathBuf, area: &PathBuf, script: &str) -> String {
    // **Cleared, not just created.** Several cases ATTACH a second file or
    // VACUUM into one, and a directory left over from the previous run makes
    // the next one answer a different question: the ATTACH case reported
    // that its table already existed, against a file an earlier run wrote.
    let _ = std::fs::remove_dir_all(area);
    let _ = std::fs::create_dir_all(area);
    let database = area.join("probe.db");
    let Ok(mut child) = Command::new(program)
        .arg(&database)
        .current_dir(area)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    else {
        return String::new();
    };
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        let _ = stdin.write_all(script.as_bytes());
    }
    let Ok(output) = child.wait_with_output() else {
        return String::new();
    };
    let mut text = String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n");
    text.push_str(&String::from_utf8_lossy(&output.stderr).replace("\r\n", "\n"));
    text.trim().to_string()
}

/// The groups the categories are graded in, and what each one is about.
///
/// **Eight tests rather than one, and rather than twenty-three (task-2066
/// section 4.4.13).** The table holds 235 cases in 23 categories, each one
/// spawning two shells and comparing their bytes, and all of them used to run
/// in a single `#[test]` - one libtest thread, start to finish, with a failure
/// that named the table rather than the part of it that moved. Twenty-three
/// tests would parallelise further and would be twenty-three functions whose
/// doc comments all said the same thing; eight groups is where the categories
/// stop being arbitrary and start being about something a reader can name.
///
/// A category that belongs to no group is graded by nothing, so
/// `every_category_in_the_table_is_graded_by_a_group` fails when one is added
/// and not placed here. It found one on the first run after the split: `fts5`
/// had seven cases and was named by no group, which is the state the whole
/// table would have been left in if the guard had been left out.
const GROUPS: &[(&str, &[&str])] = &[
    ("reading rows", &["read", "join", "desc"]),
    ("the command line's own output", &["surface"]),
    ("writing rows", &["constraint", "write", "dml", "trigger"]),
    ("the interactive shell", &["shell", "explain"]),
    (
        "pragmas, schema and transactions",
        &["pragma", "ddl", "txn"],
    ),
    (
        "affinity, types and operators",
        &["affinity", "types", "operator", "syntax"],
    ),
    ("the engine's own semantics", &["semantics", "differs"]),
    (
        "the built-in functions",
        &["ext", "extension", "json", "time"],
    ),
    ("full text search", &["fts5"]),
];

/// What one group's cases did.
struct Graded {
    /// How many of the group's cases the two shells answered the same way.
    agreed: usize,
    /// How many cases the group holds.
    total: usize,
    /// One line per case whose disposition is no longer true of it.
    wrong: Vec<String>,
}

/// Runs every case of one group and reports what it found.
///
/// **Every case is run before anything is asserted.** The first disagreement
/// does not stop the group, because a change that moves one construct usually
/// moves several and a reader fixing them one run at a time learns that the
/// hard way.
///
/// @param group - the group's name, as `GROUPS` spells it
/// @param reference - the pinned SQLite shell
/// @param ours - the shell this repository builds
fn grade(group: &str, reference: &PathBuf, ours: &PathBuf) -> Graded {
    let area = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("semantics");
    let kinds = kinds_of(group);
    let mut graded = Graded {
        agreed: 0,
        total: 0,
        wrong: Vec::new(),
    };
    for case in CASES.iter().filter(|case| kinds.contains(&case.kind)) {
        graded.total += 1;
        let theirs = run(reference, &area.join(case.name).join("sqlite"), case.script);
        let mine = run(ours, &area.join(case.name).join("inillucent"), case.script);
        let same = theirs == mine;
        if same {
            graded.agreed += 1;
        }
        match (case.expect, same) {
            (Agrees, false) => graded.wrong.push(format!(
                "{} [{}] was agreeing and no longer does\n  sqlite: {:?}\n  ours  : {:?}",
                case.name, case.kind, theirs, mine
            )),
            // **A case that starts agreeing is a failure too.** It is why the
            // disposition is declared at all: the row has to move, or the next
            // reader is told this construct is still broken.
            (Differs, true) => graded.wrong.push(format!(
                "{} [{}] now agrees - move its row to Agrees",
                case.name, case.kind
            )),
            _ => {}
        }
    }
    graded
}

/// The categories one group covers.
///
/// @param group - the group's name, as `GROUPS` spells it
fn kinds_of(group: &str) -> &'static [&'static str] {
    GROUPS
        .iter()
        .find(|(name, _)| *name == group)
        .map(|(_, kinds)| *kinds)
        .unwrap_or(&[])
}

/// Grades one group and fails with everything it found.
///
/// **The count is asserted as well as the disagreements**, because the value of
/// this table is a number - "89 of 92 agree" is what a reader of
/// `docs/feature-comparison.md` is comparing against - and a group that ran no
/// cases at all would otherwise report a pass. The number is now per group
/// rather than for the whole table; the whole table's number is what
/// `every_category_in_the_table_is_graded_by_a_group` keeps honest, by making
/// the eight groups add up to all 235 rows.
///
/// @param group - the group's name, as `GROUPS` spells it
fn check(group: &str) {
    let Some(reference) = reference() else {
        inillucent_compat::differential::skipping("the pinned SQLite shell is not built");
        return;
    };
    let ours = inillucent_compat::cliproc::program("inillucent-shell");
    let graded = grade(group, &reference, &ours);
    assert!(
        graded.total > 0,
        "the group '{group}' graded no cases at all, so its categories are named in \
         GROUPS and by no row of the table"
    );
    assert!(
        graded.wrong.is_empty(),
        "{} of {} agreed in '{group}'\n{}",
        graded.agreed,
        graded.total,
        graded.wrong.join("\n")
    );
    let declared = CASES
        .iter()
        .filter(|case| kinds_of(group).contains(&case.kind) && matches!(case.expect, Agrees))
        .count();
    assert_eq!(
        graded.agreed, declared,
        "{} of {} agreed in '{group}', and the table declares {declared} that should",
        graded.agreed, graded.total
    );
}

/// The cases about reading rows answer as the table says.
///
/// `read`, `join` and `desc`: what a `SELECT` gives back, what an outer join
/// gives back where there is nothing on the other side, and what a descending
/// index gives back when the scan and the index disagree about order.
#[test]
fn the_cases_about_reading_rows_answer_as_the_table_says() {
    check("reading rows");
}

/// The cases about the command line's own output answer as the table says.
///
/// `surface` is the largest category and the one least about SQL: how a value
/// is printed, what a column is called when nobody named it, and what comes out
/// when a statement produces no rows.
#[test]
fn the_cases_about_the_command_lines_output_answer_as_the_table_says() {
    check("the command line's own output");
}

/// The cases about writing rows answer as the table says.
///
/// `constraint`, `write`, `dml` and `trigger`: what a conflicting write does,
/// which row it leaves behind, and what `changes()` says afterwards.
#[test]
fn the_cases_about_writing_rows_answer_as_the_table_says() {
    check("writing rows");
}

/// The cases about the interactive shell answer as the table says.
///
/// `shell` and `explain`: the dot commands, and the plan a statement prints
/// when it is asked to explain itself rather than run.
#[test]
fn the_cases_about_the_interactive_shell_answer_as_the_table_says() {
    check("the interactive shell");
}

/// The cases about pragmas, schema and transactions answer as the table says.
///
/// `pragma`, `ddl` and `txn`: what a pragma answers, what `ALTER TABLE` does to
/// a schema, and what a rollback leaves.
#[test]
fn the_cases_about_pragmas_schema_and_transactions_answer_as_the_table_says() {
    check("pragmas, schema and transactions");
}

/// The cases about affinity, types and operators answer as the table says.
///
/// `affinity`, `types`, `operator` and `syntax`: the rules that decide what a
/// value *is* before anything is done with it, which is where two engines that
/// agree about every statement can still disagree about every answer.
#[test]
fn the_cases_about_affinity_types_and_operators_answer_as_the_table_says() {
    check("affinity, types and operators");
}

/// The cases about the engine's own semantics answer as the table says.
///
/// `semantics` and `differs`: the constructs where this engine and SQLite are
/// known to answer differently, including the seven
/// `docs/feature-comparison.md` measured and argued for.
#[test]
fn the_cases_about_the_engines_own_semantics_answer_as_the_table_says() {
    check("the engine's own semantics");
}

/// The cases about the built-in functions answer as the table says.
///
/// `ext`, `extension`, `json` and `time`: the functions that are not part of
/// the language, where a difference is a difference in one function rather than
/// in how statements are run.
#[test]
fn the_cases_about_the_built_in_functions_answer_as_the_table_says() {
    check("the built-in functions");
}

/// The full text search cases answer as the table says.
///
/// `fts5`: the virtual table module, its `MATCH` operator and the auxiliary
/// functions that rank what it returns. It is its own group because it is the
/// one category here that is not SQL - the statement around it is ordinary and
/// the whole answer comes from a module.
#[test]
fn the_full_text_search_cases_answer_as_the_table_says() {
    check("full text search");
}

/// Every category the table uses is graded by one of the groups.
///
/// **This is what makes the split safe.** With one test over the whole table, a
/// new `kind` was graded whether or not anybody thought about it. With eight,
/// a category named in a case and in no group is a case that runs nowhere and
/// a suite that reports a pass for it - the exact shape §1.2 of the testing
/// standard calls a test that cannot fail.
///
/// It also checks the other direction: a group naming a category no case uses
/// is a group that will silently shrink to nothing as rows are renamed.
#[test]
fn every_category_in_the_table_is_graded_by_a_group() {
    let grouped: Vec<&str> = GROUPS
        .iter()
        .flat_map(|(_, kinds)| kinds.iter().copied())
        .collect();

    let mut ungraded: Vec<&str> = CASES
        .iter()
        .map(|case| case.kind)
        .filter(|kind| !grouped.contains(kind))
        .collect();
    ungraded.sort_unstable();
    ungraded.dedup();
    assert!(
        ungraded.is_empty(),
        "these categories are used by a case and named by no group, so their cases are \
         run by no test: {ungraded:?}"
    );

    let empty: Vec<&&str> = grouped
        .iter()
        .filter(|kind| !CASES.iter().any(|case| case.kind == **kind))
        .collect();
    assert!(
        empty.is_empty(),
        "these categories are named by a group and used by no case: {empty:?}"
    );

    let counted: usize = GROUPS
        .iter()
        .map(|(name, _)| {
            CASES
                .iter()
                .filter(|case| kinds_of(name).contains(&case.kind))
                .count()
        })
        .sum();
    assert_eq!(
        counted,
        CASES.len(),
        "the groups add up to {counted} cases and the table holds {}, so a case is \
         graded twice or not at all",
        CASES.len()
    );
}

/// The table still declares the seven measured differences.
///
/// Rule 1.3: a known difference is recorded as a test that asserts it.
/// `docs/feature-comparison.md` argues for seven, and a table that quietly lost
/// those rows would agree with itself and say nothing. A difference that the
/// engine has closed is moved to `Agrees` and its row in the comparison
/// document goes with it, which is a change somebody makes on purpose.
///
/// **This needs no shell**, so it is the one case in this file that grades
/// something on a machine where the pinned SQLite is not built.
#[test]
fn the_table_still_declares_the_seven_measured_differences() {
    let declared = CASES
        .iter()
        .filter(|case| matches!(case.expect, Differs))
        .count();
    assert!(
        declared >= 7,
        "this file declares {declared} differences and docs/feature-comparison.md records \
         seven, so the table has lost rows rather than the engine having closed them"
    );
}
