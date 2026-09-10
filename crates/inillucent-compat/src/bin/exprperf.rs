//! What closure compilation is worth, against the thing it replaces.
//!
//! Invariant: both arms evaluate the *same expression tree* over the *same
//! batches* and their answers are compared value for value before any time is
//! read. A faster arm that computes something else is not a faster arm.
//!
//! ## The comparison, and why it is this one
//!
//! The TDD's Phase 1 acceptance asks for closure compilation measured against
//! the old VM's expression evaluator. Timing the old VM end to end would answer
//! a different question, already measured: the VM was **not** where the scan
//! time went. The full VM path measured
//! 156.5 ns/row and deleting the interpreter entirely left 140-195 ns/row,
//! because the cost was in the storage layer underneath it. Repeating that
//! measurement would tell us about the pager again.
//!
//! The question closure compilation actually answers is narrower and is the one
//! measured here: **given an expression tree, does resolving the type dispatch
//! once at prepare time beat resolving it per row?** So the two arms are:
//!
//! - **compiled** - `inillucent_exec::compile`, which picks a specialised node per
//!   operator from the static types the binder derived, so a row costs one
//!   indirect call to a body with no type branch in it;
//! - **interpreted** - a tree-walking evaluator over the identical `Expr`,
//!   which matches on the node kind and on the operand classes for every row.
//!   This is what the old VM's `OP_Add` and `OP_Lt` do per row, without the
//!   register file and the pager underneath.
//!
//! Holding everything else equal is what makes the difference attributable.
//!
//! Usage: inillucent-exprperf [rounds]

use std::time::Instant;

use inillucent_base::DbResult;
use inillucent_exec::batch::{Batch, Vector};
use inillucent_exec::expr::{compile, truth, ArithOp, CompareOp, Expr, StaticType};
use inillucent_tree::datum::{Datum, OwnedDatum};

/// How many rows one batch holds.
const ROWS: usize = 2048;

fn main() {
    let rounds: u32 = std::env::args()
        .nth(1)
        .and_then(|text| text.parse().ok())
        .unwrap_or(200);
    match run(rounds) {
        Ok(()) => {}
        Err(error) => {
            eprintln!("failed: {}", error.message());
            std::process::exit(1);
        }
    }
}

