//! The Phase 2 assurance campaigns: eviction, corrupt pages, and the
//! metamorphic plan sweep.
//!
//! Invariant: every acceptance item the TDD states for Phase 2 that is a
//! *test* rather than a measurement is checked here, and each one is written so
//! that it would fail if the property stopped holding rather than so that it
//! passes today. The three are:
//!
//! - **"eviction campaign with a 64-frame pool green"**: the same trees, the
//!   same queries, and a pool far too small to hold them.
//! - **"corrupt-page fuzz targets never panic"**: the stable counterparts of
//!   the `fuzz/` targets, so a checkout with no nightly toolchain still gets
//!   the coverage - from a seeded generator rather than from coverage feedback.
//! - **the metamorphic plan tests**: every query under every applicable
//!   `PRAGMA inillucent.force_plan` alternative must produce the same digest.
//!
//! ## Why the campaigns build their own fixture
//!
//! They do not need the scorecard's 100,000-row database and they must run in a
//! `cargo test` on a machine that has never built one. Each builds a small
//! database in a memory VFS, which also means the eviction campaign can use a
//! page size that makes a 64-frame pool genuinely too small without needing a
//! large file.

use std::cell::RefCell;
use std::rc::Rc;

use inillucent_exec::physical::{ForcePlan, Params, SourceLayout, TreeCatalog};
use inillucent_exec::StaticType;
use inillucent_pool::interior::{self, InteriorRef};
use inillucent_pool::meta::Meta;
use inillucent_pool::{Database, Options, Pool};
use inillucent_tree::datum::{Datum, OwnedDatum};
use inillucent_tree::leaf::LeafRef;
use inillucent_tree::types::{ColumnSpec, PhysicalType};
use inillucent_tree::{key, PagedTree};
use inillucent_vfs::{DbPath, MemoryVfs};

/// A tiny catalog over one table tree and one index tree.
///
/// The executor is built against a `TreeCatalog` the caller supplies, so a test
/// can hand it two trees and nothing else - no schema, no binder, no file
/// format beyond the two trees themselves.
struct Fixture {
    database: Database,
    table: PagedTree,
    index: PagedTree,
    table_layout: std::rc::Rc<SourceLayout>,
    index_layout: std::rc::Rc<SourceLayout>,
    /// The schema the binder reads, built from the same DDL text the trees
    /// were built to match.
    catalog: inillucent_sql::catalog_view::StaticCatalog,
}

impl Fixture {
    /// Parses, binds and plans one statement against the fixture's schema.
    ///
    /// The real front end: the lexer, binder and planner the rearchitecture
    /// keeps. A plan built by hand is a plan nobody writes, and the physical
    /// pass's job is to run what the planner produces.
    ///
    /// @param sql - the statement text
    fn plan(&self, sql: &str) -> inillucent_base::DbResult<inillucent_sql::plan::PhysicalPlan> {
        let limits = inillucent_base::limits::Limits::default();
        let parsed = inillucent_sql::parser::parse_next_statement(sql.as_bytes(), 0, &limits)
            .map_err(|error| inillucent_base::error::misuse(format!("{sql}: {error:?}")))?;
        let authorizer = inillucent_sql::bind::AllowAll;
        let mut binder = inillucent_sql::bind::Binder::new(&self.catalog, &parsed.ast, &authorizer)
            .with_source(sql.as_bytes());
        let bound = binder
            .bind_statement(&parsed.statement)
            .map_err(|error| inillucent_base::error::misuse(format!("{sql}: {error:?}")))?;
        match bound {
            inillucent_sql::bind::BoundStatement::Select(select) => {
                Ok(inillucent_sql::plan::plan_select_with(
                    *select,
                    inillucent_sql::plan::Levers::default(),
                ))
            }
            _ => Err(inillucent_base::error::misuse(format!(
                "{sql} is not a read-only statement"
            ))),
        }
    }
}

