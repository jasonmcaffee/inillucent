//! Where a rowid lookup's nanoseconds actually go, stage by stage.
//!
//! An earlier measurement found three numbers - a resident fetch, a descent, a descent
//! plus a leaf search - and concluded that `range.lookaside` and `join.range`
//! lose on a per-probe constant. They do, but three numbers do not say which
//! part of the constant is worth attacking, and this file exists to
//! stop guessing about that.
//!
//! So this takes the probe apart into every stage it has, and it measures the
//! same probe under two *orders*: the order the index hands the rowids over,
//! which is random with respect to the table tree, and the sorted order a
//! batched-key-access plan would put them in. The difference between those two
//! is the whole of hypothesis A, measured before any of it is built.
//!
//! It also separates what a gate workload pays to *build* its operator chain
//! from what it pays to run it, because the gate's own breakdown reports the
//! two added together, and a build cost that is half of a point lookup is not
//! a thing to discover after optimising the half that was already cheap.
//!
//! Invariant: every rung of every ladder here binds a fresh value on each
//! execution, the way the gate does. An instrument that repeats one parameter
//! measures a working set that stays in cache rather than the workload, and
//! this one did: its first version probed the same two hundred rowids two
//! thousand times and read twenty per cent faster than the gate for that
//! reason, which is also how it produced the 2.7x sorted-order figure that a
//! built implementation then failed to reproduce.
//!
//! Usage:
//!   inillucent-probeprofile <sqlite fixture> [--scale S] [--page-size N] [--frames N]

/// The engine's own allocator, installed for this program.
///
/// **Part of the build, not of a workload.** SQLite ships its own memory
/// subsystem and is compiled as one translation unit; a Rust workspace measured
/// on the platform allocator is being measured on a build configuration rather
/// than on an engine, which is the same reasoning that fixed fat LTO and one
/// codegen unit in the release profile. The Windows C runtime heap was
/// measured at 59% of a trivial compile and this size-classed free list at
/// 17% overall, which is why Phase 3's Part E names it the cheapest first move.
#[global_allocator]
static ALLOCATOR: inillucent_alloc::Pooled = inillucent_alloc::Pooled;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use inillucent_compat::newengine::ImportedDatabase;
use inillucent_compat::perf::{plan_for, Bind, Workload};
use inillucent_exec::physical::Params;
use inillucent_tree::datum::{Datum, OwnedDatum};
use inillucent_tree::leaf::LeafRef;

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let Some(fixture) = arguments.first().filter(|value| !value.starts_with("--")) else {
        eprintln!(
            "usage: inillucent-probeprofile <sqlite fixture> [--scale S] [--page-size N] \
             [--frames N]"
        );
        return ExitCode::from(2);
    };
    match run(&arguments, fixture) {
        Ok(()) => ExitCode::SUCCESS,
        Err(reason) => {
            eprintln!("{reason}");
            ExitCode::FAILURE
        }
    }
}

/// Returns the value of a `--name value` flag.
///
/// @param arguments - the command line
/// @param name - the flag to look for
fn flag(arguments: &[String], name: &str) -> Option<String> {
    arguments
        .iter()
        .position(|value| value == name)
        .and_then(|at| arguments.get(at.saturating_add(1)))
        .cloned()
}

/// Returns the reason an engine error carries.
///
/// @param error - the error to explain
fn why(error: &inillucent_base::DbError) -> String {
    error
        .detail()
        .map(str::to_string)
        .unwrap_or_else(|| error.message().to_string())
}

/// Runs every stage and prints the tables.
///
/// @param arguments - the command line
/// @param fixture - the SQLite database to import
fn run(arguments: &[String], fixture: &str) -> Result<(), String> {
    let scale = flag(arguments, "--scale").unwrap_or_else(|| "medium".to_string());
    let page_size = flag(arguments, "--page-size")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(32_768);
    let frames = flag(arguments, "--frames")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(4_096);
    let plan = plan_for(&scale);
    let rows = plan.rows;

    let database = ImportedDatabase::import_with(PathBuf::from(fixture), page_size, frames)
        .map_err(|error| format!("import failed: {}", why(&error)))?;
    database.warm().map_err(|error| why(&error))?;
    println!("scale {scale}, {rows} rows, page size {page_size}, {frames} frames");

    probe_stages(&database, rows)?;
    build_against_run(&database, &plan.workloads, rows)?;
    compile_stages(&database, &plan.workloads, rows)?;
    index_probe_stages(&database)?;
    decompose(&database, rows)?;
    Ok(())
}

