//! The `matrix` tier of `inillucent-compat`'s integration tests: the SQL
//! statement matrix at every change: Layer 1 at the default arm, every pair of axis values at the default arm with the Layer 3 wrappings and properties, and the retained corpus at every arm.
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
mod lists;
mod maintenance;
mod pragma;
mod retained;
mod schema;
mod select;
mod subquery;
mod transaction;
mod trigger;
mod update;
mod vector;
mod vtab;
mod window;
