/*
** An independent C program, compiled against the official sqlite3.h and linked
** against either engine.
**
** Invariant: it uses nothing but the header. No inillucent symbol, no inillucent
** constant, no assumption that is not in the file a caller of SQLite already
** has - which is the whole point. If this compiles against the official header
** and links against inillucent, then inillucent's declarations agree with the
** header's; if it then prints the same lines as it does against SQLite, the
** behaviour agrees too. Either half alone proves much less than people expect.
**
** Everything it prints is deterministic. No addresses, no timings, no
** iteration over a hash table: the two runs are compared byte for byte, so a
** line that could legitimately differ has no place here.
*/

#include <stdio.h>
#include <string.h>
#include <stdlib.h>

#include "sqlite3.h"

/* Prints a labelled result code, so a failure says which call and what code. */
static void code(const char *label, int rc) {
  printf("%s=%d\n", label, rc);
}

/* Prints a labelled string, writing NULL rather than crashing on one. */
static void text(const char *label, const char *value) {
  printf("%s=%s\n", label, value ? value : "(null)");
}

/* ----------------------------------------------------------- open and close */
static void probe_open(void) {
  sqlite3 *db = 0;
  int rc = sqlite3_open(":memory:", &db);
  code("open", rc);
  code("open.errcode", sqlite3_errcode(db));
  code("open.autocommit", sqlite3_get_autocommit(db));
  code("open.readonly", sqlite3_db_readonly(db, "main"));
  printf("open.version=%s\n", sqlite3_libversion());
  printf("open.versionnum=%d\n", sqlite3_libversion_number());
  code("close", sqlite3_close(db));

  /* A close with a statement still open is refused, and says so. */
  sqlite3_stmt *stmt = 0;
  rc = sqlite3_open(":memory:", &db);
  code("open2", rc);
  code("prepare", sqlite3_prepare_v2(db, "SELECT 1", -1, &stmt, 0));
  code("close.busy", sqlite3_close(db));
  code("finalize", sqlite3_finalize(stmt));
  code("close.after", sqlite3_close(db));
}

/* ------------------------------------------------ prepare, step and finalize */
static void probe_statements(sqlite3 *db) {
  sqlite3_stmt *stmt = 0;
  const char *tail = 0;
  const char *sql = "SELECT 1 AS a, 'two' AS b; SELECT 3;";
  code("stmt.prepare", sqlite3_prepare_v2(db, sql, -1, &stmt, &tail));
  text("stmt.tail", tail);
  text("stmt.sql", sqlite3_sql(stmt));
  code("stmt.readonly", sqlite3_stmt_readonly(stmt));
  code("stmt.busy.before", sqlite3_stmt_busy(stmt));
  code("stmt.columns", sqlite3_column_count(stmt));
  code("stmt.step", sqlite3_step(stmt));
  code("stmt.busy.during", sqlite3_stmt_busy(stmt));
  code("stmt.datacount", sqlite3_data_count(stmt));
  code("stmt.step.done", sqlite3_step(stmt));
  code("stmt.reset", sqlite3_reset(stmt));
  code("stmt.step.again", sqlite3_step(stmt));
  code("stmt.finalize", sqlite3_finalize(stmt));

  /* A statement that will not compile reports where and why. */
  stmt = 0;
  code("stmt.bad", sqlite3_prepare_v2(db, "SELECT FROM", -1, &stmt, 0));
  printf("stmt.bad.null=%d\n", stmt == 0);
  code("stmt.finalize.null", sqlite3_finalize(0));
}

