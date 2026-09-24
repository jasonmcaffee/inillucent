/*
 * The public C ABI, exercised from C.
 *
 * Invariant: every handle type, every documented null behaviour and every
 * destruction order in `include/inillucent_driver.h` is called here from a C
 * caller compiled against that header. `tests/abi.rs` compares the header, the
 * manifest and the exported symbols; it cannot call anything, so before this
 * file 53 exported functions had five structural tests and no behaviour at all.
 *
 * The destruction-order block near the end is the reason this exists.
 * `inillucent_conn_free` used to release the connection while a statement
 * still held a pointer to it, and stepping that statement afterwards read
 * freed memory - in a language where nothing would have said so. Under an
 * address sanitizer these cases are what catches that; without one they still
 * fail loudly, because a released connection leaves a session behind and the
 * queries below stop answering.
 *
 * Output is one `ok <name>` or `FAIL <name>: <what>` line per check, then
 * `done`. The Rust harness that builds this asserts there is no FAIL line and
 * that `done` was reached, so a program that dies half way through is a
 * failure rather than a pass with fewer lines.
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#ifdef _WIN32
#include <io.h>
#else
#include <dirent.h>
#endif

#include "inillucent_driver.h"

/* How many checks failed. The process exit code, so a harness that only looks
 * at the status still learns something. */
static int failures = 0;

/*
 * Records one check.
 *
 * @param name - what was checked
 * @param passed - whether it held
 * @param detail - what was seen instead, printed only on a failure
 */
static void check(const char *name, int passed, const char *detail)
{
    if (passed) {
        printf("ok %s\n", name);
    } else {
        printf("FAIL %s: %s\n", name, detail ? detail : "");
        failures += 1;
    }
    fflush(stdout);
}

/*
 * Records a check whose failure is best described by a status code.
 *
 * @param name - what was checked
 * @param found - the status that came back
 * @param wanted - the status the header promises
 */
static void check_status(const char *name, int32_t found, int32_t wanted)
{
    char detail[128];
    snprintf(detail, sizeof detail, "status %d, wanted %d", (int)found, (int)wanted);
    check(name, found == wanted, detail);
}

/*
 * Removes every numbered log segment of a database, `<path>-wal.<n>`.
 *
 * The engine writes its log as numbered segments rather than one `-wal`
 * file, and their numbers are not predictable once a checkpoint has removed
 * the early ones, so they are found by listing the directory. Every path this
 * program uses is a bare name in its working directory, which is what the
 * listing below reads.
 *
 * @param path - the database file, a name in the working directory
 */
static void scrub_segments(const char *path)
{
    char prefix[512];
    size_t length;
    snprintf(prefix, sizeof prefix, "%s-wal.", path);
    length = strlen(prefix);
#ifdef _WIN32
    {
        char pattern[520];
        struct _finddata_t found;
        intptr_t search;
        snprintf(pattern, sizeof pattern, "%s*", prefix);
        search = _findfirst(pattern, &found);
        if (search == -1) {
            return;
        }
        do {
            if (strncmp(found.name, prefix, length) == 0) {
                remove(found.name);
            }
        } while (_findnext(search, &found) == 0);
        _findclose(search);
    }
#else
    {
        DIR *directory = opendir(".");
        struct dirent *entry;
        if (directory == NULL) {
            return;
        }
        while ((entry = readdir(directory)) != NULL) {
            if (strncmp(entry->d_name, prefix, length) == 0) {
                remove(entry->d_name);
            }
        }
        closedir(directory);
    }
#endif
}

/*
 * Removes a database and its companion files, so a rerun starts clean.
 *
 * The numbered log segments are the ones that matter (task-2110, bug 1): a
 * database opened beside the last run's segments replays them, and before
 * this removed them every run left one more set behind.
 *
 * @param path - the database file
 */
static void scrub(const char *path)
{
    char companion[512];
    remove(path);
    snprintf(companion, sizeof companion, "%s-wal", path);
    remove(companion);
    snprintf(companion, sizeof companion, "%s-journal", path);
    remove(companion);
    snprintf(companion, sizeof companion, "%s-shm", path);
    remove(companion);
    scrub_segments(path);
}

/* ---------------------------------------------------------------- */
/* The library itself                                                */
/* ---------------------------------------------------------------- */

/* Checks the calls that need no handle at all. */
static void library_surface(void)
{
    const char *name = NULL;
    const char *note = NULL;
    int32_t state = 0;
    size_t total = 0;
    size_t nth = 0;
    int every_name = 1;

    check("abi_version_has_a_major",
          inillucent_abi_version() >= 1000000u,
          "the major version is zero");
    check("version_is_a_string",
          inillucent_version() != NULL && inillucent_version()[0] != '\0',
          "the version string is empty");

    total = inillucent_capability_count();
    check("capabilities_are_listed", total > 0, "no capabilities at all");

    /* Every row reads, and the out-parameters are all optional. */
    for (nth = 0; nth < total; nth += 1) {
        name = NULL;
        state = INILLUCENT_SUPPORT_UNKNOWN;
        note = NULL;
        if (inillucent_capability(nth, &name, &state, &note) != INILLUCENT_OK) {
            every_name = 0;
            break;
        }
        if (name == NULL || name[0] == '\0' || note == NULL) {
            every_name = 0;
            break;
        }
        if (state != INILLUCENT_SUPPORT_NO && state != INILLUCENT_SUPPORT_YES &&
            state != INILLUCENT_SUPPORT_PARTIAL) {
            every_name = 0;
            break;
        }
    }
    check("every_capability_reads", every_name, "a capability row was empty or unnamed");
    check_status("capability_out_of_range_refuses",
                 inillucent_capability(total, &name, &state, &note),
                 INILLUCENT_INVALID_STATE);
    check_status("capability_out_parameters_are_optional",
                 inillucent_capability(0, NULL, NULL, NULL),
                 INILLUCENT_OK);
    check("an_unknown_capability_is_unknown",
          inillucent_supports("no-such-capability-exists") == INILLUCENT_SUPPORT_UNKNOWN,
          "an invented capability was not reported unknown");
    check("a_null_capability_name_is_unknown",
          inillucent_supports(NULL) == INILLUCENT_SUPPORT_UNKNOWN,
          "a null name was not reported unknown");
}

