//! The two lists of cases that are allowed to disagree: `known.list` for
//! defects, and `deliberate.toml` for differences that are not defects.
//!
//! Invariant: **a listed case that still disagrees passes, a listed case that
//! now agrees fails, and a listed id that no case has fails.** The second and
//! third halves are what make the first worth having. A list that only grows
//! is a list of things nobody takes off it, and a fixed defect left listed
//! reads as coverage and is not. This is the two way check
//! `differential_part8.rs` made on its `allow.list`, over every case in the
//! matrix instead of one corpus.
//!
//! A `known.list` line is the case id, a tab, the bug number on task-2136
//! ("Inillucent Test Suite Bugs Found"), a tab, and one sentence. A
//! `deliberate.toml` rule is matched by construct rather than by case id,
//! because a difference such as `DELETE ... LIMIT`, which the pinned SQLite is
//! built without, would otherwise need a line for every generated case that
//! happens to use it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::statement_matrix::grade::{Failure, KINDS};

/// One `known.list` line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Known {
    /// The bug's number on task-2136, or the word the old allow list used.
    pub bug: String,
    /// The arms the defect shows at, from a bug field written `65@small-pool,sqlite-page`;
    /// empty is every arm. A line is stale only when the case agreed at one of these.
    pub arms: Vec<String>,
    /// Why the case is listed.
    pub reason: String,
}

/// Reads `known.list`.
///
/// @param path - the file
pub fn read_known(path: &Path) -> Result<BTreeMap<String, Known>, String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Ok(BTreeMap::new());
    };
    let mut out = BTreeMap::new();
    for (number, line) in text.lines().enumerate() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let mut fields = line.splitn(3, '\t');
        let id = fields.next().unwrap_or("").trim().to_string();
        let written = fields.next().unwrap_or("").trim();
        let (bug, arms) = match written.split_once('@') {
            Some((number, arms)) => (
                number.to_string(),
                arms.split(',').map(|arm| arm.trim().to_string()).collect(),
            ),
            None => (written.to_string(), Vec::new()),
        };
        let reason = fields.next().unwrap_or("").trim().to_string();
        if id.is_empty() || bug.is_empty() || reason.is_empty() {
            return Err(format!(
                "{}:{}: a line needs the case id, a tab, the bug number, a tab, and a sentence",
                path.display(),
                number.saturating_add(1)
            ));
        }
        if out
            .insert(id.clone(), Known { bug, arms, reason })
            .is_some()
        {
            return Err(format!("{}: `{id}` is listed twice", path.display()));
        }
    }
    Ok(out)
}

/// One `deliberate.toml` rule.
#[derive(Clone, Debug)]
pub struct Deliberate {
    /// The rule's name.
    pub name: String,
    /// Every one of these must appear in the statement, ignoring case. A rule
    /// needs this, or a `detail`, or both.
    pub contains: Vec<String>,
    /// Which difference kinds the rule covers; empty is every kind.
    pub kinds: Vec<String>,
    /// Text the difference's description must hold, such as the error one
    /// engine gave; empty matches any.
    pub detail: String,
    /// Case ids the rule is limited to; empty is every case. Only for an
    /// answer that is not a construct but a draw, such as the rowid SQLite
    /// picks at random once the largest one is taken.
    pub cases: Vec<String>,
    /// Why this is not a defect.
    pub reason: String,
}

impl Deliberate {
    /// Whether the rule covers a failure.
    ///
    /// @param failure - the difference
    pub fn covers(&self, failure: &Failure) -> bool {
        let upper = failure.sql.to_ascii_uppercase();
        let kind = format!("{:?}", failure.kind).to_ascii_lowercase();
        (!self.contains.is_empty() || !self.detail.is_empty() || !self.cases.is_empty())
            && self
                .contains
                .iter()
                .all(|needle| upper.contains(&needle.to_ascii_uppercase()))
            && (self.kinds.is_empty() || self.kinds.iter().any(|wanted| *wanted == kind))
            && (self.detail.is_empty() || failure.detail.contains(&self.detail))
            && (self.cases.is_empty() || self.cases.iter().any(|id| *id == failure.case))
    }
}

/// Reads `deliberate.toml`.
///
/// @param path - the file
pub fn read_deliberate(path: &Path) -> Result<Vec<Deliberate>, String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Ok(Vec::new());
    };
    let document = crate::toml_lite::parse(&text)?;
    let mut rules = Vec::new();
    for table in document.array("rule") {
        let list = |key: &str| -> Vec<String> {
            table
                .get(key)
                .and_then(|value| value.as_list())
                .map(|items| items.to_vec())
                .unwrap_or_default()
        };
        let text = |key: &str| -> Result<String, String> {
            table
                .get(key)
                .and_then(|value| value.as_str())
                .map(str::to_string)
                .ok_or_else(|| format!("{}: a rule has no `{key}`", path.display()))
        };
        let name = text("name")?;
        let reason = text("reason")?;
        let contains = list("contains");
        if contains.is_empty() && text("detail").is_err() && list("cases").is_empty() {
            return Err(format!(
                "{}: rule `{name}` has no `contains` and no `detail`, so it would cover every                  difference",
                path.display()
            ));
        }
        let kinds = list("kinds");
        if let Some(unknown) = kinds.iter().find(|kind| !is_kind(kind)) {
            return Err(format!(
                "{}: rule `{name}` names no kind `{unknown}`",
                path.display()
            ));
        }
        rules.push(Deliberate {
            name,
            contains,
            kinds,
            detail: text("detail").unwrap_or_default(),
            cases: list("cases"),
            reason,
        });
    }
    Ok(rules)
}

/// The directory the matrix corpora live in.
pub fn corpus_root() -> PathBuf {
    crate::workspace_root().join("crates/inillucent-compat/tests/corpora/matrix")
}

/// How the lists judged one case over every arm it ran at.
#[derive(Clone, Debug)]
pub enum Judged {
    /// It agreed and is not listed.
    Pass,
    /// It disagreed and is listed, or every difference is deliberate.
    Expected,
    /// It disagreed and is not listed.
    Fail(Vec<Failure>),
    /// It is listed and agreed at every arm: the line must come off.
    Stale(Known),
}

/// Judges one case from its failures at every arm it ran at.
///
/// @param id - the case id
/// @param failures - every failure, at every arm; empty when it agreed
/// @param ran_at - the arms it ran at; empty when it was skipped everywhere
/// @param known - `known.list`
/// @param deliberate - `deliberate.toml`
pub fn judge(
    id: &str,
    failures: Vec<Failure>,
    ran_at: &[String],
    known: &BTreeMap<String, Known>,
    deliberate: &[Deliberate],
) -> Judged {
    let remaining: Vec<Failure> = failures
        .into_iter()
        .filter(|failure| !deliberate.iter().any(|rule| rule.covers(failure)))
        .collect();
    match (remaining.is_empty(), known.get(id)) {
        // Stale only where the defect was said to show: a line for a failure
        // at one arm says nothing about a cadence that never ran that arm.
        (true, Some(listed))
            if ran_at
                .iter()
                .any(|arm| listed.arms.is_empty() || listed.arms.contains(arm)) =>
        {
            Judged::Stale(listed.clone())
        }
        (true, _) => Judged::Pass,
        (false, Some(_)) => Judged::Expected,
        (false, None) => Judged::Fail(remaining),
    }
}

/// Whether a kind is one a deliberate rule may name.
///
/// @param word - the lower case kind name
pub fn is_kind(word: &str) -> bool {
    KINDS
        .iter()
        .any(|kind| format!("{kind:?}").to_ascii_lowercase() == word)
}