impl Fixture {
    /// Returns the pool the fixture's two trees live in.
    fn pool(&self) -> &Pool {
        self.database.pool()
    }
}

impl TreeCatalog for Fixture {
    fn pool_for(&self, root: u32) -> Option<&Pool> {
        let _ = root;
        Some(self.database.pool())
    }

    fn tree(&self, root: u32) -> Option<&PagedTree> {
        match root {
            1 => Some(&self.table),
            2 => Some(&self.index),
            _ => None,
        }
    }

    fn layout(&self, root: u32) -> Option<&std::rc::Rc<SourceLayout>> {
        match root {
            1 => Some(&self.table_layout),
            2 => Some(&self.index_layout),
            _ => None,
        }
    }

    fn covering_candidates(&self, table_root: u32) -> Vec<u32> {
        if table_root == 1 {
            vec![2]
        } else {
            Vec::new()
        }
    }
}

/// Builds a `(id, category, label)` table and a `(category, id)` index.
///
/// @param rows - how many rows the table holds
/// @param page_size - the page size to build at
/// @param frames - how many frames the pool holds
fn fixture(rows: i64, page_size: usize, frames: usize) -> Fixture {
    let vfs = MemoryVfs::new();
    let path = DbPath::new("campaign.rdb");
    let mut database = Database::create(
        &vfs,
        &path,
        Options::default()
            .with_page_size(page_size)
            .with_frames(frames),
    )
    .expect("a fresh database");

    let labels: Vec<String> = (0..rows).map(|n| format!("label-{n:06}")).collect();
    let table_rows: Vec<Vec<OwnedDatum>> = (0..rows)
        .map(|n| {
            vec![
                OwnedDatum::Int(n),
                OwnedDatum::Int(n % 64),
                OwnedDatum::Text(labels[n as usize].clone().into_bytes()),
            ]
        })
        .collect();
    let borrowed: Vec<Vec<Datum<'_>>> = table_rows
        .iter()
        .map(|row| row.iter().map(OwnedDatum::borrow).collect())
        .collect();
    let table = PagedTree::bulk_build(
        &mut database,
        1,
        vec![
            ColumnSpec::key(PhysicalType::Int64),
            ColumnSpec::new(PhysicalType::Int64),
            ColumnSpec::new(PhysicalType::Text),
        ],
        1,
        &borrowed,
    )
    .expect("a table tree");

    let mut entries: Vec<Vec<OwnedDatum>> = (0..rows)
        .map(|n| vec![OwnedDatum::Int(n % 64), OwnedDatum::Int(n)])
        .collect();
    entries.sort_by_key(|row| match (row.first(), row.get(1)) {
        (Some(OwnedDatum::Int(a)), Some(OwnedDatum::Int(b))) => (*a, *b),
        _ => (0, 0),
    });
    let borrowed: Vec<Vec<Datum<'_>>> = entries
        .iter()
        .map(|row| row.iter().map(OwnedDatum::borrow).collect())
        .collect();
    let index = PagedTree::bulk_build(
        &mut database,
        2,
        vec![
            ColumnSpec::key(PhysicalType::Int64),
            ColumnSpec::key(PhysicalType::Int64),
        ],
        2,
        &borrowed,
    )
    .expect("an index tree");

    // The schema the binder reads. It is parsed from DDL rather than built
    // field by field, so the affinities, the rowid alias and the index's key
    // columns are whatever the dialect says they are rather than whatever a
    // test author remembered.
    let mut info = inillucent_catalog::load::table_from_create_sql(
        b"CREATE TABLE t(id INTEGER PRIMARY KEY, category INTEGER NOT NULL, label TEXT NOT NULL)",
        0,
        1,
    )
    .expect("the fixture's DDL parses");
    let index_info = inillucent_catalog::load::index_from_create_sql(
        b"CREATE INDEX t_category ON t(category, id)",
        &info,
        2,
    )
    .expect("the fixture's index DDL parses");
    info.indexes.push(index_info);
    let catalog = inillucent_sql::catalog_view::StaticCatalog::empty().with_table(info);

    Fixture {
        catalog,
        database,
        table,
        index,
        table_layout: std::rc::Rc::new(SourceLayout {
            tree_key: 1,
            slots: vec![Some(0), Some(1), Some(2)],
            rowid: Some(0),
            // A rowid table's row is identified by its rowid, tree column 0.
            identity: vec![0],
            types: vec![StaticType::Int, StaticType::Int, StaticType::Text],
            width: 3,
            key_columns: vec![0],
        }),
        index_layout: std::rc::Rc::new(SourceLayout {
            tree_key: 2,
            slots: vec![Some(1), Some(0), None],
            rowid: Some(1),
            // An index entry over a rowid table ends with that rowid, which is
            // what a non-covering seek probes the table with.
            identity: vec![1],
            types: vec![StaticType::Int, StaticType::Int],
            width: 2,
            key_columns: vec![0, 1],
        }),
    }
}

