/*
 * inillucent_driver.h - the C ABI over the inillucent relational engine.
 *
 * This header is the whole contract. A binding author needs this file and
 * drivers/README.md, and nothing else: everything below the line is an opaque
 * pointer, and no struct layout crosses the boundary.
 *
 *
 * WHAT TO READ FIRST
 *
 * The engine this drives is deliberately incomplete in places, and it REFUSES
 * what it has not implemented rather than answering it wrongly. So there is a
 * status of its own for that - INILLUCENT_UNSUPPORTED - and it is not the same
 * status a mistyped statement gets. A binding that folds the two together has
 * thrown away the point of this driver: an application needs to be able to say
 * "this engine cannot do that yet" rather than "check your spelling".
 *
 * inillucent_capability() enumerates what the engine does, so an application
 * can ask before it composes a statement rather than after. Every row of that
 * table is checked against the running engine by a test, in both directions -
 * a claim of support that fails, and a claim of absence that now works, both
 * fail it. That is the difference between this and every JDBC driver's
 * hand-written supportsXxx().
 *
 *
 * THREE OWNERSHIP RULES, AND THERE ARE NO OTHERS
 *
 * 1. A handle named by a _free (or _close) function is yours to free, exactly
 *    once. Freeing NULL is a no-op, so a finaliser need not check.
 *
 * 2. Every pointer this library RETURNS points inside the handle you asked,
 *    is valid until that handle is freed, and is never freed by you. It does
 *    not move: a result is materialised, so a pointer into row 0 stays valid
 *    while row 900000 is read.
 *
 * 3. Every pointer you PASS IN is copied before the call returns. You may free
 *    your buffer on the next line.
 *
 *
 * DESTRUCTION ORDER: THERE IS ONLY ONE RULE, AND IT IS ABOUT inillucent_close
 *
 * A statement and a transaction share the connection they were made on. You
 * may free the four handles in ANY order:
 *
 *   inillucent_conn_free(conn);          -- legal with a live stmt or txn
 *   inillucent_stmt_execute(stmt, ...);  -- still works
 *   inillucent_stmt_free(stmt);          -- the connection's state goes here
 *
 * The connection's state lives until the last of its handles is freed, so
 * freeing it early releases the handle and nothing else. Nothing dangles and
 * nothing needs a defined error, because there is no wrong order to report.
 *
 * inillucent_close is the exception, and it REFUSES rather than dangling.
 * It returns INILLUCENT_INVALID_STATE, with a message, while any connection
 * on the database is still alive - and a connection counts as alive while any
 * statement or transaction made on it is alive, even if you already freed the
 * inillucent_conn. The database is left open and usable; free the children and
 * close again.
 *
 * A statement or transaction outliving its connection keeps the session that
 * connection opened, so temp tables, ATTACHed databases and connection
 * pragmas are all still there.
 *
 * inillucent_rows owns its data outright. It depends on nothing and can be
 * freed at any time, before or after everything else.
 *
 * Text is NOT guaranteed NUL-terminated - a text value may contain a NUL byte,
 * and pretending otherwise would truncate it silently. Byte pointers therefore
 * come with a length out-parameter. The few strings that ARE C strings say so
 * on the function.
 *
 *
 * THREADS
 *
 * One file is one buffer pool and the engine is single threaded. A
 * inillucent_db and everything under it must be confined to one thread, or
 * every call on it serialised by a lock you own. There is no lock inside.
 * Two databases on two files are independent.
 */

#ifndef INILLUCENT_DRIVER_H
#define INILLUCENT_DRIVER_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* ------------------------------------------------------------------ */
/* Handles. Every one is opaque; none has a layout you may rely on.    */
/* ------------------------------------------------------------------ */

typedef struct inillucent_db    inillucent_db;    /* an open database file  */
typedef struct inillucent_conn  inillucent_conn;  /* a connection to one    */
typedef struct inillucent_stmt  inillucent_stmt;  /* a statement + bindings */
typedef struct inillucent_rows  inillucent_rows;  /* what a statement said  */
typedef struct inillucent_txn   inillucent_txn;   /* an open transaction    */
typedef struct inillucent_error inillucent_error; /* one failure            */

/* ------------------------------------------------------------------ */
/* Status codes. Frozen at 1.0; a new one takes the next number.       */
/* ------------------------------------------------------------------ */

