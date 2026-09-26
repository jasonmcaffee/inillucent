//! The `matrix_deep` tier of `inillucent-compat`'s integration tests: the SQL
//! statement matrix at every merge: Layer 1 and every pair of axis values at every arm, and every triple at the default arm.
//!
//! Invariant: **this file declares one module per statement family and nothing
//! else, and each module is one target in `tests/selection.toml`, run by
//! `inillucent-testrun` in a process of its own.** The design is
//! `tasks/task-2135-sql-statement-matrix-tdd.md`; section 8 says what each
//! cadence runs and why.

mod compound;
mod constraint;
mod cte;
mod ddl_index;
mod ddl_table;
mod ddl_view;
mod delete;
mod expression;
mod function;
mod insert;
mod join;
mod maintenance;
mod pragma;
mod retained;
mod schema;
mod select;
mod subquery;
mod surfaces;
mod transaction;
mod trigger;
mod update;
mod vector;
mod vtab;
mod window;
