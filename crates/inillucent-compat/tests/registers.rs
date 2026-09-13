//! The **enumeration** surface, compared against the pinned SQLite 3.53.4.
//!
//! Invariant: the four registers a caller can introspect - functions, modules,
//! pragmas and collations - report the same names on both engines, and every
//! name that does not match is written down here with the reason.
//!
//! ## Why a whole suite for four lists
//!
//! Every other differential test in this workspace probes by **calling**: it
//! runs a statement on both engines and compares the answers. That shape cannot
//! find a register that under-reports, because a register that under-reports
//! answers every call correctly - it just does not admit that it can.
//!
//! An audit found exactly that, by enumerating rather than calling:
//! `pragma_function_list` answered **161** names where SQLite answers **218**,
//! and `pragma_module_list` **14** where SQLite answers **19**, while the
//! functionality behind most of the difference was present and byte-identical.
//! `dbstat`, `sqlite_dbpage`, `sqlite_stmt`, `bytecode`, `tables_used`,
//! `completion`, `matchinfo`, `offsets`, `regexp`, `unistr`, `median` and the
//! rest all answered when they were called; they were simply not named. An
//! application that reads a register to decide what it may use was being told
//! less than the truth, with **no error** - which made it the one silent
//! difference this project had.
//!
//! This suite is what makes that class of gap fail a build rather than wait for
//! the next audit. It is deliberately written as an exclusion list: a name that
//! differs has to be *named here*, with why, or the test fails.

use inillucent_compat::differential::{compare, Step};

/// Where this suite's scratch databases live.
const AREA: &str = "registers";

/// Functions this engine has and the pinned **library** does not.
///
/// **The oracle is the library, and that is the right basis for this test.**
/// The rest of this workspace's differential suites drive
/// `compat/oracle/sqlite_driver.c` linked against the pinned amalgamation, not
/// `sqlite3.exe`, and the two registers are not the same: the shell registers
/// forty-one convenience functions of its own - `base64`, `sha3`, `ieee754*`,
/// `readfile`, `shell_*` and the rest - that no application linking SQLite ever
/// had. Comparing against the shell would make this test demand forty-one
/// functions that are not SQLite's; comparing against the library asks the
/// question an embedder actually cares about. The shell-level tally is in
/// `docs/feature-comparison.md`, where it belongs, and is a fact about two command
/// line programs.
///
/// What is left, then, is names **this** engine has and the library does not,
/// which is the harmless direction and is listed so a new one has to be added
/// here deliberately: the retrieval engine's vector surface, the geopoly and
/// sqlar surfaces this build compiles in, FTS3/4's auxiliary functions, the
/// percentile family, and `regexp`/`unknown`/`sqlite_offset`, which the pinned
/// build does not compile.
const OURS_ONLY_FUNCTIONS: &[&str] = &[
    "binary_quantize",
    "cosine_distance",
    "geopoly_area",
    "geopoly_bbox",
    "geopoly_blob",
    "geopoly_ccw",
    "geopoly_contains_point",
    "geopoly_debug",
    "geopoly_group_bbox",
    "geopoly_json",
    "geopoly_overlap",
    "geopoly_regular",
    "geopoly_svg",
    "geopoly_within",
    "geopoly_xform",
    "hamming_distance",
    "inner_product",
    "jaccard_distance",
    "l1_distance",
    "l2_distance",
    "l2_normalize",
    "matchinfo",
    "median",
    "offsets",
    "optimize",
    "percentile",
    "percentile_cont",
    "percentile_disc",
    "regexp",
    "sqlar_compress",
    "sqlar_uncompress",
    "sqlite_offset",
    "subvector",
    "unknown",
    "vector_add",
    "vector_concat",
    "vector_dims",
    "vector_distance_cos",
    "vector_distance_l2",
    "vector_dot",
    "vector_mul",
    "vector_norm",
    "vector_sub",
];