#define INILLUCENT_OK             0
#define INILLUCENT_UNSUPPORTED    1  /* this engine cannot do that yet     */
#define INILLUCENT_SYNTAX         2  /* the statement is not valid SQL     */
#define INILLUCENT_NOT_FOUND      3  /* no such table, column or index     */
#define INILLUCENT_CONSTRAINT     4  /* a constraint refused the write     */
#define INILLUCENT_READONLY       5
#define INILLUCENT_BUSY           6
#define INILLUCENT_INTERRUPTED    7
#define INILLUCENT_CORRUPT        8
#define INILLUCENT_IO             9
#define INILLUCENT_FULL          10
#define INILLUCENT_TOO_BIG       11
#define INILLUCENT_INVALID_STATE 12  /* you broke this API's own contract  */
/* The same code, under the name the handle rules use for it: a freed handle, a
 * double free, or a bind index past the statement's parameter count. Every one
 * of those was undefined behaviour before task-1980 - a heap corruption, a
 * silently wrong value, or an allocation of tens of gigabytes. */
#define INILLUCENT_MISUSE        INILLUCENT_INVALID_STATE
#define INILLUCENT_INTERNAL      13  /* a defect - please report it        */

/* Value kinds, as inillucent_value_type reports them. Also frozen. */
#define INILLUCENT_NULL     0
#define INILLUCENT_INTEGER  1
#define INILLUCENT_REAL     2
#define INILLUCENT_TEXT     3
#define INILLUCENT_BLOB     4

/* Capability states, as inillucent_capability and inillucent_supports
 * report them. PARTIAL means "yes, with the limit the note names", and a
 * binding that treats it as YES without reading the note will be surprised. */
#define INILLUCENT_SUPPORT_NO       0
#define INILLUCENT_SUPPORT_YES      1
#define INILLUCENT_SUPPORT_PARTIAL (-1)
#define INILLUCENT_SUPPORT_UNKNOWN (-2) /* no such capability in this build */

/* Flags for inillucent_open. */
#define INILLUCENT_OPEN_CREATE      0x0001 /* make the file if absent      */
#define INILLUCENT_OPEN_READONLY    0x0002 /* refuse anything but a query  */
#define INILLUCENT_OPEN_DIAGNOSTICS 0x0004 /* see inillucent_error_detail  */

/* ------------------------------------------------------------------ */
/* The library itself.                                                 */
/* ------------------------------------------------------------------ */

/* major*1000000 + minor*1000 + patch. Check the MAJOR at load and refuse a
 * mismatch by name; that is the whole reason this exists. */
uint32_t    inillucent_abi_version(void);

/* What the driver calls itself. A C string, valid forever. */
const char *inillucent_version(void);

/* How many capabilities this build knows about. */
size_t inillucent_capability_count(void);

/* Reads one capability. `name` and `note` are C strings valid forever;
 * `state` is one of the INILLUCENT_SUPPORT_* values. Any out-parameter may be
 * NULL if you do not want it. Returns INILLUCENT_INVALID_STATE if `nth` is
 * past the end. */
int32_t inillucent_capability(size_t nth, const char **name, int32_t *state,
                              const char **note);

/* Looks one up by name. Returns an INILLUCENT_SUPPORT_* value, and
 * INILLUCENT_SUPPORT_UNKNOWN for a name this build has never heard of - which
 * you should treat as "no", never as "yes": a capability that was never
 * declared was certainly never checked. */
int32_t inillucent_supports(const char *name);

/* ------------------------------------------------------------------ */
/* A database.                                                         */
/* ------------------------------------------------------------------ */

/* Opens (and by default creates) a database. `path` is UTF-8.
 * On failure *out is left alone and *error, if you passed one, is set. */
int32_t inillucent_open(const char *path, uint32_t flags, inillucent_db **out,
                        inillucent_error **error);

/* Checkpoints and closes. Refuses with INILLUCENT_INVALID_STATE while any
 * connection on it is still open, rather than leaving them dangling. */
int32_t inillucent_close(inillucent_db *db, inillucent_error **error);

/* Makes everything written so far durable in the file. */
int32_t inillucent_checkpoint(inillucent_db *db, inillucent_error **error);

/* Walks every tree and reports the first thing that is wrong. */
int32_t inillucent_integrity_check(inillucent_db *db, inillucent_error **error);