/// Times both arms over every predicate.
///
/// @param rounds - how many times each arm evaluates each batch
fn run(rounds: u32) -> DbResult<()> {
    // Two integer columns and one text column, dense, as a scan of a clean leaf
    // produces them.
    let keys: Vec<i64> = (0..ROWS as i64)
        .map(|n| (n * 2_654_435_761) % 100_000)
        .collect();
    let categories: Vec<i64> = (0..ROWS as i64).map(|n| n % 64).collect();
    let key_bytes: Vec<u8> = keys.iter().flat_map(|value| value.to_le_bytes()).collect();
    let category_bytes: Vec<u8> = categories
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect();
    let labels: Vec<Vec<u8>> = (0..ROWS)
        .map(|n| format!("row {n} lorem ipsum dolor sit amet").into_bytes())
        .collect();
    let label_values: Vec<Datum<'_>> = labels.iter().map(|bytes| Datum::Text(bytes)).collect();

    let batch = Batch::new(
        ROWS,
        vec![
            Vector::Int64 {
                width: 8,
                base: 0,
                bytes: &key_bytes,
                class: None,
            },
            Vector::Int64 {
                width: 8,
                base: 0,
                bytes: &category_bytes,
                class: None,
            },
            Vector::Values(&label_values),
        ],
    );
    // The static types the binder derives from `key INTEGER`, `category
    // INTEGER`, `label TEXT`.
    let types = [StaticType::Int, StaticType::Int, StaticType::Text];

    let predicates: Vec<(&str, Expr)> = vec![
        (
            "category = 7",
            Expr::Compare(
                CompareOp::Equal,
                Box::new(Expr::Column(1)),
                Box::new(Expr::Literal(OwnedDatum::Int(7))),
            ),
        ),
        (
            "key > 50000",
            Expr::Compare(
                CompareOp::Greater,
                Box::new(Expr::Column(0)),
                Box::new(Expr::Literal(OwnedDatum::Int(50_000))),
            ),
        ),
        (
            "key + category > 50000",
            Expr::Compare(
                CompareOp::Greater,
                Box::new(Expr::Arith(
                    ArithOp::Add,
                    Box::new(Expr::Column(0)),
                    Box::new(Expr::Column(1)),
                )),
                Box::new(Expr::Literal(OwnedDatum::Int(50_000))),
            ),
        ),
        (
            "category = 7 AND key > 50000",
            Expr::And(
                Box::new(Expr::Compare(
                    CompareOp::Equal,
                    Box::new(Expr::Column(1)),
                    Box::new(Expr::Literal(OwnedDatum::Int(7))),
                )),
                Box::new(Expr::Compare(
                    CompareOp::Greater,
                    Box::new(Expr::Column(0)),
                    Box::new(Expr::Literal(OwnedDatum::Int(50_000))),
                )),
            ),
        ),
        (
            "length(label) > 30",
            Expr::Compare(
                CompareOp::Greater,
                Box::new(Expr::Length(Box::new(Expr::Column(2)))),
                Box::new(Expr::Literal(OwnedDatum::Int(30))),
            ),
        ),
    ];

    println!("{ROWS} rows per batch, {rounds} rounds, nanoseconds per row");
    println!(
        "  {:<32} {:>12} {:>12} {:>8}  matched",
        "predicate", "compiled", "interpreted", "speedup"
    );
    let mut speedups: Vec<f64> = Vec::new();
    for (name, expr) in &predicates {
        let compiled = compile(expr, &types)?;

        // The correctness gate: both arms over every row, compared before any
        // time is read.
        let mut matched = 0u32;
        for row in 0..ROWS {
            let fast = compiled.value(&batch, row)?;
            let fast = fast.get();
            let slow = interpret(expr, &batch, row)?;
            if fast.compare(&slow) != std::cmp::Ordering::Equal || fast.is_null() != slow.is_null()
            {
                return Err(inillucent_base::error::misuse(format!(
                    "{name} row {row}: compiled {fast:?}, interpreted {slow:?}"
                )));
            }
            if truth(&fast) == Some(true) {
                matched = matched.saturating_add(1);
            }
        }

        let fast_ns = time(rounds, || {
            let mut kept = 0u32;
            for row in 0..ROWS {
                if truth(&compiled.value(&batch, row)?.get()) == Some(true) {
                    kept = kept.saturating_add(1);
                }
            }
            Ok(kept)
        })?;
        let slow_ns = time(rounds, || {
            let mut kept = 0u32;
            for row in 0..ROWS {
                if truth(&interpret(expr, &batch, row)?) == Some(true) {
                    kept = kept.saturating_add(1);
                }
            }
            Ok(kept)
        })?;
        let speedup = slow_ns / fast_ns.max(f64::MIN_POSITIVE);
        speedups.push(speedup);
        println!("  {name:<32} {fast_ns:>11.2}  {slow_ns:>11.2}  {speedup:>7.2}x  {matched}");
    }
    let geomean =
        (speedups.iter().map(|value| value.ln()).sum::<f64>() / speedups.len() as f64).exp();
    println!();
    println!("  geometric mean speedup of compilation over interpretation: {geomean:.2}x");
    Ok(())
}

/// Times one arm and returns nanoseconds per row.
///
/// @param rounds - how many times to run the body
/// @param body - the arm to time
fn time(rounds: u32, mut body: impl FnMut() -> DbResult<u32>) -> DbResult<f64> {
    let warm = body()?;
    let started = Instant::now();
    for _ in 0..rounds {
        let out = std::hint::black_box(body()?);
        if out != warm {
            return Err(inillucent_base::error::misuse(
                "an arm was not deterministic",
            ));
        }
    }
    Ok(started.elapsed().as_secs_f64() * 1e9 / (f64::from(rounds) * ROWS as f64))
}