/// Library functions the reference has and this engine does not.
///
/// **Four, and each one hands out something this engine has no equivalent of.**
/// `fts5(t)` returns a pointer to the `fts5_api` structure - a C address for a
/// caller that will call through it, and there is no C API here to point at.
/// `fts5_locale`, `fts5_get_locale` and `fts5_insttoken` belong to FTS5's
/// locale and instance-token machinery, which this index does not implement; a
/// stub that answered them would be a wrong answer rather than a missing one,
/// and this project treats those as the more serious of the two.
///
/// It was six before `fts5_source_id()` and `optimize()` were implemented -
/// both have faithful answers here - and the fifty-one functions that were
/// present and unlisted were named. `fts3_tokenizer` is the sixth and
/// is absent from the pinned *library* too, so it is a difference against the
/// shell only and is recorded in `docs/feature-comparison.md`.
const STILL_ABSENT: &[&str] = &["fts5", "fts5_get_locale", "fts5_insttoken", "fts5_locale"];

/// Modules this engine has and the pinned library does not register.
///
/// The library registers `json_each`, `json_tree`, `geopoly` and the rest
/// lazily, so a register nobody has provoked does not name them - which is the
/// same under-report this suite exists to catch, on the other side of the
/// comparison. `inillucent_search` and `ivfflat` are the retrieval engine's,
/// and the six engine-answered eponymous tables are this engine naming what it
/// can do.
const OURS_ONLY_MODULES: &[&str] = &[
    "bytecode",
    "completion",
    "fsdir",
    "fts3",
    "fts4",
    "generate_series",
    "geopoly",
    "inillucent_search",
    "ivfflat",
    "json_each",
    "json_tree",
    "sqlite_dbpage",
    "sqlite_stmt",
    "tables_used",
    "zipfile",
];

/// Collations this engine has and the pinned library does not register.
///
/// `decimal` and `uint` are the reference **CLI's** bundled extensions -
/// `decimal` compares two numeric strings by value rather than by bytes, and
/// `uint` compares a string of digits by magnitude - and this engine registers
/// both too, documented in `docs/feature-comparison.md`'s "Collations - 5 of
/// 5" section. The pinned library the oracle links against is the bare
/// `sqlite3.c` amalgamation, which never loads a shell extension, so the two
/// names are recorded here rather than asserted away.
const OURS_ONLY_COLLATIONS: &[&str] = &["decimal", "uint"];

/// Modules the pinned library has and this engine does not.
///
/// **One, and it is about which front-end this suite drives.** The harness
/// opens a `SessionDatabase`, which is the driver front-end; `dbstat` is
/// answered by the new engine's own eponymous-table path and reaches the shell,
/// not this one. It is listed here rather than filtered silently, because that
/// difference between two front-ends of the same build is worth being able to
/// see.
///
/// `pragma_module_list` is not a gap at all: the reference creates its `pragma`
/// module the first time one is used, so the very query that enumerates the
/// modules puts its own name in the answer.
///
/// `fts4aux` and `fts3tokenize` are the two modules the audit named against the
/// *shell*; the pinned library does not register them either, so they are not
/// in this comparison and are recorded in `docs/feature-comparison.md`.
const OURS_MISSING_MODULES: &[&str] = &["dbstat", "pragma_module_list"];

/// Asserts that one step really was compared.
///
/// Zero means the pinned oracle is not built and the harness skipped, which is
/// a skip rather than a pass everywhere else in this suite too.
///
/// @param compared - how many steps the harness compared
fn assert_one(compared: usize) {
    if compared == 0 {
        return;
    }
    assert_eq!(compared, 1, "the register was compared");
}

/// Leaks a built query so it can be a `Step`.
///
/// `Step` holds `&'static str` because every other suite's SQL is a literal.
/// These are assembled from the exclusion lists above, and one leak per test in
/// a test binary is the cheapest honest way to hand the harness a `'static`.
///
/// @param sql - the query
fn fixed(sql: String) -> &'static str {
    Box::leak(sql.into_boxed_str())
}

/// Returns a SQL list literal of quoted names.
///
/// @param names - the names to quote
fn quoted(names: &[&[&str]]) -> String {
    let mut out = String::new();
    for group in names {
        for name in *group {
            if !out.is_empty() {
                out.push(',');
            }
            out.push('\'');
            out.push_str(name);
            out.push('\'');
        }
    }
    out
}

/// Returns the query that lists one register, minus the names allowed to differ.
///
/// **`group_concat` of a sorted, de-duplicated projection**, so the comparison
/// is one string per engine and a difference names itself in the assertion
/// rather than showing up as "row 137 differs".
///
/// @param register - the pragma's name, without its `pragma_` prefix
/// @param allowed - the name groups that may differ
fn listing(register: &str, allowed: &[&[&str]]) -> String {
    format!(
        "SELECT group_concat(name, ' ') FROM \
         (SELECT DISTINCT name FROM pragma_{register} WHERE name NOT IN ({}) ORDER BY name)",
        quoted(allowed)
    )
}