/* ---------------------------------------------------------------- */
/* Null and misuse behaviour                                         */
/* ---------------------------------------------------------------- */

/*
 * Every documented null behaviour, in one place.
 *
 * The header's rule is that freeing NULL is a no-op "so a finaliser need not
 * check", and a finaliser is exactly where a null arrives. An accessor given
 * null returns the zero of its type rather than reading through it.
 */
static void null_behaviour(void)
{
    inillucent_error *error = NULL;

    /* None of these may fault. */
    inillucent_conn_free(NULL);
    inillucent_stmt_free(NULL);
    inillucent_rows_free(NULL);
    inillucent_txn_rollback(NULL);
    inillucent_error_free(NULL);
    check("freeing_null_is_a_no_op", 1, NULL);

    check("path_of_null_is_null", inillucent_path(NULL) == NULL, "a path came back");
    check("column_count_of_null_is_zero",
          inillucent_rows_column_count(NULL) == 0, "a count came back");
    check("column_name_of_null_is_null",
          inillucent_rows_column_name(NULL, 0) == NULL, "a name came back");
    check("rows_count_of_null_is_zero", inillucent_rows_count(NULL) == 0, "a count came back");
    check("rows_total_of_null_is_zero", inillucent_rows_total(NULL) == 0, "a total came back");
    check("rows_more_of_null_is_zero", inillucent_rows_more(NULL) == 0, "more came back");
    check("value_type_of_null_is_null_kind",
          inillucent_value_type(NULL, 0, 0) == INILLUCENT_NULL, "a type came back");
    check("value_int_of_null_is_zero", inillucent_value_int(NULL, 0, 0) == 0, "a value came back");
    check("error_message_of_null_is_null",
          inillucent_error_message(NULL) == NULL, "a message came back");
    check("error_feature_of_null_is_null",
          inillucent_error_feature(NULL) == NULL, "a feature came back");
    check("error_detail_of_null_is_null",
          inillucent_error_detail(NULL) == NULL, "a detail came back");

    check_status("opening_a_null_path_is_misuse",
                 inillucent_open(NULL, INILLUCENT_OPEN_CREATE, NULL, &error),
                 INILLUCENT_INVALID_STATE);
    check("a_misuse_reports_a_message",
          error != NULL && inillucent_error_message(error) != NULL,
          "no error handle came back from a misuse");
    inillucent_error_free(error);
    error = NULL;

    check_status("closing_null_is_misuse",
                 inillucent_close(NULL, &error), INILLUCENT_INVALID_STATE);
    inillucent_error_free(error);
    error = NULL;

    check_status("connecting_to_null_is_misuse",
                 inillucent_connect(NULL, NULL, &error), INILLUCENT_INVALID_STATE);
    inillucent_error_free(error);
    error = NULL;

    check_status("executing_on_null_is_misuse",
                 inillucent_execute(NULL, "SELECT 1", 10, NULL, &error),
                 INILLUCENT_INVALID_STATE);
    inillucent_error_free(error);
}

/* ---------------------------------------------------------------- */
/* Rows, values and statements                                       */
/* ---------------------------------------------------------------- */

/*
 * Prepares, binds each of the five kinds, runs and reads back.
 *
 * @param conn - a live connection
 */
