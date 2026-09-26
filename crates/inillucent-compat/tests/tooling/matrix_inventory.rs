//! Every statement form the engine accepts has a statement matrix case.
//!
//! Invariant: **`statement_matrix::inventory::report` finds nothing missing,
//! and `corpora/matrix/counts.toml` is what the generators make today.** The
//! report walks every case the change cadence runs with the engine's own
//! parser and reads the function, pragma, module and collation lists from a
//! live database, so a new variant, function or pragma without a case fails
//! here. The report is written to `_agent_output/matrix/inventory.md` on every
//! run so a person can read the coverage. Section 9 of
//! `tasks/task-2135-sql-statement-matrix-tdd.md` is the design.

use inillucent_compat::statement_matrix::{inventory, surfaces, templates};

/// The scratch directory the live registers are read in.
fn scratch() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("matrix-inventory")
}

#[test]
fn every_form_and_register_name_has_a_case() {
    let report = inventory::report(&scratch(), surfaces::CASES).expect("the inventory runs");
    let path = inillucent_compat::workspace_root().join("_agent_output/matrix/inventory.md");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, &report.markdown);
    assert!(
        report.missing.is_empty(),
        "{}\nno matrix case reaches these; add a Layer 1 case for each:\n  {}",
        report.summary,
        report.missing.join("\n  ")
    );
}

#[test]
fn the_walker_reports_a_form_no_case_reaches() {
    // The inventory can only fail if the walker records what a case reaches
    // and nothing else: one SELECT reaches Statement::Select and not
    // Statement::Insert.
    let mut seen = inventory::Seen::default();
    inventory::walk_sql("SELECT abs(1) FROM t", "only", &mut seen);
    assert_eq!(
        seen.forms.get("Statement::Select").map(String::as_str),
        Some("only")
    );
    assert!(!seen.forms.contains_key("Statement::Insert"));
    assert_eq!(seen.functions.get("abs").map(String::as_str), Some("only"));
    assert_eq!(seen.modules.get("t").map(String::as_str), Some("only"));
}

#[test]
fn counts_toml_is_what_the_generators_make() {
    let path = inillucent_compat::statement_matrix::known::corpus_root().join("counts.toml");
    let recorded = std::fs::read_to_string(&path).expect("counts.toml is readable");
    let made = templates::counts_toml().expect("the templates generate");
    assert!(
        recorded.replace("\r\n", "\n").trim() == made.trim(),
        "{} is not what the templates generate. A change that grows or shrinks a family \
         rewrites it with `inillucent-matrix counts > {}`, and the diff is where somebody \
         decides whether the time is worth it.",
        path.display(),
        path.display()
    );
}
