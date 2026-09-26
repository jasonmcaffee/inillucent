//! Proving every statement form the engine accepts has a matrix case.
//!
//! Invariant: nothing here yet beyond the family list; section 9.1 lands in
//! phase 2.

/// Every statement family, in the order section 4.1 of the design lists them,
/// plus `retained`, the shrunk failures.
pub const FAMILIES: &[&str] = &[
    "select",
    "join",
    "compound",
    "cte",
    "window",
    "subquery",
    "expression",
    "function",
    "insert",
    "update",
    "delete",
    "ddl_table",
    "ddl_index",
    "ddl_view",
    "trigger",
    "constraint",
    "transaction",
    "vtab",
    "schema",
    "maintenance",
    "pragma",
    "vector",
    "retained",
];

/// The inventory report.
#[derive(Clone, Debug, Default)]
pub struct Report {
    /// The whole report, as Markdown.
    pub markdown: String,
    /// One line.
    pub summary: String,
    /// Everything that has no case.
    pub missing: Vec<String>,
}

/// Builds the report.
pub fn report() -> Result<Report, String> {
    Ok(Report::default())
}
