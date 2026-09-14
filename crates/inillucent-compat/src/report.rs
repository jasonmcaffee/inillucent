//! The compatibility report and the checks that gate it.
//!
//! Invariant: `compat-report.json` is generated, never edited, and it is the
//! release gate. Markdown is a view of the same data. A capability reaches
//! `pass` in the report only when the manifest says so *and* a recorded test
//! result on every required platform agrees.
//!
//! The four problems the generator exists to catch are the ones a hand-kept
//! parity table always ends up with: a duplicated identifier, a claim with no
//! test behind it, a source link that no longer resolves, and a release claim
//! with no platform evidence.

use std::collections::{BTreeMap, BTreeSet};

use crate::hash::sha256_hex;
use crate::manifest::{Manifest, SourceRegister, Status};
use crate::results::ResultSet;

/// One thing wrong with the manifest or its evidence.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct Problem {
    /// The machine-readable kind, used by CI to group failures.
    pub kind: &'static str,
    /// The capability the problem is about, when it is about one.
    pub capability: String,
    /// What is wrong, in one sentence.
    pub detail: String,
}

/// The generated compatibility report.
#[derive(Clone, Debug)]
pub struct Report {
    /// The SQLite release the manifest is measured against.
    pub reference: String,
    /// How many rows hold each status.
    pub counts: BTreeMap<String, usize>,
    /// One entry per capability, with the status the evidence supports.
    pub rows: Vec<ReportRow>,
    /// Everything wrong with the manifest or its evidence.
    pub problems: Vec<Problem>,
}

/// One capability as the report sees it.
#[derive(Clone, Debug)]
pub struct ReportRow {
    /// The capability identifier.
    pub id: String,
    /// What the manifest claims.
    pub claimed: Status,
    /// What the recorded evidence supports.
    pub evidenced: Status,
    /// The phase that owns the row.
    pub phase: String,
    /// The profile the row belongs to.
    pub profile: String,
    /// The platforms a passing result was recorded on.
    pub platforms: Vec<String>,
    /// The test identifiers the row cites.
    pub tests: Vec<String>,
}

/// Which platforms a release claim needs evidence from.
pub const REQUIRED_PLATFORMS: [&str; 2] = ["windows-x86_64", "linux-x86_64"];

/// Builds the report from a manifest, its source register, and the recorded
/// test results.
pub fn generate(manifest: &Manifest, sources: &SourceRegister, results: &ResultSet) -> Report {
    let mut problems = Vec::new();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut rows = Vec::new();

    for capability in &manifest.capabilities {
        if !seen.insert(capability.id.as_str()) {
            problems.push(Problem {
                kind: "duplicate-id",
                capability: capability.id.clone(),
                detail: "the manifest declares this capability more than once".to_string(),
            });
        }
        if capability.status != Status::Missing && capability.tests.is_empty() {
            problems.push(Problem {
                kind: "missing-test",
                capability: capability.id.clone(),
                detail: format!(
                    "status is `{}` but the row cites no test",
                    capability.status.as_str()
                ),
            });
        }
        if !sources.urls.contains(&capability.source) {
            problems.push(Problem {
                kind: "dead-source-link",
                capability: capability.id.clone(),
                detail: format!("`{}` is not in compat/sources.toml", capability.source),
            });
        }
        // **A cited identifier nothing has ever recorded (task-1946, M5).**
        // `missing-test` above asks only whether the row cites something, so a
        // row citing a name that does not exist read as an unevidenced claim
        // and was filed under "no passing result recorded" - the same words a
        // skipped suite produces, and a skipped suite is fixed by running it.
        // `ext.fts5.queries` sat in the Problems table under those words
        // because it cited `inillucent-compat::a_tokenizer_this_build_has_not_
        // got_is_refused`, which lives in `inillucent-ext`, and no Linux run
        // could ever have cleared it.
        //
        // Only asked when something has been recorded: with no results at all
        // every identifier is unrecorded, and that is the state a checkout is
        // in before `inillucent-evidence` has run.
        if !results.is_empty() {
            let unrecorded = results.never_recorded(&capability.tests);
            if !unrecorded.is_empty() {
                problems.push(Problem {
                    kind: "unrecorded-test",
                    capability: capability.id.clone(),
                    detail: format!(
                        "no run has recorded an outcome for {} - the name is wrong, or the test is gone",
                        unrecorded.join(", ")
                    ),
                });
            }
        }
        let platforms = results.platforms_passing(&capability.tests);
        let evidenced = evidence_status(capability.status, &capability.tests, &platforms, results);
        if capability.status == Status::Pass && evidenced != Status::Pass {
            problems.push(Problem {
                kind: "unsupported-release-claim",
                capability: capability.id.clone(),
                detail: describe_missing_evidence(&capability.tests, &platforms, results),
            });
        }
        rows.push(ReportRow {
            id: capability.id.clone(),
            claimed: capability.status,
            evidenced,
            phase: capability.phase.clone(),
            profile: capability.profile.clone(),
            platforms,
            tests: capability.tests.clone(),
        });
    }

    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for row in &rows {
        *counts
            .entry(row.evidenced.as_str().to_string())
            .or_insert(0) += 1;
    }
    problems.sort();
    Report {
        reference: manifest.reference.clone(),
        counts,
        rows,
        problems,
    }
}

