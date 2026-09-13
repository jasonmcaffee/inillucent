//! Roadmap item 3: a `SELECT`'s compiled chain, reused across executions.
//!
//! Invariant: **the cached path and a statement built fresh, from scratch,
//! answer the same rows at every point along a sequence of writes and DDL** -
//! not only right after the chain is first compiled, but after every
//! `INSERT`, `UPDATE`, `DELETE`, `BEGIN`/`ROLLBACK`, `CREATE INDEX`, `ALTER
//! TABLE`, `DETACH`, function registration and `PRAGMA case_sensitive_like`
//! this file can put between two readings of it.
//!
//! `ImportedDatabase::execute_any` is the cached path: the same SQL text asked
//! of the same connection twice finds the same `Rc<Cached>`, and
//! `physical::Slot` inside its `Cached::Select` is built once and reused from
//! then on - see `inillucent_engine::execute_select_cached`.
//! `ImportedDatabase::run_with` is the ground truth: it never touches
//! `Cached` at all, planning and building a fresh pipeline every single call
//! the way the engine did before this ticket. A difference between the two
//! is exactly the defect roadmap item 3 warns a reused chain could have - a
//! stale tree, a stale `LIKE` setting, a stale row.

use inillucent_compat::newengine::ImportedDatabase;
use inillucent_compat::workspace_root;
use inillucent_exec::physical::Params;
use inillucent_ext::registry::FunctionFlags;
use inillucent_sql::plan::Levers;
use inillucent_tree::datum::OwnedDatum;
use inillucent_value::Value;

/// How many frames these databases get; they are tiny.
const FRAMES: usize = 256;

/// The page size, which is the engine's default.
const PAGE_SIZE: usize = 32_768;

/// Returns a clean path for one test's database.
///
/// @param name - the test's name
fn scratch(name: &str) -> std::path::PathBuf {
    let area = workspace_root().join("_agent_output/compiled-chain-reuse");
    let _ = std::fs::create_dir_all(&area);
    let path = area.join(format!("{name}.rdb"));
    let _ = std::fs::remove_file(&path);
    path
}

/// Runs one statement, panicking with its text and detail on refusal.
///
/// @param database - the connection
/// @param sql - the statement text
fn exec(database: &mut ImportedDatabase, sql: &str) {
    database
        .execute_any(sql, &Params::new())
        .unwrap_or_else(|error| panic!("{sql}: {}", error.detail().unwrap_or_default()));
}

/// Asserts that the cached path and a from-scratch build answer the same
/// rows for one `SELECT`, right now.
///
/// @param database - the connection
/// @param select - the query, read through both paths
/// @param params - the values bound to `?1`, `?2`, ...
/// @param when - what just happened, for the failure message
fn assert_cache_agrees_with_fresh(
    database: &mut ImportedDatabase,
    select: &str,
    params: &Params,
    when: &str,
) {
    let cached = database
        .execute_any(select, params)
        .unwrap_or_else(|error| {
            panic!(
                "{when}, cached: {select}: {}",
                error.detail().unwrap_or_default()
            )
        });
    let (fresh_rows, fresh_names) = database.run_with(select, params).unwrap_or_else(|error| {
        panic!(
            "{when}, fresh: {select}: {}",
            error.detail().unwrap_or_default()
        )
    });
    assert_eq!(
        cached.names, fresh_names,
        "{when}: the cached path and a fresh build named different columns for {select}"
    );
    assert_eq!(
        cached.rows, fresh_rows,
        "{when}: the cached path answered differently from a fresh build for {select}"
    );
}

/// Returns two identically named-but-separate databases: one with the plan
/// cache on (the default, so `Cached::Update`/`Delete`/`Insert`'s
/// `CachedQuery` slot is built once and reused across every later call to the
/// same SQL text, exactly as production does) and one with it off.
///
/// **What "off" buys as a write-path reference.** `Levers::PLAN_CACHE`
/// governs `ImportedDatabase::compiled`'s *outer*, per-text cache; with it
/// off, every `execute_any` call gets a brand new `Cached::Update` (or
/// `Delete`, or `Insert`) with an untried `CachedQuery` slot, so the slot is
/// still tried - `keys_of` may still build and run a `physical::Compiled` for
/// that one call - but the built chain is dropped with the rest of the
/// `Cached` value at the end of the call rather than kept for the next one.
/// Building and using a chain once is exactly what running it uncached
/// answers, so the two connections applying the *same* sequence of
/// statements and disagreeing afterward is what a stale reused key set, a
/// stale reused join tower, or a stale reused row set would show up as.
///
/// @param name - the test's name, for the scratch paths
fn cached_and_fresh_pair(name: &str) -> (ImportedDatabase, ImportedDatabase) {
    let cached = ImportedDatabase::create(scratch(&format!("{name}-cached")), PAGE_SIZE, FRAMES)
        .expect("a fresh database is created");
    let mut fresh = ImportedDatabase::create(scratch(&format!("{name}-fresh")), PAGE_SIZE, FRAMES)
        .expect("a fresh database is created");
    fresh.disable_optimizations(Levers::PLAN_CACHE);
    (cached, fresh)
}

/// Runs one statement on both databases, panicking with its text and detail
/// on either's refusal.
///
/// @param cached - the plan-cache-on connection
/// @param fresh - the plan-cache-off connection
/// @param sql - the statement text
fn exec_both(cached: &mut ImportedDatabase, fresh: &mut ImportedDatabase, sql: &str) {
    exec(cached, sql);
    exec(fresh, sql);
}