/* Copies the database to a path, and opens and checks the copy before
 * returning - because a backup nobody checked is a file that is assumed to be
 * a database. */
int32_t inillucent_backup_to(inillucent_db *db, const char *path,
                             inillucent_error **error);

/* The file this database is in, as a C string valid until it is closed. */
const char *inillucent_path(const inillucent_db *db);

/* ------------------------------------------------------------------ */
/* A connection.                                                       */
/* ------------------------------------------------------------------ */

int32_t inillucent_connect(inillucent_db *db, inillucent_conn **out,
                           inillucent_error **error);

/* Frees the connection handle. Legal while a statement or transaction made on
 * it is still alive: they share the connection's state, which lives until the
 * last of them is freed. The database still refuses to close until then. */
void inillucent_conn_free(inillucent_conn *conn);

/* Runs one statement with nothing bound and collects every row it produced.
 * `limit` caps the rows HANDED BACK, not the rows produced - see
 * inillucent_rows_total and inillucent_rows_more. */
int32_t inillucent_execute(inillucent_conn *conn, const char *sql,
                           uint64_t limit, inillucent_rows **out,
                           inillucent_error **error);

/* Runs several statements separated by semicolons, for their effect. */
int32_t inillucent_execute_batch(inillucent_conn *conn, const char *sql,
                                 inillucent_error **error);

int64_t inillucent_last_insert_rowid(inillucent_conn *conn);
int64_t inillucent_total_changes(inillucent_conn *conn);

/* 1 while a transaction is open, 0 otherwise. */
int32_t inillucent_in_transaction(inillucent_conn *conn);

/* The schema's generation, which changes when the schema does. Compare it to
 * know whether a cached table description is stale. */
uint64_t inillucent_schema_cookie(inillucent_conn *conn);

/* Asks a running statement to stop. Safe to call from another thread while a
 * statement is running - it is the one call here that is - because it sets a
 * flag rather than touching the statement. The statement then fails with
 * INILLUCENT_INTERRUPTED and the connection stays usable.
 *
 * inillucent_supports("cancel") reports PARTIAL, and the limit it is reporting
 * is WHEN rather than whether: the flag is read at every leaf of a scan and
 * every batch a result collects, so a long scan, a large result and a slow join
 * all stop, while a single operator part-way through one indivisible piece of
 * work finishes it first. Draw a Stop button; do not promise it is instant.
 *
 * A cancel with nothing running cancels nothing: the next statement clears the
 * flag as it starts. */
int32_t inillucent_cancel(inillucent_conn *conn, inillucent_error **error);

/* ------------------------------------------------------------------ */
/* A statement, for binding values and running more than once.          */
/* ------------------------------------------------------------------ */

int32_t inillucent_prepare(inillucent_conn *conn, const char *sql,
                           inillucent_stmt **out, inillucent_error **error);

/* Frees the statement. Legal before or after its connection is freed. */
void inillucent_stmt_free(inillucent_stmt *stmt);

/* Parameters are one-based, matching ?1, ?2 in the SQL. Binding an index past
 * the end grows the binding list with NULLs, which is what makes binding out
 * of order work. Every one of these copies what it is given. */
int32_t inillucent_bind_null(inillucent_stmt *stmt, uint32_t index);
int32_t inillucent_bind_int(inillucent_stmt *stmt, uint32_t index, int64_t value);
int32_t inillucent_bind_real(inillucent_stmt *stmt, uint32_t index, double value);
int32_t inillucent_bind_text(inillucent_stmt *stmt, uint32_t index,
                             const char *value, size_t len);
int32_t inillucent_bind_blob(inillucent_stmt *stmt, uint32_t index,
                             const uint8_t *value, size_t len);

/* Unbinds everything. */
void inillucent_clear_bindings(inillucent_stmt *stmt);

/* Runs it with what is bound. May be called repeatedly with new bindings. */
int32_t inillucent_stmt_execute(inillucent_stmt *stmt, uint64_t limit,
                                inillucent_rows **out, inillucent_error **error);

/* ------------------------------------------------------------------ */
/* A result. Materialised, so every pointer below is stable until free. */
/* ------------------------------------------------------------------ */

void inillucent_rows_free(inillucent_rows *rows);

size_t      inillucent_rows_column_count(const inillucent_rows *rows);