/// Returns the strongest status the recorded evidence supports.
///
/// A claim is never allowed to exceed its evidence, but a row may claim less
/// than it could: a capability whose tests all pass may still be `partial`
/// because its owner knows a case is missing.
fn evidence_status(
    claimed: Status,
    tests: &[String],
    platforms: &[String],
    results: &ResultSet,
) -> Status {
    if claimed == Status::Missing || claimed == Status::IntentionalDeviation {
        return claimed;
    }
    if tests.is_empty() {
        return Status::Missing;
    }
    if results.any_failed(tests) {
        return Status::Partial;
    }
    let covered = REQUIRED_PLATFORMS
        .iter()
        .all(|platform| platforms.iter().any(|recorded| recorded == platform));
    if claimed == Status::Pass && covered {
        return Status::Pass;
    }
    if claimed == Status::Pass {
        return Status::Partial;
    }
    claimed
}

/// Explains why a release claim is not supported.
fn describe_missing_evidence(
    tests: &[String],
    platforms: &[String],
    results: &ResultSet,
) -> String {
    if results.any_failed(tests) {
        return "a cited test has a recorded failure".to_string();
    }
    let missing: Vec<&str> = REQUIRED_PLATFORMS
        .iter()
        .filter(|platform| !platforms.iter().any(|recorded| recorded == *platform))
        .copied()
        .collect();
    if missing.is_empty() {
        return "no passing result was recorded for the cited tests".to_string();
    }
    format!("no passing result recorded on {}", missing.join(", "))
}

impl Report {
    /// Reports whether the report is clean enough to release from.
    pub fn is_clean(&self) -> bool {
        self.problems.is_empty()
    }

    /// Renders the machine-readable report.
    ///
    /// The JSON is written by hand and in a fixed key order so that two runs
    /// with the same inputs produce byte-identical output, which is what makes
    /// the report reproducible and diffable.
    pub fn to_json(&self) -> String {
        let mut out = String::new();
        out.push_str("{\n");
        out.push_str(&format!(
            "  \"reference\": {},\n",
            json_string(&self.reference)
        ));
        out.push_str("  \"counts\": {\n");
        let mut first = true;
        for (status, count) in &self.counts {
            if !first {
                out.push_str(",\n");
            }
            first = false;
            out.push_str(&format!("    {}: {count}", json_string(status)));
        }
        out.push_str("\n  },\n  \"capabilities\": [\n");
        for (index, row) in self.rows.iter().enumerate() {
            if index > 0 {
                out.push_str(",\n");
            }
            out.push_str(&row_json(row));
        }
        out.push_str("\n  ],\n  \"problems\": [\n");
        for (index, problem) in self.problems.iter().enumerate() {
            if index > 0 {
                out.push_str(",\n");
            }
            out.push_str(&format!(
                "    {{\"kind\": {}, \"capability\": {}, \"detail\": {}}}",
                json_string(problem.kind),
                json_string(&problem.capability),
                json_string(&problem.detail)
            ));
        }
        out.push_str("\n  ]\n}\n");
        out
    }