/// Returns every row of a tree, as a comparable digest.
///
/// @param tree - the tree to read
/// @param pool - the buffer pool
fn digest_of(tree: &PagedTree, pool: &Pool) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    tree.visit_leaves(pool, &mut |leaf| {
        for row in 0..leaf.row_count() {
            for column in 0..leaf.column_count() {
                let mut encoded = Vec::new();
                key::encode_into(&leaf.value(row, column)?, &mut encoded);
                for byte in encoded {
                    hash ^= u64::from(byte);
                    hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
                }
            }
        }
        Ok(true)
    })
    .expect("a readable tree");
    hash
}

/// A reused statement answers what a freshly built pipeline answers, and
/// refuses to be reused when it cannot.
///
/// **This is the test the `Sink::reset` contract exists for.** A statement
/// builds its operator chain once and runs it many times, so every operator
/// that accumulates - an aggregate, a sorter, a top-n, a distinct set, a limit
/// counter - has to be returned to its pre-input state between executions. An
/// operator that forgot would fold the previous execution's rows into this
/// one's answer, and no test of a single execution can see that. So each
/// statement is run five times over five parameter sets, deliberately not in
/// ascending order, and every run is compared against a pipeline built from
/// scratch for the same parameters.
///
/// The other half is the refusal. A parameter that reaches anything but the
/// source is folded into the chain when the chain is built, and re-running that
/// chain against new values would answer the old question. The builder decides
/// that by counting the parameter reads the chain's construction makes, and
/// this asserts both verdicts occur in the list below - a test in which every
/// statement happened to be re-runnable would be checking the counter's
/// optimism rather than the counter.
#[test]
fn a_reused_statement_answers_what_a_rebuilt_pipeline_does() {
    let fixture = fixture(4_000, 4_096, 512);
    let statements = [
        "SELECT count(*), sum(category) FROM t WHERE id >= ?1",
        "SELECT category, count(*) FROM t WHERE id >= ?1 GROUP BY category ORDER BY category",
        "SELECT id FROM t WHERE id >= ?1 ORDER BY label LIMIT 7",
        "SELECT DISTINCT category FROM t WHERE id >= ?1 ORDER BY category",
        "SELECT id, label FROM t WHERE id >= ?1 AND id <= ?1 + 20",
        "SELECT label FROM t WHERE id = ?1",
        // The parameter is the LIMIT, so it is in the chain rather than the
        // source and the statement must refuse to be re-bound.
        "SELECT id FROM t WHERE id >= 1 ORDER BY id LIMIT ?1",
    ];
    let mut reusable = 0usize;
    let mut refused = 0usize;
    for sql in statements {
        let plan = fixture.plan(sql).expect(sql);
        let prepared =
            inillucent_exec::physical::prepare(&plan, &fixture, ForcePlan::default()).expect(sql);
        let reused_rows: Rc<RefCell<Vec<Vec<OwnedDatum>>>> = Rc::new(RefCell::new(Vec::new()));
        let first = Params::from_values(vec![OwnedDatum::Int(1)]);
        let mut statement = inillucent_exec::physical::build_statement(
            &plan,
            &fixture,
            &prepared,
            &first,
            Box::new(inillucent_exec::ops::CollectInto::new(Rc::clone(
                &reused_rows,
            ))),
        )
        .expect(sql);
        if !statement.rebindable() {
            refused = refused.saturating_add(1);
            let params = Params::from_values(vec![OwnedDatum::Int(2)]);
            assert!(
                statement.run(&params).is_err(),
                "{sql} kept a parameter in its chain and still agreed to re-run"
            );
            continue;
        }
        reusable = reusable.saturating_add(1);
        for bound in [1_i64, 3_000, 17, 3_999, 17] {
            let params = Params::from_values(vec![OwnedDatum::Int(bound)]);
            reused_rows.borrow_mut().clear();
            statement.run(&params).expect(sql);
            let reused = reused_rows.borrow().clone();

            let fresh_rows: Rc<RefCell<Vec<Vec<OwnedDatum>>>> = Rc::new(RefCell::new(Vec::new()));
            let (mut pipeline, _) = inillucent_exec::physical::build_prepared(
                &plan,
                &fixture,
                &prepared,
                &params,
                Box::new(inillucent_exec::ops::CollectInto::new(Rc::clone(
                    &fresh_rows,
                ))),
            )
            .expect(sql);
            pipeline.run().expect(sql);
            let fresh = fresh_rows.borrow().clone();

            assert_eq!(reused, fresh, "{sql} at ?1 = {bound}");
        }
    }
    assert!(reusable >= 4, "only {reusable} statements were re-runnable");
    assert!(refused >= 1, "no statement exercised the refusal");
}