/// Takes the prefix probe into a secondary index apart, stage by stage.
///
/// `join.range`'s ladder says the 201 probes into `side_owner` cost more than
/// the 201 rowid lookups into `main_table` do, which is the opposite of what
/// the shape suggests - the index is a quarter of the size and its entries are
/// two integers. So this measures the same stages the rowid probe was measured
/// in, on the index tree, and on the exact keys the join hands it.
///
/// @param database - the imported trees
fn index_probe_stages(database: &ImportedDatabase) -> Result<(), String> {
    let Some(side) = database.table_root("side_table") else {
        return Ok(());
    };
    let candidates = database.candidates(side);
    let Some(index_root) = candidates.first().copied() else {
        return Ok(());
    };
    let Some(tree) = inillucent_exec::physical::TreeCatalog::tree(database, index_root) else {
        return Ok(());
    };
    let Some(pool) = inillucent_exec::physical::TreeCatalog::pool_for(database, index_root) else {
        return Ok(());
    };
    println!();
    println!(
        "side_owner: root page {:?}, height {}, {} leaves, {} rows, key columns {}",
        tree.root(),
        tree.height(),
        tree.leaf_count(),
        tree.row_count(),
        tree.key_columns()
    );

    // The owner values a `join.range` execution probes with: the rowids of the
    // 201 main_table rows a key range covers, which scatter across side_owner.
    let keys: Vec<i64> = (1..=4_096i64)
        .map(|nth| nth.wrapping_mul(2_654_435_761) % 100_000)
        .map(|value| value.max(1))
        .collect();

    println!();
    println!("## one prefix probe into side_owner   (nanoseconds each)");
    let encode = per_over(8, keys.len(), || {
        for key in &keys {
            std::hint::black_box(tree.encode_key_small(&[Datum::Int(*key)]));
        }
        Ok(())
    })?;
    println!("  {:<38} {encode:>10.1}", "encode_key_small");

    let encoded: Vec<Vec<u8>> = keys
        .iter()
        .map(|key| tree.encode_key(&[Datum::Int(*key)]))
        .collect();
    let descend = per_over(8, encoded.len(), || {
        for key in &encoded {
            let (guard, _) = tree.descend_guard(pool, key).map_err(|error| why(&error))?;
            std::hint::black_box(guard.bytes().len());
        }
        Ok(())
    })?;
    println!("  {:<38} {descend:>10.1}", "descend_guard");

    // The class-array walk that `all_typed` does, on its own. Every bound over
    // an integer column asks for it, so it is paid once per probe and twice per
    // skip-scan seek.
    let (guard, _) = tree
        .descend_guard(pool, encoded.first().map(Vec::as_slice).unwrap_or(&[]))
        .map_err(|error| why(&error))?;
    let leaf = LeafRef::parse(guard.bytes()).map_err(|error| why(&error))?;
    let column = leaf.column(0).map_err(|error| why(&error))?;
    let directory = per_over(2_000, 16, || {
        for _ in 0..16 {
            std::hint::black_box(leaf.column(0).map_err(|error| why(&error))?);
        }
        Ok(())
    })?;
    println!("  {:<38} {directory:>10.1}", "leaf.column(0)");
    let typed = per_over(2_000, 16, || {
        for _ in 0..16 {
            std::hint::black_box(column.all_typed());
        }
        Ok(())
    })?;
    println!(
        "  {:<38} {typed:>10.1}   ({} rows)",
        "+ all_typed",
        leaf.row_count()
    );
    drop(guard);

    let bounded = per_over(8, encoded.len(), || {
        for (index, key) in encoded.iter().enumerate() {
            let (guard, _) = tree.descend_guard(pool, key).map_err(|error| why(&error))?;
            let leaf = LeafRef::parse(guard.bytes()).map_err(|error| why(&error))?;
            let value = keys.get(index).copied().unwrap_or(0);
            let at = leaf
                .lower_bound(&[Datum::Int(value)])
                .map_err(|error| why(&error))?;
            std::hint::black_box(at);
        }
        Ok(())
    })?;
    println!("  {:<38} {bounded:>10.1}", "+ parse + lower_bound");

    let whole = per_over(8, keys.len(), || {
        for key in &keys {
            let mut seen = 0usize;
            tree.visit_equal(pool, &[Datum::Int(*key)], &mut |_leaf, start, end| {
                seen = seen.saturating_add(end.saturating_sub(start));
                Ok(true)
            })
            .map_err(|error| why(&error))?;
            std::hint::black_box(seen);
        }
        Ok(())
    })?;
    println!("  {:<38} {whole:>10.1}", "tree.visit_equal");
    Ok(())
}