    /// Renders the human-readable scorecard.
    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "# inillucent compatibility with SQLite {}\n\n",
            self.reference
        ));
        out.push_str("Generated from `compat/sqlite-3.53.4.toml`. Do not edit.\n\n");
        out.push_str("| status | capabilities |\n|---|---|\n");
        for (status, count) in &self.counts {
            out.push_str(&format!("| {status} | {count} |\n"));
        }
        out.push_str(&format!("| **total** | **{}** |\n\n", self.rows.len()));

        out.push_str("## By phase\n\n| phase | pass | partial | missing | deviation |\n|---|---|---|---|---|\n");
        for (phase, tally) in self.by_phase() {
            out.push_str(&format!(
                "| {phase} | {} | {} | {} | {} |\n",
                tally.pass, tally.partial, tally.missing, tally.deviation
            ));
        }
        if !self.problems.is_empty() {
            out.push_str("\n## Problems\n\n| kind | capability | detail |\n|---|---|---|\n");
            for problem in &self.problems {
                out.push_str(&format!(
                    "| {} | `{}` | {} |\n",
                    problem.kind, problem.capability, problem.detail
                ));
            }
        }
        out.push_str("\n## Capabilities\n\n| id | claimed | evidenced | platforms | tests |\n|---|---|---|---|---|\n");
        for row in &self.rows {
            out.push_str(&format!(
                "| `{}` | {} | {} | {} | {} |\n",
                row.id,
                row.claimed.as_str(),
                row.evidenced.as_str(),
                if row.platforms.is_empty() {
                    "-".to_string()
                } else {
                    row.platforms.join(", ")
                },
                row.tests.len()
            ));
        }
        out
    }

    /// Returns the digest of the machine-readable report, which is what a
    /// reproducibility check compares between two runs and two platforms.
    pub fn digest(&self) -> String {
        sha256_hex(self.to_json().as_bytes())
    }

    /// Returns the per-phase tally the scorecard shows.
    fn by_phase(&self) -> Vec<(String, PhaseTally)> {
        let mut tallies: BTreeMap<String, PhaseTally> = BTreeMap::new();
        for row in &self.rows {
            let tally = tallies.entry(row.phase.clone()).or_default();
            match row.evidenced {
                Status::Pass => tally.pass += 1,
                Status::Partial => tally.partial += 1,
                Status::Missing => tally.missing += 1,
                Status::IntentionalDeviation => tally.deviation += 1,
            }
        }
        tallies.into_iter().collect()
    }
}

/// How many capabilities of each status one phase holds.
#[derive(Clone, Copy, Debug, Default)]
struct PhaseTally {
    pass: usize,
    partial: usize,
    missing: usize,
    deviation: usize,
}

/// Renders one capability row as JSON.
fn row_json(row: &ReportRow) -> String {
    let platforms: Vec<String> = row
        .platforms
        .iter()
        .map(|platform| json_string(platform))
        .collect();
    let tests: Vec<String> = row.tests.iter().map(|test| json_string(test)).collect();
    format!(
        "    {{\"id\": {}, \"claimed\": {}, \"evidenced\": {}, \"phase\": {}, \"profile\": {}, \"platforms\": [{}], \"tests\": [{}]}}",
        json_string(&row.id),
        json_string(row.claimed.as_str()),
        json_string(row.evidenced.as_str()),
        json_string(&row.phase),
        json_string(&row.profile),
        platforms.join(", "),
        tests.join(", ")
    )
}