/// The TDD's eviction campaign: the same answers through a 64-frame pool.
///
/// The pool holds 64 frames of 512 bytes - 32 KiB - over a database of several
/// hundred pages, so every query evicts many times over. What is checked is not
/// that eviction happens but that the *answers do not change*: the digest of
/// every tree read through the small pool equals the digest read through one
/// large enough to hold everything.
///
/// It also asserts eviction really happened, because a campaign that silently
/// stopped evicting would keep passing while checking nothing.
#[test]
fn a_sixty_four_frame_pool_answers_what_a_large_one_does() {
    let large = fixture(4_000, 512, 4_096);
    let wanted_table = digest_of(&large.table, large.pool());
    let wanted_index = digest_of(&large.index, large.pool());

    let small = fixture(4_000, 512, 64);
    assert!(
        small.database.pool().page_count() > 200,
        "the fixture is too small for a 64-frame pool to be small"
    );
    small.database.pool().reset_stats();

    for round in 0..3 {
        assert_eq!(
            digest_of(&small.table, small.pool()),
            wanted_table,
            "the table read differently on round {round}"
        );
        assert_eq!(
            digest_of(&small.index, small.pool()),
            wanted_index,
            "the index read differently on round {round}"
        );
    }
    let stats = small.database.pool().stats();
    assert!(
        stats.evicted > 0,
        "nothing was evicted, so nothing was tested"
    );
    assert!(small.database.pool().resident() <= 64);

    // Point probes through the small pool, which is the path that holds a
    // parent pinned while it fetches a child and then swizzles into it.
    for key in (0..4_000i64).step_by(37) {
        let found = small
            .table
            .point(small.pool(), &[Datum::Int(key)])
            .expect("a readable tree")
            .expect("every key is there");
        assert_eq!(found.first(), Some(&OwnedDatum::Int(key)));
    }
    // And the integrity checker, which walks every interior page and compares
    // every separator against its child's first key.
    small.table.check(small.pool()).expect("a sound table");
    small.index.check(small.pool()).expect("a sound index");
}