static void statements(inillucent_conn *conn)
{
    inillucent_error *error = NULL;
    inillucent_stmt *stmt = NULL;
    inillucent_rows *rows = NULL;
    const uint8_t *bytes = NULL;
    size_t len = 0;
    const char *column = NULL;
    /* Text with a NUL in the middle: the header promises the byte accessor
     * does not truncate there, and a C string would. */
    static const char embedded[] = "ab\0cd";
    static const uint8_t blob[] = {0x00, 0xff, 0x10};

    check_status("prepare",
                 inillucent_prepare(conn, "INSERT INTO t (i, r, s, b, n) VALUES (?1,?2,?3,?4,?5)",
                                    &stmt, &error),
                 INILLUCENT_OK);
    inillucent_error_free(error);
    error = NULL;
    if (stmt == NULL) {
        check("prepare_handed_back_a_handle", 0, "no statement handle");
        return;
    }

    check_status("bind_int", inillucent_bind_int(stmt, 1, 42), INILLUCENT_OK);
    check_status("bind_real", inillucent_bind_real(stmt, 2, 1.5), INILLUCENT_OK);
    check_status("bind_text",
                 inillucent_bind_text(stmt, 3, embedded, sizeof embedded - 1), INILLUCENT_OK);
    check_status("bind_blob",
                 inillucent_bind_blob(stmt, 4, blob, sizeof blob), INILLUCENT_OK);
    check_status("bind_null", inillucent_bind_null(stmt, 5), INILLUCENT_OK);
    /* Binding out of order grows the list with NULLs, which is what a binding
     * built from a dictionary needs - but only inside the statement's own
     * parameter count. An index past it used to grow the list to match
     * whatever number arrived (task-1979, D3), so `?9` on a five parameter
     * statement was accepted and a large index asked the allocator for tens of
     * gigabytes. */
    check_status("bind_past_the_declared_count",
                 inillucent_bind_int(stmt, 9, 7), INILLUCENT_MISUSE);
    check_status("bind_index_zero_is_refused",
                 inillucent_bind_int(stmt, 0, 7), INILLUCENT_INVALID_STATE);
    check_status("bind_on_null_statement_is_refused",
                 inillucent_bind_int(NULL, 1, 7), INILLUCENT_INVALID_STATE);

    /* Nine bound values against five parameters is a misuse the engine
     * reports rather than a crash, so clear and rebind exactly five. */
    inillucent_clear_bindings(stmt);
    inillucent_clear_bindings(NULL);
    check("clearing_bindings_on_null_is_a_no_op", 1, NULL);
    (void)inillucent_bind_int(stmt, 1, 42);
    (void)inillucent_bind_real(stmt, 2, 1.5);
    (void)inillucent_bind_text(stmt, 3, embedded, sizeof embedded - 1);
    (void)inillucent_bind_blob(stmt, 4, blob, sizeof blob);
    (void)inillucent_bind_null(stmt, 5);

    check_status("stmt_execute", inillucent_stmt_execute(stmt, 10, &rows, &error), INILLUCENT_OK);
    inillucent_error_free(error);
    error = NULL;
    check("an_insert_reports_one_row_changed",
          rows != NULL && inillucent_rows_affected(rows) == 1, "affected was not 1");
    inillucent_rows_free(rows);
    rows = NULL;

    /* The same statement again with new bindings, which is what a prepared
     * statement is for. */
    (void)inillucent_bind_int(stmt, 1, 43);
    check_status("stmt_execute_again",
                 inillucent_stmt_execute(stmt, 10, &rows, &error), INILLUCENT_OK);
    inillucent_error_free(error);
    error = NULL;
    inillucent_rows_free(rows);
    rows = NULL;
    inillucent_stmt_free(stmt);
    stmt = NULL;

    check_status("query",
                 inillucent_execute(conn, "SELECT i, r, s, b, n FROM t ORDER BY i", 10,
                                    &rows, &error),
                 INILLUCENT_OK);
    inillucent_error_free(error);
    error = NULL;
    if (rows == NULL) {
        check("the_query_handed_back_rows", 0, "no rows handle");
        return;
    }

    check("five_columns", inillucent_rows_column_count(rows) == 5, "column count is wrong");
    check("two_rows", inillucent_rows_count(rows) == 2, "row count is wrong");
    check("the_total_is_exact", inillucent_rows_total(rows) == 2, "total is wrong");
    check("nothing_was_cut_off", inillucent_rows_more(rows) == 0, "more was set");
    check("a_query_changed_nothing", inillucent_rows_affected(rows) == -1, "affected was not -1");
    check("elapsed_is_reported", inillucent_rows_elapsed_us(rows) < 60000000u, "elapsed is absurd");
    check("a_tag_is_reported",
          inillucent_rows_tag(rows) != NULL && inillucent_rows_tag(rows)[0] != '\0',
          "the tag is empty");

    column = inillucent_rows_column_name(rows, 0);
    check("the_first_column_is_named", column != NULL && strcmp(column, "i") == 0, "wrong name");
    check("a_column_past_the_end_is_null",
          inillucent_rows_column_name(rows, 99) == NULL, "a name came back");
    check("a_declared_type_is_reported",
          inillucent_rows_column_type(rows, 0) != NULL, "no declared type");

    check("an_integer_reads_back",
          inillucent_value_type(rows, 0, 0) == INILLUCENT_INTEGER &&
              inillucent_value_int(rows, 0, 0) == 42,
          "the integer did not read back");
    check("a_real_reads_back",
          inillucent_value_type(rows, 0, 1) == INILLUCENT_REAL &&
              inillucent_value_real(rows, 0, 1) > 1.4 &&
              inillucent_value_real(rows, 0, 1) < 1.6,
          "the real did not read back");

    bytes = inillucent_value_bytes(rows, 0, 2, &len);
    check("text_with_a_nul_reads_back_whole",
          bytes != NULL && len == sizeof embedded - 1 &&
              memcmp(bytes, embedded, sizeof embedded - 1) == 0,
          "the text was truncated at the NUL");

    bytes = inillucent_value_bytes(rows, 0, 3, &len);
    check("a_blob_reads_back",
          bytes != NULL && len == sizeof blob && memcmp(bytes, blob, sizeof blob) == 0,
          "the blob did not read back");

    check("a_null_is_null", inillucent_value_type(rows, 0, 4) == INILLUCENT_NULL, "not null");
    bytes = inillucent_value_bytes(rows, 0, 4, &len);
    check("bytes_of_a_null_is_null_and_zero",
          bytes == NULL && len == 0, "a null gave back bytes");

    /* A cell that is not there reports the null kind rather than reading past
     * the end, which is the documented behaviour. */
    check("a_cell_past_the_end_is_the_null_kind",
          inillucent_value_type(rows, 99, 99) == INILLUCENT_NULL, "a type came back");
    bytes = inillucent_value_bytes(rows, 99, 99, &len);
    check("bytes_past_the_end_are_null_and_zero", bytes == NULL && len == 0, "bytes came back");

    inillucent_rows_free(rows);
}

