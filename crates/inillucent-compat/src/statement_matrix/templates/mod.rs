//! Layer 2: interaction cases, generated from covering arrays over each
//! family's axes (section 5.2 of the design).
//!
//! Invariant: **every generated case comes from one row of a covering array
//! that [`cover::build`] has checked holds every allowed pair (on a change)
//! or triple (on a merge) of the family's axis values, and its id is a hash of
//! its text.** A template is a function from one row to one case; it never
//! decides which combinations exist. That is the covering array's job, so a
//! combination cannot be forgotten by the person writing the template.
//!
//! A family declares its axes, a constraint over a partial row, and a builder.
//! The shared axes of section 4.2 (source, access, placement, data, affinity,
//! binding, transaction) are built by [`scene`] and [`frame`], so every family
//! that takes an axis means the same thing by each of its values.
//!
//! Generation is memoised per family and strength for the life of the
//! process: the eight groups of a family run as threads of one process, and
//! each would otherwise build the same arrays.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, OnceLock};

use crate::statement_matrix::case::Case;
use crate::statement_matrix::cover::{self, Partial};

pub mod frame;
pub mod scene;

mod constraint;
mod ddl;
mod dml;
mod maintenance;
mod pragma;
mod reads;
mod schema;
mod transaction;
mod trigger;
mod vector;
mod vtab;
mod window;

/// One axis: its name and its values.
#[derive(Clone, Debug)]
pub struct Axis {
    /// The name the constraint and the builder use.
    pub name: &'static str,
    /// The values, in the order the array numbers them.
    pub values: Vec<&'static str>,
}

impl Axis {
    /// An axis from a name and a list of values.
    ///
    /// @param name - the axis name
    /// @param values - its values
    pub fn new(name: &'static str, values: &[&'static str]) -> Axis {
        Axis {
            name,
            values: values.to_vec(),
        }
    }
}

/// A row of the array as the constraint and the builder see it: axis values
/// by name. In a constraint, an axis the builder has not chosen yet is absent.
#[derive(Clone, Debug)]
pub struct Pick<'a> {
    axes: &'a [Axis],
    row: &'a Partial,
}

impl Pick<'_> {
    /// The value of an axis, or `None` when it is not chosen yet.
    ///
    /// @param name - the axis
    pub fn get(&self, name: &str) -> Option<&'static str> {
        let position = self.axes.iter().position(|axis| axis.name == name)?;
        let chosen = self.row.get(position).copied().flatten()?;
        self.axes.get(position)?.values.get(chosen).copied()
    }

    /// The value of an axis, or the empty string when it is not chosen.
    ///
    /// @param name - the axis
    pub fn value(&self, name: &str) -> &'static str {
        self.get(name).unwrap_or("")
    }

    /// Whether two axes are both chosen and the pair is one of `pairs`, which
    /// is how most constraints are written.
    ///
    /// @param first - one axis
    /// @param second - another
    /// @param pairs - the forbidden value pairs; `*` matches any value
    pub fn forbids(&self, first: &str, second: &str, pairs: &[(&str, &str)]) -> bool {
        let (Some(left), Some(right)) = (self.get(first), self.get(second)) else {
            return false;
        };
        pairs
            .iter()
            .any(|(one, two)| (*one == "*" || *one == left) && (*two == "*" || *two == right))
    }
}

/// One statement family's template.
pub struct Family {
    /// The family name, which is also its test module.
    pub name: &'static str,
    /// Its axes.
    pub axes: Vec<Axis>,
    /// Whether a partial row may exist.
    pub allowed: fn(&Pick) -> bool,
    /// The case for one full row, or `None` for a row that makes no case.
    pub build: fn(&Pick) -> Option<Case>,
}

/// Every family with a template, by name.
pub fn families() -> Vec<Family> {
    let mut all = Vec::new();
    all.extend(reads::families());
    all.push(window::family());
    all.extend(dml::families());
    all.extend(ddl::families());
    all.push(trigger::family());
    all.push(constraint::family());
    all.push(transaction::family());
    all.push(vtab::family());
    all.push(schema::family());
    all.push(maintenance::family());
    all.push(pragma::family());
    all.push(vector::family());
    all
}

/// What generating one family at one strength produced.
#[derive(Clone, Debug, Default)]
pub struct Generated {
    /// The cases, unique by id, in array order.
    pub cases: Vec<Case>,
    /// Rows in the covering array.
    pub rows: usize,
    /// Combinations the constraints removed.
    pub removed: usize,
    /// Combinations the array covers.
    pub required: usize,
}

/// The memo of generated families.
fn memo() -> &'static Mutex<BTreeMap<(String, usize, bool), Result<Generated, String>>> {
    static MEMO: OnceLock<Mutex<BTreeMap<(String, usize, bool), Result<Generated, String>>>> =
        OnceLock::new();
    MEMO.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Generates one family at one strength, once per process.
///
/// @param family - the family
/// @param strength - two or three
pub fn generated(family: &str, strength: usize) -> Result<Generated, String> {
    generated_with(family, strength, false)
}