/// Eviction while a descent is in flight does not lose the page it is on.
///
/// A pool with barely more frames than the tree is deep is the case where a
/// descent's own parent is the best eviction candidate. The parent is pinned
/// across the child's fetch for exactly this reason, and the swizzle re-checks
/// that the frame still holds the page it is about to annotate.
#[test]
fn a_descent_survives_a_pool_that_can_barely_hold_it() {
    let fixture = fixture(20_000, 512, 8);
    assert!(fixture.table.height() >= 2, "the tree must be deep enough");
    for key in (0..20_000i64).step_by(211) {
        let found = fixture
            .table
            .point(fixture.pool(), &[Datum::Int(key)])
            .expect("a readable tree")
            .expect("every key is there");
        assert_eq!(found.first(), Some(&OwnedDatum::Int(key)));
    }
    assert!(fixture.database.pool().stats().evicted > 0);
}

/// The stable counterpart of the `leaf_page`, `interior_page` and `meta_page`
/// fuzz targets.
///
/// Every byte of a well-formed page of each kind is flipped in turn and every
/// accessor is called on the result. Nothing may panic; an error is the right
/// answer and so is a plausible-but-wrong value, because a decoder is only
/// obliged to be *safe* on bytes that pass its checks - the checksum is what
/// makes it obliged to be right, and that is a different test.
#[test]
fn corrupt_pages_never_panic() {
    let fixture = fixture(400, 512, 256);
    let pool = fixture.pool();

    // One leaf and one interior page, taken out of a real tree so that they
    // are well formed before they are damaged.
    let leaf_page = {
        let guard = pool.fetch(fixture.table.first_leaf()).expect("a leaf");
        guard.bytes().to_vec()
    };
    let interior_page = {
        let guard = pool.fetch(fixture.table.root()).expect("a root");
        guard.bytes().to_vec()
    };
    let mut meta_page = vec![0u8; 512];
    Meta::fresh(512, 99)
        .encode(&mut meta_page)
        .expect("a meta page");

    for (name, page) in [
        ("leaf", &leaf_page),
        ("interior", &interior_page),
        ("meta", &meta_page),
    ] {
        // Every byte, and every bit of the first sixty-four - the headers,
        // where a single bit decides how the rest is read.
        for index in 0..page.len() {
            let mut damaged = page.clone();
            if let Some(byte) = damaged.get_mut(index) {
                *byte = byte.wrapping_add(0x5A);
            }
            exercise_every_decoder(&damaged);
        }
        for index in 0..64.min(page.len()) {
            for bit in 0..8 {
                let mut damaged = page.clone();
                if let Some(byte) = damaged.get_mut(index) {
                    *byte ^= 1 << bit;
                }
                exercise_every_decoder(&damaged);
            }
        }
        // And truncation at every length, which is the other way a page
        // arrives wrong.
        for length in 0..page.len() {
            exercise_every_decoder(page.get(..length).unwrap_or(&[]));
        }
        let _ = name;
    }
}

/// Runs every decoder over one run of bytes and reads everything it exposes.
///
/// @param page - the bytes to decode
fn exercise_every_decoder(page: &[u8]) {
    let _ = Meta::decode(page);
    let _ = interior::swip_offsets_of(page);
    if let Ok(parsed) = InteriorRef::parse(page) {
        let _ = parsed.validate();
        for slot in 0..parsed.count().min(512) {
            let _ = parsed.key(slot);
        }
        for child in 0..parsed.children().min(512) {
            let _ = parsed.swip(child);
        }
        let _ = parsed.rightmost();
        let _ = parsed.child_for(b"probe");
    }
    if let Ok(leaf) = LeafRef::parse(page) {
        let _ = leaf.integrity();
        let rows = leaf.row_count().min(512);
        let columns = leaf.column_count().min(32);
        for row in 0..rows {
            let _ = leaf.is_tombstoned(row);
            for column in 0..columns {
                let _ = leaf.value(row, column);
            }
        }
        for entry in 0..leaf.delta_count().min(32) {
            for column in 0..columns {
                let _ = leaf.delta_value(entry, column);
            }
        }
        let _ = leaf.search(&[Datum::Int(1)]);
        let _ = leaf.lower_bound(&[Datum::Int(1)]);
        let _ = leaf.upper_bound(&[Datum::Int(1)]);
        let _ = leaf.live();
    }
}