/// Every function both engines have is named by both engines' registers.
///
/// This is the test that would have caught the under-reporting register
/// before it was fixed: fifty-seven names, all of them answering, none of
/// them listed.
#[test]
fn the_function_register_names_what_the_engine_answers() {
    let query = listing("function_list", &[STILL_ABSENT, OURS_ONLY_FUNCTIONS]);
    let compared = compare(AREA, "functions", &[Step::Query(fixed(query))]);
    assert_eq!(compared, 1, "the function register was compared");
}

/// The overloads agree too, not only the names.
///
/// A register that named `substr` once where the reference names it at two and
/// at three arguments would tell an application that a call will not bind when
/// it will. The arity is the column an application actually reads.
#[test]
fn the_function_register_agrees_about_arity() {
    let query = format!(
        "SELECT group_concat(row, ' ') FROM \
         (SELECT DISTINCT name || '/' || narg AS row FROM pragma_function_list \
          WHERE name NOT IN ({}) ORDER BY name, narg)",
        quoted(&[STILL_ABSENT, OURS_ONLY_FUNCTIONS])
    );
    let compared = compare(AREA, "function-arity", &[Step::Query(fixed(query))]);
    assert_eq!(compared, 1, "the function arities were compared");
}

/// Every module both engines have is named by both engines' registers.
#[test]
fn the_module_register_names_what_the_engine_answers() {
    let query = listing("module_list", &[OURS_MISSING_MODULES, OURS_ONLY_MODULES]);
    let compared = compare(AREA, "modules", &[Step::Query(fixed(query))]);
    assert_eq!(compared, 1, "the module register was compared");
}

/// The pragma register agrees exactly, with nothing excluded.
///
/// It did against the *shell* before this ticket. Against the pinned library it
/// did not: this front-end's own register was missing `checkpoint_fullfsync`,
/// `data_store_directory`, `default_cache_size`, `fullfsync` and
/// `temp_store_directory`, all five of which the engine answers and one of
/// which - `default_cache_size` - it has a whole branch for. Five more names
/// that were present and unlisted, found by enumerating rather than calling.
#[test]
fn the_pragma_register_agrees_exactly() {
    let compared = compare(
        AREA,
        "pragmas",
        &[Step::Query(
            // `defensive` is the one name this engine has that the pinned
            // library's register does not: it is a `SQLITE_DBCONFIG` flag there
            // and a pragma here, and both honour it.
            "SELECT group_concat(name, ' ') FROM \
             (SELECT DISTINCT name FROM pragma_pragma_list WHERE name <> 'defensive' ORDER BY name)",
        )],
    );
    assert_one(compared);
}

/// The collation register agrees, apart from the two names this engine
/// bundles that the pinned library does not.
#[test]
fn the_collation_register_agrees_exactly() {
    let query = listing("collation_list", &[OURS_ONLY_COLLATIONS]);
    let compared = compare(AREA, "collations", &[Step::Query(fixed(query))]);
    assert_one(compared);
}

/// The names that exist but only in a window frame say so, rather than saying
/// they do not exist.
///
/// Eleven window functions and the auxiliary functions read as twenty-three
/// missing functions in the enumeration audit, because calling one outside
/// its context answered `no such function`. Every one of them is present and
/// byte-identical when called properly, and the reference's own wording says
/// which of the two things went wrong.
#[test]
fn a_name_out_of_context_reports_the_context_and_not_an_absence() {
    let compared = compare(
        AREA,
        "out-of-context",
        &[
            Step::Query("SELECT row_number()"),
            Step::Query("SELECT rank()"),
            Step::Query("SELECT dense_rank()"),
            Step::Query("SELECT cume_dist()"),
            Step::Query("SELECT percent_rank()"),
            Step::Query("SELECT ntile(2)"),
            Step::Query("SELECT lag(1)"),
            Step::Query("SELECT lead(1)"),
            Step::Query("SELECT first_value(1)"),
            Step::Query("SELECT last_value(1)"),
            Step::Query("SELECT nth_value(1, 1)"),
        ],
    );
    assert_eq!(
        compared, 11,
        "every window name was compared out of context"
    );
}