/* A C string valid until the result is freed; NULL if `nth` is past the end. */
const char *inillucent_rows_column_name(const inillucent_rows *rows, size_t nth);

/* The type the schema declared, or "" for an expression, which has none. */
const char *inillucent_rows_column_type(const inillucent_rows *rows, size_t nth);

/* How many rows you were handed. */
size_t inillucent_rows_count(const inillucent_rows *rows);

/* How many the statement produced, EXACTLY - not an estimate. The engine
 * materialises, so this was counted rather than guessed, which is what lets a
 * grid say "1-200 of 4,317" honestly. */
size_t inillucent_rows_total(const inillucent_rows *rows);

/* 1 when the limit cut something off. */
int32_t inillucent_rows_more(const inillucent_rows *rows);

/* Rows changed, or -1 for a statement that changed nothing (a query). */
int64_t inillucent_rows_affected(const inillucent_rows *rows);

uint64_t inillucent_rows_elapsed_us(const inillucent_rows *rows);

/* A one-line summary for a status bar - "SELECT 27". A C string. */
const char *inillucent_rows_tag(const inillucent_rows *rows);

/* One of the INILLUCENT_NULL..INILLUCENT_BLOB values, or INILLUCENT_NULL for
 * a cell that is not there - so check the counts rather than probing. */
int32_t inillucent_value_type(const inillucent_rows *rows, size_t row, size_t column);

int64_t inillucent_value_int(const inillucent_rows *rows, size_t row, size_t column);
double  inillucent_value_real(const inillucent_rows *rows, size_t row, size_t column);

/* Text or blob bytes, with the length written to *len. NOT NUL-terminated: a
 * text value may contain a NUL byte and truncating there would lose data
 * silently. Returns NULL and sets *len to 0 for NULL, a number, or a cell that
 * is not there. */
const uint8_t *inillucent_value_bytes(const inillucent_rows *rows, size_t row,
                                      size_t column, size_t *len);

/* ------------------------------------------------------------------ */
/* A transaction.                                                      */
/* ------------------------------------------------------------------ */

/*
 * WHY THIS IS A HANDLE AND NOT A PAIR OF FUNCTIONS
 *
 * The rule this exists to serve is that a check on what a write DID must
 * happen before the COMMIT, not after: a postcondition tested after the commit
 * is a report about something that has already happened rather than a guard
 * against it. So a transaction is a handle you hold while you read
 * inillucent_txn_affected for each statement, decide, and only then commit.
 *
 * A handle freed without commit or rollback rolls back.
 */
int32_t inillucent_txn_begin(inillucent_conn *conn, inillucent_txn **out,
                             inillucent_error **error);

/* Runs one statement inside it, writing how many rows it changed to
 * *affected. On failure the transaction is rolled back and the handle is
 * spent: free it. */
int32_t inillucent_txn_execute(inillucent_txn *txn, const char *sql,
                               uint64_t *affected, inillucent_error **error);

/* Commits. The handle is spent either way; free it. */
int32_t inillucent_txn_commit(inillucent_txn *txn, inillucent_error **error);

/* Rolls back and frees. Never fails in a way you can act on, so it returns
 * nothing. */
void inillucent_txn_rollback(inillucent_txn *txn);

/* ------------------------------------------------------------------ */
/* A failure.                                                          */
/* ------------------------------------------------------------------ */

int32_t inillucent_error_status(const inillucent_error *error);

/* What happened, in the engine's own words. A C string, safe to show a person:
 * it never holds a file-system path, a bound value, or page bytes. */
const char *inillucent_error_message(const inillucent_error *error);

/* The construct the engine has not implemented - "an outer join", "VACUUM".
 * NULL unless the status is INILLUCENT_UNSUPPORTED. Finer-grained than the
 * capability table on purpose, so an application can name what it hit without
 * owning a list of every phrase. */
const char *inillucent_error_feature(const inillucent_error *error);

/* Internal diagnostic text. NULL unless the database was opened with
 * INILLUCENT_OPEN_DIAGNOSTICS. It MAY hold a path or a bound value, so do not
 * show it to a person and do not send it to a shared log. */
const char *inillucent_error_detail(const inillucent_error *error);

/* The byte offset into the statement, or -1 when there is none. */
int32_t inillucent_error_offset(const inillucent_error *error);

void inillucent_error_free(inillucent_error *error);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* INILLUCENT_DRIVER_H */