/// The stable counterpart of the `memcmp_key` fuzz target.
///
/// Byte order and value order agree over a seeded sweep of mixed-class tuples,
/// including the integers above 2^53 that the encoding used to conflate.
#[test]
fn the_key_encoding_orders_like_the_comparison() {
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let texts: Vec<Vec<u8>> = (0..8).map(|n| format!("t{n}").into_bytes()).collect();
    let mut tuples: Vec<Vec<OwnedDatum>> = Vec::new();
    for _ in 0..2_000 {
        let value = match next() % 6 {
            0 => OwnedDatum::Null,
            1 => OwnedDatum::Int(next() as i64),
            2 => OwnedDatum::Int((next() % 1_000) as i64),
            3 => OwnedDatum::Real(f64::from_bits(next())),
            4 => OwnedDatum::Text(
                texts
                    .get((next() % texts.len() as u64) as usize)
                    .cloned()
                    .unwrap_or_default(),
            ),
            _ => OwnedDatum::Blob(vec![(next() % 256) as u8; (next() % 4) as usize]),
        };
        tuples.push(vec![value]);
    }
    for left in &tuples {
        for right in tuples.iter().take(64) {
            let a: Vec<Datum<'_>> = left.iter().map(OwnedDatum::borrow).collect();
            let b: Vec<Datum<'_>> = right.iter().map(OwnedDatum::borrow).collect();
            if a.iter()
                .chain(b.iter())
                .any(|value| matches!(value, Datum::Real(number) if number.is_nan()))
            {
                continue;
            }
            let by_value = inillucent_tree::leaf::compare_rows(&a, &b, 1);
            let by_bytes = key::encode(&a).as_bytes().cmp(key::encode(&b).as_bytes());
            assert_eq!(by_bytes, by_value, "{a:?} against {b:?}");
        }
    }
}

/// The metamorphic plan sweep: every lever, one answer.
///
/// The TDD asks that "every SLT `SELECT` and every scorecard query is run under
/// each applicable `PRAGMA inillucent.force_plan` alternative and must produce the
/// same digest". This is that sweep over a set of queries that between them
/// reach every lever: a covering index against a table scan, a streaming
/// grouped aggregate against a hash one, an adjacent `DISTINCT` against a hash
/// set, a bounded heap against a full sort, and a skip scan against a walk.
///
/// The queries go through the real lexer, binder and planner - the front end
/// the rearchitecture keeps - because a plan built by hand is a plan nobody
/// writes, and the physical pass's job is to run what the planner produces.
///
/// A lever that changed no plan would make the sweep vacuous, so it also
/// asserts the operator chains were not all identical.
#[test]
fn every_forced_plan_gives_the_same_answer() {
    let fixture = fixture(2_000, 1_024, 512);
    let queries = [
        "SELECT DISTINCT category FROM t ORDER BY category",
        "SELECT category, count(*) FROM t GROUP BY category ORDER BY category",
        "SELECT count(*), sum(id), max(category) FROM t",
        "SELECT id FROM t ORDER BY label LIMIT 10",
        "SELECT id, label FROM t WHERE id = 500",
        "SELECT count(id) FROM t WHERE id BETWEEN 100 AND 300",
    ];
    for sql in queries {
        let plan = fixture
            .plan(sql)
            .unwrap_or_else(|error| panic!("{sql}: planning failed: {:?}", error.detail()));
        let mut answers: Vec<Vec<Vec<OwnedDatum>>> = Vec::new();
        let mut chains: Vec<String> = Vec::new();
        for (lever, forced) in ForcePlan::alternatives() {
            let Ok(prepared) = inillucent_exec::physical::prepare(&plan, &fixture, forced) else {
                continue;
            };
            let rows = Rc::new(RefCell::new(Vec::new()));
            let sink = Box::new(inillucent_exec::ops::CollectInto::new(Rc::clone(&rows)));
            let built = inillucent_exec::physical::build_prepared(
                &plan,
                &fixture,
                &prepared,
                &Params::new(),
                sink,
            );
            let (mut pipeline, shape) = match built {
                Ok(built) => built,
                Err(error) => panic!("{sql} under '{lever}': {:?}", error.detail()),
            };
            pipeline
                .run()
                .unwrap_or_else(|error| panic!("{sql} under '{lever}': {:?}", error.detail()));
            chains.push(format!("{lever}: {}", shape.operators.join(" -> ")));
            let collected = rows.borrow().clone();
            answers.push(collected);
        }
        assert!(answers.len() >= 2, "{sql}: only one plan was reachable");
        let first = answers.first().cloned().unwrap_or_default();
        assert!(
            !first.is_empty(),
            "{sql}: the query returned nothing to compare"
        );
        for (index, answer) in answers.iter().enumerate() {
            assert_eq!(
                answer,
                &first,
                "{sql}: plan {} answered differently\n{}",
                chains.get(index).map(String::as_str).unwrap_or(""),
                chains.join("\n")
            );
        }
        let distinct: std::collections::HashSet<&String> = chains.iter().collect();
        assert!(
            distinct.len() > 1,
            "{sql}: every lever produced the same chain, so nothing was tested:\n{}",
            chains.join("\n")
        );
    }
}