/// Runs one parameterized write on both databases with the same parameters,
/// panicking with its text and detail on either's refusal.
///
/// @param cached - the plan-cache-on connection
/// @param fresh - the plan-cache-off connection
/// @param sql - the statement text
/// @param params - the values bound to `?1`, `?2`, ...
fn exec_both_with(
    cached: &mut ImportedDatabase,
    fresh: &mut ImportedDatabase,
    sql: &str,
    params: &Params,
) {
    cached
        .execute_any(sql, params)
        .unwrap_or_else(|error| panic!("cached db: {sql}: {}", error.detail().unwrap_or_default()));
    fresh
        .execute_any(sql, params)
        .unwrap_or_else(|error| panic!("fresh db: {sql}: {}", error.detail().unwrap_or_default()));
}

/// Asserts the two databases answer the same rows for one read-only query,
/// **and report the same counters** - `changes()`, `total_changes()` and
/// `last_insert_rowid()`.
///
/// The counters matter as much as the rows here, and for a reason specific
/// to `keys_of`/`run_cached_query`: they are read off the connection, not off
/// the answer to any one query, so a bug that ran a write's keys query an
/// extra time - or recorded it as though it were the write itself - would
/// leave the *rows* both connections read identical while the *counters*
/// silently diverged. Comparing only rows, as the earlier version of these
/// tests did, is exactly how a regression like that gets through: both arms
/// would have been equally wrong.
///
/// @param cached - the plan-cache-on connection
/// @param fresh - the plan-cache-off connection
/// @param select - the query, read from both connections
/// @param when - what just happened, for the failure message
fn assert_databases_agree(
    cached: &mut ImportedDatabase,
    fresh: &mut ImportedDatabase,
    select: &str,
    when: &str,
) {
    let (cached_rows, cached_names) =
        cached
            .run_with(select, &Params::new())
            .unwrap_or_else(|error| {
                panic!(
                    "{when}, cached db: {select}: {}",
                    error.detail().unwrap_or_default()
                )
            });
    let (fresh_rows, fresh_names) =
        fresh
            .run_with(select, &Params::new())
            .unwrap_or_else(|error| {
                panic!(
                    "{when}, fresh db: {select}: {}",
                    error.detail().unwrap_or_default()
                )
            });
    assert_eq!(
        cached_names, fresh_names,
        "{when}: the two connections named different columns for {select}"
    );
    assert_eq!(
        cached_rows, fresh_rows,
        "{when}: the connection that reuses its write-path chain disagrees with the one \
         that never does, for {select}"
    );
    assert_eq!(
        cached.changes(),
        fresh.changes(),
        "{when}: changes() disagrees between the two connections"
    );
    assert_eq!(
        cached.total_changes(),
        fresh.total_changes(),
        "{when}: total_changes() disagrees between the two connections - keys_of's own \
         query answered twice, or counted as a write itself, would show up exactly here \
         and nowhere in the rows"
    );
    assert_eq!(
        cached.last_insert_rowid(),
        fresh.last_insert_rowid(),
        "{when}: last_insert_rowid() disagrees between the two connections"
    );
}

