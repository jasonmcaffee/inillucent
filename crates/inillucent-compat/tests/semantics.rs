//! Every construct the second review probed, answered by both shells.
//!
//! Invariant: **each case declares whether it agrees with SQLite, and a case
//! that changes its mind fails.** A construct that starts agreeing fails this
//! test until its row is moved to `Agrees`, which is the same discipline
//! `new_engine_surface.rs` applies to *acceptance* applied here to *answers*.
//! Without it, a fix is invisible: the probe was a one-off script, so the nine
//! wrong answers task-1843 found could each have been repaired and then
//! silently regressed with nothing to notice.
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
//! where task-1846's partial indexes and task-1849's `UPDATE` conflict check
//! meet: neither ticket could test it alone, because before 1846 the index
//! cannot be created and before 1849 the check is never reached.
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
//! table - the merge, rather than the vectorised mini-column read - and
//! task-1846 gave it a projected implementation of its own to stop it reading
//! every column of every row. Two implementations of one merge is exactly the
//! shape that drifts, so the answer is compared against the reference here.
//!
//! ## All of them agree
//!
//! `index.partial`, `index.expr` and `without.rowid.index` - the three
//! `CREATE INDEX` forms task-1845 left refused - were closed by task-1846, and
//! their rows moved from `Differs` to `Agrees`. There is no `Differs` row left,
//! and the check below that a case which starts agreeing fails until its row is
//! moved is what will report the next one either way.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use inillucent_compat::workspace_root;

/// Whether a case is expected to agree with the reference.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Expect {
    /// Byte-for-byte the same output.
    Agrees,
    /// Still different, and named in this file's own header.
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

/// The cases: the review's 61, the agreeing shapes it did not record,
/// task-1849's seven `UNIQUE`-under-`UPDATE` shapes, and task-1846's - two over
/// a table that has been written to, and five where the two tickets meet.
const CASES: &[Case] = &[
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
    Case {
        name: "select.window",
        kind: "read",
        script: "CREATE TABLE t(a INTEGER);\nINSERT INTO t VALUES (3),(1),(2);\nSELECT a, row_number() OVER (ORDER BY a) FROM t ORDER BY a;",
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
    // task-1849. An `UPDATE` onto another row's key in a secondary `UNIQUE`
    // index was performed and answered success, leaving the index with two
    // entries under one key: the first case is the ticket's own script. The
    // second is the other half of the same defect - the row must not collide
    // with *itself*, and moving the rowid moves an index entry whose key did
    // not change, which was already being refused. The rest are the paths a
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
];

/// Returns the pinned SQLite shell, if it has been downloaded.
fn reference() -> Option<PathBuf> {
    let directory = workspace_root().join(".sqlite-ref/3.53.4/shell");
    let path = directory.join(format!("sqlite3{}", std::env::consts::EXE_SUFFIX));
    path.is_file().then_some(path)
}

/// Returns this workspace's shell, building it first.
fn ours() -> Option<PathBuf> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let status = Command::new(cargo)
        .current_dir(workspace_root())
        .args(["build", "-p", "inillucent-cli"])
        .status()
        .ok()?;
    if !status.success() {
        return None;
    }
    let mut directory = std::env::current_exe().unwrap_or_default();
    directory.pop();
    directory.pop();
    let path = directory.join(format!("inillucent-shell{}", std::env::consts::EXE_SUFFIX));
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

/// The whole table, run once, with both directions checked.
///
/// One test rather than one per case, because the value of the table is the
/// count: "89 of 92 agree" is the number this ticket moved, and a suite of 92
/// tests reports it as 92 lines nobody adds up.
#[test]
fn every_probed_construct_answers_as_the_table_says() {
    let (Some(reference), Some(ours)) = (reference(), ours()) else {
        eprintln!("a shell is missing; skipping");
        return;
    };
    let area = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("semantics");
    let mut agreed = 0usize;
    let mut wrong: Vec<String> = Vec::new();
    for case in CASES {
        let theirs = run(
            &reference,
            &area.join(case.name).join("sqlite"),
            case.script,
        );
        let mine = run(&ours, &area.join(case.name).join("inillucent"), case.script);
        let same = theirs == mine;
        if same {
            agreed += 1;
        }
        match (case.expect, same) {
            (Agrees, false) => wrong.push(format!(
                "{} [{}] was agreeing and no longer does
  sqlite: {:?}
  ours  : {:?}",
                case.name, case.kind, theirs, mine
            )),
            // **A case that starts agreeing is a failure too.** It is the whole
            // point of declaring the disposition: the row has to move, or the
            // next reader is told this construct is still broken.
            (Differs, true) => wrong.push(format!(
                "{} [{}] now agrees - move its row to Agrees",
                case.name, case.kind
            )),
            _ => {}
        }
    }
    assert!(
        wrong.is_empty(),
        "{} of {} agreed
{}",
        agreed,
        CASES.len(),
        wrong.join(
            "
"
        )
    );
    assert_eq!(
        agreed,
        CASES.len(),
        "only {agreed} of {} agreed, and this file names none that should not",
        CASES.len()
    );
}