/// Times the two losing workloads against the queries they are built out of.
///
/// `join.range` is a span scan, then an index probe per outer row, then a rowid
/// lookup per match, then an aggregate. Timing only the whole thing says it is
/// slow; timing the ladder says which of those four it is, and each rung here
/// is the rung above it plus exactly one stage.
///
/// **The bound value moves on every execution, the way the gate's does.** A
/// ladder that probed the same two hundred rowids two thousand times measured a
/// working set that stays in cache, which is not the workload: `Bind::Scatter`
/// moves the range on each iteration, so every execution touches leaves the
/// last one did not. The first version of this instrument did not, and it read
/// twenty per cent faster than the gate for exactly that reason.
///
/// @param database - the imported trees
/// @param rows - how many rows the base table holds
fn decompose(database: &ImportedDatabase, rows: u32) -> Result<(), String> {
    let ladder: [(&str, &str, bool); 13] = [
        (
            "point by rowid, label",
            "SELECT label FROM main_table WHERE id = ?1",
            true,
        ),
        (
            "point by rowid, count",
            "SELECT count(*) FROM main_table WHERE id = ?1",
            true,
        ),
        (
            "span only",
            "SELECT count(key) FROM main_table WHERE key BETWEEN ?1 AND ?1 + 200",
            true,
        ),
        (
            "span + rowid lookup",
            "SELECT count(category) FROM main_table WHERE key BETWEEN ?1 AND ?1 + 200",
            true,
        ),
        (
            "span + lookup + length()",
            "SELECT sum(length(label)) FROM main_table WHERE key BETWEEN ?1 AND ?1 + 200",
            true,
        ),
        (
            "span + index probe",
            "SELECT count(*) FROM main_table JOIN side_table ON side_table.owner =              main_table.id WHERE main_table.key BETWEEN ?1 AND ?1 + 200",
            true,
        ),
        (
            "span + probe + rowid lookup",
            "SELECT count(side_table.note) FROM main_table JOIN side_table ON              side_table.owner = main_table.id WHERE main_table.key BETWEEN ?1 AND ?1 + 200",
            true,
        ),
        (
            "point probe + index probe",
            "SELECT count(*) FROM main_table JOIN side_table ON side_table.owner =              main_table.id WHERE main_table.id = ?1",
            true,
        ),
        ("full scan, count only", "SELECT count(*) FROM main_table", false),
        (
            "full scan, count(label)",
            "SELECT count(label) FROM main_table",
            false,
        ),
        (
            "full scan, max(label)",
            "SELECT max(label) FROM main_table",
            false,
        ),
        (
            "sort by id, limit 100",
            "SELECT id FROM main_table ORDER BY id LIMIT 100",
            false,
        ),
        (
            "sort by label, limit 100",
            "SELECT id FROM main_table ORDER BY label LIMIT 100",
            false,
        ),
    ];
    println!();
    println!("## the ladder, microseconds per execution   (statement reused, bind moves)");
    println!("  {:<30} {:>12}", "query", "run");
    for (name, sql, bound) in ladder {
        let Ok(plan) = database.plan(sql) else {
            println!("  {name:<30} {:>12}", "no plan");
            continue;
        };
        let Ok(prepared) = database.prepare(&plan) else {
            println!("  {name:<30} {:>12}", "refused");
            continue;
        };
        let mut params = Params::from_values(if bound {
            vec![bind_value(Bind::Scatter, 1, rows)]
        } else {
            Vec::new()
        });
        let sink = Box::new(Counting { rows: 0 });
        let mut statement = match database.statement(&plan, &prepared, &params, sink) {
            Ok(statement) => statement,
            Err(error) => {
                println!("  {name:<30} {:>12}", why(&error));
                continue;
            }
        };
        let iterations: u32 = if bound { 2_000 } else { 40 };
        let mut iteration = 0u32;
        let elapsed = per(iterations, || {
            if bound {
                iteration = iteration.wrapping_add(1);
                params.refill([bind_value(Bind::Scatter, iteration, rows)]);
            }
            statement.run(&params).map_err(|error| why(&error))?;
            Ok(())
        })?;
        println!("  {name:<30} {:>12.2}", elapsed / 1000.0);
    }
    Ok(())
}