/// The same prepared `SELECT` answers what a from-scratch build does, across
/// every write and DDL shape roadmap item 3 named: `INSERT`, `UPDATE`,
/// `DELETE`, `BEGIN`/`ROLLBACK`, `CREATE INDEX`, `ALTER TABLE`, `DETACH`, a
/// function registration, and `PRAGMA case_sensitive_like`.
///
/// **This is the test that would have caught the rejected `Rc<PagedTree>`
/// design.** A cache keyed on a tree handle rather than re-asking the catalog
/// every time would keep answering from the tree that was there when the
/// chain was built - a wrong answer with nothing to catch it, per the design
/// review this ticket implements. `physical::Compiled` never stores a tree or
/// a pool between executions, so every one of these interleavings re-reads
/// the connection fresh; this test is what proves that rather than arguing
/// it.
#[test]
fn a_cached_select_answers_what_a_fresh_build_does_across_every_interleaving() {
    let mut database = ImportedDatabase::create(scratch("interleaved"), PAGE_SIZE, FRAMES)
        .expect("a fresh database is created");
    exec(
        &mut database,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, label TEXT NOT NULL, note TEXT)",
    );
    for (id, label) in [
        (1, "alpha"),
        (2, "bravo"),
        (3, "charlie"),
        (4, "Delta"),
        (5, "echo"),
    ] {
        exec(
            &mut database,
            &format!("INSERT INTO t (id, label, note) VALUES ({id}, '{label}', 'n{id}')"),
        );
    }

    // The statement under test: single-stage (a rowid range, so it is a
    // candidate for `physical::try_compile`), with a residual `LIKE` so the
    // `PRAGMA case_sensitive_like` step has something to change the answer of.
    let select = "SELECT id, label FROM t WHERE id >= ?1 AND label LIKE ?2 ORDER BY id";
    let params = Params::from_values(vec![OwnedDatum::Int(1), OwnedDatum::Text(b"%a%".to_vec())]);

    // Warm the cache: the first call is what builds the `Compiled` chain and
    // stores it as `Slot::Reusable`; every call after this one answers from
    // it rather than rebuilding.
    assert_cache_agrees_with_fresh(&mut database, select, &params, "before anything");

    exec(
        &mut database,
        "INSERT INTO t (id, label, note) VALUES (6, 'foxtrot', 'n6')",
    );
    assert_cache_agrees_with_fresh(&mut database, select, &params, "after INSERT");

    exec(&mut database, "UPDATE t SET label = 'ALPHA' WHERE id = 1");
    assert_cache_agrees_with_fresh(&mut database, select, &params, "after UPDATE");

    exec(&mut database, "DELETE FROM t WHERE id = 3");
    assert_cache_agrees_with_fresh(&mut database, select, &params, "after DELETE");

    exec(&mut database, "BEGIN");
    exec(
        &mut database,
        "INSERT INTO t (id, label, note) VALUES (99, 'ninety-nine', NULL)",
    );
    exec(&mut database, "ROLLBACK");
    assert_cache_agrees_with_fresh(&mut database, select, &params, "after BEGIN; ...; ROLLBACK");

    exec(&mut database, "CREATE INDEX t_label ON t(label)");
    assert_cache_agrees_with_fresh(&mut database, select, &params, "after CREATE INDEX");

    exec(
        &mut database,
        "ALTER TABLE t ADD COLUMN weight INTEGER DEFAULT 0",
    );
    assert_cache_agrees_with_fresh(&mut database, select, &params, "after ALTER TABLE");

    let aux = scratch("interleaved-aux");
    exec(
        &mut database,
        &format!("ATTACH DATABASE '{}' AS aux", aux.display()),
    );
    exec(
        &mut database,
        "CREATE TABLE aux.side (id INTEGER PRIMARY KEY)",
    );
    exec(&mut database, "DETACH DATABASE aux");
    assert_cache_agrees_with_fresh(&mut database, select, &params, "after ATTACH; ...; DETACH");

    database
        .create_scalar_function(
            "shout",
            1,
            FunctionFlags::external(),
            std::sync::Arc::new(|arguments: &[Value<'static>]| {
                let text = arguments
                    .first()
                    .and_then(Value::as_text)
                    .map(|held| String::from_utf8_lossy(held.raw()).to_ascii_uppercase())
                    .unwrap_or_default();
                let bytes = inillucent_value::Bytes::owned(text.as_bytes())?;
                Ok(Value::Text(inillucent_value::TextValue::new(
                    bytes,
                    inillucent_value::TextEncoding::Utf8,
                )))
            }),
        )
        .expect("registers");
    assert_cache_agrees_with_fresh(
        &mut database,
        select,
        &params,
        "after registering a function",
    );

    exec(&mut database, "PRAGMA case_sensitive_like = 1");
    assert_cache_agrees_with_fresh(
        &mut database,
        select,
        &params,
        "after PRAGMA case_sensitive_like = 1",
    );

    exec(&mut database, "PRAGMA case_sensitive_like = 0");
    exec(
        &mut database,
        "INSERT INTO t (id, label, note) VALUES (7, 'GAMMA', 'n7')",
    );
    assert_cache_agrees_with_fresh(
        &mut database,
        select,
        &params,
        "after PRAGMA case_sensitive_like = 0, then another INSERT",
    );
}

/// A cached chain whose **source key** is an uncorrelated subquery answers
/// the current table, on every execution, not only the first.
///
/// **This is defect 1 of the design review, pinned.** `WHERE id = (SELECT
/// max(id) FROM t)` folds to a rowid seek whose key is the subquery's answer,
/// which `source_for_run` evaluates fresh on every call - but only if
/// something folds the subquery into *this* execution's `Params` first.
/// `Compiled::run` does that before it asks for the source; before this
/// ticket, nothing on the reused path did, and the second execution of a
/// statement like this refused with "a correlated subquery used as a value"
/// for a block that was never correlated at all.
#[test]
fn a_cached_source_key_subquery_reflects_the_current_table() {
    let mut database = ImportedDatabase::create(scratch("source-key-subquery"), PAGE_SIZE, FRAMES)
        .expect("a fresh database is created");
    exec(
        &mut database,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, label TEXT NOT NULL)",
    );
    exec(
        &mut database,
        "INSERT INTO t (id, label) VALUES (1, 'first')",
    );
    let select = "SELECT label FROM t WHERE id = (SELECT max(id) FROM t)";

    // First execution: builds and stores the `Compiled` chain.
    assert_cache_agrees_with_fresh(&mut database, select, &Params::new(), "first execution");
    let first = database
        .execute_any(select, &Params::new())
        .expect("first execution answers");
    assert_eq!(first.rows, vec![vec![OwnedDatum::Text(b"first".to_vec())]]);

    // A later row changes what `max(id)` is; the **reused** chain has to see
    // it, not answer with the row that was the maximum when it was built.
    exec(
        &mut database,
        "INSERT INTO t (id, label) VALUES (2, 'second')",
    );
    assert_cache_agrees_with_fresh(
        &mut database,
        select,
        &Params::new(),
        "second execution, after a row that moves the maximum",
    );
    let second = database
        .execute_any(select, &Params::new())
        .expect("second execution answers");
    assert_eq!(
        second.rows,
        vec![vec![OwnedDatum::Text(b"second".to_vec())]]
    );
}