/*
 * A limit cuts the rows handed back without changing the exact total.
 *
 * @param conn - a live connection
 */
static void limits(inillucent_conn *conn)
{
    inillucent_error *error = NULL;
    inillucent_rows *rows = NULL;

    check_status("limited_query",
                 inillucent_execute(conn, "SELECT i FROM t ORDER BY i", 1, &rows, &error),
                 INILLUCENT_OK);
    inillucent_error_free(error);
    check("a_limit_cuts_the_rows_handed_back",
          rows != NULL && inillucent_rows_count(rows) == 1, "the limit was not applied");
    check("the_total_is_still_exact",
          rows != NULL && inillucent_rows_total(rows) == 2, "the total followed the limit");
    check("more_says_something_was_cut_off",
          rows != NULL && inillucent_rows_more(rows) == 1, "more was not set");
    inillucent_rows_free(rows);
}

/* ---------------------------------------------------------------- */
/* Failures                                                          */
/* ---------------------------------------------------------------- */

/*
 * Every way a failure is reported, including the one that is optional.
 *
 * @param conn - a live connection
 * @param diagnostics - whether the database was opened asking for detail
 */
static void failure_reporting(inillucent_conn *conn, int diagnostics)
{
    inillucent_error *error = NULL;
    int32_t status = 0;

    status = inillucent_execute(conn, "SELEKT 1", 10, NULL, &error);
    check("a_bad_statement_is_not_ok", status != INILLUCENT_OK, "nonsense parsed");
    check("a_failure_hands_back_an_error", error != NULL, "no error handle");
    if (error != NULL) {
        check("the_error_status_matches_the_return",
              inillucent_error_status(error) == status, "the two disagree");
        check("the_error_has_a_message",
              inillucent_error_message(error) != NULL &&
                  inillucent_error_message(error)[0] != '\0',
              "the message is empty");
        check("the_error_has_an_offset",
              inillucent_error_offset(error) >= -1, "the offset is below -1");
        if (diagnostics) {
            /* Detail is what INILLUCENT_OPEN_DIAGNOSTICS buys, and it may
             * legitimately be absent for a failure with nothing internal to
             * say - so this checks it is readable, not that it is there. */
            (void)inillucent_error_detail(error);
        }
    }
    inillucent_error_free(error);
    error = NULL;

    /* A failure with no error out-parameter must still be a failure, and must
     * not leak the error it built. */
    check("a_failure_with_no_out_parameter_still_fails",
          inillucent_execute(conn, "SELEKT 1", 10, NULL, NULL) != INILLUCENT_OK,
          "nonsense parsed with no error parameter");

    /* A construct the engine has not built reports a feature name rather than
     * a syntax error, which is the distinction the whole driver is shaped
     * around. It is only a check when the engine actually refuses one. */
    status = inillucent_execute(conn, "SELECT 1 FROM t WHERE i IN (SELECT i FROM t) FOR UPDATE",
                                10, NULL, &error);
    if (status == INILLUCENT_UNSUPPORTED && error != NULL) {
        check("an_unsupported_construct_names_the_feature",
              inillucent_error_feature(error) != NULL, "no feature named");
    }
    inillucent_error_free(error);
}

/*
 * Cancellation, which the header says is always refused by name.
 *
 * @param conn - a live connection
 */
static void cancellation(inillucent_conn *conn)
{
    inillucent_error *error = NULL;
    int32_t status = inillucent_cancel(conn, &error);
    int32_t claimed = inillucent_supports("cancel");

    /* Whatever the answer is, the capability table and the call must agree.
     * The header promises UNSUPPORTED today and says a binding should wire it
     * once so it starts working the day the capability flips - so this checks
     * the two together rather than freezing the answer. */
    if (claimed == INILLUCENT_SUPPORT_NO) {
        check_status("cancel_is_refused_while_the_table_says_no",
                     status, INILLUCENT_UNSUPPORTED);
    } else {
        check("cancel_works_when_the_table_says_it_does",
              status == INILLUCENT_OK, "the table claims cancel and the call refused");
    }
    inillucent_error_free(error);
}

/* ---------------------------------------------------------------- */
/* Transactions                                                      */
/* ---------------------------------------------------------------- */

/*
 * Commit, rollback, and the two spent-handle refusals.
 *
 * @param conn - a live connection
 */