/// Times every stage of a rowid lookup, in random and in sorted key order.
///
/// @param database - the imported trees
/// @param rows - how many rows the base table holds
fn probe_stages(database: &ImportedDatabase, rows: u32) -> Result<(), String> {
    let Some(root) = database.table_root("main_table") else {
        return Err("no main_table".to_string());
    };
    let Some(tree) = inillucent_exec::physical::TreeCatalog::tree(database, root) else {
        return Err("no tree for main_table".to_string());
    };
    let Some(pool) = inillucent_exec::physical::TreeCatalog::pool_for(database, root) else {
        return Ok(());
    };
    println!();
    println!(
        "main_table: root page {:?}, height {}, {} leaves, {} rows",
        tree.root(),
        tree.height(),
        tree.leaf_count(),
        tree.row_count()
    );

    // The rowids `range.lookaside` probes: what an index range over `key`
    // hands to the table tree, which is a scatter with respect to the rowid.
    const KEYS: usize = 4_096;
    let scattered: Vec<i64> = (0..KEYS as u64)
        .map(|nth| 1 + (nth.wrapping_mul(2_654_435_761) % u64::from(rows.max(1))) as i64)
        .collect();
    let mut sorted = scattered.clone();
    sorted.sort_unstable();

    let encoded_scattered: Vec<Vec<u8>> = scattered
        .iter()
        .map(|key| tree.encode_key(&[Datum::Int(*key)]))
        .collect();
    let encoded_sorted: Vec<Vec<u8>> = sorted
        .iter()
        .map(|key| tree.encode_key(&[Datum::Int(*key)]))
        .collect();

    println!();
    println!("## one rowid lookup, taken apart   (nanoseconds each)");
    println!("  {:<38} {:>10} {:>10}", "stage", "scattered", "sorted");

    let root_page = tree.root();
    let fetch = per(200_000, || {
        let guard = pool.fetch(root_page).map_err(|error| why(&error))?;
        std::hint::black_box(guard.bytes().len());
        Ok(())
    })?;
    println!(
        "  {:<38} {fetch:>10.1} {:>10}",
        "pool.fetch of a resident page", "-"
    );

    let encode = per(200_000, || {
        for key in scattered.iter().take(16) {
            std::hint::black_box(tree.encode_key_small(&[Datum::Int(*key)]));
        }
        Ok(())
    })? / 16.0;
    println!("  {:<38} {encode:>10.1} {:>10}", "encode_key_small", "-");

    let descend = |keys: &[Vec<u8>]| -> Result<f64, String> {
        per_over(8, keys.len(), || {
            for key in keys {
                let (guard, _) = tree.descend_guard(pool, key).map_err(|error| why(&error))?;
                std::hint::black_box(guard.bytes().len());
            }
            Ok(())
        })
    };
    let (a, b) = (descend(&encoded_scattered)?, descend(&encoded_sorted)?);
    println!("  {:<38} {a:>10.1} {b:>10.1}", "descend_guard");

    let parsed = |keys: &[Vec<u8>]| -> Result<f64, String> {
        per_over(8, keys.len(), || {
            for key in keys {
                let (guard, _) = tree.descend_guard(pool, key).map_err(|error| why(&error))?;
                let leaf = LeafRef::parse(guard.bytes()).map_err(|error| why(&error))?;
                std::hint::black_box(leaf.row_count());
            }
            Ok(())
        })
    };
    let (a, b) = (parsed(&encoded_scattered)?, parsed(&encoded_sorted)?);
    println!("  {:<38} {a:>10.1} {b:>10.1}", "+ LeafRef::parse");

    let searched = |keys: &[Vec<u8>], values: &[i64]| -> Result<f64, String> {
        per_over(8, keys.len(), || {
            for (index, key) in keys.iter().enumerate() {
                let (guard, _) = tree.descend_guard(pool, key).map_err(|error| why(&error))?;
                let leaf = LeafRef::parse(guard.bytes()).map_err(|error| why(&error))?;
                let value = values.get(index).copied().unwrap_or(0);
                let found = leaf
                    .search(&[Datum::Int(value)])
                    .map_err(|error| why(&error))?;
                std::hint::black_box(found.is_ok());
            }
            Ok(())
        })
    };
    let (a, b) = (
        searched(&encoded_scattered, &scattered)?,
        searched(&encoded_sorted, &sorted)?,
    );
    println!("  {:<38} {a:>10.1} {b:>10.1}", "+ leaf.search");

    // The whole probe the executor runs, reading `label` - which is the column
    // `range.lookaside` asks for and the one a PAX leaf has to reach a second
    // mini-column for.
    let whole = |values: &[i64]| -> Result<f64, String> {
        per_over(8, values.len(), || {
            for value in values {
                let found = tree
                    .probe(pool, &[Datum::Int(*value)], |leaf, row| {
                        Ok(OwnedDatum::from_datum(&leaf.value_at(row, 3)?))
                    })
                    .map_err(|error| why(&error))?;
                std::hint::black_box(found.is_some());
            }
            Ok(())
        })
    };
    let (a, b) = (whole(&scattered)?, whole(&sorted)?);
    println!("  {:<38} {a:>10.1} {b:>10.1}", "tree.probe reading label");

    // The same probe reading only the key column, so the difference above says
    // what reaching a second mini-column costs.
    let keyonly = |values: &[i64]| -> Result<f64, String> {
        per_over(8, values.len(), || {
            for value in values {
                let found = tree
                    .probe(pool, &[Datum::Int(*value)], |leaf, row| {
                        Ok(OwnedDatum::from_datum(&leaf.value_at(row, 0)?))
                    })
                    .map_err(|error| why(&error))?;
                std::hint::black_box(found.is_some());
            }
            Ok(())
        })
    };
    let (a, b) = (keyonly(&scattered)?, keyonly(&sorted)?);
    println!(
        "  {:<38} {a:>10.1} {b:>10.1}",
        "tree.probe reading the key only"
    );

    // What a batched plan could reach: the sorted rowids found by advancing
    // through the leaf chain once instead of descending once per key.
    let chained = chain_walk(tree, pool, &sorted)?;
    println!(
        "  {:<38} {:>10} {chained:>10.1}",
        "sorted walk of the leaf chain", "-"
    );
    Ok(())
}