/// A correlated subquery is never cached, and every execution answers for
/// *its own* outer row rather than the first execution's.
///
/// **This is defect 3 of the design review, guarded against by construction.**
/// `crate::correlate::Correlated` copies `params.without_subqueries()` at
/// build time; a chain that kept one across executions would answer every
/// later call with the parameters the first call bound. `try_compile`'s
/// verdict refuses to keep a chain that built one - `upper.correlations` is
/// non-empty - so every execution of a correlated query falls back to
/// `run_any_prepared`, which is what this test is over: two calls, two
/// different bound values, and each has to answer for the row it was
/// actually asked about.
#[test]
fn a_correlated_select_answers_for_its_own_outer_row_every_time() {
    let mut database = ImportedDatabase::create(scratch("correlated-select"), PAGE_SIZE, FRAMES)
        .expect("a fresh database is created");
    exec(
        &mut database,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, category INTEGER NOT NULL)",
    );
    for (id, category) in [(1, 10), (2, 10), (3, 20), (4, 20), (5, 20)] {
        exec(
            &mut database,
            &format!("INSERT INTO t (id, category) VALUES ({id}, {category})"),
        );
    }
    let select =
        "SELECT id, (SELECT count(*) FROM t AS other WHERE other.category = t.category) FROM t \
         WHERE id = ?1";
    for id in [1_i64, 3, 2, 5] {
        let params = Params::from_values(vec![OwnedDatum::Int(id)]);
        assert_cache_agrees_with_fresh(&mut database, select, &params, &format!("id = {id}"));
    }
    let outcome = database
        .execute_any(select, &Params::from_values(vec![OwnedDatum::Int(3)]))
        .expect("answers");
    // id 3's category (20) has three members; id 1's category (10) has two.
    // A chain that kept the first execution's row would answer 2 for every
    // later id instead of counting the category the row actually asked about.
    assert_eq!(
        outcome.rows,
        vec![vec![OwnedDatum::Int(3), OwnedDatum::Int(3)]]
    );
}

/// Stage 3: an `UPDATE` whose assigned value reads the table it is writing,
/// run the same way three times with different range bounds and a table that
/// is growing between runs, matches a connection that never reuses its
/// `Cached::Update` chain.
///
/// **This is the shape the coordinator named as the one that catches a stale
/// key set.** `UPDATE t SET value = ... WHERE id BETWEEN ?1 AND ?2` is a
/// range, not a rowid equality, so `keys_of` does not take the
/// `rowid_seek_key` shortcut and the row set genuinely comes from
/// `CachedQuery`'s slot on the second and third calls. I did not revert
/// `CachedQuery`/`keys_of` to check this fails without Stage 3 - the
/// coordinator's to run - but what it would show: before Stage 3, `Update`
/// carried a bare `(Box<PhysicalPlan>, Box<Prepared>)` with no slot, so every
/// call rebuilt fresh and the "cached" and "fresh" connections below would
/// already agree without Stage 3 doing anything - this test proves the reused
/// chain is correct, not that it exists; `execprofile`'s `write.update.indexed`
/// number is what would show that.
#[test]
fn an_update_whose_value_reads_the_table_it_writes_answers_correctly() {
    let (mut cached, mut fresh) = cached_and_fresh_pair("update-self-read");
    let schema = "CREATE TABLE t (id INTEGER PRIMARY KEY, value INTEGER NOT NULL)";
    exec_both(&mut cached, &mut fresh, schema);
    for (id, value) in [(1, 10), (2, 30), (3, 20), (4, 5), (5, 40)] {
        exec_both(
            &mut cached,
            &mut fresh,
            &format!("INSERT INTO t (id, value) VALUES ({id}, {value})"),
        );
    }
    let update = "UPDATE t SET value = (SELECT max(value) FROM t) + 1 WHERE id BETWEEN ?1 AND ?2";
    let check = "SELECT id, value FROM t ORDER BY id";

    // First call: ids 1-2, maximum is 40 (id 5) - untouched by the range.
    exec_both_with(
        &mut cached,
        &mut fresh,
        update,
        &Params::from_values(vec![OwnedDatum::Int(1), OwnedDatum::Int(2)]),
    );
    assert_databases_agree(
        &mut cached,
        &mut fresh,
        check,
        "after the first UPDATE, ids 1-2",
    );

    // A row lands between calls, moving what "the maximum" is for the next one.
    exec_both(
        &mut cached,
        &mut fresh,
        "INSERT INTO t (id, value) VALUES (6, 99)",
    );

    // Second call, same statement, different bounds - the range a stale
    // `CachedQuery` would answer from the first call's key set instead of
    // rebinding.
    exec_both_with(
        &mut cached,
        &mut fresh,
        update,
        &Params::from_values(vec![OwnedDatum::Int(3), OwnedDatum::Int(4)]),
    );
    assert_databases_agree(
        &mut cached,
        &mut fresh,
        check,
        "after the second UPDATE, ids 3-4",
    );

    exec_both(
        &mut cached,
        &mut fresh,
        "INSERT INTO t (id, value) VALUES (7, 1)",
    );

    // Third call, a range that includes rows the first two calls already
    // rewrote, over a table that has grown twice since the chain was built.
    exec_both_with(
        &mut cached,
        &mut fresh,
        update,
        &Params::from_values(vec![OwnedDatum::Int(1), OwnedDatum::Int(6)]),
    );
    assert_databases_agree(
        &mut cached,
        &mut fresh,
        check,
        "after the third UPDATE, ids 1-6",
    );
}