/// The physical pass refuses what it does not implement, by name.
///
/// `physical.rs` was at 0% coverage at the end of Phase 1 because it was only
/// ever reached through a measurement binary. These are the refusals - the
/// whitelist's whole point is that an unhandled construct fails loudly rather
/// than returning a plausible wrong answer, and a refusal nobody tests is a
/// refusal that could quietly become an approximation.
#[test]
fn the_physical_pass_refuses_what_it_cannot_run() {
    let fixture = fixture(200, 1_024, 256);
    // **This list shrinks as constructs are implemented, and each one moves to
    // `the_physical_pass_answers_what_it_implements` with an expected answer
    // rather than being deleted.** `HAVING`, an aggregate with
    // `DISTINCT`, a `VALUES` arm, a query with no FROM term, a subquery source
    // and a keyless join have moved out of it; what is left is what the pass
    // still routes to the VM.
    let refused = [
        (
            "a compound query",
            "SELECT id FROM t UNION SELECT id FROM t",
        ),
        (
            "a window function",
            "SELECT id, row_number() OVER () FROM t",
        ),
        // An outer join left this list: `NestedLoopJoin` reads the
        // inner side once and evaluates the `ON` over each pair, which is what
        // distinguishes "no partner" from "a partner that failed the condition"
        // and so what a null extension needs. It is graded against the pinned
        // shell by `advanced_sql::joins_match_the_oracle`, 21 statements.
    ];
    for (what, sql) in refused {
        let Ok(plan) = fixture.plan(sql) else {
            // The binder refused it first, which is also a refusal.
            continue;
        };
        let outcome = inillucent_exec::physical::run(&plan, &fixture, &Params::new());
        assert!(
            outcome.is_err(),
            "{what}: `{sql}` was run rather than refused"
        );
    }
}