/* ------------------------------------------------------------------ binding */
static void probe_binding(sqlite3 *db) {
  sqlite3_stmt *stmt = 0;
  const char *sql = "SELECT ?1, ?2, ?3, ?4, :named";
  code("bind.prepare", sqlite3_prepare_v2(db, sql, -1, &stmt, 0));
  code("bind.count", sqlite3_bind_parameter_count(stmt));
  code("bind.index", sqlite3_bind_parameter_index(stmt, ":named"));
  text("bind.name", sqlite3_bind_parameter_name(stmt, 5));
  code("bind.int", sqlite3_bind_int(stmt, 1, 42));
  code("bind.double", sqlite3_bind_double(stmt, 2, 1.5));
  code("bind.text", sqlite3_bind_text(stmt, 3, "hi", -1, SQLITE_TRANSIENT));
  code("bind.blob", sqlite3_bind_blob(stmt, 4, "\x01\x02", 2, SQLITE_TRANSIENT));
  code("bind.null", sqlite3_bind_null(stmt, 5));
  code("bind.range", sqlite3_bind_int(stmt, 99, 1));
  code("bind.step", sqlite3_step(stmt));
  code("bind.col0", sqlite3_column_int(stmt, 0));
  printf("bind.col1=%.1f\n", sqlite3_column_double(stmt, 1));
  text("bind.col2", (const char *)sqlite3_column_text(stmt, 2));
  code("bind.col3.bytes", sqlite3_column_bytes(stmt, 3));
  code("bind.col4.type", sqlite3_column_type(stmt, 4));
  code("bind.clear", sqlite3_clear_bindings(stmt));
  code("bind.reset", sqlite3_reset(stmt));
  code("bind.step.cleared", sqlite3_step(stmt));
  code("bind.col0.cleared", sqlite3_column_type(stmt, 0));
  code("bind.finalize", sqlite3_finalize(stmt));
}

/* -------------------------------------------------------- column meta-data */
static void probe_columns(sqlite3 *db) {
  code("cols.create",
       sqlite3_exec(db, "CREATE TABLE t(a INTEGER, b TEXT)", 0, 0, 0));
  code("cols.insert",
       sqlite3_exec(db, "INSERT INTO t VALUES (7, 'seven')", 0, 0, 0));
  printf("cols.rowid=%lld\n", (long long)sqlite3_last_insert_rowid(db));
  code("cols.changes", sqlite3_changes(db));

  sqlite3_stmt *stmt = 0;
  code("cols.prepare",
       sqlite3_prepare_v2(db, "SELECT a, b, a + 1 FROM t", -1, &stmt, 0));
  code("cols.count", sqlite3_column_count(stmt));
  text("cols.name0", sqlite3_column_name(stmt, 0));
  text("cols.name1", sqlite3_column_name(stmt, 1));
  text("cols.decl0", sqlite3_column_decltype(stmt, 0));
  text("cols.decl2", sqlite3_column_decltype(stmt, 2));
  text("cols.table0", sqlite3_column_table_name(stmt, 0));
  text("cols.origin0", sqlite3_column_origin_name(stmt, 0));
  text("cols.db0", sqlite3_column_database_name(stmt, 0));
  text("cols.table2", sqlite3_column_table_name(stmt, 2));
  code("cols.step", sqlite3_step(stmt));
  code("cols.type0", sqlite3_column_type(stmt, 0));
  code("cols.type1", sqlite3_column_type(stmt, 1));
  code("cols.int0", sqlite3_column_int(stmt, 0));
  text("cols.text1", (const char *)sqlite3_column_text(stmt, 1));
  code("cols.bytes1", sqlite3_column_bytes(stmt, 1));
  code("cols.finalize", sqlite3_finalize(stmt));

  const char *declared = 0;
  const char *collation = 0;
  int notnull = 0, primary = 0, autoinc = 0;
  code("cols.meta",
       sqlite3_table_column_metadata(db, "main", "t", "a", &declared, &collation,
                                     &notnull, &primary, &autoinc));
  text("cols.meta.type", declared);
  printf("cols.meta.notnull=%d\n", notnull);
  printf("cols.meta.pk=%d\n", primary);
}

/* Counts the rows an exec callback is handed. */
static int count_rows(void *context, int columns, char **values, char **names) {
  int *counter = (int *)context;
  *counter += 1;
  if (*counter == 1) {
    printf("exec.columns=%d\n", columns);
    text("exec.name0", names[0]);
    text("exec.value0", values[0]);
  }
  return 0;
}

/* --------------------------------------------------------------------- exec */
static void probe_exec(sqlite3 *db) {
  int rows = 0;
  char *message = 0;
  code("exec.run",
       sqlite3_exec(db, "SELECT a FROM t", count_rows, &rows, &message));
  printf("exec.rows=%d\n", rows);
  printf("exec.message=%d\n", message != 0);
  code("exec.bad", sqlite3_exec(db, "SELECT nope", 0, 0, &message));
  printf("exec.bad.message=%d\n", message != 0);
  sqlite3_free(message);
}