/// Stage 3: an `UPDATE` with a correlated subquery in its assignment (not its
/// key), run three times over a growing table with different range bounds,
/// matches a connection that never reuses its `Cached::Update` chain.
///
/// The correlated read is answered per row regardless of caching -
/// `crate::correlate::Correlated` is a `SELECT`-side concern and this
/// statement's `WHERE` is the plain range `keys_of` caches - so what this
/// adds over the previous test is a second, independent assignment shape
/// exercising the same `CachedQuery` slot.
#[test]
fn a_correlated_subquery_over_the_written_table_answers_per_row() {
    let (mut cached, mut fresh) = cached_and_fresh_pair("update-correlated");
    let schema = "CREATE TABLE t (id INTEGER PRIMARY KEY, rank_below INTEGER)";
    exec_both(&mut cached, &mut fresh, schema);
    for id in [1, 2, 3, 4] {
        exec_both(
            &mut cached,
            &mut fresh,
            &format!("INSERT INTO t (id, rank_below) VALUES ({id}, 0)"),
        );
    }
    let update = "UPDATE t SET rank_below = \
                  (SELECT count(*) FROM t AS other WHERE other.id < t.id) \
                  WHERE id BETWEEN ?1 AND ?2";
    let check = "SELECT id, rank_below FROM t ORDER BY id";

    exec_both_with(
        &mut cached,
        &mut fresh,
        update,
        &Params::from_values(vec![OwnedDatum::Int(1), OwnedDatum::Int(2)]),
    );
    assert_databases_agree(
        &mut cached,
        &mut fresh,
        check,
        "after the first UPDATE, ids 1-2",
    );

    exec_both(
        &mut cached,
        &mut fresh,
        "INSERT INTO t (id, rank_below) VALUES (5, 0)",
    );
    exec_both_with(
        &mut cached,
        &mut fresh,
        update,
        &Params::from_values(vec![OwnedDatum::Int(3), OwnedDatum::Int(5)]),
    );
    assert_databases_agree(
        &mut cached,
        &mut fresh,
        check,
        "after the second UPDATE, ids 3-5",
    );

    exec_both(
        &mut cached,
        &mut fresh,
        "INSERT INTO t (id, rank_below) VALUES (6, 0)",
    );
    exec_both_with(
        &mut cached,
        &mut fresh,
        update,
        &Params::from_values(vec![OwnedDatum::Int(1), OwnedDatum::Int(6)]),
    );
    assert_databases_agree(
        &mut cached,
        &mut fresh,
        check,
        "after the third UPDATE, ids 1-6",
    );
}

/// Stage 2 and Stage 3 together: a self-join `UPDATE ... FROM`'s keys query is
/// itself a join, run three times with different range bounds over a growing
/// table, matching a connection that never reuses its `Cached::Update` chain.
///
/// **This is the one write shape that touches both stages.** `keys_of`'s own
/// plan for `UPDATE t SET ... FROM t AS other WHERE ... AND t.id BETWEEN ?1
/// AND ?2` is a two-stage plan - `t` outer, `other` joined by rowid - so
/// `physical::try_compile` has to build a `JoinRecipe` for it, not just an
/// `Upper` chain, before `CachedQuery`'s slot has anything to reuse.
#[test]
fn a_self_join_update_writes_the_other_rows_value() {
    let (mut cached, mut fresh) = cached_and_fresh_pair("update-self-join");
    let schema = "CREATE TABLE t (id INTEGER PRIMARY KEY, value INTEGER NOT NULL)";
    exec_both(&mut cached, &mut fresh, schema);
    for (id, value) in [(1, 100), (2, 200), (3, 300)] {
        exec_both(
            &mut cached,
            &mut fresh,
            &format!("INSERT INTO t (id, value) VALUES ({id}, {value})"),
        );
    }
    // Every row in range takes on the value of the row before it in id order.
    let update = "UPDATE t SET value = other.value FROM t AS other \
                  WHERE other.id = t.id - 1 AND t.id BETWEEN ?1 AND ?2";
    let check = "SELECT id, value FROM t ORDER BY id";

    exec_both_with(
        &mut cached,
        &mut fresh,
        update,
        &Params::from_values(vec![OwnedDatum::Int(2), OwnedDatum::Int(2)]),
    );
    assert_databases_agree(
        &mut cached,
        &mut fresh,
        check,
        "after the first UPDATE, id 2",
    );

    exec_both(
        &mut cached,
        &mut fresh,
        "INSERT INTO t (id, value) VALUES (4, 400)",
    );
    exec_both_with(
        &mut cached,
        &mut fresh,
        update,
        &Params::from_values(vec![OwnedDatum::Int(3), OwnedDatum::Int(4)]),
    );
    assert_databases_agree(
        &mut cached,
        &mut fresh,
        check,
        "after the second UPDATE, ids 3-4",
    );

    exec_both(
        &mut cached,
        &mut fresh,
        "INSERT INTO t (id, value) VALUES (5, 500)",
    );
    exec_both_with(
        &mut cached,
        &mut fresh,
        update,
        &Params::from_values(vec![OwnedDatum::Int(1), OwnedDatum::Int(5)]),
    );
    assert_databases_agree(
        &mut cached,
        &mut fresh,
        check,
        "after the third UPDATE, ids 1-5",
    );
}

