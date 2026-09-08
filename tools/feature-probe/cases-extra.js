// The second batch: the shell, parameters, limits, and the corners the first
// batch's areas left uncovered. Same rules - a whole script through both
// shells, every byte compared.

/** Builds one case. @param area - the section it lands in @param feature - the human name @param id - stable key @param sql - the whole script */
const c = (area, feature, id, sql, extra = {}) => ({ area, feature, id, sql, ...extra });

const T = 'CREATE TABLE t(id INTEGER PRIMARY KEY, a INTEGER, b TEXT);\nINSERT INTO t VALUES (1,10,\'p\'),(2,20,\'q\'),(3,30,\'r\'),(4,20,\'s\'),(5,50,\'t\');\n';

module.exports = [
  // ---------------------------------------------------------------- SHELL
  c('shell', '.tables', 'sh.tables', T + "CREATE TABLE u(x);\n.tables"),
  c('shell', '.schema and .schema TABLE', 'sh.schema', T + "CREATE INDEX ia ON t(a);\n.schema\n.schema t"),
  c('shell', '.fullschema', 'sh.fullschema', T + ".fullschema"),
  c('shell', '.indexes', 'sh.indexes', T + "CREATE INDEX ia ON t(a);\n.indexes\n.indexes t"),
  c('shell', '.databases', 'sh.databases', ".databases"),
  c('shell', '.headers on', 'sh.headers', T + ".headers on\nSELECT id, a FROM t ORDER BY id LIMIT 2;"),
  c('shell', '.mode csv', 'sh.mode.csv', T + ".mode csv\nSELECT id, b FROM t ORDER BY id LIMIT 2;"),
  c('shell', '.mode json', 'sh.mode.json', T + ".mode json\nSELECT id, b FROM t ORDER BY id LIMIT 2;"),
  c('shell', '.mode line', 'sh.mode.line', T + ".mode line\nSELECT id, b FROM t ORDER BY id LIMIT 1;"),
  c('shell', '.mode column', 'sh.mode.column', T + ".mode column\nSELECT id, b FROM t ORDER BY id LIMIT 2;"),
  c('shell', '.mode insert', 'sh.mode.insert', T + ".mode insert\nSELECT id, b FROM t ORDER BY id LIMIT 2;"),
  c('shell', '.mode quote, markdown, box, table, html', 'sh.mode.rest', T + ".mode quote\nSELECT id FROM t LIMIT 1;\n.mode markdown\nSELECT id FROM t LIMIT 1;\n.mode box\nSELECT id FROM t LIMIT 1;\n.mode table\nSELECT id FROM t LIMIT 1;\n.mode html\nSELECT id FROM t LIMIT 1;"),
  c('shell', '.separator and .nullvalue', 'sh.separator', "CREATE TABLE t(a,b);\nINSERT INTO t VALUES (1,NULL);\n.separator ;\n.nullvalue NULL\nSELECT a,b FROM t;"),
  c('shell', '.dump', 'sh.dump', T + "CREATE INDEX ia ON t(a);\n.dump"),
  c('shell', '.import a CSV file', 'sh.import', "CREATE TABLE t(a,b);\n.mode csv\n.import data.csv t\nSELECT count(*) FROM t;", { files: { 'data.csv': '1,x\n2,y\n' } }),
  c('shell', '.output to a file and back', 'sh.output', T + ".output out.txt\nSELECT count(*) FROM t;\n.output stdout\nSELECT 'done';"),
  c('shell', '.once', 'sh.once', T + ".once once.txt\nSELECT count(*) FROM t;\nSELECT 'after';"),
  c('shell', '.read a script file', 'sh.read', "CREATE TABLE t(a);\n.read more.sql\nSELECT count(*) FROM t;", { files: { 'more.sql': "INSERT INTO t VALUES (1),(2);\n" } }),
  c('shell', '.backup and .restore', 'sh.backup', T + ".backup copy.db\nSELECT 'backed up';"),
  c('shell', '.save', 'sh.save', T + ".save saved.db\nSELECT 'saved';"),
  c('shell', '.clone', 'sh.clone', T + ".clone cloned.db\nSELECT 'cloned';"),
  c('shell', '.changes on', 'sh.changes', "CREATE TABLE t(a);\n.changes on\nINSERT INTO t VALUES (1),(2);"),
  c('shell', '.echo on', 'sh.echo', ".echo on\nSELECT 1;"),
  c('shell', '.bail on', 'sh.bail', ".bail on\nSELECT nosuch();\nSELECT 'never';"),
  c('shell', '.eqp on', 'sh.eqp', T + ".eqp on\nSELECT count(*) FROM t;"),
  c('shell', '.width', 'sh.width', T + ".mode column\n.width 4 4\nSELECT id, b FROM t LIMIT 1;"),
  c('shell', '.parameter set and a named parameter', 'sh.parameter', ".parameter init\n.parameter set :x 5\nSELECT :x + 1;"),
  c('shell', '.sha3sum', 'sh.sha3sum', T + ".sha3sum"),
  c('shell', '.lint fkey-indexes', 'sh.lint', "CREATE TABLE p(id INTEGER PRIMARY KEY);\nCREATE TABLE ch(id INTEGER PRIMARY KEY, pid REFERENCES p);\n.lint fkey-indexes"),
  c('shell', '.limit', 'sh.limit', ".limit"),
  c('shell', '.vfsinfo / .vfslist', 'sh.vfs', ".vfslist"),
  c('shell', '.stats on', 'sh.stats', T + ".stats on\nSELECT count(*) FROM t;"),
  c('shell', '.timeout', 'sh.timeout', ".timeout 1000\nSELECT 'ok';"),
  c('shell', '.recover', 'sh.recover', T + ".recover"),
  c('shell', '.selftest', 'sh.selftest', T + ".selftest"),
  c('shell', '.log', 'sh.log', ".log stderr\nSELECT 'ok';"),
  c('shell', '.open a second file', 'sh.open', "CREATE TABLE t(a);\n.open other.db\nCREATE TABLE u(b);\nSELECT count(*) FROM sqlite_schema;"),
  c('shell', '.help exists', 'sh.help', ".help .mode"),
  c('shell', 'An unknown dot command', 'sh.unknown', ".nosuchcommand\nSELECT 1;"),

  // ------------------------------------------------------------ PARAMETERS
  c('params', 'Parameters through .parameter set, all spellings', 'par.spellings', ".parameter init\n.parameter set :a 1\n.parameter set @b 2\n.parameter set $c 3\nSELECT :a, @b, $c;"),

  // ---------------------------------------------------------------- PRAGMA
  c('pragma', 'PRAGMA user_version round trip', 'prag.user.version', "PRAGMA user_version;\nPRAGMA user_version = 12;\nPRAGMA user_version;"),
  c('pragma', 'PRAGMA application_id round trip', 'prag.application.id', "PRAGMA application_id;\nPRAGMA application_id = 99;\nPRAGMA application_id;"),
  c('pragma', 'PRAGMA table_info on a view', 'prag.table.info.view', T + "CREATE VIEW v AS SELECT a, b FROM t;\nPRAGMA table_info(v);"),
  c('pragma', 'PRAGMA index_info on an implicit primary-key index', 'prag.index.info.pk', "CREATE TABLE t(a TEXT PRIMARY KEY, b);\nPRAGMA index_list(t);"),
  c('pragma', 'PRAGMA journal_mode reported by default', 'prag.journal.default', "PRAGMA journal_mode;"),
  c('pragma', 'PRAGMA wal_checkpoint(TRUNCATE)', 'prag.wal.truncate', T + "PRAGMA wal_checkpoint(TRUNCATE);"),
  c('pragma', 'PRAGMA count_changes and other deprecated ones', 'prag.deprecated', "PRAGMA count_changes;\nPRAGMA full_column_names;\nPRAGMA short_column_names;\nPRAGMA empty_result_callbacks;"),
  c('pragma', 'PRAGMA collation_list after a CREATE', 'prag.collation.list', "CREATE TABLE t(a TEXT COLLATE NOCASE);\nPRAGMA collation_list;"),

  // ----------------------------------------------------------- MORE SQL
  c('ddl-table', 'DEFAULT CURRENT_TIMESTAMP and friends', 'ddl.default.timestamp', "CREATE TABLE t(a DEFAULT CURRENT_TIMESTAMP, b DEFAULT CURRENT_DATE, c DEFAULT CURRENT_TIME);\nINSERT INTO t DEFAULT VALUES;\nSELECT length(a), length(b), length(c), typeof(a) FROM t;"),
  c('ddl-table', 'CHECK containing a subquery', 'ddl.check.subquery', "CREATE TABLE u(a);\nCREATE TABLE t(a INTEGER CHECK (a IN (SELECT a FROM u)));"),
  c('ddl-table', 'INTEGER PRIMARY KEY DESC is not a rowid alias', 'ddl.pk.desc', "CREATE TABLE t(id INTEGER PRIMARY KEY DESC, a);\nINSERT INTO t VALUES (1,'x');\nSELECT id, typeof(id) FROM t;\nSELECT count(*) FROM pragma_index_list('t');"),
  c('ddl-table', 'A rowid reference in a WITHOUT ROWID table', 'ddl.without.rowid.rowid', "CREATE TABLE t(k TEXT PRIMARY KEY, v) WITHOUT ROWID;\nINSERT INTO t VALUES ('a',1);\nSELECT rowid FROM t;"),
  c('ddl-table', 'A WITHOUT ROWID table with no primary key', 'ddl.without.rowid.nopk', "CREATE TABLE t(a, b) WITHOUT ROWID;"),
  c('alter', 'ADD COLUMN NOT NULL DEFAULT on a populated table', 'alter.add.notnull', T + "ALTER TABLE t ADD COLUMN c INTEGER NOT NULL DEFAULT 3;\nSELECT group_concat(c) FROM (SELECT c FROM t ORDER BY id);"),
  c('alter', 'ADD COLUMN NOT NULL with no default', 'alter.add.notnull.nodefault', T + "ALTER TABLE t ADD COLUMN c INTEGER NOT NULL;"),
  c('alter', 'ADD COLUMN UNIQUE', 'alter.add.unique', T + "ALTER TABLE t ADD COLUMN c INTEGER UNIQUE;"),
  c('dml', 'Upsert on a WITHOUT ROWID table', 'dml.upsert.without.rowid', "CREATE TABLE t(k TEXT PRIMARY KEY, v INTEGER) WITHOUT ROWID;\nINSERT INTO t VALUES ('a',1);\nINSERT INTO t VALUES ('a',2) ON CONFLICT(k) DO UPDATE SET v=v+excluded.v;\nSELECT k,v FROM t;"),
  c('dml', 'A correlated UPDATE subquery', 'dml.update.correlated', "CREATE TABLE t(id INTEGER PRIMARY KEY, a INTEGER);\nCREATE TABLE u(id INTEGER PRIMARY KEY, b INTEGER);\nINSERT INTO t VALUES (1,0),(2,0);\nINSERT INTO u VALUES (1,7),(2,9);\nUPDATE t SET a = (SELECT b FROM u WHERE u.id = t.id);\nSELECT group_concat(a) FROM (SELECT a FROM t ORDER BY id);"),
  c('dml', 'RETURNING beside a trigger', 'dml.returning.trigger', T + "CREATE TABLE log(m);\nCREATE TRIGGER ti AFTER INSERT ON t BEGIN INSERT INTO log VALUES (new.id); END;\nINSERT INTO t VALUES (6,60,'u') RETURNING id;\nSELECT count(*) FROM log;"),
  c('select', 'DISTINCT with an ORDER BY on a column not selected', 'select.distinct.orderby', T + "SELECT DISTINCT a FROM t ORDER BY b;"),
  c('select', 'ORDER BY a window function', 'select.orderby.window', T + "SELECT id FROM t ORDER BY row_number() OVER (ORDER BY a DESC), id;"),
  c('select', 'HAVING referring to a select alias', 'select.having.alias', T + "SELECT a, count(*) n FROM t GROUP BY a HAVING n > 1;"),
  c('select', 'count(DISTINCT) with two arguments', 'select.count.distinct.two', T + "SELECT count(DISTINCT a, b) FROM t;"),
  c('join', 'LEFT JOIN ... USING', 'join.left.using', "CREATE TABLE a(k INTEGER, x TEXT);\nCREATE TABLE b(k INTEGER, y TEXT);\nINSERT INTO a VALUES (1,'p'),(2,'q');\nINSERT INTO b VALUES (1,'m');\nSELECT k, x, y FROM a LEFT JOIN b USING (k) ORDER BY k;"),
  c('join', 'NATURAL LEFT JOIN', 'join.natural.left', "CREATE TABLE a(k INTEGER, x TEXT);\nCREATE TABLE b(k INTEGER, y TEXT);\nINSERT INTO a VALUES (1,'p'),(2,'q');\nINSERT INTO b VALUES (1,'m');\nSELECT * FROM a NATURAL LEFT JOIN b ORDER BY k;"),
  c('join', 'USING with three tables', 'join.using.three', "CREATE TABLE a(k INTEGER, x TEXT);\nCREATE TABLE b(k INTEGER, y TEXT);\nCREATE TABLE c(k INTEGER, z TEXT);\nINSERT INTO a VALUES (1,'p');\nINSERT INTO b VALUES (1,'q');\nINSERT INTO c VALUES (1,'r');\nSELECT * FROM a JOIN b USING (k) JOIN c USING (k);"),
  c('fn-core', 'printf %q, %Q and %w', 'fn.printf.quote', "SELECT printf('%q',\"it's\"), printf('%Q',\"it's\"), printf('%Q',NULL);"),
  c('fn-core', 'substr with negative and omitted lengths', 'fn.substr.negative', "SELECT substr('abcdef',2), substr('abcdef',-3,2), substr('abcdef',2,-1), substr('abcdef',0,3);"),
  c('fn-core', 'abs of the smallest integer', 'fn.abs.min', "SELECT abs(-9223372036854775807), abs(-9.0), abs('abc'), abs(NULL);"),
  c('fn-core', 'round to negative and large digits', 'fn.round.digits', "SELECT round(1234.5678,-2), round(1.005,2), round(2.5), round(-2.5), round(1e308,2);"),
  c('fn-core', 'char with zero and out-of-range code points', 'fn.char.edge', "SELECT length(char(0)), char(65,0x1F600), unicode('');"),
  c('fn-core', 'instr and length on blobs', 'fn.blob.ops', "SELECT instr(x'0102030405',x'0304'), length(x'010203'), substr(x'0102030405',2,2)=x'0203';"),
  c('fn-agg', 'sum of text and of a mixed column', 'fn.agg.sum.text', "CREATE TABLE t(a);\nINSERT INTO t VALUES (1),('2'),('abc'),(NULL);\nSELECT sum(a), typeof(sum(a)), avg(a), count(a) FROM t;"),
  c('fn-agg', 'Integer sum overflowing', 'fn.agg.sum.overflow', "CREATE TABLE t(a INTEGER);\nINSERT INTO t VALUES (9223372036854775807),(1);\nSELECT sum(a) FROM t;"),
  c('types', 'IEEE special values', 'types.ieee', "SELECT 1e999, -1e999, 1e999-1e999, typeof(1e999), 0.0/0.0;"),
  c('types', 'A very large IN list', 'types.large.in', "SELECT 5 IN (" + Array.from({ length: 500 }, (_, i) => i).join(',') + ");"),
  c('limits', 'A table with 1000 columns', 'lim.columns', "CREATE TABLE wide(" + Array.from({ length: 1000 }, (_, i) => `c${i}`).join(',') + ");\nINSERT INTO wide(c0,c999) VALUES (1,2);\nSELECT c0, c999 FROM wide;"),
  c('limits', 'A 100 term compound select', 'lim.compound', Array.from({ length: 100 }, (_, i) => `SELECT ${i}`).join(' UNION ALL ') + ' ORDER BY 1 DESC LIMIT 1;'),
  c('limits', 'A 100 deep nested expression', 'lim.nesting', 'SELECT ' + '('.repeat(100) + '1' + ')'.repeat(100) + ';'),
  c('limits', 'A 40 term join', 'lim.join', "CREATE TABLE j(a);\nINSERT INTO j VALUES (1);\nSELECT count(*) FROM " + Array.from({ length: 40 }, (_, i) => `j x${i}`).join(', ') + ";"),
  c('limits', 'Recursive CTE bounded by a LIMIT', 'lim.recursive', "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n) SELECT count(*) FROM (SELECT x FROM n LIMIT 1000);"),
  c('limits', 'Thirty attached databases', 'lim.attach', Array.from({ length: 30 }, (_, i) => `ATTACH DATABASE 'd${i}.db' AS d${i};`).join('\n') + "\nSELECT count(*) FROM pragma_database_list;"),
  c('limits', 'A 2 MB text value', 'lim.big.text', "CREATE TABLE t(a TEXT);\nINSERT INTO t VALUES (hex(zeroblob(1000000)));\nSELECT length(a) FROM t;"),

  // -------------------------------------------------------------- INTEGRITY
  c('integrity', 'integrity_check over an index and a WITHOUT ROWID table', 'int.check.wide', "CREATE TABLE t(id INTEGER PRIMARY KEY, a INTEGER, b TEXT);\nINSERT INTO t VALUES (1,10,'p'),(2,20,'q');\nCREATE INDEX ia ON t(a);\nCREATE TABLE w(k TEXT PRIMARY KEY, v) WITHOUT ROWID;\nINSERT INTO w VALUES ('a',1);\nPRAGMA integrity_check;"),
  c('integrity', 'integrity_check with a row limit argument', 'int.check.limit', T + "PRAGMA integrity_check(1);"),

  // ------------------------------------------------------- VECTOR AT SCALE
];