/* -------------------------------------------------------------------- hooks */
static int update_calls = 0;
static int commit_calls = 0;
static int rollback_calls = 0;

/* Records that a row changed. */
static void on_update(void *context, int op, const char *db, const char *table,
                      sqlite3_int64 rowid) {
  (void)context;
  (void)db;
  update_calls += 1;
  if (update_calls == 1) {
    printf("hook.op=%d\n", op);
    text("hook.table", table);
    printf("hook.rowid=%lld\n", (long long)rowid);
  }
}

/* Records that a transaction is committing. */
static int on_commit(void *context) {
  (void)context;
  commit_calls += 1;
  return 0;
}

/* Records that a transaction rolled back. */
static void on_rollback(void *context) {
  (void)context;
  rollback_calls += 1;
}

static void probe_hooks(sqlite3 *db) {
  sqlite3_update_hook(db, on_update, 0);
  sqlite3_commit_hook(db, on_commit, 0);
  sqlite3_rollback_hook(db, on_rollback, 0);
  code("hook.begin", sqlite3_exec(db, "BEGIN", 0, 0, 0));
  code("hook.insert", sqlite3_exec(db, "INSERT INTO t VALUES (8, 'x')", 0, 0, 0));
  code("hook.commit", sqlite3_exec(db, "COMMIT", 0, 0, 0));
  printf("hook.updates=%d\n", update_calls);
  printf("hook.commits=%d\n", commit_calls > 0);
  code("hook.begin2", sqlite3_exec(db, "BEGIN", 0, 0, 0));
  code("hook.insert2", sqlite3_exec(db, "INSERT INTO t VALUES (9, 'y')", 0, 0, 0));
  code("hook.rollback", sqlite3_exec(db, "ROLLBACK", 0, 0, 0));
  printf("hook.rollbacks=%d\n", rollback_calls > 0);
  /* Removing a hook hands back what was registered with it. */
  printf("hook.previous=%d\n", sqlite3_update_hook(db, 0, 0) == 0);
  code("hook.timeout", sqlite3_busy_timeout(db, 250));
}

/* --------------------------------------------------------- custom functions */
/* Doubles its argument, so the answer says the call arrived. */
static void twice(sqlite3_context *context, int argc, sqlite3_value **argv) {
  if (argc != 1) {
    sqlite3_result_error(context, "twice takes one argument", -1);
    return;
  }
  sqlite3_result_int64(context, 2 * sqlite3_value_int64(argv[0]));
}

/* Returns the application pointer, to prove it survived the registration. */
static void tag(sqlite3_context *context, int argc, sqlite3_value **argv) {
  (void)argc;
  (void)argv;
  sqlite3_result_text(context, (const char *)sqlite3_user_data(context), -1,
                      SQLITE_TRANSIENT);
}

/* The accumulator an aggregate keeps between rows. */
struct SumState {
  sqlite3_int64 total;
  int rows;
};

/* Adds one row to the running total. */
static void sum_step(sqlite3_context *context, int argc, sqlite3_value **argv) {
  struct SumState *state =
      (struct SumState *)sqlite3_aggregate_context(context, sizeof(*state));
  if (!state || argc != 1) {
    return;
  }
  state->total += sqlite3_value_int64(argv[0]);
  state->rows += 1;
}

/* Reports the total, or NULL when the group had no rows. */
static void sum_final(sqlite3_context *context) {
  struct SumState *state = (struct SumState *)sqlite3_aggregate_context(context, 0);
  if (!state) {
    sqlite3_result_null(context);
    return;
  }
  sqlite3_result_int64(context, state->total * 10 + state->rows);
}

/* Orders by length first, so the answer differs from BINARY. */
static int by_length(void *context, int left_len, const void *left,
                     int right_len, const void *right) {
  (void)context;
  if (left_len != right_len) {
    return left_len < right_len ? -1 : 1;
  }
  return memcmp(left, right, (size_t)left_len);
}