/// Stage 3: a trigger that reads its own table, via a plan-shaped `INSERT ...
/// SELECT` rather than a scalar subquery expression, fired three times with a
/// different row each time and an unrelated write between firings, matches a
/// connection that never reuses its `Cached::Insert` chain.
///
/// **The trigger's own body is `Cached::Insert(_, Some(query), _)` - the
/// insert-source shape Stage 3 added a slot to.** `CREATE TRIGGER` binds its
/// body once; every later `AFTER INSERT` firing runs the same bound
/// `INSERT INTO audit ... SELECT ... FROM t` again, so the second and third
/// firings are exactly the "same write statement, run again" case Stage 3 is
/// for, except the "parameters" that change are `new.id`/`new.value` rather
/// than a bound `?N` - and the answer moves anyway, because the `SELECT
/// sum(value) FROM t` each firing reads is `t` as it stands *this* time, which
/// only holds if the plan's own row-finding cannot go stale as `t` grows.
///
/// **Not a subquery expression, and that restriction is deliberate.**
/// `trigger::run_body` never calls `subquery::fold` for a trigger's own body
/// statements - nothing in this ticket touches `trigger.rs` - so `UPDATE t SET
/// total = (SELECT sum(value) FROM t) WHERE id = new.id` inside a trigger
/// body refuses by name with "a correlated subquery used as a value" even
/// though the subquery is not correlated at all, and `UPDATE ... FROM`
/// answers "bad parameter or other API misuse" inside a trigger body too. That
/// first gap is
/// [`a_trigger_body_subquery_is_refused_as_though_it_were_correlated`] below,
/// left as a known defect for whoever picks up trigger bodies rather than
/// worked around here. `INSERT ... SELECT sum(value) FROM t` has no subquery
/// expression at all - `sum(value)` is an ordinary aggregate over the SELECT's
/// own FROM term - so it is unaffected by either gap and is what this test
/// uses to read the table from inside the trigger.
#[test]
fn a_trigger_reading_its_own_table_answers_correctly() {
    let (mut cached, mut fresh) = cached_and_fresh_pair("trigger-self-read");
    exec_both(
        &mut cached,
        &mut fresh,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, value INTEGER NOT NULL)",
    );
    exec_both(
        &mut cached,
        &mut fresh,
        "CREATE TABLE audit (id INTEGER PRIMARY KEY, running_total INTEGER)",
    );
    exec_both(
        &mut cached,
        &mut fresh,
        "CREATE TRIGGER t_running_total AFTER INSERT ON t BEGIN \
           INSERT INTO audit (id, running_total) SELECT new.id, sum(value) FROM t; \
         END",
    );
    let check = "SELECT id, running_total FROM audit ORDER BY id";

    exec_both(
        &mut cached,
        &mut fresh,
        "INSERT INTO t (id, value) VALUES (1, 10)",
    );
    assert_databases_agree(&mut cached, &mut fresh, check, "after the first firing");

    // An unrelated write to `t` between firings, so the second firing's own
    // `sum(value) FROM t` has to see it.
    exec_both(
        &mut cached,
        &mut fresh,
        "INSERT INTO t (id, value) VALUES (2, 25)",
    );
    assert_databases_agree(&mut cached, &mut fresh, check, "after the second firing");

    exec_both(
        &mut cached,
        &mut fresh,
        "INSERT INTO t (id, value) VALUES (3, 5)",
    );
    assert_databases_agree(&mut cached, &mut fresh, check, "after the third firing");

    // Each trigger firing sees the table *after* its own row has gone in
    // (AFTER INSERT), so the running total already includes the row that
    // fired it.
    let (rows, _) = cached.run_with(check, &Params::new()).expect("reads back");
    assert_eq!(
        rows,
        vec![
            vec![OwnedDatum::Int(1), OwnedDatum::Int(10)],
            vec![OwnedDatum::Int(2), OwnedDatum::Int(35)],
            vec![OwnedDatum::Int(3), OwnedDatum::Int(40)],
        ]
    );
}

/// Pins the known defect the doc comment above names: a trigger body's own
/// uncorrelated subquery is refused as though it read an outer row, because
/// `trigger::run_body` never folds it. Reverting a fix to `trigger.rs` would
/// turn this red, which is exactly what should happen - see the testing
/// standard's rule 1.3.
#[test]
fn a_trigger_body_subquery_is_refused_as_though_it_were_correlated() {
    let mut database = ImportedDatabase::create(scratch("trigger-subquery-gap"), PAGE_SIZE, FRAMES)
        .expect("a fresh database is created");
    exec(
        &mut database,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, value INTEGER NOT NULL, total INTEGER NOT NULL DEFAULT 0)",
    );
    exec(
        &mut database,
        "CREATE TRIGGER t_running_total AFTER INSERT ON t BEGIN \
           UPDATE t SET total = (SELECT sum(value) FROM t) WHERE id = new.id; \
         END",
    );
    let refused = database
        .execute_any("INSERT INTO t (id, value) VALUES (1, 10)", &Params::new())
        .expect_err("the uncorrelated subquery in the trigger body is refused, not answered");
    let detail = refused.detail().unwrap_or_default();
    assert!(
        detail.contains("a correlated subquery used as a value"),
        "expected the unfolded-subquery refusal, got: {detail}"
    );
}