/// Walks a sorted rowid list against the leaf chain in one traversal.
///
/// The measurement hypothesis A rests on: the same rows found by advancing
/// through the leaves in key order instead of descending once per key.
///
/// @param tree - the table tree
/// @param pool - the buffer pool
/// @param sorted - the rowids, ascending
fn chain_walk(
    tree: &inillucent_tree::PagedTree,
    pool: &inillucent_pool::Pool,
    sorted: &[i64],
) -> Result<f64, String> {
    per_over(8, sorted.len(), || {
        let mut at = 0usize;
        let mut found = 0usize;
        tree.visit_leaves(pool, &mut |leaf| {
            while at < sorted.len() {
                let Some(value) = sorted.get(at) else { break };
                match leaf.search(&[Datum::Int(*value)])? {
                    Ok(row) => {
                        std::hint::black_box(leaf.value(row, 3)?);
                        found = found.saturating_add(1);
                        at = at.saturating_add(1);
                    }
                    Err(position) => {
                        if position >= leaf.row_count() {
                            return Ok(true);
                        }
                        at = at.saturating_add(1);
                    }
                }
            }
            Ok(at < sorted.len())
        })
        .map_err(|error| why(&error))?;
        std::hint::black_box(found);
        Ok(())
    })
}

/// Separates what each gate workload pays to build from what it pays to run.
///
/// @param database - the imported trees
/// @param workloads - the plan's workloads
/// @param rows - how many rows the base table holds
fn build_against_run(
    database: &ImportedDatabase,
    workloads: &[Workload],
    rows: u32,
) -> Result<(), String> {
    println!();
    println!("## build against run, per execution   (microseconds)");
    println!(
        "  {:<18} {:>10} {:>10} {:>10} {:>7}",
        "workload", "build", "run", "total", "build%"
    );
    for workload in workloads {
        if workload.mutates || workload.prepare_each {
            continue;
        }
        let Ok(plan) = database.plan(&workload.sql) else {
            continue;
        };
        let Ok(prepared) = database.prepare(&plan) else {
            continue;
        };
        let params = Params::from_values(
            workload
                .binds
                .iter()
                .map(|bind| bind_value(*bind, 1, rows))
                .collect(),
        );
        let build = per(2_000, || {
            let sink = Box::new(Counting { rows: 0 });
            let built = database
                .pipeline(&plan, &prepared, &params, sink)
                .map_err(|error| why(&error))?;
            drop(built);
            Ok(())
        })?;
        let total = per(2_000, || {
            let sink = Box::new(Counting { rows: 0 });
            let (mut pipeline, _) = database
                .pipeline(&plan, &prepared, &params, sink)
                .map_err(|error| why(&error))?;
            pipeline.run().map_err(|error| why(&error))?;
            Ok(())
        })?;
        println!(
            "  {:<18} {:>10.2} {:>10.2} {:>10.2} {:>6.0}%",
            workload.name,
            build / 1000.0,
            (total - build) / 1000.0,
            total / 1000.0,
            100.0 * build / total.max(1.0)
        );
    }
    Ok(())
}