static void transactions(inillucent_conn *conn)
{
    inillucent_error *error = NULL;
    inillucent_txn *txn = NULL;
    inillucent_rows *rows = NULL;
    uint64_t affected = 0;

    check_status("txn_begin", inillucent_txn_begin(conn, &txn, &error), INILLUCENT_OK);
    inillucent_error_free(error);
    error = NULL;
    check("in_transaction_is_set", inillucent_in_transaction(conn) == 1, "not in a transaction");
    check_status("txn_execute",
                 inillucent_txn_execute(txn, "INSERT INTO t (i) VALUES (100)", &affected, &error),
                 INILLUCENT_OK);
    inillucent_error_free(error);
    error = NULL;
    check("txn_execute_reports_the_row_count", affected == 1, "affected was not 1");
    check_status("txn_commit", inillucent_txn_commit(txn, &error), INILLUCENT_OK);
    inillucent_error_free(error);
    error = NULL;
    check_status("committing_twice_is_refused",
                 inillucent_txn_commit(txn, &error), INILLUCENT_INVALID_STATE);
    inillucent_error_free(error);
    error = NULL;
    /* A spent handle is still freed with rollback, which is the free function
     * for this handle and returns at once when there is nothing to undo. */
    inillucent_txn_rollback(txn);
    txn = NULL;
    check("in_transaction_is_clear_after_commit",
          inillucent_in_transaction(conn) == 0, "still in a transaction");

    check_status("txn_begin_again", inillucent_txn_begin(conn, &txn, &error), INILLUCENT_OK);
    inillucent_error_free(error);
    error = NULL;
    (void)inillucent_txn_execute(txn, "INSERT INTO t (i) VALUES (101)", &affected, &error);
    inillucent_error_free(error);
    error = NULL;
    inillucent_txn_rollback(txn);
    txn = NULL;

    check_status("count_after_rollback",
                 inillucent_execute(conn, "SELECT count(*) FROM t WHERE i >= 100", 10,
                                    &rows, &error),
                 INILLUCENT_OK);
    inillucent_error_free(error);
    check("a_rollback_undid_its_row",
          rows != NULL && inillucent_value_int(rows, 0, 0) == 1,
          "the rolled-back row is still there, or the committed one is not");
    inillucent_rows_free(rows);

    check("last_insert_rowid_is_reported",
          inillucent_last_insert_rowid(conn) != 0, "no rowid");
    check("total_changes_is_reported",
          inillucent_total_changes(conn) > 0, "no changes counted");
    check("a_schema_cookie_is_reported",
          inillucent_schema_cookie(conn) != 0, "no cookie");
}

/* ---------------------------------------------------------------- */
/* Destruction order                                                 */
/* ---------------------------------------------------------------- */

/*
 * A statement outliving the connection it was prepared on.
 *
 * The order the header did not mention and the implementation did not survive.
 * The statement must keep working, because it shares the connection's state
 * rather than pointing at the handle - and the database must still refuse to
 * close, because the session is still in use.
 *
 * @param path - a scratch database file
 */
static void statement_outliving_its_connection(const char *path)
{
    inillucent_error *error = NULL;
    inillucent_db *db = NULL;
    inillucent_conn *conn = NULL;
    inillucent_stmt *stmt = NULL;
    inillucent_rows *rows = NULL;

    scrub(path);
    check_status("order_open", inillucent_open(path, INILLUCENT_OPEN_CREATE, &db, &error),
                 INILLUCENT_OK);
    inillucent_error_free(error);
    error = NULL;
    check_status("order_connect", inillucent_connect(db, &conn, &error), INILLUCENT_OK);
    inillucent_error_free(error);
    error = NULL;
    (void)inillucent_execute_batch(conn, "CREATE TABLE t (i INTEGER)", &error);
    inillucent_error_free(error);
    error = NULL;
    check_status("order_prepare",
                 inillucent_prepare(conn, "SELECT 1", &stmt, &error), INILLUCENT_OK);
    inillucent_error_free(error);
    error = NULL;

    /* The connection goes first, on purpose. */
    inillucent_conn_free(conn);
    conn = NULL;

    check_status("a_statement_still_runs_after_its_connection_is_freed",
                 inillucent_stmt_execute(stmt, 10, &rows, &error), INILLUCENT_OK);
    inillucent_error_free(error);
    error = NULL;
    check("and_it_still_answers",
          rows != NULL && inillucent_value_int(rows, 0, 0) == 1, "the wrong answer");
    inillucent_rows_free(rows);
    rows = NULL;

    check_status("closing_is_refused_while_a_statement_outlives_its_connection",
                 inillucent_close(db, &error), INILLUCENT_INVALID_STATE);
    check("the_refusal_says_why",
          error != NULL && inillucent_error_message(error) != NULL, "no message");
    inillucent_error_free(error);
    error = NULL;

    inillucent_stmt_free(stmt);
    stmt = NULL;
    check_status("closing_works_once_the_statement_is_freed",
                 inillucent_close(db, &error), INILLUCENT_OK);
    inillucent_error_free(error);
    scrub(path);
}

/*
 * A transaction outliving the connection it was opened on.
 *
 * Same shape, and the rollback a freed transaction performs has to reach a
 * session that is still there.
 *
 * @param path - a scratch database file
 */
static void transaction_outliving_its_connection(const char *path)
{
    inillucent_error *error = NULL;
    inillucent_db *db = NULL;
    inillucent_conn *conn = NULL;
    inillucent_conn *second = NULL;
    inillucent_txn *txn = NULL;
    inillucent_rows *rows = NULL;
    uint64_t affected = 0;

    scrub(path);
    (void)inillucent_open(path, INILLUCENT_OPEN_CREATE, &db, &error);
    inillucent_error_free(error);
    error = NULL;
    (void)inillucent_connect(db, &conn, &error);
    inillucent_error_free(error);
    error = NULL;
    (void)inillucent_execute_batch(conn, "CREATE TABLE t (i INTEGER)", &error);
    inillucent_error_free(error);
    error = NULL;
    (void)inillucent_txn_begin(conn, &txn, &error);
    inillucent_error_free(error);
    error = NULL;

    inillucent_conn_free(conn);
    conn = NULL;

    check_status("a_transaction_still_runs_after_its_connection_is_freed",
                 inillucent_txn_execute(txn, "INSERT INTO t (i) VALUES (1)", &affected, &error),
                 INILLUCENT_OK);
    inillucent_error_free(error);
    error = NULL;
    check_status("committing_after_the_connection_is_freed_works",
                 inillucent_txn_commit(txn, &error), INILLUCENT_OK);
    inillucent_error_free(error);
    error = NULL;

    check_status("closing_is_refused_while_a_transaction_outlives_its_connection",
                 inillucent_close(db, &error), INILLUCENT_INVALID_STATE);
    inillucent_error_free(error);
    error = NULL;

    inillucent_txn_rollback(txn);
    txn = NULL;

    /* The write really landed, read on a connection opened after the one that
     * made it was freed. */
    (void)inillucent_connect(db, &second, &error);
    inillucent_error_free(error);
    error = NULL;
    (void)inillucent_execute(second, "SELECT count(*) FROM t", 10, &rows, &error);
    inillucent_error_free(error);
    error = NULL;
    check("the_committed_row_survived_its_connection",
          rows != NULL && inillucent_value_int(rows, 0, 0) == 1, "the row is not there");
    inillucent_rows_free(rows);
    inillucent_conn_free(second);

    check_status("closing_works_once_every_child_is_freed",
                 inillucent_close(db, &error), INILLUCENT_OK);
    inillucent_error_free(error);
    scrub(path);
}