/// Stage 2: an `IndexNestedLoopJoin`'s cached chain answers what a fresh
/// build does, across writes to **both** sides of the join, `CREATE INDEX`,
/// `BEGIN`/`ROLLBACK`, and an unrelated `ATTACH`/`DETACH`.
///
/// **The inner side is the one a stale cache would get wrong first.** A
/// `JoinRecipe` holds a `root` page id and asks `catalog.tree(root)` and
/// `catalog.pool_for(root)` fresh on every `Compiled::run` - never a stored
/// `&PagedTree` - so a write to the *joined* table between two executions has
/// to reach the second one exactly the way a write to the outer table
/// already had to under Stage 1. This test's middle section is deliberately
/// about `inner_t`, not `outer_t`, because a Stage 1 regression in the outer
/// half would already have been caught and this is the half that is new.
///
/// I did not revert `JoinRecipe`/`Compiled::run` to check this fails without
/// Stage 2 - that is the coordinator's to run. What it would show: before
/// Stage 2, `try_compile` returns `Ok(None)` for any plan with more than one
/// stage, so this two-stage join always falls back to `run_any_prepared` -
/// every assertion here would still pass, just without ever exercising
/// `physical::Compiled`'s join tower at all. The number that actually shows
/// Stage 2 doing something is the paired measurement in `execprofile`, not a
/// failure of this test.
#[test]
fn a_cached_index_nested_loop_join_answers_what_a_fresh_build_does_across_every_interleaving() {
    let mut database = ImportedDatabase::create(scratch("join-interleaved"), PAGE_SIZE, FRAMES)
        .expect("a fresh database is created");
    exec(
        &mut database,
        "CREATE TABLE outer_t (id INTEGER PRIMARY KEY, label TEXT NOT NULL)",
    );
    exec(
        &mut database,
        "CREATE TABLE inner_t (id INTEGER PRIMARY KEY, owner INTEGER NOT NULL, note TEXT NOT NULL)",
    );
    exec(&mut database, "CREATE INDEX inner_owner ON inner_t(owner)");
    for (id, label) in [(1, "alpha"), (2, "bravo"), (3, "charlie")] {
        exec(
            &mut database,
            &format!("INSERT INTO outer_t (id, label) VALUES ({id}, '{label}')"),
        );
    }
    for (id, owner, note) in [(1, 1, "x"), (2, 1, "y"), (3, 2, "z")] {
        exec(
            &mut database,
            &format!("INSERT INTO inner_t (id, owner, note) VALUES ({id}, {owner}, '{note}')"),
        );
    }

    let select = "SELECT outer_t.label, inner_t.note FROM outer_t JOIN inner_t \
                  ON inner_t.owner = outer_t.id WHERE outer_t.id = ?1 ORDER BY inner_t.id";
    let params = Params::from_values(vec![OwnedDatum::Int(1)]);

    assert_cache_agrees_with_fresh(&mut database, select, &params, "before anything");

    exec(
        &mut database,
        "UPDATE outer_t SET label = 'ALPHA' WHERE id = 1",
    );
    assert_cache_agrees_with_fresh(
        &mut database,
        select,
        &params,
        "after UPDATE on the outer table",
    );

    // The inner (joined) table, which is the side a stale tree reference
    // would answer wrong.
    exec(
        &mut database,
        "INSERT INTO inner_t (id, owner, note) VALUES (4, 1, 'w')",
    );
    assert_cache_agrees_with_fresh(
        &mut database,
        select,
        &params,
        "after INSERT on the inner table",
    );

    exec(&mut database, "DELETE FROM inner_t WHERE id = 1");
    assert_cache_agrees_with_fresh(
        &mut database,
        select,
        &params,
        "after DELETE on the inner table",
    );

    exec(&mut database, "BEGIN");
    exec(
        &mut database,
        "UPDATE inner_t SET note = 'should-not-stick' WHERE owner = 1",
    );
    exec(&mut database, "ROLLBACK");
    assert_cache_agrees_with_fresh(
        &mut database,
        select,
        &params,
        "after BEGIN; UPDATE inner_t; ROLLBACK",
    );

    exec(&mut database, "CREATE INDEX inner_note ON inner_t(note)");
    assert_cache_agrees_with_fresh(
        &mut database,
        select,
        &params,
        "after CREATE INDEX on the inner table",
    );

    exec(
        &mut database,
        "ALTER TABLE inner_t ADD COLUMN extra INTEGER DEFAULT 0",
    );
    assert_cache_agrees_with_fresh(
        &mut database,
        select,
        &params,
        "after ALTER TABLE on the inner table",
    );

    let aux = scratch("join-interleaved-aux");
    exec(
        &mut database,
        &format!("ATTACH DATABASE '{}' AS aux", aux.display()),
    );
    exec(
        &mut database,
        "CREATE TABLE aux.side (id INTEGER PRIMARY KEY)",
    );
    exec(&mut database, "DETACH DATABASE aux");
    assert_cache_agrees_with_fresh(
        &mut database,
        select,
        &params,
        "after an unrelated ATTACH; ...; DETACH",
    );

    // A different outer id, over the same cached chain - proves the key is
    // read from the row rebound each call, not fixed at the first execution.
    let params_2 = Params::from_values(vec![OwnedDatum::Int(2)]);
    assert_cache_agrees_with_fresh(
        &mut database,
        select,
        &params_2,
        "a different outer id, same cached chain",
    );
}