/// The physical pass answers the shapes it does implement, correctly.
///
/// The other half of the coverage gap: a whitelist that refuses everything
/// would pass the test above.
#[test]
fn the_physical_pass_answers_what_it_implements() {
    let fixture = fixture(500, 1_024, 256);
    let cases: [(&str, usize); 14] = [
        ("SELECT id FROM t", 500),
        ("SELECT DISTINCT category FROM t ORDER BY category", 64),
        ("SELECT category, count(*) FROM t GROUP BY category", 64),
        ("SELECT count(*) FROM t", 1),
        ("SELECT id FROM t ORDER BY id DESC LIMIT 7", 7),
        ("SELECT id FROM t WHERE id = 42", 1),
        ("SELECT id FROM t WHERE id BETWEEN 10 AND 19", 10),
        ("SELECT id FROM t WHERE id > 495", 4),
        // Moved here from the refusal list. Each one carries the
        // answer rather than only the acceptance: a construct that stopped
        // being refused and started being wrong would otherwise read as
        // progress.
        (
            "SELECT category FROM t GROUP BY category HAVING count(*) > 1",
            64,
        ),
        (
            "SELECT category FROM t GROUP BY category HAVING count(*) > 8",
            0,
        ),
        ("SELECT count(DISTINCT category) FROM t", 1),
        ("VALUES (1), (2)", 2),
        ("SELECT 1", 1),
        ("SELECT * FROM (SELECT id FROM t WHERE id < 7)", 7),
    ];
    for (sql, wanted) in cases {
        let plan = fixture
            .plan(sql)
            .unwrap_or_else(|error| panic!("{sql}: {:?}", error.detail()));
        let (rows, shape) = inillucent_exec::physical::run(&plan, &fixture, &Params::new())
            .unwrap_or_else(|error| panic!("{sql}: {:?}", error.detail()));
        assert_eq!(rows.len(), wanted, "{sql}");
        assert!(!shape.names.is_empty(), "{sql} named no columns");
    }
}

/// A bound parameter reaches the seek key, the range bounds and the predicate.
///
/// Parameters were compiled in as literals at build time in Phase 1 because
/// nothing bound one; every read family in the scorecard binds one, so this is
/// what makes the gate's numbers about the queries the scorecard names.
#[test]
fn parameters_reach_every_place_a_plan_uses_one() {
    let fixture = fixture(500, 1_024, 256);
    let plan = fixture
        .plan("SELECT id FROM t WHERE id = ?1")
        .expect("a plan");
    for key in [0i64, 1, 250, 499, 500, -1] {
        let params = Params::from_values(vec![OwnedDatum::Int(key)]);
        let (rows, _) = inillucent_exec::physical::run(&plan, &fixture, &params).expect("it runs");
        let wanted = usize::from((0..500).contains(&key));
        assert_eq!(rows.len(), wanted, "id = {key}");
    }
    let plan = fixture
        .plan("SELECT count(id) FROM t WHERE id BETWEEN ?1 AND ?1 + 9")
        .expect("a plan");
    for low in [0i64, 100, 495] {
        let params = Params::from_values(vec![OwnedDatum::Int(low)]);
        let (rows, _) = inillucent_exec::physical::run(&plan, &fixture, &params).expect("it runs");
        let wanted = (low..=low + 9).filter(|key| (0..500).contains(key)).count();
        assert_eq!(
            rows.first().and_then(|row| row.first()),
            Some(&OwnedDatum::Int(wanted as i64)),
            "between {low} and {}",
            low + 9
        );
    }
}

/// `PRAGMA inillucent.force_plan` parses its operator list and refuses a name it
/// does not know.
#[test]
fn the_force_plan_pragma_parses() {
    assert_eq!(ForcePlan::parse("").expect("empty"), ForcePlan::default());
    let forced = ForcePlan::parse("scan, sort ,hashgroup").expect("a list");
    assert!(forced.table_scan && forced.full_sort && forced.hash_group);
    assert!(!forced.hash_distinct && !forced.no_skip_scan);
    assert!(ForcePlan::parse("noskip").expect("one").no_skip_scan);
    assert!(ForcePlan::parse("distinct").expect("one").hash_distinct);
    let error = ForcePlan::parse("hashjoin").expect_err("an unknown name");
    assert!(
        error.detail().unwrap_or("").contains("hashjoin"),
        "the refusal must name what it did not know: {error:?}"
    );
    assert!(ForcePlan::alternatives().len() >= 5);
}