/// Prints where compiling a statement goes, for the workloads the plan marks
/// `prepare: each`.
///
/// **The build-against-run table above cannot answer this**, because it hoists
/// `plan` and `prepare` out of the loop - which is right for a workload that
/// prepares once, and is exactly wrong for `open.prepare`, whose whole subject
/// is the compile. `prepare.trivial` is 0.32x on this engine and Phase 1's
/// analysis of the same workload was about the *old* engine's per-statement
/// read transaction, so it does not carry over. This is the split that
/// replaces guessing about it.
///
/// Note what is *not* here: the plan cache. `ImportedDatabase::plan` parses,
/// binds and plans on every call, so this is the compile the gate times and
/// the cache is in neither.
///
/// @param database - the imported fixture
/// @param workloads - the plan's workloads
/// @param rows - how many rows the base table holds
fn compile_stages(
    database: &ImportedDatabase,
    workloads: &[Workload],
    rows: u32,
) -> Result<(), String> {
    println!();
    println!("## compiling a statement, stage by stage   (nanoseconds)");
    println!(
        "  {:<18} {:>8} {:>8} {:>8} {:>10} {:>8} {:>8}",
        "workload", "parse", "+bind", "+plan", "+physical", "+build", "+run"
    );
    for workload in workloads {
        if !workload.prepare_each || workload.mutates {
            continue;
        }
        let params = Params::from_values(
            workload
                .binds
                .iter()
                .map(|bind| bind_value(*bind, 1, rows))
                .collect(),
        );
        if database.plan(&workload.sql).is_err() {
            continue;
        }
        // The three stages inside `plan`, because "the plan is 64% of it" is
        // not yet a thing anybody can act on. The arena the TDD names as the
        // remedy would sit under whichever of these is the allocation.
        let limits = inillucent_base::limits::Limits::default();
        let parsed = per(2_000, || {
            let _ =
                inillucent_sql::parser::parse_next_statement(workload.sql.as_bytes(), 0, &limits)
                    .map_err(|error| format!("{error:?}"))?;
            Ok(())
        })?;
        let bound = per(2_000, || {
            let parsed =
                inillucent_sql::parser::parse_next_statement(workload.sql.as_bytes(), 0, &limits)
                    .map_err(|error| format!("{error:?}"))?;
            let authorizer = inillucent_sql::bind::AllowAll;
            let mut binder = inillucent_sql::bind::Binder::new(
                database.catalog_view(),
                &parsed.ast,
                &authorizer,
            )
            .with_source(workload.sql.as_bytes());
            let _ = binder
                .bind_statement(&parsed.statement)
                .map_err(|error| format!("{error:?}"))?;
            Ok(())
        })?;
        let planned = per(2_000, || {
            let _ = database.plan(&workload.sql).map_err(|error| why(&error))?;
            Ok(())
        })?;
        let physical = per(2_000, || {
            let plan = database.plan(&workload.sql).map_err(|error| why(&error))?;
            let _ = database.prepare(&plan).map_err(|error| why(&error))?;
            Ok(())
        })?;
        let built = per(2_000, || {
            let plan = database.plan(&workload.sql).map_err(|error| why(&error))?;
            let prepared = database.prepare(&plan).map_err(|error| why(&error))?;
            let sink = Box::new(Counting { rows: 0 });
            let built = database
                .pipeline(&plan, &prepared, &params, sink)
                .map_err(|error| why(&error))?;
            drop(built);
            Ok(())
        })?;
        let whole = per(2_000, || {
            let plan = database.plan(&workload.sql).map_err(|error| why(&error))?;
            let prepared = database.prepare(&plan).map_err(|error| why(&error))?;
            let sink = Box::new(Counting { rows: 0 });
            let (mut pipeline, _) = database
                .pipeline(&plan, &prepared, &params, sink)
                .map_err(|error| why(&error))?;
            pipeline.run().map_err(|error| why(&error))?;
            Ok(())
        })?;
        println!(
            "  {:<18} {parsed:>8.1} {bound:>8.1} {planned:>8.1} {physical:>10.1} {built:>8.1} {whole:>8.1}",
            workload.name
        );
    }
    Ok(())
}