/*
 * A result outliving everything, which it may because it owns its data.
 *
 * @param path - a scratch database file
 */
static void rows_outliving_everything(const char *path)
{
    inillucent_error *error = NULL;
    inillucent_db *db = NULL;
    inillucent_conn *conn = NULL;
    inillucent_rows *rows = NULL;
    const char *name = NULL;

    scrub(path);
    (void)inillucent_open(path, INILLUCENT_OPEN_CREATE, &db, &error);
    inillucent_error_free(error);
    error = NULL;
    (void)inillucent_connect(db, &conn, &error);
    inillucent_error_free(error);
    error = NULL;
    (void)inillucent_execute(conn, "SELECT 7 AS seven", 10, &rows, &error);
    inillucent_error_free(error);
    error = NULL;

    inillucent_conn_free(conn);
    check_status("closing_works_with_only_a_result_outstanding",
                 inillucent_close(db, &error), INILLUCENT_OK);
    inillucent_error_free(error);

    /* Every pointer into the result is still good, which is rule 2 of the
     * header's ownership rules taken to its limit. */
    name = inillucent_rows_column_name(rows, 0);
    check("a_result_outlives_the_database_it_came_from",
          rows != NULL && inillucent_value_int(rows, 0, 0) == 7 && name != NULL &&
              strcmp(name, "seven") == 0,
          "the result did not survive");
    inillucent_rows_free(rows);
    scrub(path);
}

/*
 * The database refuses to close while a connection is open on it.
 *
 * @param path - a scratch database file
 */
static void closing_is_refused_while_connected(const char *path)
{
    inillucent_error *error = NULL;
    inillucent_db *db = NULL;
    inillucent_conn *conn = NULL;

    scrub(path);
    (void)inillucent_open(path, INILLUCENT_OPEN_CREATE, &db, &error);
    inillucent_error_free(error);
    error = NULL;
    (void)inillucent_connect(db, &conn, &error);
    inillucent_error_free(error);
    error = NULL;

    check_status("closing_is_refused_while_connected",
                 inillucent_close(db, &error), INILLUCENT_INVALID_STATE);
    inillucent_error_free(error);
    error = NULL;

    /* The refusal left the database usable rather than half closed, which is
     * what makes it a refusal rather than a failure. */
    check_status("the_database_still_works_after_a_refused_close",
                 inillucent_checkpoint(db, &error), INILLUCENT_OK);
    inillucent_error_free(error);
    error = NULL;

    inillucent_conn_free(conn);
    check_status("closing_works_once_the_connection_is_freed",
                 inillucent_close(db, &error), INILLUCENT_OK);
    inillucent_error_free(error);
    scrub(path);
}

/* ---------------------------------------------------------------- */
/* Whole-database calls                                              */
/* ---------------------------------------------------------------- */

/*
 * Checkpoint, integrity check, backup and the path accessor.
 *
 * @param db - a live database
 * @param path - the file it was opened on
 * @param backup - where the copy goes
 */
static void database_surface(inillucent_db *db, const char *path, const char *backup)
{
    inillucent_error *error = NULL;
    const char *reported = NULL;

    check_status("checkpoint", inillucent_checkpoint(db, &error), INILLUCENT_OK);
    inillucent_error_free(error);
    error = NULL;
    check_status("integrity_check", inillucent_integrity_check(db, &error), INILLUCENT_OK);
    inillucent_error_free(error);
    error = NULL;

    reported = inillucent_path(db);
    check("the_path_is_reported",
          reported != NULL && strstr(reported, "capi-lifecycle") != NULL,
          reported ? reported : "no path");
    (void)path;

    scrub(backup);
    check_status("backup_to", inillucent_backup_to(db, backup, &error), INILLUCENT_OK);
    inillucent_error_free(error);
    error = NULL;
    check_status("backing_up_to_a_null_path_is_misuse",
                 inillucent_backup_to(db, NULL, &error), INILLUCENT_INVALID_STATE);
    inillucent_error_free(error);
    error = NULL;

    /* **The copy is opened and read, not just counted.** The header says the
     * call opens and checks the copy before returning, "because a backup
     * nobody checked is a file that is assumed to be a database" - so a check
     * that only read the status would be asserting the absence of a crash. */
    {
        inillucent_db *copy = NULL;
        inillucent_conn *reader = NULL;
        inillucent_rows *rows = NULL;
        (void)inillucent_open(backup, 0, &copy, &error);
        inillucent_error_free(error);
        error = NULL;
        (void)inillucent_connect(copy, &reader, &error);
        inillucent_error_free(error);
        error = NULL;
        (void)inillucent_execute(reader, "SELECT count(*) FROM t", 10, &rows, &error);
        inillucent_error_free(error);
        error = NULL;
        check("the_backup_is_a_readable_database_with_the_rows_in_it",
              rows != NULL && inillucent_value_int(rows, 0, 0) > 0,
              "the copy did not read back");
        inillucent_rows_free(rows);
        inillucent_conn_free(reader);
        (void)inillucent_close(copy, &error);
        inillucent_error_free(error);
        error = NULL;
    }
    scrub(backup);
}