/// The axis values the change cadence leaves to the merge cadence.
///
/// **Section 8.1 of the design, applied.** Phase 3 measured the change tier at
/// 94 s of wall clock with every placement and every source in its pairs,
/// against a budget of 60. A case whose placement is a trigger body, an
/// `UPDATE ... FROM` or an `INSERT ... SELECT`, or whose source is a `TEMP`
/// table or an attached one, writes, so it cannot share a fixture, and it is
/// reopened and checked: about 90 ms of wall clock against half a
/// millisecond for a case that reads. The design says the budget is then kept
/// by running strength two over a subset of axis values on a change and the
/// full strength two on a merge, and never by dropping a family. These are
/// the values left out; every pair holding one of them is covered on every
/// merge, at every arm.
pub const MERGE_ONLY: &[(&str, &str)] = &[
    ("placement", "trigger"),
    ("placement", "update_from"),
    ("placement", "insert_select"),
    ("source", "temp"),
    ("source", "attached"),
];

/// Whether a partial row keeps to the change cadence's subset.
fn in_change_subset(pick: &Pick) -> bool {
    !MERGE_ONLY
        .iter()
        .any(|(axis, value)| pick.get(axis) == Some(*value))
}

/// The cases the change cadence runs: every pair, over the axis values that
/// are not in [`MERGE_ONLY`].
///
/// @param family - the family
pub fn generate_change(family: &str) -> Result<Vec<Case>, String> {
    Ok(generated_with(family, 2, true)?.cases)
}

/// Generates one family at one strength, over every value or over the change
/// cadence's subset, once per process.
///
/// @param family - the family
/// @param strength - two or three
/// @param subset - whether to leave out the values in [`MERGE_ONLY`]
pub fn generated_with(family: &str, strength: usize, subset: bool) -> Result<Generated, String> {
    let key = (family.to_string(), strength, subset);
    if let Ok(held) = memo().lock() {
        if let Some(found) = held.get(&key) {
            return found.clone();
        }
    }
    let result = build_family(family, strength, subset);
    if let Ok(mut held) = memo().lock() {
        held.insert(key, result.clone());
    }
    result
}

/// Builds the array and the cases for one family.
fn build_family(family: &str, strength: usize, subset: bool) -> Result<Generated, String> {
    let Some(template) = families().into_iter().find(|one| one.name == family) else {
        return Ok(Generated::default());
    };
    let sizes: Vec<usize> = template.axes.iter().map(|axis| axis.values.len()).collect();
    let axes = &template.axes;
    let allowed = |row: &Partial| {
        let pick = Pick { axes, row };
        (template.allowed)(&pick) && (!subset || in_change_subset(&pick))
    };
    let array = cover::build(&sizes, strength, &allowed)
        .map_err(|problem| format!("family {family}: {problem}"))?;
    let mut seen = BTreeSet::new();
    let mut cases = Vec::new();
    for row in &array.rows {
        let partial: Partial = row.iter().map(|value| Some(*value)).collect();
        let Some(mut case) = (template.build)(&Pick {
            axes,
            row: &partial,
        }) else {
            continue;
        };
        case.family = family.to_string();
        case.origin = format!(
            "template {family}, strength {strength}, row {}",
            describe(axes, row)
        );
        case.assign_id();
        if seen.insert(case.id.clone()) {
            cases.push(case);
        }
    }
    Ok(Generated {
        cases,
        rows: array.rows.len(),
        removed: array.removed,
        required: array.required,
    })
}

/// Names a row's values, for a case's origin.
fn describe(axes: &[Axis], row: &[usize]) -> String {
    axes.iter()
        .zip(row.iter())
        .map(|(axis, value)| {
            format!(
                "{}={}",
                axis.name,
                axis.values.get(*value).copied().unwrap_or("?")
            )
        })
        .collect::<Vec<String>>()
        .join(" ")
}

/// The generated cases of a family at a strength.
///
/// @param family - the family
/// @param strength - two or three
pub fn generate(family: &str, strength: usize) -> Result<Vec<Case>, String> {
    Ok(generated(family, strength)?.cases)
}

/// The strength three cases that strength two does not already produce.
///
/// @param family - the family
pub fn generate_only_triples(family: &str) -> Result<Vec<Case>, String> {
    let pairs: BTreeSet<String> = generate(family, 2)?
        .into_iter()
        .map(|case| case.id)
        .collect();
    Ok(generate(family, 3)?
        .into_iter()
        .filter(|case| !pairs.contains(&case.id))
        .collect())
}

/// The `counts.toml` text for every family: rows, cases, and the combinations
/// the constraints removed, at each strength.
pub fn counts_toml() -> Result<String, String> {
    let mut out = String::from(
        "# How many cases each statement matrix family generates, and how many axis\n\
         # combinations its constraints remove, at each strength.\n\
         #\n\
         # Written by `inillucent-matrix counts` and checked by\n\
         # `tooling::matrix_inventory`. The matrix's time budget is kept by these\n\
         # numbers rather than by a clock: a change that grows a family has to change\n\
         # this file, which is where somebody decides whether the time is worth it.\n\
         # (tasks/task-2135-sql-statement-matrix-tdd.md, sections 8.3 and 9.2)\n",
    );
    for family in families() {
        for (strength, subset, cadence) in [
            (2usize, true, "change"),
            (2, false, "merge"),
            (3, false, "merge"),
        ] {
            let made = generated_with(family.name, strength, subset)?;
            out.push_str(&format!(
                "\n[[count]]\nfamily = \"{}\"\ncadence = \"{cadence}\"\nstrength = {strength}\nrows = {}\ncases = {}\nrequired = {}\nremoved = {}\n",
                family.name,
                made.rows,
                made.cases.len(),
                made.required,
                made.removed
            ));
        }
    }
    Ok(out)
}