/// Renders a string as a JSON literal.
pub fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other if (other as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", other as u32)),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::results::TestResult;

    /// Builds a one-row manifest for a test.
    fn manifest_with(status: &str, tests: &str) -> Manifest {
        Manifest::parse(&format!(
            "reference = \"sqlite-3.53.4\"\n\n[[capability]]\nid = \"a.b\"\nsource = \"https://sqlite.org/lang.html\"\nprofile = \"default\"\nphase = \"phase 0\"\nstatus = \"{status}\"\ntests = [{tests}]\n"
        ))
        .expect("the manifest parses")
    }

    /// Returns a register holding the one URL the test manifest cites.
    fn sources() -> SourceRegister {
        SourceRegister::parse("[[source]]\nurl = \"https://sqlite.org/lang.html\"\n")
            .expect("the register parses")
    }

    /// Returns a result set where `test` passed on the given platforms.
    fn passing(test: &str, platforms: &[&str]) -> ResultSet {
        let mut results = ResultSet::default();
        for platform in platforms {
            results.record(TestResult {
                test_id: test.to_string(),
                platform: platform.to_string(),
                passed: true,
                seed: 0,
                artifact_sha256: String::new(),
            });
        }
        results
    }

    /// A duplicated identifier makes the report ambiguous and must be caught.
    #[test]
    fn duplicate_ids_are_detected() {
        let manifest = Manifest::parse(
            "reference = \"x\"\n\n[[capability]]\nid = \"a\"\nsource = \"https://sqlite.org/lang.html\"\nprofile = \"default\"\nphase = \"p\"\nstatus = \"missing\"\ntests = []\n\n[[capability]]\nid = \"a\"\nsource = \"https://sqlite.org/lang.html\"\nprofile = \"default\"\nphase = \"p\"\nstatus = \"missing\"\ntests = []\n",
        )
        .expect("the manifest parses");
        let report = generate(&manifest, &sources(), &ResultSet::default());
        assert!(report
            .problems
            .iter()
            .any(|problem| problem.kind == "duplicate-id"));
    }

    /// A claim with no test behind it is the failure a parity table always
    /// drifts into, so it is a hard error.
    #[test]
    fn missing_tests_are_detected() {
        let report = generate(
            &manifest_with("partial", ""),
            &sources(),
            &ResultSet::default(),
        );
        assert!(report
            .problems
            .iter()
            .any(|problem| problem.kind == "missing-test"));
    }

    /// A cited test nothing records is a wrong name, and says so in those words
    /// rather than the ones a skipped suite produces.
    #[test]
    fn a_cited_test_no_run_has_recorded_is_detected() {
        let manifest = manifest_with("pass", "\"t1\", \"typo\"");
        let report = generate(&manifest, &sources(), &passing("t1", &REQUIRED_PLATFORMS));
        let problem = report
            .problems
            .iter()
            .find(|problem| problem.kind == "unrecorded-test")
            .expect("the unrecorded citation is reported");
        assert!(
            problem.detail.contains("typo"),
            "the report names which identifier: {}",
            problem.detail
        );

        // And a row whose every citation was recorded raises nothing.
        let sound = generate(
            &manifest_with("pass", "\"t1\""),
            &sources(),
            &passing("t1", &REQUIRED_PLATFORMS),
        );
        assert!(sound.is_clean(), "{:?}", sound.problems);
    }

    /// A source that is not in the register is a dead link.
    #[test]
    fn dead_source_links_are_detected() {
        let manifest = Manifest::parse(
            "reference = \"x\"\n\n[[capability]]\nid = \"a\"\nsource = \"https://sqlite.org/gone.html\"\nprofile = \"default\"\nphase = \"p\"\nstatus = \"missing\"\ntests = []\n",
        )
        .expect("the manifest parses");
        let report = generate(&manifest, &sources(), &ResultSet::default());
        assert!(report
            .problems
            .iter()
            .any(|problem| problem.kind == "dead-source-link"));
    }

    /// A pass claim with no platform evidence is downgraded and reported.
    #[test]
    fn unsupported_release_claims_are_detected_and_downgraded() {
        let manifest = manifest_with("pass", "\"t1\"");
        let report = generate(&manifest, &sources(), &ResultSet::default());
        assert!(report
            .problems
            .iter()
            .any(|problem| problem.kind == "unsupported-release-claim"));
        assert_eq!(
            report.rows.first().map(|row| row.evidenced),
            Some(Status::Partial)
        );

        let one_platform = generate(&manifest, &sources(), &passing("t1", &["windows-x86_64"]));
        assert!(
            !one_platform.is_clean(),
            "one platform is not enough for a release claim"
        );

        let both = generate(&manifest, &sources(), &passing("t1", &REQUIRED_PLATFORMS));
        assert!(both.is_clean(), "{:?}", both.problems);
        assert_eq!(
            both.rows.first().map(|row| row.evidenced),
            Some(Status::Pass)
        );
    }

    /// A recorded failure demotes a passing claim even when both platforms ran.
    #[test]
    fn a_recorded_failure_demotes_a_claim() {
        let mut results = passing("t1", &REQUIRED_PLATFORMS);
        results.record(TestResult {
            test_id: "t1".to_string(),
            platform: "linux-x86_64".to_string(),
            passed: false,
            seed: 7,
            artifact_sha256: String::new(),
        });
        let report = generate(&manifest_with("pass", "\"t1\""), &sources(), &results);
        assert_eq!(
            report.rows.first().map(|row| row.evidenced),
            Some(Status::Partial)
        );
        assert!(report
            .problems
            .iter()
            .any(|problem| problem.detail.contains("recorded failure")));
    }

    /// The same inputs must produce a byte-identical report, on any platform,
    /// or the report cannot be a release gate.
    #[test]
    fn the_report_is_reproducible() {
        let manifest = manifest_with("pass", "\"t1\"");
        let results = passing("t1", &REQUIRED_PLATFORMS);
        let first = generate(&manifest, &sources(), &results);
        let second = generate(&manifest, &sources(), &results);
        assert_eq!(first.to_json(), second.to_json());
        assert_eq!(first.digest(), second.digest());
        assert!(first.to_markdown().contains("phase 0"));
    }
}
