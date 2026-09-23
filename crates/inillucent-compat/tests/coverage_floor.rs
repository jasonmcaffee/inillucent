//! The measured coverage of each crate only goes up.
//!
//! Invariant: **a crate's coverage is a number somebody has to be told about
//! when it falls.** `docs/repository.md` carries a table of what
//! `tools/coverage.mjs` measured, it is refreshed by an hour-long run somebody
//! does on purpose, and until this file existed nothing read it. A refresh that
//! moved `inillucent-exec` from 92% to 60% would have been a diff nobody
//! queried, because the table is thirty lines of numbers and the eye slides off
//! it.
//!
//! ## What it grades, and what it cannot
//!
//! It grades the *published* table, not the live code, so it fails when the
//! table is refreshed rather than when a commit drops coverage. That is the
//! only thing it can do: measuring the real number takes about an hour, which
//! is not something a test suite does. What it buys is that the hour-long run
//! ends in a pass or a failure rather than in a diff.
//!
//! ## Why the floors sit a point under what was measured
//!
//! The suite set is not the same on every machine. The pinned SQLite oracle,
//! the Nikaya checkout, a live PostgreSQL and the gate fixtures are each absent
//! somewhere, and every suite that skips takes its share of the lines with it.
//! A floor pinned to the third digit of one machine's number would be a red
//! build on the next machine rather than a fact about the code. A point is
//! wider than that noise and far narrower than the fall this is for.
//!
//! The band above the floor is the other half of the ratchet, and it is the
//! half that keeps the floor honest: a floor four points below the measurement
//! cannot fail, so the test says to raise it. That is the same shape
//! `policy.rs` uses for module size.

use inillucent_compat::workspace_root;

/// The lowest region and line coverage each crate may publish.
///
/// One row per row of the table in `docs/repository.md`, and
/// `every_published_crate_has_a_floor` fails when the two lists stop matching -
/// a crate added to the workspace and not to this list would otherwise publish
/// any number at all.
///
/// Read as: crate, region coverage floor, line coverage floor.
const FLOORS: &[(&str, f64, f64)] = &[
    ("inillucent-compat", 39.6, 41.3),
    ("inillucent-exec", 89.3, 91.0),
    ("inillucent-sql", 85.1, 87.2),
    ("inillucent-engine", 86.3, 87.9),
    ("inillucent-tree", 91.0, 92.8),
    ("inillucent-cli", 47.4, 49.1),
    ("inillucent-storage", 81.9, 81.8),
    ("inillucent-scalar", 83.8, 83.9),
    ("inillucent-ext", 82.8, 82.7),
    ("inillucent-remote", 79.8, 77.5),
    ("inillucent-pool", 92.4, 93.3),
    ("inillucent-search", 81.0, 81.4),
    ("inillucent-transaction", 84.3, 87.5),
    ("inillucent-value", 94.4, 94.6),
    ("inillucent-vfs", 79.3, 80.1),
    ("inillucent-migrate", 76.5, 78.4),
    ("inillucent-base", 87.7, 88.0),
    ("inillucent-catalog", 77.2, 79.4),
    ("inillucent-wal", 92.7, 95.8),
    ("inillucent-txn", 88.8, 87.0),
    ("inillucent-sim", 94.0, 94.4),
    ("inillucent-driver", 64.1, 63.6),
    // **The one floor that is zero, and it is honest about being zero.** The C
    // API is exercised by `drivers/c/tests`, a C harness cargo does not build
    // and llvm-cov therefore never sees, so the 0.9% published here is what the
    // Rust tests happen to touch and not what the binding is worth. A floor
    // above zero would be a number about the harness rather than about the
    // code. The band check is what grades this row: it fires the moment the
    // figure climbs, which is what would happen if the binding gained Rust
    // tests.
    ("inillucent-driver-capi", 0.0, 0.0),
    ("inillucent-sqlite-reader", 85.6, 88.9),
    ("inillucent-alloc", 90.3, 83.8),
    ("total", 76.2, 76.8),
];

/// How far above its floor a crate may sit before the floor is asked to move.
///
/// Four points. Wide enough that ordinary work does not send somebody back
/// here, narrow enough that a floor cannot quietly become unfailable.
const BAND: f64 = 4.0;