/* ---------------------------------------------------------------- */
/* Misuse: a freed handle, a double free, an index nothing declared  */
/* ---------------------------------------------------------------- */

/*
 * Every way a caller can break the handle contract, answered by a status.
 *
 * **Before task-1980 each of these was undefined behaviour (task-1979, D1 and
 * D3).** Freeing a database, a connection or an error twice corrupted the heap
 * and ended the process; using a freed handle sometimes returned a silently
 * wrong value - an empty path, a query that answered OK against a freed
 * connection - and sometimes aborted, which the panic guard cannot intercept
 * because an abort is not an unwind. A bind index had no upper bound at all, so
 * one call with a large one asked the allocator for about 137 GB and stalled
 * the process for tens of seconds.
 *
 * Every check below therefore asserts two things at once: the status, and that
 * the process is still here to print the next line.
 *
 * @param path - a database file of this group's own
 */
static void misuse_is_a_status(const char *path)
{
    inillucent_error *error = NULL;
    inillucent_db *db = NULL;
    inillucent_conn *conn = NULL;
    inillucent_stmt *stmt = NULL;
    const char *held = NULL;

    scrub(path);
    check_status("misuse_open",
                 inillucent_open(path, INILLUCENT_OPEN_CREATE, &db, &error),
                 INILLUCENT_OK);
    inillucent_error_free(error);
    error = NULL;
    if (db == NULL) {
        check("misuse_open_handle", 0, "no database handle");
        return;
    }
    check_status("misuse_connect", inillucent_connect(db, &conn, &error), INILLUCENT_OK);
    inillucent_error_free(error);
    error = NULL;
    if (conn == NULL) {
        check("misuse_connect_handle", 0, "no connection handle");
        return;
    }
    check_status("misuse_schema",
                 inillucent_execute_batch(conn, "CREATE TABLE m (a INTEGER, b TEXT)", &error),
                 INILLUCENT_OK);
    inillucent_error_free(error);
    error = NULL;

    /* A bind index past the statement's own parameter count. */
    check_status("misuse_prepare",
                 inillucent_prepare(conn, "INSERT INTO m (a, b) VALUES (?1, ?2)", &stmt, &error),
                 INILLUCENT_OK);
    inillucent_error_free(error);
    error = NULL;
    if (stmt == NULL) {
        check("misuse_prepare_handle", 0, "no statement handle");
        return;
    }
    check_status("bind_index_at_the_ceiling",
                 inillucent_bind_int(stmt, 0xffffffffu, 1),
                 INILLUCENT_MISUSE);
    check_status("bind_index_one_past_the_count",
                 inillucent_bind_int(stmt, 3, 1),
                 INILLUCENT_MISUSE);
    check_status("bind_index_zero", inillucent_bind_int(stmt, 0, 1), INILLUCENT_MISUSE);
    check_status("bind_index_in_range", inillucent_bind_int(stmt, 1, 1), INILLUCENT_OK);
    check_status("bind_index_two_in_range", inillucent_bind_int(stmt, 2, 2), INILLUCENT_OK);

    /* A freed statement, used and then freed again. */
    inillucent_stmt_free(stmt);
    check_status("bind_after_free", inillucent_bind_int(stmt, 1, 1), INILLUCENT_MISUSE);
    inillucent_stmt_free(stmt);
    check("statement_double_free_is_survivable", 1, NULL);
    stmt = NULL;

    /* A freed connection, used and then freed again. */
    inillucent_conn_free(conn);
    check_status("prepare_after_conn_free",
                 inillucent_prepare(conn, "SELECT 1", &stmt, &error),
                 INILLUCENT_MISUSE);
    inillucent_error_free(error);
    error = NULL;
    inillucent_conn_free(conn);
    check("connection_double_free_is_survivable", 1, NULL);
    conn = NULL;

    /* A freed database, read and then closed again. */
    check_status("close", inillucent_close(db, &error), INILLUCENT_OK);
    inillucent_error_free(error);
    error = NULL;
    held = inillucent_path(db);
    check("path_after_close_is_null", held == NULL, "a freed database answered a path");
    check_status("close_twice", inillucent_close(db, &error), INILLUCENT_MISUSE);
    inillucent_error_free(error);
    error = NULL;
    db = NULL;

    /* A freed error, freed again. */
    check_status("error_open_missing",
                 inillucent_open("capi-misuse-not-there.rdb", 0, &db, &error),
                 INILLUCENT_NOT_FOUND);
    inillucent_error_free(error);
    inillucent_error_free(error);
    check("error_double_free_is_survivable", 1, NULL);
    error = NULL;

    scrub(path);
}

/*
 * A TEXT value far past 32 KiB, bound and read back whole.
 *
 * **32,768 bytes used to fail and 32,767 to succeed (task-1979, D2).** A BLOB
 * of the same size went through, and a 40,000 byte TEXT built by a literal
 * expression stored and read back at its full length - so the ceiling was in
 * the bind path rather than in storage.
 *
 * @param path - a database file of this group's own
 */
