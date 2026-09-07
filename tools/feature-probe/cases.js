// The case table for the task-1858 feature probe.
//
// One row per feature a SQLite application can reach from SQL. `sql` is a whole
// script; it is run over a fresh database by both shells and every byte of both
// streams is compared. Nothing here reads the clock, the process id or the
// random generator, because a case that cannot answer the same way twice cannot
// answer a question about parity.

/** Builds one case. @param area - the section it lands in @param feature - the human name @param id - stable key @param sql - the whole script */
const c = (area, feature, id, sql, extra = {}) => ({ area, feature, id, sql, ...extra });

const T = 'CREATE TABLE t(id INTEGER PRIMARY KEY, a INTEGER, b TEXT);\nINSERT INTO t VALUES (1,10,\'p\'),(2,20,\'q\'),(3,30,\'r\'),(4,20,\'s\'),(5,50,\'t\');\n';
const TWO = T + 'CREATE TABLE u(id INTEGER PRIMARY KEY, tid INTEGER, w TEXT);\nINSERT INTO u VALUES (1,1,\'aa\'),(2,2,\'bb\'),(3,9,\'cc\');\n';

module.exports = [
  // ---------------------------------------------------------------- SELECT
  c('select', 'SELECT with WHERE, ORDER BY, LIMIT', 'select.basic', T + "SELECT id,a,b FROM t WHERE a >= 20 ORDER BY a DESC, id LIMIT 3;"),
  c('select', 'SELECT DISTINCT', 'select.distinct', T + "SELECT DISTINCT a FROM t ORDER BY a;"),
  c('select', 'GROUP BY with HAVING', 'select.groupby', T + "SELECT a, count(*) FROM t GROUP BY a HAVING count(*) > 1 ORDER BY a;"),
  c('select', 'GROUP BY expression', 'select.groupby.expr', T + "SELECT a/20, count(*) FROM t GROUP BY a/20 ORDER BY 1;"),
  c('select', 'ORDER BY ordinal', 'select.orderby.ordinal', T + "SELECT b, a FROM t ORDER BY 2, 1;"),
  c('select', 'ORDER BY NULLS FIRST / LAST', 'select.orderby.nulls', "CREATE TABLE t(a);\nINSERT INTO t VALUES (1),(NULL),(3);\nSELECT a FROM t ORDER BY a NULLS FIRST;\nSELECT a FROM t ORDER BY a NULLS LAST;"),
  c('select', 'LIMIT with OFFSET, both forms', 'select.limit.offset', T + "SELECT id FROM t ORDER BY id LIMIT 2 OFFSET 2;\nSELECT id FROM t ORDER BY id LIMIT 2, 2;"),
  c('select', 'VALUES as a statement and in FROM', 'select.values', "VALUES (1,'a'),(2,'b');\nSELECT * FROM (VALUES (3,'c'),(4,'d')) ORDER BY 1;"),
  c('select', 'SELECT with no FROM', 'select.nofrom', "SELECT 1, 'a', NULL, 2.5;"),
  c('select', 'Qualified star and table alias', 'select.star.qualified', TWO + "SELECT t.*, u.w FROM t JOIN u ON u.tid = t.id ORDER BY t.id;"),
  c('select', 'Column and expression aliases', 'select.alias', T + "SELECT a AS x, a*2 y FROM t ORDER BY x LIMIT 2;"),
  c('select', 'Aggregate over empty set', 'select.agg.empty', T + "SELECT count(*), sum(a), avg(a), max(a), min(a), total(a) FROM t WHERE a > 1000;"),
  c('select', 'GROUP BY with an ORDER BY on an aggregate', 'select.groupby.orderagg', T + "SELECT a, count(*) n FROM t GROUP BY a ORDER BY n DESC, a;"),
  c('select', 'Bare column with an aggregate', 'select.bare.column', T + "SELECT id, max(a) FROM t;"),
  c('select', 'DISTINCT over several columns', 'select.distinct.multi', T + "SELECT DISTINCT a, b FROM t ORDER BY a, b;"),

  // ------------------------------------------------------------------ JOIN
  c('join', 'INNER JOIN with ON', 'join.inner', TWO + "SELECT t.id, u.w FROM t JOIN u ON u.tid = t.id ORDER BY t.id;"),
  c('join', 'LEFT OUTER JOIN', 'join.left', TWO + "SELECT t.id, u.w FROM t LEFT JOIN u ON u.tid = t.id ORDER BY t.id;"),
  c('join', 'RIGHT OUTER JOIN', 'join.right', TWO + "SELECT t.id, u.w FROM t RIGHT JOIN u ON u.tid = t.id ORDER BY t.id, u.id;"),
  c('join', 'FULL OUTER JOIN', 'join.full', TWO + "SELECT t.id, u.w FROM t FULL JOIN u ON u.tid = t.id ORDER BY t.id, u.id;"),
  c('join', 'CROSS JOIN', 'join.cross', TWO + "SELECT count(*) FROM t CROSS JOIN u;"),
  c('join', 'NATURAL JOIN', 'join.natural', "CREATE TABLE a(k INTEGER, x TEXT);\nCREATE TABLE b(k INTEGER, y TEXT);\nINSERT INTO a VALUES (1,'p'),(2,'q');\nINSERT INTO b VALUES (1,'m'),(3,'n');\nSELECT * FROM a NATURAL JOIN b ORDER BY k;"),
  c('join', 'JOIN ... USING', 'join.using', "CREATE TABLE a(k INTEGER, x TEXT);\nCREATE TABLE b(k INTEGER, y TEXT);\nINSERT INTO a VALUES (1,'p'),(2,'q');\nINSERT INTO b VALUES (1,'m'),(3,'n');\nSELECT * FROM a JOIN b USING (k) ORDER BY k;"),
  c('join', 'Self join', 'join.self', T + "SELECT x.id, y.id FROM t x JOIN t y ON y.a = x.a AND y.id > x.id ORDER BY x.id;"),
  c('join', 'Four table join', 'join.four', TWO + "CREATE TABLE v(id INTEGER PRIMARY KEY, uid INTEGER);\nINSERT INTO v VALUES (1,1),(2,2);\nCREATE TABLE w(id INTEGER PRIMARY KEY, vid INTEGER);\nINSERT INTO w VALUES (1,1),(2,2);\nSELECT t.id, u.w, v.id, w.id FROM t JOIN u ON u.tid=t.id JOIN v ON v.uid=u.id JOIN w ON w.vid=v.id ORDER BY t.id;"),
  c('join', 'LEFT JOIN with a WHERE on the right table', 'join.left.where', TWO + "SELECT t.id FROM t LEFT JOIN u ON u.tid=t.id WHERE u.w IS NULL ORDER BY t.id;"),
  c('join', 'Comma join with a WHERE', 'join.comma', TWO + "SELECT t.id, u.w FROM t, u WHERE u.tid = t.id ORDER BY t.id;"),
  c('join', 'LEFT JOIN on a subquery', 'join.left.subquery', TWO + "SELECT t.id, s.n FROM t LEFT JOIN (SELECT tid, count(*) n FROM u GROUP BY tid) s ON s.tid = t.id ORDER BY t.id;"),

  // -------------------------------------------------------------- COMPOUND
  c('compound', 'UNION', 'compound.union', T + "SELECT a FROM t WHERE a<30 UNION SELECT a FROM t WHERE a>20 ORDER BY 1;"),
  c('compound', 'UNION ALL', 'compound.unionall', T + "SELECT a FROM t WHERE a<30 UNION ALL SELECT a FROM t WHERE a>20 ORDER BY 1;"),
  c('compound', 'EXCEPT', 'compound.except', T + "SELECT a FROM t EXCEPT SELECT a FROM t WHERE a>20 ORDER BY 1;"),
  c('compound', 'INTERSECT', 'compound.intersect', T + "SELECT a FROM t WHERE a<40 INTERSECT SELECT a FROM t WHERE a>15 ORDER BY 1;"),
  c('compound', 'Compound with LIMIT', 'compound.limit', T + "SELECT a FROM t UNION ALL SELECT a FROM t ORDER BY a LIMIT 3;"),
  c('compound', 'Three-way compound', 'compound.three', T + "SELECT 1 UNION SELECT 2 UNION ALL SELECT 3 ORDER BY 1;"),

  // -------------------------------------------------------------- SUBQUERY
  c('subquery', 'Scalar subquery', 'subq.scalar', T + "SELECT id, (SELECT max(a) FROM t) FROM t ORDER BY id LIMIT 2;"),
  c('subquery', 'IN with a subquery', 'subq.in', TWO + "SELECT id FROM t WHERE id IN (SELECT tid FROM u) ORDER BY id;"),
  c('subquery', 'NOT IN with NULLs', 'subq.notin.null', "CREATE TABLE a(x);\nCREATE TABLE b(y);\nINSERT INTO a VALUES (1),(2);\nINSERT INTO b VALUES (1),(NULL);\nSELECT x FROM a WHERE x NOT IN (SELECT y FROM b) ORDER BY x;"),
  c('subquery', 'EXISTS and NOT EXISTS', 'subq.exists', TWO + "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.tid=t.id) ORDER BY id;\nSELECT id FROM t WHERE NOT EXISTS (SELECT 1 FROM u WHERE u.tid=t.id) ORDER BY id;"),
  c('subquery', 'Correlated scalar subquery', 'subq.correlated', TWO + "SELECT t.id, (SELECT count(*) FROM u WHERE u.tid = t.id) FROM t ORDER BY t.id;"),
  c('subquery', 'Derived table in FROM', 'subq.derived', T + "SELECT s.a FROM (SELECT a FROM t WHERE a > 15) s ORDER BY s.a;"),
  c('subquery', 'Row value comparison', 'subq.rowvalue', T + "SELECT id FROM t WHERE (a, b) = (20, 'q');"),
  c('subquery', 'Row value IN', 'subq.rowvalue.in', T + "SELECT id FROM t WHERE (a,b) IN (VALUES (20,'q'),(30,'r')) ORDER BY id;"),
  c('subquery', 'Row value with a subquery', 'subq.rowvalue.subq', T + "SELECT id FROM t WHERE (a,b) = (SELECT a,b FROM t WHERE id=3);"),
  c('subquery', 'Subquery in SELECT list with a correlated LIMIT', 'subq.correlated.limit', TWO + "SELECT t.id, (SELECT w FROM u WHERE u.tid=t.id ORDER BY u.id LIMIT 1) FROM t ORDER BY t.id;"),

  // ------------------------------------------------------------------- CTE
  c('cte', 'WITH, one term', 'cte.simple', T + "WITH s AS (SELECT a FROM t WHERE a>15) SELECT count(*) FROM s;"),
  c('cte', 'WITH, several terms', 'cte.multi', T + "WITH s AS (SELECT a FROM t), r AS (SELECT a FROM s WHERE a>20) SELECT group_concat(a) FROM (SELECT a FROM r ORDER BY a);"),
  c('cte', 'WITH RECURSIVE', 'cte.recursive', "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<10) SELECT group_concat(x) FROM n;"),
  c('cte', 'Recursive tree walk', 'cte.recursive.tree', "CREATE TABLE org(id INTEGER PRIMARY KEY, parent INTEGER, name TEXT);\nINSERT INTO org VALUES (1,NULL,'a'),(2,1,'b'),(3,2,'c'),(4,1,'d');\nWITH RECURSIVE w(id,name,depth) AS (SELECT id,name,0 FROM org WHERE parent IS NULL UNION ALL SELECT o.id,o.name,w.depth+1 FROM org o JOIN w ON o.parent=w.id) SELECT group_concat(name || ':' || depth) FROM (SELECT name,depth FROM w ORDER BY id);"),
  c('cte', 'CTE column list', 'cte.column.list', "WITH s(p,q) AS (SELECT 1,2) SELECT p+q FROM s;"),
  c('cte', 'MATERIALIZED and NOT MATERIALIZED', 'cte.materialized', T + "WITH s AS MATERIALIZED (SELECT a FROM t) SELECT count(*) FROM s;\nWITH r AS NOT MATERIALIZED (SELECT a FROM t) SELECT count(*) FROM r;"),
  c('cte', 'WITH on INSERT', 'cte.on.insert', T + "CREATE TABLE d(a INTEGER);\nWITH s AS (SELECT a FROM t WHERE a>20) INSERT INTO d SELECT a FROM s;\nSELECT group_concat(a) FROM (SELECT a FROM d ORDER BY a);"),
  c('cte', 'WITH on UPDATE and DELETE', 'cte.on.dml', T + "WITH s AS (SELECT id FROM t WHERE a=20) UPDATE t SET b='z' WHERE id IN (SELECT id FROM s);\nWITH s AS (SELECT id FROM t WHERE a=50) DELETE FROM t WHERE id IN (SELECT id FROM s);\nSELECT group_concat(id || b) FROM (SELECT id,b FROM t ORDER BY id);"),

  // ---------------------------------------------------------------- WINDOW
  c('window', 'row_number, rank, dense_rank', 'win.rank', T + "SELECT id, row_number() OVER w, rank() OVER w, dense_rank() OVER w FROM t WINDOW w AS (ORDER BY a) ORDER BY id;"),
  c('window', 'ntile, cume_dist, percent_rank', 'win.dist', T + "SELECT id, ntile(2) OVER (ORDER BY a), round(cume_dist() OVER (ORDER BY a),3), round(percent_rank() OVER (ORDER BY a),3) FROM t ORDER BY id;"),
  c('window', 'lag and lead', 'win.lag', T + "SELECT id, lag(a) OVER (ORDER BY id), lead(a,1,-1) OVER (ORDER BY id) FROM t ORDER BY id;"),
  c('window', 'first_value, last_value, nth_value', 'win.value', T + "SELECT id, first_value(a) OVER w, last_value(a) OVER w, nth_value(a,2) OVER w FROM t WINDOW w AS (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING) ORDER BY id;"),
  c('window', 'PARTITION BY', 'win.partition', T + "SELECT id, a, count(*) OVER (PARTITION BY a) FROM t ORDER BY id;"),
  c('window', 'ROWS frame', 'win.frame.rows', T + "SELECT id, sum(a) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM t ORDER BY id;"),
  c('window', 'RANGE frame', 'win.frame.range', T + "SELECT id, sum(a) OVER (ORDER BY a RANGE BETWEEN 10 PRECEDING AND 10 FOLLOWING) FROM t ORDER BY id;"),
  c('window', 'GROUPS frame', 'win.frame.groups', T + "SELECT id, sum(a) OVER (ORDER BY a GROUPS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM t ORDER BY id;"),
  c('window', 'EXCLUDE clauses', 'win.frame.exclude', T + "SELECT id, sum(a) OVER (ORDER BY a ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING EXCLUDE CURRENT ROW) FROM t ORDER BY id;"),
  c('window', 'Aggregate with FILTER over a window', 'win.filter', T + "SELECT id, count(*) FILTER (WHERE a>15) OVER (ORDER BY id) FROM t ORDER BY id;"),
  c('window', 'Named WINDOW clause reused', 'win.named', T + "SELECT id, sum(a) OVER w, avg(a) OVER w FROM t WINDOW w AS (ORDER BY id) ORDER BY id;"),

  // ------------------------------------------------------------------- DML
  c('dml', 'INSERT VALUES, multi-row', 'dml.insert.values', "CREATE TABLE t(a,b);\nINSERT INTO t VALUES (1,'x'),(2,'y');\nSELECT count(*) FROM t;"),
  c('dml', 'INSERT ... SELECT', 'dml.insert.select', T + "CREATE TABLE d(a INTEGER);\nINSERT INTO d SELECT a FROM t WHERE a>15;\nSELECT count(*) FROM d;"),
  c('dml', 'INSERT DEFAULT VALUES', 'dml.insert.default', "CREATE TABLE t(a INTEGER DEFAULT 7, b TEXT DEFAULT 'z');\nINSERT INTO t DEFAULT VALUES;\nSELECT a,b FROM t;"),
  c('dml', 'INSERT OR IGNORE', 'dml.insert.or.ignore', T + "INSERT OR IGNORE INTO t VALUES (1,99,'zz');\nSELECT a FROM t WHERE id=1;"),
  c('dml', 'INSERT OR REPLACE', 'dml.insert.or.replace', T + "INSERT OR REPLACE INTO t VALUES (1,99,'zz');\nSELECT a,b FROM t WHERE id=1;"),
  c('dml', 'INSERT OR ROLLBACK inside a transaction', 'dml.insert.or.rollback', T + "BEGIN;\nINSERT INTO t VALUES (6,60,'u');\nINSERT OR ROLLBACK INTO t VALUES (1,99,'zz');\nSELECT count(*) FROM t;"),
  c('dml', 'INSERT OR FAIL', 'dml.insert.or.fail', T + "INSERT OR FAIL INTO t VALUES (6,60,'u'),(1,99,'zz'),(7,70,'v');\nSELECT count(*) FROM t;"),
  c('dml', 'INSERT OR ABORT', 'dml.insert.or.abort', T + "INSERT OR ABORT INTO t VALUES (6,60,'u'),(1,99,'zz');\nSELECT count(*) FROM t;"),
  c('dml', 'REPLACE INTO', 'dml.replace', T + "REPLACE INTO t VALUES (1,99,'zz');\nSELECT a FROM t WHERE id=1;"),
  c('dml', 'UPDATE with a WHERE', 'dml.update', T + "UPDATE t SET b='z' WHERE a=20;\nSELECT group_concat(b) FROM (SELECT b FROM t ORDER BY id);"),
  c('dml', 'UPDATE ... FROM', 'dml.update.from', TWO + "UPDATE t SET b = u.w FROM u WHERE u.tid = t.id;\nSELECT group_concat(b) FROM (SELECT b FROM t ORDER BY id);"),
  c('dml', 'UPDATE OR IGNORE onto a unique key', 'dml.update.or.ignore', "CREATE TABLE t(id INTEGER PRIMARY KEY, a TEXT UNIQUE);\nINSERT INTO t VALUES (1,'x'),(2,'y');\nUPDATE OR IGNORE t SET a='x' WHERE id=2;\nSELECT group_concat(a) FROM (SELECT a FROM t ORDER BY id);"),
  c('dml', 'UPDATE OR REPLACE onto a unique key', 'dml.update.or.replace', "CREATE TABLE t(id INTEGER PRIMARY KEY, a TEXT UNIQUE);\nINSERT INTO t VALUES (1,'x'),(2,'y');\nUPDATE OR REPLACE t SET a='x' WHERE id=2;\nSELECT group_concat(id||a) FROM (SELECT id,a FROM t ORDER BY id);"),
  c('dml', 'DELETE with a WHERE', 'dml.delete', T + "DELETE FROM t WHERE a=20;\nSELECT count(*) FROM t;"),
  c('dml', 'DELETE all rows', 'dml.delete.all', T + "DELETE FROM t;\nSELECT count(*) FROM t;"),
  c('dml', 'DELETE ... ORDER BY ... LIMIT', 'dml.delete.limit', T + "DELETE FROM t ORDER BY a DESC LIMIT 2;\nSELECT count(*) FROM t;"),
  c('dml', 'UPDATE ... ORDER BY ... LIMIT', 'dml.update.limit', T + "UPDATE t SET b='z' ORDER BY a DESC LIMIT 1;\nSELECT group_concat(b) FROM (SELECT b FROM t ORDER BY id);"),
  c('dml', 'RETURNING on INSERT', 'dml.returning.insert', "CREATE TABLE t(id INTEGER PRIMARY KEY, a);\nINSERT INTO t VALUES (1,10),(2,20) RETURNING id, a;"),
  c('dml', 'RETURNING on UPDATE and DELETE', 'dml.returning.updel', T + "UPDATE t SET a=a+1 WHERE id=1 RETURNING id, a;\nDELETE FROM t WHERE id=2 RETURNING id;"),
  c('dml', 'RETURNING with an expression', 'dml.returning.expr', "CREATE TABLE t(id INTEGER PRIMARY KEY, a);\nINSERT INTO t VALUES (1,10) RETURNING a*2 AS doubled;"),
  c('dml', 'INSERT into a WITHOUT ROWID table', 'dml.insert.without.rowid', "CREATE TABLE t(k TEXT PRIMARY KEY, v TEXT) WITHOUT ROWID;\nINSERT INTO t VALUES ('b','2'),('a','1');\nSELECT group_concat(k||v) FROM (SELECT k,v FROM t ORDER BY k);"),

  // ---------------------------------------------------------------- UPSERT
  c('upsert', 'ON CONFLICT DO NOTHING', 'upsert.nothing', T + "INSERT INTO t VALUES (1,99,'zz') ON CONFLICT(id) DO NOTHING;\nSELECT a FROM t WHERE id=1;"),
  c('upsert', 'ON CONFLICT DO UPDATE with excluded', 'upsert.update', T + "INSERT INTO t VALUES (1,99,'zz') ON CONFLICT(id) DO UPDATE SET a=excluded.a;\nSELECT a,b FROM t WHERE id=1;"),
  c('upsert', 'ON CONFLICT DO UPDATE with a WHERE', 'upsert.update.where', T + "INSERT INTO t VALUES (1,99,'zz') ON CONFLICT(id) DO UPDATE SET a=excluded.a WHERE t.a > 500;\nSELECT a FROM t WHERE id=1;"),
  c('upsert', 'ON CONFLICT on a secondary unique index', 'upsert.secondary', "CREATE TABLE t(id INTEGER PRIMARY KEY, k TEXT UNIQUE, v INTEGER);\nINSERT INTO t VALUES (1,'a',1);\nINSERT INTO t VALUES (2,'a',2) ON CONFLICT(k) DO UPDATE SET v=v+excluded.v;\nSELECT group_concat(id||k||v) FROM (SELECT id,k,v FROM t ORDER BY id);"),
  c('upsert', 'Upsert without a conflict target', 'upsert.no.target', T + "INSERT INTO t VALUES (1,99,'zz') ON CONFLICT DO NOTHING;\nSELECT a FROM t WHERE id=1;"),
  c('upsert', 'Two ON CONFLICT clauses', 'upsert.two.clauses', "CREATE TABLE t(id INTEGER PRIMARY KEY, k TEXT UNIQUE, v INTEGER);\nINSERT INTO t VALUES (1,'a',1);\nINSERT INTO t VALUES (1,'b',2) ON CONFLICT(k) DO UPDATE SET v=9 ON CONFLICT(id) DO UPDATE SET v=8;\nSELECT group_concat(id||k||v) FROM (SELECT id,k,v FROM t ORDER BY id);"),
  c('upsert', 'Upsert with RETURNING', 'upsert.returning', T + "INSERT INTO t VALUES (1,99,'zz') ON CONFLICT(id) DO UPDATE SET a=excluded.a RETURNING id, a;"),

  // ----------------------------------------------------------- CREATE TABLE
  c('ddl-table', 'CREATE TABLE with typed columns', 'ddl.table.basic', "CREATE TABLE t(a INTEGER, b TEXT, c REAL, d BLOB, e NUMERIC);\nSELECT sql FROM sqlite_schema WHERE name='t';"),
  c('ddl-table', 'CREATE TABLE IF NOT EXISTS', 'ddl.table.ifnotexists', "CREATE TABLE t(a);\nCREATE TABLE IF NOT EXISTS t(a);\nSELECT count(*) FROM sqlite_schema WHERE name='t';"),
  c('ddl-table', 'CREATE TABLE ... AS SELECT', 'ddl.table.as.select', T + "CREATE TABLE d AS SELECT a, b FROM t WHERE a>15;\nSELECT sql FROM sqlite_schema WHERE name='d';\nSELECT count(*) FROM d;"),
  c('ddl-table', 'WITHOUT ROWID', 'ddl.table.without.rowid', "CREATE TABLE t(k TEXT PRIMARY KEY, v) WITHOUT ROWID;\nSELECT sql FROM sqlite_schema WHERE name='t';"),
  c('ddl-table', 'STRICT', 'ddl.table.strict', "CREATE TABLE t(a INTEGER, b TEXT) STRICT;\nINSERT INTO t VALUES (1,'x');\nINSERT INTO t VALUES ('abc','y');\nSELECT count(*) FROM t;"),
  c('ddl-table', 'STRICT with ANY', 'ddl.table.strict.any', "CREATE TABLE t(a ANY) STRICT;\nINSERT INTO t VALUES (1),('x'),(2.5);\nSELECT group_concat(typeof(a)) FROM (SELECT typeof(a) a FROM t);"),
  c('ddl-table', 'Generated column, VIRTUAL', 'ddl.table.generated.virtual', "CREATE TABLE t(a INTEGER, b INTEGER GENERATED ALWAYS AS (a*2) VIRTUAL);\nINSERT INTO t(a) VALUES (3);\nSELECT a,b FROM t;"),
  c('ddl-table', 'Generated column, STORED', 'ddl.table.generated.stored', "CREATE TABLE t(a INTEGER, b INTEGER GENERATED ALWAYS AS (a*2) STORED);\nINSERT INTO t(a) VALUES (3);\nSELECT a,b FROM t;"),
  c('ddl-table', 'DEFAULT expressions', 'ddl.table.default', "CREATE TABLE t(a INTEGER DEFAULT 5, b TEXT DEFAULT 'q', c INTEGER DEFAULT (2+3));\nINSERT INTO t DEFAULT VALUES;\nSELECT a,b,c FROM t;"),
  c('ddl-table', 'Quoted and reserved-word identifiers', 'ddl.table.quoted', "CREATE TABLE t(\"left\" INTEGER, [right] TEXT, `order` INTEGER);\nINSERT INTO t VALUES (1,'x',2);\nSELECT \"left\", [right], `order` FROM t;"),
  c('ddl-table', 'Typeless columns', 'ddl.table.typeless', "CREATE TABLE t(a, b);\nINSERT INTO t VALUES (1,'x');\nSELECT typeof(a), typeof(b) FROM t;"),
  c('ddl-table', 'DROP TABLE and IF EXISTS', 'ddl.table.drop', "CREATE TABLE t(a);\nDROP TABLE t;\nDROP TABLE IF EXISTS t;\nSELECT count(*) FROM sqlite_schema WHERE name='t';"),
  c('ddl-table', 'Table-level PRIMARY KEY over two columns', 'ddl.table.pk.composite', "CREATE TABLE t(a INTEGER, b INTEGER, PRIMARY KEY(a,b));\nINSERT INTO t VALUES (1,1),(1,2);\nINSERT INTO t VALUES (1,1);\nSELECT count(*) FROM t;"),

  // ----------------------------------------------------------- CREATE INDEX
  c('ddl-index', 'CREATE INDEX', 'ddl.index.basic', T + "CREATE INDEX ia ON t(a);\nSELECT count(*) FROM t WHERE a=20;"),
  c('ddl-index', 'CREATE UNIQUE INDEX', 'ddl.index.unique', "CREATE TABLE t(a);\nCREATE UNIQUE INDEX ia ON t(a);\nINSERT INTO t VALUES (1);\nINSERT INTO t VALUES (1);\nSELECT count(*) FROM t;"),
  c('ddl-index', 'Descending index', 'ddl.index.desc', T + "CREATE INDEX ia ON t(a DESC);\nSELECT count(*) FROM t WHERE a >= 20;\nSELECT group_concat(a) FROM (SELECT a FROM t WHERE a > 15 ORDER BY a);"),
  c('ddl-index', 'Partial index', 'ddl.index.partial', T + "CREATE INDEX ia ON t(a) WHERE a > 15;\nSELECT count(*) FROM t WHERE a > 15;\nSELECT sql FROM sqlite_schema WHERE name='ia';"),
  c('ddl-index', 'Index on an expression', 'ddl.index.expr', T + "CREATE INDEX ia ON t(a*2);\nSELECT count(*) FROM t WHERE a*2 = 40;"),
  c('ddl-index', 'Index on a WITHOUT ROWID table', 'ddl.index.without.rowid', "CREATE TABLE t(k TEXT PRIMARY KEY, v INTEGER) WITHOUT ROWID;\nINSERT INTO t VALUES ('a',1),('b',2);\nCREATE INDEX iv ON t(v);\nSELECT k FROM t WHERE v=2;"),
  c('ddl-index', 'Index with COLLATE', 'ddl.index.collate', "CREATE TABLE t(a TEXT);\nINSERT INTO t VALUES ('A'),('a'),('B');\nCREATE INDEX ia ON t(a COLLATE NOCASE);\nSELECT group_concat(a) FROM (SELECT a FROM t WHERE a='a' COLLATE NOCASE ORDER BY rowid);"),
  c('ddl-index', 'Composite index', 'ddl.index.composite', T + "CREATE INDEX iab ON t(a,b);\nSELECT id FROM t WHERE a=20 AND b='s';"),
  c('ddl-index', 'DROP INDEX', 'ddl.index.drop', T + "CREATE INDEX ia ON t(a);\nDROP INDEX ia;\nSELECT count(*) FROM sqlite_schema WHERE name='ia';"),
  c('ddl-index', 'REINDEX', 'ddl.index.reindex', T + "CREATE INDEX ia ON t(a);\nREINDEX;\nREINDEX ia;\nSELECT count(*) FROM t WHERE a=20;"),
  c('ddl-index', 'INDEXED BY and NOT INDEXED', 'ddl.index.indexed.by', T + "CREATE INDEX ia ON t(a);\nSELECT id FROM t INDEXED BY ia WHERE a=20 ORDER BY id;\nSELECT id FROM t NOT INDEXED WHERE a=20 ORDER BY id;"),
  c('ddl-index', 'ANALYZE writes sqlite_stat1', 'ddl.index.analyze', T + "CREATE INDEX ia ON t(a);\nANALYZE;\nSELECT tbl, idx FROM sqlite_stat1 ORDER BY idx;"),

  // ------------------------------------------------------------ VIEWS, etc.
  c('ddl-view', 'CREATE VIEW', 'ddl.view.basic', T + "CREATE VIEW v AS SELECT a FROM t WHERE a>15;\nSELECT count(*) FROM v;"),
  c('ddl-view', 'CREATE VIEW with a column list', 'ddl.view.columns', T + "CREATE VIEW v(x) AS SELECT a FROM t;\nSELECT group_concat(x) FROM (SELECT x FROM v ORDER BY x);"),
  c('ddl-view', 'DROP VIEW', 'ddl.view.drop', T + "CREATE VIEW v AS SELECT a FROM t;\nDROP VIEW v;\nSELECT count(*) FROM sqlite_schema WHERE name='v';"),
  c('ddl-view', 'Writing through an INSTEAD OF trigger', 'ddl.view.instead.of', T + "CREATE VIEW v AS SELECT id,a FROM t;\nCREATE TRIGGER vi INSTEAD OF INSERT ON v BEGIN INSERT INTO t(id,a,b) VALUES (new.id,new.a,'via'); END;\nINSERT INTO v VALUES (9,90);\nSELECT id,a,b FROM t WHERE id=9;"),
  c('ddl-view', 'A view over a join', 'ddl.view.join', TWO + "CREATE VIEW v AS SELECT t.id, u.w FROM t JOIN u ON u.tid=t.id;\nSELECT group_concat(id||w) FROM (SELECT id,w FROM v ORDER BY id);"),

  // -------------------------------------------------------------- TRIGGERS
  c('ddl-trigger', 'AFTER INSERT trigger', 'trg.after.insert', T + "CREATE TABLE log(m TEXT);\nCREATE TRIGGER ti AFTER INSERT ON t BEGIN INSERT INTO log VALUES ('i' || new.id); END;\nINSERT INTO t VALUES (6,60,'u');\nSELECT m FROM log;"),
  c('ddl-trigger', 'BEFORE UPDATE trigger with OLD and NEW', 'trg.before.update', T + "CREATE TABLE log(m TEXT);\nCREATE TRIGGER tu BEFORE UPDATE ON t BEGIN INSERT INTO log VALUES (old.a || '->' || new.a); END;\nUPDATE t SET a=99 WHERE id=1;\nSELECT m FROM log;"),
  c('ddl-trigger', 'AFTER DELETE trigger', 'trg.after.delete', T + "CREATE TABLE log(m TEXT);\nCREATE TRIGGER td AFTER DELETE ON t BEGIN INSERT INTO log VALUES ('d' || old.id); END;\nDELETE FROM t WHERE id=2;\nSELECT m FROM log;"),
  c('ddl-trigger', 'Trigger WHEN clause', 'trg.when', T + "CREATE TABLE log(m TEXT);\nCREATE TRIGGER ti AFTER INSERT ON t WHEN new.a > 100 BEGIN INSERT INTO log VALUES ('big'); END;\nINSERT INTO t VALUES (6,60,'u'),(7,600,'v');\nSELECT count(*) FROM log;"),
  c('ddl-trigger', 'UPDATE OF column trigger', 'trg.update.of', T + "CREATE TABLE log(m TEXT);\nCREATE TRIGGER tu AFTER UPDATE OF b ON t BEGIN INSERT INTO log VALUES ('b'); END;\nUPDATE t SET a=1 WHERE id=1;\nUPDATE t SET b='z' WHERE id=1;\nSELECT count(*) FROM log;"),
  c('ddl-trigger', 'RAISE(ABORT) in a trigger', 'trg.raise.abort', T + "CREATE TRIGGER ti BEFORE INSERT ON t BEGIN SELECT RAISE(ABORT,'nope') WHERE new.a < 0; END;\nINSERT INTO t VALUES (6,-1,'u');\nSELECT count(*) FROM t;"),
  c('ddl-trigger', 'RAISE(IGNORE) in a trigger', 'trg.raise.ignore', T + "CREATE TRIGGER ti BEFORE INSERT ON t BEGIN SELECT RAISE(IGNORE) WHERE new.a < 0; END;\nINSERT INTO t VALUES (6,-1,'u');\nSELECT count(*) FROM t;"),
  c('ddl-trigger', 'Recursive triggers', 'trg.recursive', "PRAGMA recursive_triggers=ON;\nCREATE TABLE t(a INTEGER);\nCREATE TABLE log(m INTEGER);\nCREATE TRIGGER ti AFTER INSERT ON t WHEN new.a < 5 BEGIN INSERT INTO t VALUES (new.a+1); END;\nINSERT INTO t VALUES (1);\nSELECT count(*) FROM t;"),
  c('ddl-trigger', 'DROP TRIGGER', 'trg.drop', T + "CREATE TRIGGER ti AFTER INSERT ON t BEGIN SELECT 1; END;\nDROP TRIGGER ti;\nSELECT count(*) FROM sqlite_schema WHERE type='trigger';"),
  c('ddl-trigger', 'Trigger firing an UPDATE on another table', 'trg.cascade', TWO + "CREATE TRIGGER td AFTER DELETE ON t BEGIN DELETE FROM u WHERE tid = old.id; END;\nDELETE FROM t WHERE id=1;\nSELECT count(*) FROM u;"),

  // ----------------------------------------------------------- ALTER TABLE
  c('alter', 'ALTER TABLE RENAME TO', 'alter.rename.table', T + "ALTER TABLE t RENAME TO t2;\nSELECT count(*) FROM t2;"),
  c('alter', 'ALTER TABLE RENAME COLUMN', 'alter.rename.column', T + "ALTER TABLE t RENAME COLUMN b TO bb;\nSELECT bb FROM t WHERE id=1;"),
  c('alter', 'ALTER TABLE ADD COLUMN', 'alter.add.column', T + "ALTER TABLE t ADD COLUMN c INTEGER DEFAULT 7;\nSELECT c FROM t WHERE id=1;"),
  c('alter', 'ALTER TABLE DROP COLUMN', 'alter.drop.column', T + "ALTER TABLE t DROP COLUMN b;\nSELECT sql FROM sqlite_schema WHERE name='t';"),
  c('alter', 'Rename propagates into a view and a trigger', 'alter.rename.propagates', T + "CREATE VIEW v AS SELECT a FROM t;\nALTER TABLE t RENAME TO t2;\nSELECT sql FROM sqlite_schema WHERE name='v';"),

  // ----------------------------------------------------------- CONSTRAINTS
  c('constraint', 'NOT NULL', 'con.notnull', "CREATE TABLE t(a INTEGER NOT NULL);\nINSERT INTO t VALUES (NULL);\nSELECT count(*) FROM t;"),
  c('constraint', 'UNIQUE', 'con.unique', "CREATE TABLE t(a INTEGER UNIQUE);\nINSERT INTO t VALUES (1);\nINSERT INTO t VALUES (1);\nSELECT count(*) FROM t;"),
  c('constraint', 'CHECK on INSERT', 'con.check.insert', "CREATE TABLE t(a INTEGER CHECK (a > 0));\nINSERT INTO t VALUES (5);\nINSERT INTO t VALUES (-5);\nSELECT count(*) FROM t;"),
  c('constraint', 'CHECK on UPDATE', 'con.check.update', "CREATE TABLE t(a INTEGER CHECK (a > 0));\nINSERT INTO t VALUES (5);\nUPDATE t SET a=-1;\nSELECT a FROM t;"),
  c('constraint', 'Table-level CHECK over two columns', 'con.check.table', "CREATE TABLE t(a INTEGER, b INTEGER, CHECK (a < b));\nINSERT INTO t VALUES (1,2);\nINSERT INTO t VALUES (5,2);\nSELECT count(*) FROM t;"),
  c('constraint', 'PRIMARY KEY AUTOINCREMENT', 'con.autoincrement', "CREATE TABLE t(id INTEGER PRIMARY KEY AUTOINCREMENT, a);\nINSERT INTO t(a) VALUES (1);\nDELETE FROM t;\nINSERT INTO t(a) VALUES (2);\nSELECT id FROM t;\nSELECT seq FROM sqlite_sequence WHERE name='t';"),
  c('constraint', 'A constraint carrying its own ON CONFLICT', 'con.on.conflict.clause', "CREATE TABLE t(a TEXT UNIQUE ON CONFLICT IGNORE, b TEXT);\nINSERT INTO t VALUES ('x','1');\nINSERT INTO t VALUES ('x','2');\nSELECT group_concat(a||b) FROM (SELECT a,b FROM t ORDER BY rowid);"),
  c('constraint', 'NOT NULL ON CONFLICT REPLACE with a DEFAULT', 'con.notnull.replace', "CREATE TABLE t(a INTEGER, b TEXT NOT NULL ON CONFLICT REPLACE DEFAULT 'd');\nINSERT INTO t VALUES (1,NULL);\nSELECT a,b FROM t;"),
  c('constraint', 'Foreign key, immediate', 'con.fk.immediate', "PRAGMA foreign_keys=ON;\nCREATE TABLE p(id INTEGER PRIMARY KEY);\nCREATE TABLE ch(id INTEGER PRIMARY KEY, pid INTEGER REFERENCES p(id));\nINSERT INTO p VALUES (1);\nINSERT INTO ch VALUES (1,1);\nINSERT INTO ch VALUES (2,9);\nSELECT count(*) FROM ch;"),
  c('constraint', 'Foreign key ON DELETE CASCADE', 'con.fk.cascade', "PRAGMA foreign_keys=ON;\nCREATE TABLE p(id INTEGER PRIMARY KEY);\nCREATE TABLE ch(id INTEGER PRIMARY KEY, pid INTEGER REFERENCES p(id) ON DELETE CASCADE);\nINSERT INTO p VALUES (1),(2);\nINSERT INTO ch VALUES (1,1),(2,2);\nDELETE FROM p WHERE id=1;\nSELECT count(*) FROM ch;"),
  c('constraint', 'Foreign key ON DELETE SET NULL and SET DEFAULT', 'con.fk.setnull', "PRAGMA foreign_keys=ON;\nCREATE TABLE p(id INTEGER PRIMARY KEY);\nCREATE TABLE ch(id INTEGER PRIMARY KEY, pid INTEGER DEFAULT 2 REFERENCES p(id) ON DELETE SET NULL);\nCREATE TABLE ch2(id INTEGER PRIMARY KEY, pid INTEGER DEFAULT 2 REFERENCES p(id) ON DELETE SET DEFAULT);\nINSERT INTO p VALUES (1),(2);\nINSERT INTO ch VALUES (1,1);\nINSERT INTO ch2 VALUES (1,1);\nDELETE FROM p WHERE id=1;\nSELECT pid FROM ch;\nSELECT pid FROM ch2;"),
  c('constraint', 'Foreign key ON UPDATE CASCADE', 'con.fk.update.cascade', "PRAGMA foreign_keys=ON;\nCREATE TABLE p(id INTEGER PRIMARY KEY);\nCREATE TABLE ch(id INTEGER PRIMARY KEY, pid INTEGER REFERENCES p(id) ON UPDATE CASCADE);\nINSERT INTO p VALUES (1);\nINSERT INTO ch VALUES (1,1);\nUPDATE p SET id=5 WHERE id=1;\nSELECT pid FROM ch;"),
  c('constraint', 'Deferred foreign key', 'con.fk.deferred', "PRAGMA foreign_keys=ON;\nCREATE TABLE p(id INTEGER PRIMARY KEY);\nCREATE TABLE ch(id INTEGER PRIMARY KEY, pid INTEGER REFERENCES p(id) DEFERRABLE INITIALLY DEFERRED);\nBEGIN;\nINSERT INTO ch VALUES (1,1);\nINSERT INTO p VALUES (1);\nCOMMIT;\nSELECT count(*) FROM ch;"),
  c('constraint', 'PRAGMA foreign_key_check', 'con.fk.check', "CREATE TABLE p(id INTEGER PRIMARY KEY);\nCREATE TABLE ch(id INTEGER PRIMARY KEY, pid INTEGER REFERENCES p(id));\nINSERT INTO ch VALUES (1,9);\nPRAGMA foreign_key_check;"),
  c('constraint', 'PRAGMA foreign_key_list', 'con.fk.list', "CREATE TABLE p(id INTEGER PRIMARY KEY);\nCREATE TABLE ch(id INTEGER PRIMARY KEY, pid INTEGER REFERENCES p(id) ON DELETE CASCADE);\nPRAGMA foreign_key_list(ch);"),
  c('constraint', 'A row colliding on two unique indexes', 'con.two.unique', "CREATE TABLE t(id INTEGER PRIMARY KEY, a TEXT UNIQUE, b TEXT UNIQUE);\nINSERT INTO t VALUES (1,'x','1'),(2,'y','2');\nINSERT INTO t VALUES (3,'x','2');\nUPDATE t SET a='y' WHERE id=1;\nSELECT count(*) FROM t;"),

  // --------------------------------------------------------------- TYPES
  c('types', 'Affinity: text into INTEGER', 'types.affinity.int', "CREATE TABLE t(a INTEGER);\nINSERT INTO t VALUES ('42'),('abc'),(1.0);\nSELECT typeof(a), a FROM t;"),
  c('types', 'Affinity: number into TEXT', 'types.affinity.text', "CREATE TABLE t(a TEXT);\nINSERT INTO t VALUES (42),(1.5);\nSELECT typeof(a), a FROM t;"),
  c('types', 'Affinity: integer into REAL', 'types.affinity.real', "CREATE TABLE t(a REAL);\nINSERT INTO t VALUES (1),(2.5),('3');\nSELECT typeof(a), a FROM t;"),
  c('types', 'Affinity: BLOB column keeps the class', 'types.affinity.blob', "CREATE TABLE t(a BLOB);\nINSERT INTO t VALUES (1),('x'),(x'4142');\nSELECT typeof(a), quote(a) FROM t;"),
  c('types', 'Affinity: NUMERIC', 'types.affinity.numeric', "CREATE TABLE t(a NUMERIC);\nINSERT INTO t VALUES ('42'),('4.5'),('abc'),(7);\nSELECT typeof(a), a FROM t;"),
  c('types', 'Affinity through an INTEGER PRIMARY KEY', 'types.affinity.rowid', "CREATE TABLE t(id INTEGER PRIMARY KEY, a);\nINSERT INTO t VALUES ('42','x');\nSELECT id, typeof(id) FROM t;"),
  c('types', 'CAST between every class', 'types.cast', "SELECT CAST('42abc' AS INTEGER), CAST('3.9' AS INTEGER), CAST(3.9 AS INTEGER), CAST(42 AS TEXT), CAST('abc' AS REAL), typeof(CAST(1 AS BLOB)), CAST(x'4142' AS TEXT);"),
  c('types', 'Comparison across storage classes', 'types.compare.classes', "SELECT 1 < 'a', 'a' < x'00', NULL < 1, 2 < 10, '2' < '10';"),
  c('types', 'Integer overflow becomes real', 'types.int.overflow', "SELECT 9223372036854775807 + 1, typeof(9223372036854775807 + 1), 9223372036854775807, -9223372036854775808;"),
  c('types', 'Real formatting', 'types.real.format', "SELECT 1.0, 0.1+0.2, 1e300*1e300, 1.0/3.0, 2.0, -0.0;"),
  c('types', 'Integer division and modulo', 'types.int.division', "SELECT 7/2, -7/2, 7%3, -7%3, 7.0/2, 1/0, 1%0;"),
  c('types', 'Hex integer literals and blob literals', 'types.hex', "SELECT 0x10, 0xFF, quote(x'0011ff'), length(x'0011ff'), hex(x'0011ff');"),
  c('types', 'TRUE, FALSE and NULL keywords', 'types.boolean', "SELECT TRUE, FALSE, TRUE AND FALSE, NULL IS NULL, typeof(TRUE);"),
  c('types', 'Unicode text round trip', 'types.unicode', "CREATE TABLE t(a TEXT);\nINSERT INTO t VALUES ('café 日本語 🚀');\nSELECT a, length(a), octet_length(a), upper(a) FROM t;"),
  c('types', 'NULL ordering and arithmetic', 'types.null', "SELECT NULL+1, NULL||'a', NULL=NULL, NULL IS NULL, coalesce(NULL,2), max(NULL,1);"),
  c('types', 'A value wider than a page', 'types.large.value', "CREATE TABLE t(a TEXT);\nINSERT INTO t VALUES (hex(zeroblob(200000)));\nSELECT length(a), substr(a,1,4) FROM t;"),
  c('types', 'A blob wider than a page', 'types.large.blob', "CREATE TABLE t(a BLOB);\nINSERT INTO t VALUES (zeroblob(200000));\nSELECT length(a), typeof(a) FROM t;"),

  // ------------------------------------------------------------- OPERATORS
  c('operators', 'Concatenation and arithmetic', 'op.arith', "SELECT 1+2, 3-4, 5*6, 'a'||'b'||1, -(-3), +4;"),
  c('operators', 'Bitwise operators', 'op.bitwise', "SELECT 6 & 3, 6 | 3, 6 << 2, 6 >> 1, ~6;"),
  c('operators', 'IS, IS NOT, IS DISTINCT FROM', 'op.is', "SELECT 1 IS 1, NULL IS NULL, 1 IS NOT NULL, 1 IS DISTINCT FROM NULL, 1 IS NOT DISTINCT FROM 1;"),
  c('operators', 'BETWEEN and NOT BETWEEN', 'op.between', T + "SELECT group_concat(a) FROM (SELECT a FROM t WHERE a BETWEEN 20 AND 30 ORDER BY a);\nSELECT count(*) FROM t WHERE a NOT BETWEEN 20 AND 30;"),
  c('operators', 'IN with a list', 'op.in.list', T + "SELECT count(*) FROM t WHERE a IN (10,20,999);\nSELECT count(*) FROM t WHERE a NOT IN (10,20);"),
  c('operators', 'LIKE with and without ESCAPE', 'op.like', "SELECT 'abc' LIKE 'a%', 'ABC' LIKE 'a%', 'a_c' LIKE 'a\\_c' ESCAPE '\\', 'axc' LIKE 'a\\_c' ESCAPE '\\', 'abc' LIKE 'A_C';"),
  c('operators', 'GLOB', 'op.glob', "SELECT 'abc' GLOB 'a*', 'ABC' GLOB 'a*', 'abc' GLOB 'a[bx]c', 'abc' GLOB '?bc';"),
  c('operators', 'REGEXP without a registered function', 'op.regexp', "SELECT 'abc' REGEXP 'a.c';"),
  c('operators', 'CASE, both forms', 'op.case', T + "SELECT id, CASE WHEN a>25 THEN 'big' ELSE 'small' END, CASE a WHEN 10 THEN 'ten' ELSE 'other' END FROM t ORDER BY id;"),
  c('operators', 'JSON -> and ->>', 'op.json.arrow', "SELECT '{\"a\":{\"b\":2}}' -> '$.a' ->> '$.b', '[1,2,3]' ->> 1, typeof('{\"a\":1}' -> '$.a');"),
  c('operators', 'Operator precedence', 'op.precedence', "SELECT 2+3*4, (2+3)*4, NOT 0 AND 1, 1 OR 0 AND 0, 1 < 2 = 1;"),
  c('operators', 'String comparison and BINARY collation', 'op.string.compare', "SELECT 'a' < 'b', 'A' < 'a', 'a' = 'A', 'a' = 'A' COLLATE NOCASE;"),

  // ------------------------------------------------------------- COLLATION
  c('collation', 'BINARY, NOCASE and RTRIM', 'coll.builtin', "SELECT 'a'='A' COLLATE BINARY, 'a'='A' COLLATE NOCASE, 'a '='a' COLLATE RTRIM;"),
  c('collation', 'COLLATE in a column definition', 'coll.column', "CREATE TABLE t(a TEXT COLLATE NOCASE);\nINSERT INTO t VALUES ('A'),('a');\nSELECT count(*) FROM t WHERE a='a';"),
  c('collation', 'COLLATE in ORDER BY', 'coll.orderby', "CREATE TABLE t(a TEXT);\nINSERT INTO t VALUES ('B'),('a'),('A'),('b');\nSELECT group_concat(a) FROM (SELECT a FROM t ORDER BY a COLLATE NOCASE, a);"),
  c('collation', 'A unique index under NOCASE', 'coll.unique.nocase', "CREATE TABLE t(a TEXT COLLATE NOCASE UNIQUE);\nINSERT INTO t VALUES ('A');\nINSERT INTO t VALUES ('a');\nSELECT count(*) FROM t;"),
  c('collation', 'PRAGMA collation_list', 'coll.list', "PRAGMA collation_list;"),

  // --------------------------------------------------------- CORE FUNCTIONS
  c('fn-core', 'abs, sign, round, max, min', 'fn.numeric', "SELECT abs(-3), sign(-3), round(2.567,2), round(2.5), max(1,9,3), min(1,9,3);"),
  c('fn-core', 'length, substr, instr, replace', 'fn.string1', "SELECT length('abcd'), substr('abcdef',2,3), substr('abcdef',-2), instr('abcdef','cd'), replace('aXbXc','X','-');"),
  c('fn-core', 'upper, lower, trim, ltrim, rtrim', 'fn.string2', "SELECT upper('aBc'), lower('aBc'), trim('  x  '), ltrim('xxaxx','x'), rtrim('xxaxx','x'), trim('xxaxx','x');"),
  c('fn-core', 'printf and format', 'fn.printf', "SELECT printf('%05.2f',3.14159), printf('%d-%s',42,'x'), printf('%08.3d',42), format('%.3f',1.0/3), printf('%w','x');"),
  c('fn-core', 'quote, hex, unhex, char, unicode', 'fn.encoding', "SELECT quote('a''b'), quote(NULL), hex('ab'), unhex('4142'), char(65,66), unicode('A');"),
  c('fn-core', 'coalesce, ifnull, nullif, iif', 'fn.conditional', "SELECT coalesce(NULL,NULL,3), ifnull(NULL,2), nullif(1,1), nullif(1,2), iif(1>0,'y','n');"),
  c('fn-core', 'typeof, likelihood, likely, unlikely', 'fn.meta', "SELECT typeof(1), typeof('a'), typeof(1.5), typeof(NULL), typeof(x'41'), likelihood(1,0.5), likely(1), unlikely(1);"),
  c('fn-core', 'zeroblob, randomblob length, octet_length', 'fn.blob', "SELECT length(zeroblob(10)), typeof(zeroblob(10)), length(randomblob(8)), octet_length('café');"),
  c('fn-core', 'changes, total_changes and last_insert_rowid', 'fn.changes', "CREATE TABLE t(id INTEGER PRIMARY KEY, a);\nINSERT INTO t VALUES (1,1),(2,2);\nSELECT changes(), total_changes(), last_insert_rowid();\nUPDATE t SET a=9;\nSELECT changes(), total_changes();"),
  c('fn-core', 'concat and concat_ws', 'fn.concat', "SELECT concat('a','b',1), concat_ws('-','a','b',NULL,'c');"),
  c('fn-core', 'glob and like as functions', 'fn.match', "SELECT like('a%','abc'), glob('a*','abc'), like('a\\_c','a_c','\\');"),
  c('fn-core', 'sqlite_version and sqlite_source_id exist', 'fn.version', "SELECT length(sqlite_version())>0, typeof(sqlite_version());"),
  c('fn-core', 'load_extension', 'fn.load.extension', "SELECT load_extension('nosuch');"),

  // ---------------------------------------------------- AGGREGATE FUNCTIONS
  c('fn-agg', 'count, sum, total, avg', 'fn.agg.basic', T + "SELECT count(*), count(b), sum(a), total(a), avg(a), typeof(sum(a)), typeof(total(a)) FROM t;"),
  c('fn-agg', 'max, min, group_concat, string_agg', 'fn.agg.text', T + "SELECT max(a), min(a), group_concat(b), group_concat(b,'-'), string_agg(b,'+') FROM (SELECT a,b FROM t ORDER BY id);"),
  c('fn-agg', 'DISTINCT inside an aggregate', 'fn.agg.distinct', T + "SELECT count(DISTINCT a), sum(DISTINCT a), group_concat(DISTINCT a) FROM t;"),
  c('fn-agg', 'FILTER on an aggregate', 'fn.agg.filter', T + "SELECT count(*) FILTER (WHERE a>15), sum(a) FILTER (WHERE b<'s') FROM t;"),
  c('fn-agg', 'group_concat with an ORDER BY argument', 'fn.agg.orderby', T + "SELECT group_concat(b ORDER BY a DESC) FROM t;"),
  c('fn-agg', 'Aggregates over NULLs', 'fn.agg.null', "CREATE TABLE t(a);\nINSERT INTO t VALUES (1),(NULL),(3);\nSELECT count(*), count(a), sum(a), avg(a), max(a), min(a), total(a) FROM t;"),

  // ---------------------------------------------------- DATE/TIME FUNCTIONS
  c('fn-time', 'date, time, datetime on a fixed instant', 'fn.time.basic', "SELECT date('2024-03-01 12:34:56'), time('2024-03-01 12:34:56'), datetime('2024-03-01 12:34:56');"),
  c('fn-time', 'julianday and unixepoch', 'fn.time.epoch', "SELECT julianday('2024-03-01'), unixepoch('2024-03-01'), datetime(1709251200,'unixepoch'), datetime(2460370.5);"),
  c('fn-time', 'strftime, the whole specifier table', 'fn.time.strftime', "SELECT strftime('%Y|%m|%d|%H|%M|%S|%j|%s|%w|%W|%U|%V|%G|%g|%e|%F|%I|%k|%l|%p|%P|%R|%T|%u|%%','2024-03-01 09:05:07');"),
  c('fn-time', 'strftime fractional seconds', 'fn.time.frac', "SELECT strftime('%f|%J','2024-03-01 09:05:07.250');"),
  c('fn-time', 'Modifiers: days, months, years', 'fn.time.mod1', "SELECT date('2024-01-31','+1 month'), date('2024-03-01','-1 day'), date('2024-03-01','+1 year'), datetime('2024-03-01','+90 minutes');"),
  c('fn-time', 'Modifiers: start of, weekday', 'fn.time.mod2', "SELECT date('2024-03-15','start of month'), date('2024-03-15','start of year'), datetime('2024-03-15 10:00','start of day'), date('2024-03-01','weekday 0');"),
  c('fn-time', 'Modifiers: ceiling, floor, subsec, auto', 'fn.time.mod3', "SELECT datetime('2024-03-01 12:00:00','subsec'), date('2460370.5','auto'), datetime('2024-03-01','+1 day','+1 hour');"),
  c('fn-time', 'timediff', 'fn.time.timediff', "SELECT timediff('2024-03-01','2023-01-15');"),
  c('fn-time', 'Julian day round trip', 'fn.time.roundtrip', "SELECT datetime(julianday('2024-03-01 12:34:56')), date(unixepoch('2024-03-01'),'unixepoch');"),

  // -------------------------------------------------------- MATH FUNCTIONS
  c('fn-math', 'Trigonometric functions', 'fn.math.trig', "SELECT round(sin(1),6), round(cos(1),6), round(tan(1),6), round(asin(0.5),6), round(acos(0.5),6), round(atan(1),6), round(atan2(1,2),6);"),
  c('fn-math', 'Hyperbolic functions', 'fn.math.hyp', "SELECT round(sinh(1),6), round(cosh(1),6), round(tanh(1),6), round(asinh(1),6), round(acosh(2),6), round(atanh(0.5),6);"),
  c('fn-math', 'Logs, powers and roots', 'fn.math.log', "SELECT round(ln(2),6), round(log(10,100),6), round(log2(8),6), round(log10(1000),6), round(exp(1),6), round(pow(2,10),6), round(power(2,3),6), round(sqrt(2),6);"),
  c('fn-math', 'ceil, floor, trunc, mod, pi, degrees, radians', 'fn.math.round', "SELECT ceil(1.2), ceiling(1.2), floor(1.8), trunc(1.8), trunc(-1.8), mod(7,3), round(pi(),6), round(degrees(pi()),6), round(radians(180),6);"),

  // -------------------------------------------------------- JSON FUNCTIONS
  c('fn-json', 'json and json_valid', 'fn.json.valid', "SELECT json(' {\"a\":1} '), json_valid('{}'), json_valid('{'), json_valid('{a:1}',6), json_valid('{}',4);"),
  c('fn-json', 'json_array, json_object, json_quote', 'fn.json.build', "SELECT json_array(1,'a',NULL,json_array(2)), json_object('a',1,'b','x'), json_quote('a\"b');"),
  c('fn-json', 'json_extract and json_type', 'fn.json.extract', "SELECT json_extract('{\"a\":{\"b\":[1,2]}}','$.a.b[1]'), json_extract('{\"a\":1}','$.a','$.a'), json_type('{\"a\":[1]}','$.a'), json_type('null');"),
  c('fn-json', 'json_insert, json_replace, json_set, json_remove', 'fn.json.modify', "SELECT json_insert('{\"a\":1}','$.b',2), json_replace('{\"a\":1}','$.a',9), json_set('{\"a\":1}','$.a',9,'$.c',3), json_remove('{\"a\":1,\"b\":2}','$.a');"),
  c('fn-json', 'json_patch, json_array_length, json_pretty', 'fn.json.misc', "SELECT json_patch('{\"a\":1}','{\"b\":2}'), json_array_length('[1,2,3]'), json_array_length('{\"a\":[1,2]}','$.a'), json_pretty('{\"a\":1}');"),
  c('fn-json', 'json_group_array and json_group_object', 'fn.json.group', T + "SELECT json_group_array(a), json_group_object(b,a) FROM (SELECT a,b FROM t ORDER BY id);"),
  c('fn-json', 'json_each', 'fn.json.each', "SELECT key, value, type FROM json_each('{\"a\":1,\"b\":[2,3]}') ORDER BY key;"),
  c('fn-json', 'json_tree', 'fn.json.tree', "SELECT count(*), group_concat(type) FROM (SELECT type FROM json_tree('{\"a\":[1,2]}') ORDER BY id);"),
  c('fn-json', 'jsonb round trip', 'fn.json.b', "SELECT json(jsonb('{\"a\":1}')), typeof(jsonb('{\"a\":1}')), jsonb_extract(jsonb('{\"a\":2}'),'$.a');"),
  c('fn-json', 'json_error_position', 'fn.json.error', "SELECT json_error_position('{\"a\":1}'), json_error_position('{\"a\":}');"),
  c('fn-json', 'JSON stored in a column and queried', 'fn.json.column', "CREATE TABLE t(d TEXT);\nINSERT INTO t VALUES ('{\"n\":1}'),('{\"n\":2}');\nSELECT group_concat(json_extract(d,'$.n')) FROM (SELECT d FROM t ORDER BY rowid);\nSELECT count(*) FROM t WHERE d->>'$.n' > 1;"),

  // -------------------------------------------------- TABLE-VALUED FUNCTIONS
  c('tvf', 'generate_series', 'tvf.series', "SELECT group_concat(value) FROM generate_series(1,5);\nSELECT count(*) FROM generate_series(1,10,2);"),
  c('tvf', 'generate_series with LIMIT and no stop', 'tvf.series.limit', "SELECT group_concat(value) FROM (SELECT value FROM generate_series(1) LIMIT 3);"),
  c('tvf', 'pragma_table_info as a table', 'tvf.pragma', T + "SELECT group_concat(name) FROM pragma_table_info('t');"),
  c('tvf', 'pragma_index_list and pragma_index_info', 'tvf.pragma.index', T + "CREATE INDEX ia ON t(a);\nSELECT group_concat(name) FROM pragma_index_list('t');\nSELECT group_concat(name) FROM pragma_index_info('ia');"),
  c('tvf', 'json_each joined against a table', 'tvf.json.join', "CREATE TABLE t(id INTEGER, d TEXT);\nINSERT INTO t VALUES (1,'[1,2]'),(2,'[3]');\nSELECT group_concat(t.id || ':' || j.value) FROM t, json_each(t.d) j;"),

  // --------------------------------------------------------------- PRAGMAS
  c('pragma', 'PRAGMA table_info', 'prag.table.info', T + "PRAGMA table_info(t);"),
  c('pragma', 'PRAGMA table_xinfo with a generated column', 'prag.table.xinfo', "CREATE TABLE t(a INTEGER, b INTEGER GENERATED ALWAYS AS (a*2) VIRTUAL);\nPRAGMA table_xinfo(t);"),
  c('pragma', 'PRAGMA table_list', 'prag.table.list', T + "PRAGMA table_list;"),
  c('pragma', 'PRAGMA index_list, index_info, index_xinfo', 'prag.index', T + "CREATE INDEX ia ON t(a);\nPRAGMA index_list(t);\nPRAGMA index_info(ia);\nPRAGMA index_xinfo(ia);"),
  c('pragma', 'PRAGMA database_list', 'prag.database.list', "PRAGMA database_list;"),
  c('pragma', 'PRAGMA integrity_check and quick_check', 'prag.integrity', T + "PRAGMA integrity_check;\nPRAGMA quick_check;"),
  c('pragma', 'PRAGMA user_version and application_id', 'prag.versions', "PRAGMA user_version=7;\nPRAGMA user_version;\nPRAGMA application_id=42;\nPRAGMA application_id;"),
  c('pragma', 'PRAGMA page_size, page_count, freelist_count', 'prag.page', T + "PRAGMA page_size;\nPRAGMA page_count;\nPRAGMA freelist_count;"),
  c('pragma', 'PRAGMA cache_size and synchronous', 'prag.cache', "PRAGMA cache_size=-4000;\nPRAGMA cache_size;\nPRAGMA synchronous=FULL;\nPRAGMA synchronous;"),
  c('pragma', 'PRAGMA journal_mode', 'prag.journal.mode', "PRAGMA journal_mode;\nPRAGMA journal_mode=WAL;\nPRAGMA journal_mode=DELETE;\nPRAGMA journal_mode=MEMORY;"),
  c('pragma', 'PRAGMA locking_mode and temp_store', 'prag.locking', "PRAGMA locking_mode;\nPRAGMA temp_store;\nPRAGMA temp_store=MEMORY;\nPRAGMA temp_store;"),
  c('pragma', 'PRAGMA encoding', 'prag.encoding', "PRAGMA encoding;"),
  c('pragma', 'PRAGMA auto_vacuum and incremental_vacuum', 'prag.autovacuum', "PRAGMA auto_vacuum;\nPRAGMA auto_vacuum=INCREMENTAL;\nPRAGMA auto_vacuum;\nPRAGMA incremental_vacuum(1);"),
  c('pragma', 'PRAGMA secure_delete and cell_size_check', 'prag.secure', "PRAGMA secure_delete;\nPRAGMA secure_delete=ON;\nPRAGMA cell_size_check;"),
  c('pragma', 'PRAGMA foreign_keys, defer_foreign_keys, ignore_check_constraints', 'prag.fk.switches', "PRAGMA foreign_keys;\nPRAGMA foreign_keys=ON;\nPRAGMA foreign_keys;\nPRAGMA defer_foreign_keys=ON;\nPRAGMA defer_foreign_keys;\nPRAGMA ignore_check_constraints=ON;"),
  c('pragma', 'PRAGMA recursive_triggers and legacy_alter_table', 'prag.trigger.switches', "PRAGMA recursive_triggers;\nPRAGMA recursive_triggers=ON;\nPRAGMA recursive_triggers;\nPRAGMA legacy_alter_table;"),
  c('pragma', 'PRAGMA case_sensitive_like and reverse_unordered_selects', 'prag.like.switches', "PRAGMA case_sensitive_like=ON;\nSELECT 'ABC' LIKE 'a%';\nPRAGMA reverse_unordered_selects;"),
  c('pragma', 'PRAGMA schema_version and data_version', 'prag.schema.version', "CREATE TABLE t(a);\nPRAGMA schema_version;\nPRAGMA data_version;"),
  c('pragma', 'PRAGMA optimize, shrink_memory, wal_checkpoint', 'prag.maintenance', T + "PRAGMA optimize;\nPRAGMA shrink_memory;\nPRAGMA wal_checkpoint;"),
  c('pragma', 'PRAGMA busy_timeout, threads, query_only', 'prag.runtime', "PRAGMA busy_timeout=5000;\nPRAGMA busy_timeout;\nPRAGMA threads;\nPRAGMA query_only;"),
  c('pragma', 'PRAGMA mmap_size, soft_heap_limit, hard_heap_limit', 'prag.memory', "PRAGMA mmap_size;\nPRAGMA soft_heap_limit;\nPRAGMA hard_heap_limit;"),
  c('pragma', 'PRAGMA max_page_count', 'prag.max.page', "PRAGMA max_page_count;\nPRAGMA max_page_count=1000;\nPRAGMA max_page_count;"),
  c('pragma', 'PRAGMA trusted_schema and writable_schema', 'prag.schema.switches', "PRAGMA trusted_schema;\nPRAGMA writable_schema;\nPRAGMA writable_schema=ON;\nPRAGMA writable_schema;"),
  c('pragma', 'PRAGMA analysis_limit and automatic_index', 'prag.planner', "PRAGMA analysis_limit;\nPRAGMA analysis_limit=100;\nPRAGMA automatic_index;"),
  c('pragma', 'PRAGMA module_list, function_list, pragma_list exist', 'prag.introspect', "SELECT count(*)>0 FROM pragma_module_list;\nSELECT count(*)>0 FROM pragma_function_list;\nSELECT count(*)>0 FROM pragma_pragma_list;"),
  c('pragma', 'PRAGMA compile_options exists', 'prag.compile.options', "SELECT count(*)>0 FROM pragma_compile_options;"),
  c('pragma', 'PRAGMA schema.table_info qualified by database', 'prag.qualified', T + "PRAGMA main.table_info(t);"),

  // --------------------------------------------------------------- EXPLAIN
  c('explain', 'EXPLAIN QUERY PLAN, full scan', 'xp.scan', T + "EXPLAIN QUERY PLAN SELECT * FROM t WHERE b='q';"),
  c('explain', 'EXPLAIN QUERY PLAN, index search', 'xp.index', T + "CREATE INDEX ia ON t(a);\nEXPLAIN QUERY PLAN SELECT id FROM t WHERE a=20;"),
  c('explain', 'EXPLAIN QUERY PLAN, join', 'xp.join', TWO + "EXPLAIN QUERY PLAN SELECT t.id FROM t JOIN u ON u.tid=t.id;"),
  c('explain', 'EXPLAIN QUERY PLAN, sort', 'xp.sort', T + "EXPLAIN QUERY PLAN SELECT a FROM t ORDER BY b;"),
  c('explain', 'EXPLAIN, the bytecode form', 'xp.bytecode', T + "EXPLAIN SELECT 1;"),

  // ---------------------------------------------------------- TRANSACTIONS
  c('txn', 'BEGIN, COMMIT', 'txn.commit', "CREATE TABLE t(a);\nBEGIN;\nINSERT INTO t VALUES (1);\nCOMMIT;\nSELECT count(*) FROM t;"),
  c('txn', 'BEGIN, ROLLBACK', 'txn.rollback', "CREATE TABLE t(a);\nBEGIN;\nINSERT INTO t VALUES (1);\nROLLBACK;\nSELECT count(*) FROM t;"),
  c('txn', 'DEFERRED, IMMEDIATE and EXCLUSIVE', 'txn.modes', "CREATE TABLE t(a);\nBEGIN DEFERRED;\nCOMMIT;\nBEGIN IMMEDIATE;\nCOMMIT;\nBEGIN EXCLUSIVE;\nCOMMIT;\nSELECT 'ok';"),
  c('txn', 'SAVEPOINT, RELEASE, ROLLBACK TO', 'txn.savepoint', "CREATE TABLE t(a);\nBEGIN;\nINSERT INTO t VALUES (1);\nSAVEPOINT s;\nINSERT INTO t VALUES (2);\nROLLBACK TO s;\nINSERT INTO t VALUES (3);\nRELEASE s;\nCOMMIT;\nSELECT group_concat(a) FROM (SELECT a FROM t ORDER BY a);"),
  c('txn', 'Nested savepoints', 'txn.savepoint.nested', "CREATE TABLE t(a);\nSAVEPOINT s1;\nINSERT INTO t VALUES (1);\nSAVEPOINT s2;\nINSERT INTO t VALUES (2);\nROLLBACK TO s2;\nRELEASE s1;\nSELECT group_concat(a) FROM (SELECT a FROM t ORDER BY a);"),
  c('txn', 'DDL rolled back', 'txn.ddl.rollback', "CREATE TABLE t(a);\nBEGIN;\nCREATE TABLE u(a);\nROLLBACK;\nSELECT count(*) FROM sqlite_schema WHERE name='u';"),
  c('txn', 'DROP TABLE rolled back', 'txn.drop.rollback', T + "BEGIN;\nDROP TABLE t;\nROLLBACK;\nSELECT count(*) FROM t;"),
  c('txn', 'A statement failing part way leaves nothing behind', 'txn.statement.atomic', "CREATE TABLE t(a INTEGER PRIMARY KEY);\nINSERT INTO t VALUES (3);\nINSERT INTO t SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3;\nSELECT group_concat(a) FROM (SELECT a FROM t ORDER BY a);"),
  c('txn', 'COMMIT with no transaction', 'txn.commit.none', "COMMIT;"),
  c('txn', 'Nested BEGIN', 'txn.begin.nested', "BEGIN;\nBEGIN;\nCOMMIT;"),
  c('txn', 'END as a synonym for COMMIT', 'txn.end', "CREATE TABLE t(a);\nBEGIN;\nINSERT INTO t VALUES (1);\nEND;\nSELECT count(*) FROM t;"),

  // --------------------------------------------------------------- ATTACH
  c('attach', 'ATTACH a second file and query across it', 'att.basic', "ATTACH DATABASE 'second.db' AS s;\nCREATE TABLE s.x(a);\nINSERT INTO s.x VALUES (1),(2);\nSELECT count(*) FROM s.x;\nDETACH DATABASE s;"),
  c('attach', 'Join across two databases', 'att.join', T + "ATTACH DATABASE 'second.db' AS s;\nCREATE TABLE s.x(id INTEGER, w TEXT);\nINSERT INTO s.x VALUES (1,'aa'),(2,'bb');\nSELECT group_concat(i||w) FROM (SELECT t.id AS i, x.w AS w FROM t JOIN s.x x ON x.id=t.id ORDER BY t.id);"),
  c('attach', 'A transaction spanning two databases', 'att.txn', "ATTACH DATABASE 'second.db' AS s;\nCREATE TABLE main.a(x);\nCREATE TABLE s.b(x);\nBEGIN;\nINSERT INTO main.a VALUES (1);\nINSERT INTO s.b VALUES (1);\nCOMMIT;\nSELECT (SELECT count(*) FROM main.a) + (SELECT count(*) FROM s.b);"),
  c('attach', 'A rollback spanning two databases', 'att.rollback', "ATTACH DATABASE 'second.db' AS s;\nCREATE TABLE main.a(x);\nCREATE TABLE s.b(x);\nBEGIN;\nINSERT INTO main.a VALUES (1);\nINSERT INTO s.b VALUES (1);\nROLLBACK;\nSELECT (SELECT count(*) FROM main.a) + (SELECT count(*) FROM s.b);"),
  c('attach', 'ATTACH an in-memory database', 'att.memory', "ATTACH DATABASE ':memory:' AS m;\nCREATE TABLE m.x(a);\nINSERT INTO m.x VALUES (1);\nSELECT count(*) FROM m.x;"),
  c('attach', 'PRAGMA database_list after ATTACH', 'att.database.list', "ATTACH DATABASE 'second.db' AS s;\nSELECT count(*) FROM pragma_database_list;"),

  // ----------------------------------------------------------------- TEMP
  c('temp', 'CREATE TEMP TABLE', 'temp.table', "CREATE TEMP TABLE tt(a);\nINSERT INTO tt VALUES (1),(2);\nSELECT count(*) FROM tt;\nSELECT count(*) FROM temp.tt;"),
  c('temp', 'CREATE TEMP VIEW and TEMP TRIGGER', 'temp.view.trigger', "CREATE TABLE t(a);\nCREATE TEMP VIEW v AS SELECT a FROM t;\nCREATE TEMP TRIGGER tg AFTER INSERT ON t BEGIN SELECT 1; END;\nINSERT INTO t VALUES (1);\nSELECT count(*) FROM v;"),
  c('temp', 'Temporary table is not in the main schema', 'temp.not.main', "CREATE TEMP TABLE tt(a);\nSELECT count(*) FROM sqlite_schema WHERE name='tt';\nSELECT count(*) FROM sqlite_temp_schema WHERE name='tt';"),
  c('temp', 'CREATE TEMP TABLE ... AS SELECT', 'temp.as.select', T + "CREATE TEMP TABLE tt AS SELECT a FROM t WHERE a>15;\nSELECT count(*) FROM tt;"),
  c('temp', 'A temp table shadowing a main table', 'temp.shadow', T + "CREATE TEMP TABLE t(z);\nINSERT INTO t VALUES (1);\nSELECT count(*) FROM t;\nSELECT count(*) FROM main.t;"),

  // ----------------------------------------------------------------- FTS5
  c('fts5', 'CREATE VIRTUAL TABLE ... fts5 and MATCH', 'fts.basic', "CREATE VIRTUAL TABLE f USING fts5(body);\nINSERT INTO f VALUES ('the quick brown fox'),('a slow green turtle');\nSELECT count(*) FROM f WHERE f MATCH 'quick';\nSELECT body FROM f WHERE f MATCH 'turtle';"),
  c('fts5', 'FTS5 phrase and NEAR queries', 'fts.phrase', "CREATE VIRTUAL TABLE f USING fts5(body);\nINSERT INTO f VALUES ('the quick brown fox'),('brown the quick fox');\nSELECT count(*) FROM f WHERE f MATCH '\"quick brown\"';\nSELECT count(*) FROM f WHERE f MATCH 'NEAR(quick fox, 2)';"),
  c('fts5', 'FTS5 boolean operators and prefix', 'fts.boolean', "CREATE VIRTUAL TABLE f USING fts5(body);\nINSERT INTO f VALUES ('quick brown fox'),('slow green turtle');\nSELECT count(*) FROM f WHERE f MATCH 'quick AND fox';\nSELECT count(*) FROM f WHERE f MATCH 'quick OR turtle';\nSELECT count(*) FROM f WHERE f MATCH 'quick NOT fox';\nSELECT count(*) FROM f WHERE f MATCH 'qui*';"),
  c('fts5', 'FTS5 bm25 ranking', 'fts.bm25', "CREATE VIRTUAL TABLE f USING fts5(body);\nINSERT INTO f VALUES ('fox fox fox'),('fox and other things entirely different here');\nSELECT rowid FROM f WHERE f MATCH 'fox' ORDER BY bm25(f);"),
  c('fts5', 'FTS5 highlight and snippet', 'fts.highlight', "CREATE VIRTUAL TABLE f USING fts5(body);\nINSERT INTO f VALUES ('the quick brown fox jumps');\nSELECT highlight(f,0,'[',']') FROM f WHERE f MATCH 'quick';\nSELECT snippet(f,0,'<','>','...',4) FROM f WHERE f MATCH 'brown';"),
  c('fts5', 'FTS5 column filter and multiple columns', 'fts.columns', "CREATE VIRTUAL TABLE f USING fts5(title, body);\nINSERT INTO f VALUES ('fox','turtle'),('turtle','fox');\nSELECT count(*) FROM f WHERE f MATCH 'title:fox';\nSELECT count(*) FROM f WHERE f MATCH 'body:fox';"),
  c('fts5', 'FTS5 delete and update', 'fts.delete', "CREATE VIRTUAL TABLE f USING fts5(body);\nINSERT INTO f VALUES ('alpha'),('beta');\nDELETE FROM f WHERE body='alpha';\nUPDATE f SET body='gamma' WHERE body='beta';\nSELECT count(*) FROM f WHERE f MATCH 'alpha';\nSELECT count(*) FROM f WHERE f MATCH 'gamma';"),
  c('fts5', 'FTS5 rank and the rowid', 'fts.rank', "CREATE VIRTUAL TABLE f USING fts5(body);\nINSERT INTO f VALUES ('fox'),('fox fox');\nSELECT rowid FROM f WHERE f MATCH 'fox' ORDER BY rank;"),
  c('fts5', 'FTS5 external content table', 'fts.external', "CREATE TABLE c(id INTEGER PRIMARY KEY, body TEXT);\nINSERT INTO c VALUES (1,'quick fox');\nCREATE VIRTUAL TABLE f USING fts5(body, content='c', content_rowid='id');\nINSERT INTO f(f) VALUES ('rebuild');\nSELECT count(*) FROM f WHERE f MATCH 'fox';"),
  c('fts5', 'FTS5 contentless table', 'fts.contentless', "CREATE VIRTUAL TABLE f USING fts5(body, content='');\nINSERT INTO f(rowid, body) VALUES (1,'quick fox');\nSELECT rowid FROM f WHERE f MATCH 'fox';"),
  c('fts5', "FTS5 'optimize' and 'rebuild' commands", 'fts.commands', "CREATE VIRTUAL TABLE f USING fts5(body);\nINSERT INTO f VALUES ('a'),('b');\nINSERT INTO f(f) VALUES ('optimize');\nINSERT INTO f(f) VALUES ('rebuild');\nSELECT count(*) FROM f;"),
  c('fts5', 'FTS5 tokenizer options', 'fts.tokenizer', "CREATE VIRTUAL TABLE f USING fts5(body, tokenize='porter unicode61');\nINSERT INTO f VALUES ('running quickly');\nSELECT count(*) FROM f WHERE f MATCH 'run';"),
  c('fts5', 'fts5vocab', 'fts.vocab', "CREATE VIRTUAL TABLE f USING fts5(body);\nINSERT INTO f VALUES ('alpha beta'),('beta gamma');\nCREATE VIRTUAL TABLE v USING fts5vocab(f, 'row');\nSELECT term, cnt FROM v ORDER BY term;"),
  c('fts5', 'FTS3/FTS4', 'fts.fts4', "CREATE VIRTUAL TABLE f USING fts4(body);\nINSERT INTO f VALUES ('quick fox');\nSELECT count(*) FROM f WHERE f MATCH 'fox';"),

  // ---------------------------------------------------------------- R-TREE
  c('rtree', 'CREATE VIRTUAL TABLE ... rtree and a window query', 'rtree.basic', "CREATE VIRTUAL TABLE r USING rtree(id, minX, maxX, minY, maxY);\nINSERT INTO r VALUES (1,0,1,0,1),(2,5,6,5,6);\nSELECT id FROM r WHERE minX>=4 AND maxX<=7;"),
  c('rtree', 'rtree_i32', 'rtree.i32', "CREATE VIRTUAL TABLE r USING rtree_i32(id, minX, maxX);\nINSERT INTO r VALUES (1,0,10);\nSELECT count(*) FROM r WHERE minX<=5 AND maxX>=5;"),
  c('rtree', 'An R-Tree with an auxiliary column', 'rtree.aux', "CREATE VIRTUAL TABLE r USING rtree(id, minX, maxX, +label);\nINSERT INTO r VALUES (1,0,1,'here');\nSELECT label FROM r WHERE id=1;"),

  // ------------------------------------------------------- OTHER EXTENSIONS
  c('ext', 'dbstat', 'ext.dbstat', T + "SELECT count(*)>0 FROM dbstat;"),
  c('ext', 'sqlite_dbpage', 'ext.dbpage', T + "SELECT count(*)>0 FROM sqlite_dbpage;"),
  c('ext', 'geopoly', 'ext.geopoly', "CREATE VIRTUAL TABLE g USING geopoly();\nSELECT count(*) FROM g;"),
  c('ext', 'The CSV module', 'ext.csv', "CREATE VIRTUAL TABLE c USING csv(filename='nosuch.csv');"),
  c('ext', 'sqlite_offset', 'ext.offset', T + "SELECT sqlite_offset(a) IS NOT NULL FROM t LIMIT 1;"),
  c('ext', 'The session extension (changeset)', 'ext.session', "CREATE TABLE t(a);\nSELECT count(*) FROM sqlite_schema;"),

  // -------------------------------------------------------- SCHEMA & VACUUM
  c('schema', 'sqlite_schema and sqlite_master', 'sch.schema.table', T + "SELECT type, name FROM sqlite_schema ORDER BY name;\nSELECT count(*) FROM sqlite_master;"),
  c('schema', 'The schema of an index and a trigger', 'sch.schema.sql', T + "CREATE INDEX ia ON t(a);\nCREATE TRIGGER tg AFTER INSERT ON t BEGIN SELECT 1; END;\nSELECT type, name, sql FROM sqlite_schema WHERE type IN ('index','trigger') ORDER BY name;"),
  c('schema', 'VACUUM', 'sch.vacuum', T + "DELETE FROM t WHERE id<3;\nVACUUM;\nSELECT count(*) FROM t;"),
  c('schema', 'VACUUM INTO', 'sch.vacuum.into', T + "VACUUM INTO 'copy.db';\nSELECT count(*) FROM t;"),
  c('schema', 'Rowid, oid and _rowid_ aliases', 'sch.rowid.aliases', "CREATE TABLE t(a);\nINSERT INTO t VALUES ('x');\nSELECT rowid, oid, _rowid_ FROM t;"),
  c('schema', 'sqlite_sequence after AUTOINCREMENT', 'sch.sqlite.sequence', "CREATE TABLE t(id INTEGER PRIMARY KEY AUTOINCREMENT, a);\nINSERT INTO t(a) VALUES (1),(2);\nSELECT name, seq FROM sqlite_sequence;"),

  // --------------------------------------------------------------- SYNTAX
  c('syntax', 'Comments, both forms', 'syn.comments', "-- a line comment\n/* a block\n   comment */\nSELECT 1; -- trailing"),
  c('syntax', 'Keyword case insensitivity', 'syn.case', "create TABLE t(A InTeGeR);\nInSeRt InTo t VaLuEs (1);\nsElEcT a FROM T;"),
  c('syntax', 'Identifier quoting, all four forms', 'syn.quoting', "CREATE TABLE \"my table\"(\"a b\" INTEGER);\nINSERT INTO \"my table\" VALUES (1);\nSELECT \"a b\" FROM \"my table\";\nSELECT [a b] FROM [my table];\nSELECT `a b` FROM `my table`;"),
  c('syntax', 'Reserved words as column names', 'syn.reserved', "CREATE TABLE t(\"left\" INTEGER, \"order\" TEXT, \"group\" INTEGER, \"index\" INTEGER);\nINSERT INTO t VALUES (1,'a',2,3);\nSELECT \"left\", \"order\", \"group\", \"index\" FROM t;"),
  c('syntax', 'String literals with embedded quotes', 'syn.strings', "SELECT 'it''s', length('it''s'), 'a\nb';"),
  c('syntax', 'Bare double-quoted string falling back to a literal', 'syn.dquote.fallback', "SELECT \"no such column\";"),
  c('syntax', 'Deeply nested expression', 'syn.nested', "SELECT ((((((((((1+1))))))))));"),
  c('syntax', 'Statements without a trailing semicolon', 'syn.no.semicolon', "CREATE TABLE t(a);\nINSERT INTO t VALUES (1);\nSELECT count(*) FROM t"),
  c('syntax', 'A very long identifier and a very long string', 'syn.long', "CREATE TABLE t(" + 'x'.repeat(200) + " INTEGER);\nINSERT INTO t VALUES (1);\nSELECT length('" + 'y'.repeat(5000) + "');"),

  // ---------------------------------------------------- VECTOR (ours only)
  c('vector', 'VECTOR(n) column and distance functions', 'vec.column', "CREATE TABLE e(id INTEGER PRIMARY KEY, v VECTOR(4));\nINSERT INTO e VALUES (1,x'0000803f000000000000000000000000'),(2,x'000000000000803f0000000000000000');\nSELECT id, round(vector_distance_cos(v,x'0000803f000000000000000000000000'),4) FROM e ORDER BY id;", { oursOnly: true }),
  c('vector', 'vector_distance_l2 and vector_dot', 'vec.distance', "CREATE TABLE e(id INTEGER PRIMARY KEY, v VECTOR(4));\nINSERT INTO e VALUES (1,x'0000803f000000000000000000000000'),(2,x'000000000000803f0000000000000000');\nSELECT id, round(vector_distance_l2(v,x'0000803f000000000000000000000000'),4), round(vector_dot(v,x'0000803f000000000000000000000000'),4) FROM e ORDER BY id;", { oursOnly: true }),
  c('vector', 'CREATE INDEX ... USING inillucent_hnsw', 'vec.index', "CREATE TABLE e(id INTEGER PRIMARY KEY, v VECTOR(4));\nINSERT INTO e VALUES (1,x'0000803f000000000000000000000000'),(2,x'000000000000803f0000000000000000'),(3,x'00000000000000000000803f00000000');\nCREATE INDEX ie ON e USING inillucent_hnsw (v);\nSELECT id FROM e ORDER BY vector_distance_cos(v,x'0000803f000000000000000000000000') LIMIT 2;", { oursOnly: true }),
  c('vector', 'A vector ORDER BY with a WHERE predicate', 'vec.filtered', "CREATE TABLE e(id INTEGER PRIMARY KEY, src TEXT, v VECTOR(4));\nINSERT INTO e VALUES (1,'a',x'0000803f000000000000000000000000'),(2,'b',x'6666663fcdcccc3d0000000000000000'),(3,'a',x'000000000000803f0000000000000000');\nCREATE INDEX ie ON e USING inillucent_hnsw (v);\nSELECT id FROM e WHERE src='a' ORDER BY vector_distance_cos(v,x'0000803f000000000000000000000000') LIMIT 2;", { oursOnly: true }),
  c('vector', 'The inillucent_search virtual table', 'vec.search.vtab', "CREATE VIRTUAL TABLE s USING inillucent_search(body, dims=4);\nSELECT count(*) FROM s;", { oursOnly: true }),
  c('vector', "pgvector's operator spellings", 'vec.pgvector.ops', "CREATE TABLE e(id INTEGER PRIMARY KEY, v VECTOR(4));\nINSERT INTO e VALUES (1,x'0000803f000000000000000000000000');\nSELECT v <=> x'0000803f000000000000000000000000' FROM e;", { oursOnly: true }),
];