/// A join shape `try_compile` refuses - here, a `LEFT JOIN` whose `ON` is not
/// fully enforced by the seek, which `build_nested` routes to
/// `build_materialised_join` - still answers correctly across writes, through
/// the `run_any_prepared` fallback every refused shape takes.
///
/// **Why this `ON` is not enforced.** `inner_t.note LIKE '%' || outer_t.label`
/// cannot be folded into the index seek on `owner`, so a residual is left
/// over and `on_enforced` is false - which is exactly the condition
/// `try_join_recipe` and `build_nested` both test before choosing an
/// `IndexNestedLoopJoin`. I did not verify by instrumentation that this
/// specific query lands in `Ok(None)` rather than `Some(recipe)`; what this
/// test proves either way is that the fallback answers correctly, which is
/// what matters if the shape check is ever wrong in either direction.
#[test]
fn a_left_join_with_a_residual_condition_still_answers_correctly() {
    let mut database = ImportedDatabase::create(scratch("join-materialised"), PAGE_SIZE, FRAMES)
        .expect("a fresh database is created");
    exec(
        &mut database,
        "CREATE TABLE outer_t (id INTEGER PRIMARY KEY, label TEXT NOT NULL)",
    );
    exec(
        &mut database,
        "CREATE TABLE inner_t (id INTEGER PRIMARY KEY, owner INTEGER NOT NULL, note TEXT NOT NULL)",
    );
    exec(&mut database, "CREATE INDEX inner_owner ON inner_t(owner)");
    for (id, label) in [(1, "a"), (2, "b")] {
        exec(
            &mut database,
            &format!("INSERT INTO outer_t (id, label) VALUES ({id}, '{label}')"),
        );
    }
    for (id, owner, note) in [(1, 1, "za"), (2, 1, "zz"), (3, 2, "q")] {
        exec(
            &mut database,
            &format!("INSERT INTO inner_t (id, owner, note) VALUES ({id}, {owner}, '{note}')"),
        );
    }
    let select = "SELECT outer_t.id, inner_t.note FROM outer_t LEFT JOIN inner_t \
                  ON inner_t.owner = outer_t.id AND inner_t.note LIKE '%' || outer_t.label \
                  ORDER BY outer_t.id, inner_t.id";
    let params = Params::new();
    assert_cache_agrees_with_fresh(&mut database, select, &params, "before anything");
    exec(
        &mut database,
        "INSERT INTO inner_t (id, owner, note) VALUES (4, 2, 'qb')",
    );
    assert_cache_agrees_with_fresh(
        &mut database,
        select,
        &params,
        "after INSERT on the inner table",
    );
    exec(&mut database, "DELETE FROM inner_t WHERE id = 2");
    assert_cache_agrees_with_fresh(
        &mut database,
        select,
        &params,
        "after DELETE on the inner table",
    );
}

/// H1 (task-1920): a window function reaches `run_windowed` through the
/// cached path, not a refusal.
///
/// **Why this is in this file and not in the SQL suites.** The defect was not
/// that window functions were unimplemented - `run_windowed` is about a
/// thousand lines and it works. It was that `compiled::try_compile` checked
/// `plan.compounds` and not `plan.select.windows`, so a windowed statement
/// reached `build_upper`'s `refuse_unhandled` and `run_cached_query`
/// propagated that refusal with `?`, while `run_with` - the fresh path this
/// file compares against, which nothing but a test calls - dispatched the
/// same statement to `run_windowed` and answered it. That is exactly the
/// cached-versus-fresh divergence this file exists to catch, and
/// `assert_cache_agrees_with_fresh` fails on the parent commit with
/// "unsupported: a window function reaching the pipeline builder" from the
/// cached call while the fresh call answers.
#[test]
fn a_windowed_select_answers_through_the_cached_path_too() {
    let mut database = ImportedDatabase::create(scratch("windowed-cached"), PAGE_SIZE, FRAMES)
        .expect("a fresh database is created");
    exec(
        &mut database,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, grp TEXT NOT NULL, amount INTEGER NOT NULL)",
    );
    for (id, grp, amount) in [
        (1, "a", 10),
        (2, "a", 20),
        (3, "b", 5),
        (4, "b", 50),
        (5, "b", 7),
    ] {
        exec(
            &mut database,
            &format!("INSERT INTO t (id, grp, amount) VALUES ({id}, '{grp}', {amount})"),
        );
    }
    let params = Params::new();
    for select in [
        "SELECT id, row_number() OVER (ORDER BY id) FROM t",
        "SELECT grp, amount, sum(amount) OVER (PARTITION BY grp ORDER BY id) FROM t ORDER BY id",
        "SELECT id, rank() OVER (ORDER BY amount DESC) FROM t ORDER BY id",
        "SELECT id, lag(amount) OVER (ORDER BY id), lead(amount) OVER (ORDER BY id) FROM t",
    ] {
        assert_cache_agrees_with_fresh(&mut database, select, &params, "windowed, first execution");
        // The second reading is the one a reused chain would get wrong: the
        // `Slot` is decided on the first call and every later call takes the
        // decision it recorded.
        assert_cache_agrees_with_fresh(
            &mut database,
            select,
            &params,
            "windowed, reusing the slot",
        );
    }

    exec(
        &mut database,
        "INSERT INTO t (id, grp, amount) VALUES (6, 'a', 3)",
    );
    for select in [
        "SELECT id, row_number() OVER (ORDER BY id) FROM t",
        "SELECT grp, amount, sum(amount) OVER (PARTITION BY grp ORDER BY id) FROM t ORDER BY id",
    ] {
        assert_cache_agrees_with_fresh(&mut database, select, &params, "windowed, after INSERT");
    }
}