/// Evaluates an expression by walking the tree, once per row.
///
/// The arm closure compilation is measured against: the node kind is matched on
/// for every row, and so are the operand classes inside every operator. That is
/// what an opcode's body does when it does not know its operand types, which is
/// the position a bytecode VM is in.
///
/// It is written here rather than reused from the executor because the executor
/// does not have one - which is the point. Keeping it in the harness means the
/// engine has exactly one evaluator and this is a controlled comparison rather
/// than a second code path somebody could accidentally ship.
///
/// @param expr - the expression to evaluate
/// @param batch - the batch being evaluated
/// @param row - the row's position among the batch's live rows
fn interpret<'p>(expr: &Expr, batch: &Batch<'p>, row: usize) -> DbResult<Datum<'p>> {
    Ok(match expr {
        Expr::Column(index) => batch.value(row, *index)?,
        Expr::Literal(value) => match value {
            OwnedDatum::Null => Datum::Null,
            OwnedDatum::Int(number) => Datum::Int(*number),
            OwnedDatum::Real(number) => Datum::Real(*number),
            OwnedDatum::Text(_) | OwnedDatum::Blob(_) => Datum::Null,
        },
        Expr::Arith(op, left, right) => {
            let a = interpret(left, batch, row)?;
            let b = interpret(right, batch, row)?;
            if a.is_null() || b.is_null() {
                Datum::Null
            } else {
                match (a.as_int(), b.as_int()) {
                    (Some(x), Some(y)) => {
                        let checked = match op {
                            ArithOp::Add => x.checked_add(y),
                            ArithOp::Subtract => x.checked_sub(y),
                            ArithOp::Multiply => x.checked_mul(y),
                        };
                        match checked {
                            Some(number) => Datum::Int(number),
                            None => Datum::Real(match op {
                                ArithOp::Add => x as f64 + y as f64,
                                ArithOp::Subtract => x as f64 - y as f64,
                                ArithOp::Multiply => x as f64 * y as f64,
                            }),
                        }
                    }
                    _ => {
                        let (x, y) = (
                            inillucent_exec::expr::numeric(&a),
                            inillucent_exec::expr::numeric(&b),
                        );
                        Datum::Real(match op {
                            ArithOp::Add => x + y,
                            ArithOp::Subtract => x - y,
                            ArithOp::Multiply => x * y,
                        })
                    }
                }
            }
        }
        Expr::Compare(op, left, right) => {
            let a = interpret(left, batch, row)?;
            let b = interpret(right, batch, row)?;
            if a.is_null() || b.is_null() {
                Datum::Null
            } else {
                Datum::Int(i64::from(op.holds(a.compare(&b))))
            }
        }
        Expr::And(left, right) => {
            let a = truth(&interpret(left, batch, row)?);
            if a == Some(false) {
                Datum::Int(0)
            } else {
                let b = truth(&interpret(right, batch, row)?);
                match (a, b) {
                    (_, Some(false)) => Datum::Int(0),
                    (Some(true), Some(true)) => Datum::Int(1),
                    _ => Datum::Null,
                }
            }
        }
        Expr::Or(left, right) => {
            let a = truth(&interpret(left, batch, row)?);
            if a == Some(true) {
                Datum::Int(1)
            } else {
                let b = truth(&interpret(right, batch, row)?);
                match (a, b) {
                    (_, Some(true)) => Datum::Int(1),
                    (Some(false), Some(false)) => Datum::Int(0),
                    _ => Datum::Null,
                }
            }
        }
        Expr::Not(inner) => match truth(&interpret(inner, batch, row)?) {
            Some(true) => Datum::Int(0),
            Some(false) => Datum::Int(1),
            None => Datum::Null,
        },
        Expr::IsNull(inner) => Datum::Int(i64::from(interpret(inner, batch, row)?.is_null())),
        Expr::IsNotNull(inner) => Datum::Int(i64::from(!interpret(inner, batch, row)?.is_null())),
        Expr::Length(inner) => match interpret(inner, batch, row)? {
            Datum::Null => Datum::Null,
            Datum::Text(bytes) => {
                Datum::Int(bytes.iter().filter(|byte| (**byte & 0xC0) != 0x80).count() as i64)
            }
            Datum::Blob(bytes) => Datum::Int(bytes.len() as i64),
            Datum::Int(number) => Datum::Int(number.to_string().len() as i64),
            Datum::Real(number) => {
                Datum::Int(inillucent_exec::expr::format_real(number).len() as i64)
            }
        },
        // The measurement is over the Phase 1 predicate set, which is what
        // `predicates` builds. Every other variant is unreachable from it and
        // is refused rather than approximated, so a predicate added to the
        // sweep later fails loudly instead of being measured against an arm
        // that does not implement it.
        _ => {
            return Err(inillucent_base::error::misuse(
                "the interpreted arm covers the Phase 1 predicate set only",
            ))
        }
    })
}