/// A sink that counts rows and reads nothing.
struct Counting {
    rows: u64,
}

impl inillucent_exec::Sink for Counting {
    fn push(
        &mut self,
        batch: &inillucent_exec::Batch<'_>,
    ) -> inillucent_base::DbResult<inillucent_exec::Flow> {
        self.rows = self.rows.saturating_add(batch.live() as u64);
        Ok(inillucent_exec::Flow::Continue)
    }

    fn finish(&mut self) -> inillucent_base::DbResult<()> {
        Ok(())
    }
    /// Returns the sink to its pre-input state; it keeps no rows to forget.
    fn reset(&mut self) -> inillucent_base::DbResult<()> {
        Ok(())
    }
}

/// Returns the value one bind kind produces, the way `sqlite_bench.c` does.
///
/// @param bind - the bind kind
/// @param iteration - which iteration
/// @param rows - how many rows the base table holds
fn bind_value(bind: Bind, iteration: u32, rows: u32) -> OwnedDatum {
    let iteration = u64::from(iteration);
    let rows64 = u64::from(rows.max(1));
    match bind {
        Bind::Rowid => OwnedDatum::Int(1 + (iteration % rows64) as i64),
        Bind::Scatter => {
            OwnedDatum::Int(1 + (iteration.wrapping_mul(2_654_435_761) % rows64) as i64)
        }
        Bind::Counter => OwnedDatum::Int((rows64 + 1 + iteration) as i64),
        Bind::Int => OwnedDatum::Int(
            (iteration.wrapping_mul(1_103_515_245).wrapping_add(12_345) & 0x7fff_ffff) as i64,
        ),
        Bind::Text => OwnedDatum::Text(
            format!("row {iteration} lorem ipsum dolor sit amet consectetur").into_bytes(),
        ),
        Bind::Blob => {
            OwnedDatum::Blob((0..64u64).map(|j| ((iteration + j) & 0xff) as u8).collect())
        }
    }
}

/// Times a body and returns nanoseconds per call.
///
/// @param iterations - how many times to run it
/// @param body - the work
fn per(iterations: u32, mut body: impl FnMut() -> Result<(), String>) -> Result<f64, String> {
    body()?;
    let started = Instant::now();
    for _ in 0..iterations {
        body()?;
    }
    Ok(started.elapsed().as_secs_f64() * 1e9 / f64::from(iterations))
}

/// Times a body that does `each` units of work and returns nanoseconds per unit.
///
/// @param rounds - how many times to run the body
/// @param each - how many units one body does
/// @param body - the work
fn per_over(
    rounds: u32,
    each: usize,
    mut body: impl FnMut() -> Result<(), String>,
) -> Result<f64, String> {
    body()?;
    let started = Instant::now();
    for _ in 0..rounds {
        body()?;
    }
    Ok(started.elapsed().as_secs_f64() * 1e9 / (f64::from(rounds) * each.max(1) as f64))
}