/// The published table, as pairs of crate name and its two percentages.
///
/// The total row is read too, under the name `total`, because a workspace whose
/// crates all held their floors while the total fell would mean a large new
/// crate arrived uncovered.
///
/// @param page - the whole of `docs/repository.md`
fn published(page: &str) -> Vec<(String, f64, f64)> {
    let Some(after) = page.split("<!-- coverage:begin -->").nth(1) else {
        panic!("docs/repository.md has no <!-- coverage:begin --> marker");
    };
    let Some(body) = after.split("<!-- coverage:end -->").next() else {
        panic!("docs/repository.md has no <!-- coverage:end --> marker");
    };

    let mut found = Vec::new();
    for line in body.lines() {
        let cells: Vec<&str> = line.trim().trim_matches('|').split('|').collect();
        if cells.len() != 5 {
            continue;
        }
        let name = cells[0]
            .trim()
            .trim_matches('*')
            .trim_matches('`')
            .to_string();
        let (Some(regions), Some(lines)) = (percent(cells[2]), percent(cells[4])) else {
            continue;
        };
        found.push((name, regions, lines));
    }
    found
}

/// A percentage out of one table cell, or nothing if the cell is not one.
///
/// @param cell - the cell's text, with its surrounding spaces
fn percent(cell: &str) -> Option<f64> {
    cell.trim()
        .trim_matches('*')
        .strip_suffix('%')
        .and_then(|number| number.parse::<f64>().ok())
}

/// The floor for one crate.
///
/// @param crate_name - the name as the table spells it
fn floor_of(crate_name: &str) -> Option<(f64, f64)> {
    FLOORS
        .iter()
        .find(|(name, _, _)| *name == crate_name)
        .map(|(_, regions, lines)| (*regions, *lines))
}

/// **No crate publishes less coverage than its floor.**
///
/// The failure names the crate, both numbers and the difference, because the
/// person reading it is looking at an hour-long run that has just finished and
/// needs to know whether to investigate or to raise a floor.
#[test]
fn no_crate_publishes_less_than_its_floor() {
    let page = read_the_page();
    let mut fell = Vec::new();
    for (name, regions, lines) in published(&page) {
        let Some((region_floor, line_floor)) = floor_of(&name) else {
            continue;
        };
        if regions < region_floor {
            fell.push(format!(
                "{name} publishes {regions:.1}% of regions and its floor is {region_floor:.1}%"
            ));
        }
        if lines < line_floor {
            fell.push(format!(
                "{name} publishes {lines:.1}% of lines and its floor is {line_floor:.1}%"
            ));
        }
    }
    assert!(
        fell.is_empty(),
        "coverage fell below what docs/repository.md's table had recorded. Either the \
         change that did it needs tests, or the measurement ran a smaller set of suites \
         than the one the floor was set from - `target/debug/inillucent-testrun --strict` \
         names every suite whose prerequisite was missing.\n{}",
        fell.join("\n")
    );
}

/// **A floor a crate has climbed four points above is raised.**
///
/// A floor that cannot fail is the thing §1.2 of the testing standard is about,
/// and a floor left behind by four points of real work is exactly that. The
/// failure says what to write.
#[test]
fn a_floor_that_has_been_left_behind_is_raised() {
    let page = read_the_page();
    let mut behind = Vec::new();
    for (name, regions, lines) in published(&page) {
        let Some((region_floor, line_floor)) = floor_of(&name) else {
            continue;
        };
        if regions - region_floor > BAND || lines - line_floor > BAND {
            behind.push(format!(
                "    (\"{name}\", {:>5.1}, {:>5.1}),   // was {region_floor:.1}, {line_floor:.1}",
                regions - 1.0,
                lines - 1.0
            ));
        }
    }
    assert!(
        behind.is_empty(),
        "these crates are more than {BAND:.0} points above their floors, so the floors no \
         longer say anything. Replace their rows in FLOORS with these:\n{}",
        behind.join("\n")
    );
}

/// **Every crate the table publishes has a floor, and every floor a crate.**
///
/// This is what stops the two lists drifting apart. A crate added to the
/// workspace arrives in the table on the next measurement, and without this it
/// would arrive with no floor and be graded by nothing; a crate removed leaves
/// a floor behind that grades nothing and reads as coverage.
#[test]
fn every_published_crate_has_a_floor() {
    let page = read_the_page();
    let table = published(&page);
    assert!(
        table.len() > 20,
        "only {} rows were read out of the coverage table, so the table's shape has \
         changed and this file is grading almost nothing",
        table.len()
    );

    let missing: Vec<&String> = table
        .iter()
        .filter(|(name, _, _)| floor_of(name).is_none())
        .map(|(name, _, _)| name)
        .collect();
    assert!(
        missing.is_empty(),
        "these crates are in the published table and have no floor here: {missing:?}"
    );

    let stale: Vec<&str> = FLOORS
        .iter()
        .map(|(name, _, _)| *name)
        .filter(|name| !table.iter().any(|(published, _, _)| published == name))
        .collect();
    assert!(
        stale.is_empty(),
        "these floors name a crate the published table does not: {stale:?}"
    );
}

/// The page, read from the checkout rather than from a copy.
fn read_the_page() -> String {
    let path = workspace_root().join("docs/repository.md");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|why| panic!("{} could not be read: {why}", path.display()))
}
