//! Every construct the second review probed, answered by both shells.
//!
//! Invariant: **each case declares whether it agrees with SQLite, and a case
//! that changes its mind fails.** A construct that starts agreeing fails this
//! test until its row is moved to `Agrees`, which is the same discipline
//! `new_engine_surface.rs` applies to *acceptance* applied here to *answers*.
//! Without it, a fix is invisible: the probe was a one-off script, so the nine
//! wrong answers the review found could each have been repaired and then
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
//! ## All of them agree
//!
//! `index.partial`, `index.expr` and `without.rowid.index` - the three
//! `CREATE INDEX` forms that were once left refused - are now built, and
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
    ///
    /// **Constructed by no case today**, and kept because the header this file
    /// carries is a list of the constructs that once differed: a variant that
    /// went when the last of them was fixed would take the vocabulary with it,
    /// and the next divergence would be recorded as a comment instead.
    #[allow(dead_code)]
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