static void probe_functions(sqlite3 *db) {
  static char label[] = "tagged";
  code("fn.scalar",
       sqlite3_create_function(db, "twice", 1, SQLITE_UTF8, 0, twice, 0, 0));
  code("fn.tag",
       sqlite3_create_function(db, "tag", 0, SQLITE_UTF8, label, tag, 0, 0));
  code("fn.aggregate", sqlite3_create_function(db, "sumx", 1, SQLITE_UTF8, 0, 0,
                                               sum_step, sum_final));
  code("fn.collation",
       sqlite3_create_collation(db, "BYLEN", SQLITE_UTF8, 0, by_length));

  sqlite3_stmt *stmt = 0;
  code("fn.prepare", sqlite3_prepare_v2(db, "SELECT twice(21), tag()", -1, &stmt, 0));
  code("fn.step", sqlite3_step(stmt));
  code("fn.twice", sqlite3_column_int(stmt, 0));
  text("fn.tagvalue", (const char *)sqlite3_column_text(stmt, 1));
  code("fn.finalize", sqlite3_finalize(stmt));

  code("fn.table",
       sqlite3_exec(db, "CREATE TABLE nums(n)", 0, 0, 0));
  code("fn.fill",
       sqlite3_exec(db, "INSERT INTO nums VALUES (1),(2),(3)", 0, 0, 0));
  stmt = 0;
  code("fn.agg.prepare",
       sqlite3_prepare_v2(db, "SELECT sumx(n) FROM nums", -1, &stmt, 0));
  code("fn.agg.step", sqlite3_step(stmt));
  printf("fn.agg.value=%lld\n", (long long)sqlite3_column_int64(stmt, 0));
  code("fn.agg.finalize", sqlite3_finalize(stmt));

  stmt = 0;
  code("fn.coll.table",
       sqlite3_exec(db, "CREATE TABLE words(w)", 0, 0, 0));
  code("fn.coll.fill",
       sqlite3_exec(db, "INSERT INTO words VALUES ('bbb'),('a'),('cc')", 0, 0, 0));
  code("fn.coll.prepare",
       sqlite3_prepare_v2(db, "SELECT group_concat(w) FROM "
                              "(SELECT w FROM words ORDER BY w COLLATE BYLEN)",
                          -1, &stmt, 0));
  code("fn.coll.step", sqlite3_step(stmt));
  text("fn.coll.value", (const char *)sqlite3_column_text(stmt, 0));
  code("fn.coll.finalize", sqlite3_finalize(stmt));

  /* Removing a function makes the name unknown again. */
  code("fn.remove",
       sqlite3_create_function(db, "twice", 1, SQLITE_UTF8, 0, 0, 0, 0));
  stmt = 0;
  code("fn.gone", sqlite3_prepare_v2(db, "SELECT twice(1)", -1, &stmt, 0));
  sqlite3_finalize(stmt);
}

/* ------------------------------------------------------------------- backup */
static void probe_backup(sqlite3 *source) {
  sqlite3 *copy = 0;
  code("backup.open", sqlite3_open(":memory:", &copy));
  sqlite3_backup *backup = sqlite3_backup_init(copy, "main", source, "main");
  printf("backup.init=%d\n", backup != 0);
  if (!backup) {
    sqlite3_close(copy);
    return;
  }
  int rc = sqlite3_backup_step(backup, 1);
  printf("backup.stepped=%d\n", rc == SQLITE_OK || rc == SQLITE_DONE);
  printf("backup.pages=%d\n", sqlite3_backup_pagecount(backup) > 0);
  while (rc == SQLITE_OK) {
    rc = sqlite3_backup_step(backup, 1);
  }
  code("backup.done", rc);
  printf("backup.remaining=%d\n", sqlite3_backup_remaining(backup));
  code("backup.finish", sqlite3_backup_finish(backup));

  sqlite3_stmt *stmt = 0;
  code("backup.prepare",
       sqlite3_prepare_v2(copy, "SELECT count(*) FROM t", -1, &stmt, 0));
  code("backup.step", sqlite3_step(stmt));
  code("backup.rows", sqlite3_column_int(stmt, 0));
  code("backup.finalize", sqlite3_finalize(stmt));
  code("backup.close", sqlite3_close(copy));
}