static void a_large_text_round_trips(const char *path)
{
    const size_t size = 65536;
    inillucent_error *error = NULL;
    inillucent_db *db = NULL;
    inillucent_conn *conn = NULL;
    inillucent_stmt *stmt = NULL;
    inillucent_rows *rows = NULL;
    char *text = NULL;
    const uint8_t *read_back = NULL;
    size_t read_len = 0;
    size_t nth = 0;

    text = (char *)malloc(size + 1);
    if (text == NULL) {
        check("large_text_allocated", 0, "the test could not allocate its own string");
        return;
    }
    for (nth = 0; nth < size; nth += 1) {
        text[nth] = (char)('a' + (nth % 26));
    }
    text[size] = '\0';

    scrub(path);
    check_status("large_text_open",
                 inillucent_open(path, INILLUCENT_OPEN_CREATE, &db, &error),
                 INILLUCENT_OK);
    inillucent_error_free(error);
    error = NULL;
    if (db == NULL) {
        free(text);
        return;
    }
    check_status("large_text_connect", inillucent_connect(db, &conn, &error), INILLUCENT_OK);
    inillucent_error_free(error);
    error = NULL;
    if (conn == NULL) {
        free(text);
        return;
    }
    check_status("large_text_schema",
                 inillucent_execute_batch(conn, "CREATE TABLE big (s TEXT)", &error),
                 INILLUCENT_OK);
    inillucent_error_free(error);
    error = NULL;

    check_status("large_text_prepare",
                 inillucent_prepare(conn, "INSERT INTO big (s) VALUES (?1)", &stmt, &error),
                 INILLUCENT_OK);
    inillucent_error_free(error);
    error = NULL;
    if (stmt == NULL) {
        free(text);
        return;
    }
    check_status("large_text_bind",
                 inillucent_bind_text(stmt, 1, text, size),
                 INILLUCENT_OK);
    check_status("large_text_execute",
                 inillucent_stmt_execute(stmt, 1, &rows, &error),
                 INILLUCENT_OK);
    inillucent_error_free(error);
    error = NULL;
    inillucent_rows_free(rows);
    rows = NULL;
    inillucent_stmt_free(stmt);
    stmt = NULL;

    check_status("large_text_query_prepare",
                 inillucent_prepare(conn, "SELECT s FROM big", &stmt, &error),
                 INILLUCENT_OK);
    inillucent_error_free(error);
    error = NULL;
    if (stmt == NULL) {
        free(text);
        return;
    }
    check_status("large_text_query",
                 inillucent_stmt_execute(stmt, 1, &rows, &error),
                 INILLUCENT_OK);
    inillucent_error_free(error);
    error = NULL;
    inillucent_stmt_free(stmt);
    stmt = NULL;
    if (rows == NULL) {
        free(text);
        return;
    }
    check("large_text_one_row", inillucent_rows_count(rows) == 1, "not exactly one row");
    read_back = inillucent_value_bytes(rows, 0, 0, &read_len);
    check("large_text_length", read_len == size, "the value came back a different length");
    check("large_text_bytes",
          read_back != NULL && read_len == size && memcmp(read_back, text, size) == 0,
          "the value came back different");
    inillucent_rows_free(rows);
    rows = NULL;

    inillucent_conn_free(conn);
    check_status("large_text_close", inillucent_close(db, &error), INILLUCENT_OK);
    inillucent_error_free(error);
    free(text);
    scrub(path);
}

/* ---------------------------------------------------------------- */

/* Runs every group and reports how many checks failed. */
int main(void)
{
    inillucent_error *error = NULL;
    inillucent_db *db = NULL;
    inillucent_conn *conn = NULL;
    const char *path = "capi-lifecycle.rdb";

    library_surface();
    null_behaviour();

    scrub(path);
    check_status("open",
                 inillucent_open(path,
                                 INILLUCENT_OPEN_CREATE | INILLUCENT_OPEN_DIAGNOSTICS,
                                 &db, &error),
                 INILLUCENT_OK);
    inillucent_error_free(error);
    error = NULL;
    if (db == NULL) {
        printf("FAIL open: no database handle\ndone\n");
        return 1;
    }

    check_status("connect", inillucent_connect(db, &conn, &error), INILLUCENT_OK);
    inillucent_error_free(error);
    error = NULL;
    if (conn == NULL) {
        printf("FAIL connect: no connection handle\ndone\n");
        return 1;
    }

    check_status("execute_batch",
                 inillucent_execute_batch(
                     conn, "CREATE TABLE t (i INTEGER, r REAL, s TEXT, b BLOB, n INTEGER)",
                     &error),
                 INILLUCENT_OK);
    inillucent_error_free(error);
    error = NULL;

    statements(conn);
    limits(conn);
    failure_reporting(conn, 1);
    cancellation(conn);
    transactions(conn);
    database_surface(db, path, "capi-lifecycle-backup.rdb");

    inillucent_conn_free(conn);
    check_status("close", inillucent_close(db, &error), INILLUCENT_OK);
    inillucent_error_free(error);
    scrub(path);

    misuse_is_a_status("capi-lifecycle-misuse.rdb");
    a_large_text_round_trips("capi-lifecycle-large.rdb");

    closing_is_refused_while_connected("capi-lifecycle-order-1.rdb");
    statement_outliving_its_connection("capi-lifecycle-order-2.rdb");
    transaction_outliving_its_connection("capi-lifecycle-order-3.rdb");
    rows_outliving_everything("capi-lifecycle-order-4.rdb");

    printf("done\n");
    return failures == 0 ? 0 : 1;
}
