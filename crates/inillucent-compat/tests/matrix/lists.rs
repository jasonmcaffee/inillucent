//! The lists the statement matrix grades against, checked on their own.
//!
//! Invariant: **every `known.list` line names a case some cadence runs, every
//! case id is unique, every case file parses, and every `deliberate.toml` rule
//! names only kinds that exist.** A line for a case that was renamed or removed
//! is a rule about nothing, and it would quietly stop the check that a fixed
//! defect comes off the list. The family groups cannot see this, because each
//! sees only its own share of the cases.

use std::collections::BTreeMap;

use inillucent_compat::statement_matrix::group::{work, Cadence};
use inillucent_compat::statement_matrix::inventory::FAMILIES;
use inillucent_compat::statement_matrix::known::{corpus_root, read_deliberate, read_known};

/// Every case id at every cadence, with the family it came from.
fn every_case_id() -> BTreeMap<String, Vec<String>> {
    let mut ids: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for family in FAMILIES {
        for cadence in [Cadence::Change, Cadence::Merge, Cadence::Nightly] {
            let found = work(family, cadence).unwrap_or_else(|problem| panic!("{problem}"));
            for (case, _) in found.runs {
                let origins = ids.entry(case.id.clone()).or_default();
                if !origins.contains(&case.origin) {
                    origins.push(case.origin.clone());
                }
            }
        }
    }
    ids
}

/// Every `known.list` line names a case that exists.
#[test]
fn the_known_list_names_only_cases_that_exist() {
    let known =
        read_known(&corpus_root().join("known.list")).unwrap_or_else(|problem| panic!("{problem}"));
    let ids = every_case_id();
    let orphans: Vec<&String> = known.keys().filter(|id| !ids.contains_key(*id)).collect();
    assert!(
        orphans.is_empty(),
        "these ids are in known.list and no cadence runs a case with that id. A renamed or \
         removed case left its line behind; take the line off:\n  {}",
        orphans
            .iter()
            .map(|id| id.as_str())
            .collect::<Vec<&str>>()
            .join("\n  ")
    );
    assert!(ids.len() > 1_000, "only {} case ids were found", ids.len());
}

/// No two different cases share an id. A generated id that collided with a
/// hand written one would make a `known.list` line cover both.
#[test]
fn case_ids_are_unique() {
    let mut seen: BTreeMap<String, String> = BTreeMap::new();
    let mut clashes = Vec::new();
    for family in FAMILIES {
        let mut in_family: BTreeMap<String, usize> = BTreeMap::new();
        for (case, _) in work(family, Cadence::Merge)
            .unwrap_or_else(|problem| panic!("{problem}"))
            .runs
        {
            *in_family.entry(case.id.clone()).or_default() += 1;
            if let Some(other) = seen.insert(case.id.clone(), family.to_string()) {
                if other != *family {
                    clashes.push(format!("{} in {other} and {family}", case.id));
                }
            }
        }
        for (id, count) in in_family {
            // A merge cadence runs strength two and the triples strength two
            // did not already produce, so a repeated id inside one family is a
            // case written twice.
            if count > 1 {
                clashes.push(format!("{id} appears {count} times in {family}"));
            }
        }
    }
    assert!(clashes.is_empty(), "{}", clashes.join("\n"));
}

/// `deliberate.toml` parses, and every rule names kinds that exist.
#[test]
fn the_deliberate_rules_parse() {
    let rules = read_deliberate(&corpus_root().join("deliberate.toml"))
        .unwrap_or_else(|problem| panic!("{problem}"));
    assert!(!rules.is_empty(), "deliberate.toml has no rules");
    for rule in &rules {
        assert!(
            !rule.reason.trim().is_empty(),
            "rule {} gives no reason",
            rule.name
        );
    }
}