/* ------------------------------------------------------------------ blob io */
static void probe_blob(sqlite3 *db) {
  code("blob.table", sqlite3_exec(db, "CREATE TABLE blobs(b)", 0, 0, 0));
  code("blob.insert",
       sqlite3_exec(db, "INSERT INTO blobs VALUES (x'0102030405')", 0, 0, 0));
  sqlite3_int64 rowid = sqlite3_last_insert_rowid(db);
  sqlite3_blob *blob = 0;
  code("blob.open", sqlite3_blob_open(db, "main", "blobs", "b", rowid, 1, &blob));
  if (!blob) {
    return;
  }
  code("blob.bytes", sqlite3_blob_bytes(blob));
  unsigned char buffer[5] = {0};
  code("blob.read", sqlite3_blob_read(blob, buffer, 5, 0));
  printf("blob.first=%d\n", buffer[0]);
  printf("blob.last=%d\n", buffer[4]);
  unsigned char replacement[2] = {0xAA, 0xBB};
  code("blob.write", sqlite3_blob_write(blob, replacement, 2, 1));
  code("blob.reread", sqlite3_blob_read(blob, buffer, 5, 0));
  printf("blob.changed=%d\n", buffer[1]);
  code("blob.past.end", sqlite3_blob_read(blob, buffer, 5, 3));
  code("blob.close", sqlite3_blob_close(blob));
  code("blob.close.null", sqlite3_blob_close(0));
}

/* ------------------------------------------------- serialize and deserialize */
static void probe_serialize(void) {
  sqlite3 *db = 0;
  code("ser.open", sqlite3_open(":memory:", &db));
  code("ser.create", sqlite3_exec(db, "CREATE TABLE s(x)", 0, 0, 0));
  code("ser.fill", sqlite3_exec(db, "INSERT INTO s VALUES (11),(22)", 0, 0, 0));
  sqlite3_int64 size = 0;
  unsigned char *image = sqlite3_serialize(db, "main", &size, 0);
  printf("ser.image=%d\n", image != 0);
  printf("ser.size.positive=%d\n", size > 0);
  printf("ser.header=%d\n", image && memcmp(image, "SQLite format 3", 15) == 0);

  sqlite3 *other = 0;
  code("ser.open2", sqlite3_open(":memory:", &other));
  code("ser.deserialize",
       sqlite3_deserialize(other, "main", image, size, size, 0));
  sqlite3_stmt *stmt = 0;
  code("ser.prepare",
       sqlite3_prepare_v2(other, "SELECT sum(x) FROM s", -1, &stmt, 0));
  code("ser.step", sqlite3_step(stmt));
  code("ser.sum", sqlite3_column_int(stmt, 0));
  code("ser.finalize", sqlite3_finalize(stmt));
  code("ser.close2", sqlite3_close(other));
  sqlite3_free(image);
  code("ser.close", sqlite3_close(db));
}

/* --------------------------------------------------------------- vfs and memory */
static void probe_vfs(void) {
  sqlite3_vfs *found = sqlite3_vfs_find(0);
  printf("vfs.default=%d\n", found != 0);
  printf("vfs.missing=%d\n", sqlite3_vfs_find("nosuchvfs") == 0);

  void *block = sqlite3_malloc(64);
  printf("mem.alloc=%d\n", block != 0);
  printf("mem.size=%lld\n", (long long)sqlite3_msize(block));
  block = sqlite3_realloc(block, 128);
  printf("mem.grown=%lld\n", (long long)sqlite3_msize(block));
  sqlite3_free(block);
  sqlite3_free(0);
  printf("mem.zero=%d\n", sqlite3_malloc(0) == 0);
}

/* Runs every probe against one engine, printing one section at a time. */
int main(void) {
  sqlite3 *db = 0;
  probe_open();
  if (sqlite3_open(":memory:", &db) != SQLITE_OK) {
    printf("fatal=open\n");
    return 1;
  }
  probe_statements(db);
  probe_binding(db);
  probe_columns(db);
  probe_exec(db);
  probe_hooks(db);
  probe_functions(db);
  probe_backup(db);
  probe_blob(db);
  code("main.close", sqlite3_close(db));
  probe_serialize();
  probe_vfs();
  printf("done\n");
  return 0;
}
