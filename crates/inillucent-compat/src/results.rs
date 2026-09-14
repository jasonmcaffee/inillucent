//! Recorded test results.
//!
//! Invariant: a capability's status in the report comes from results a test run
//! actually wrote, on a named platform, with a seed. A developer's belief that
//! something works is not evidence and cannot reach the report.
//!
//! Results are newline-delimited JSON so a run can append to them from any
//! language and a failing shard can be merged with a passing one.

use std::collections::BTreeMap;
use std::path::Path;

use crate::report::json_string;

/// One recorded test outcome.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TestResult {
    /// The test identifier a capability row cites.
    pub test_id: String,
    /// The platform the test ran on, such as `windows-x86_64`.
    pub platform: String,
    /// Whether it passed.
    pub passed: bool,
    /// The seed the run used, so a failure can be replayed.
    pub seed: u64,
    /// The digest of the artifact the run produced, when it produced one.
    pub artifact_sha256: String,
}

impl TestResult {
    /// Renders the result as one line of newline-delimited JSON.
    pub fn to_json(&self) -> String {
        format!(
            "{{\"test_id\": {}, \"platform\": {}, \"passed\": {}, \"seed\": {}, \"artifact_sha256\": {}}}",
            json_string(&self.test_id),
            json_string(&self.platform),
            self.passed,
            self.seed,
            json_string(&self.artifact_sha256)
        )
    }

    /// Parses one line written by `to_json`.
    ///
    /// The parser is deliberately narrow: it reads the keys it knows and
    /// refuses a line that is missing one, rather than accepting a partially
    /// understood result and treating the gap as a pass.
    pub fn parse(line: &str) -> Result<TestResult, String> {
        let test_id = extract_string(line, "test_id")?;
        let platform = extract_string(line, "platform")?;
        let passed = extract_bool(line, "passed")?;
        let seed = extract_number(line, "seed")?;
        let artifact = extract_string(line, "artifact_sha256").unwrap_or_default();
        Ok(TestResult {
            test_id,
            platform,
            passed,
            seed,
            artifact_sha256: artifact,
        })
    }
}

/// Every recorded result, indexed by test identifier.
#[derive(Clone, Debug, Default)]
pub struct ResultSet {
    by_test: BTreeMap<String, Vec<TestResult>>,
}

impl ResultSet {
    /// Adds one result.
    pub fn record(&mut self, result: TestResult) {
        self.by_test
            .entry(result.test_id.clone())
            .or_default()
            .push(result);
    }

    /// Reads results from a newline-delimited JSON file, ignoring blank lines.
    pub fn load(path: &Path) -> Result<ResultSet, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        let mut set = ResultSet::default();
        for (number, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let result = TestResult::parse(line)
                .map_err(|reason| format!("line {}: {reason}", number + 1))?;
            set.record(result);
        }
        Ok(set)
    }

    /// Reads every `*.jsonl` file in a directory, if the directory exists.
    pub fn load_directory(directory: &Path) -> Result<ResultSet, String> {
        let mut set = ResultSet::default();
        let Ok(entries) = std::fs::read_dir(directory) else {
            return Ok(set);
        };
        let mut paths: Vec<std::path::PathBuf> = entries
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "jsonl")
            })
            .collect();
        paths.sort();
        for path in paths {
            let loaded = ResultSet::load(&path)?;
            for results in loaded.by_test.into_values() {
                for result in results {
                    set.record(result);
                }
            }
        }
        Ok(set)
    }

    /// Returns the platforms on which every one of `tests` has a passing result.
    ///
    /// A capability is only as covered as its least covered test, so this is an
    /// intersection rather than a union.
    pub fn platforms_passing(&self, tests: &[String]) -> Vec<String> {
        let mut common: Option<Vec<String>> = None;
        for test in tests {
            let platforms: Vec<String> = self
                .by_test
                .get(test)
                .map(|results| {
                    let mut names: Vec<String> = results
                        .iter()
                        .filter(|result| result.passed)
                        .map(|result| result.platform.clone())
                        .collect();
                    names.sort();
                    names.dedup();
                    names
                })
                .unwrap_or_default();
            common = Some(match common {
                None => platforms,
                Some(existing) => existing
                    .into_iter()
                    .filter(|platform| platforms.contains(platform))
                    .collect(),
            });
        }
        common.unwrap_or_default()
    }

    /// Reports whether any of `tests` has a recorded failure.
    pub fn any_failed(&self, tests: &[String]) -> bool {
        tests.iter().any(|test| {
            self.by_test
                .get(test)
                .is_some_and(|results| results.iter().any(|result| !result.passed))
        })
    }

    /// Returns the cited tests no run has recorded an outcome for, on any
    /// platform.
    ///
    /// **A name nothing records is a different fault from a suite that was
    /// skipped, and it does not heal.** A skipped suite leaves the identifier
    /// recorded elsewhere - the other platform, an earlier run - so the fix is
    /// to run it. An identifier that appears in no result file at all is a test
    /// that was renamed, moved to another crate, or deleted, and no amount of
    /// running will produce it.
    ///
    /// @param tests - the identifiers a capability cites
    /// @returns those of them nothing has ever recorded, in the order cited
    pub fn never_recorded(&self, tests: &[String]) -> Vec<String> {
        tests
            .iter()
            .filter(|test| !self.by_test.contains_key(test.as_str()))
            .cloned()
            .collect()
    }

    /// Returns how many results have been recorded.
    pub fn len(&self) -> usize {
        self.by_test.values().map(Vec::len).sum()
    }

    /// Reports whether nothing has been recorded.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Renders every result as newline-delimited JSON, in a stable order.
    pub fn to_jsonl(&self) -> String {
        let mut out = String::new();
        for results in self.by_test.values() {
            for result in results {
                out.push_str(&result.to_json());
                out.push('\n');
            }
        }
        out
    }
}

/// Extracts a JSON string field from one line.
fn extract_string(line: &str, key: &str) -> Result<String, String> {
    let needle = format!("\"{key}\":");
    let start = line
        .find(&needle)
        .ok_or_else(|| format!("no `{key}` field"))?
        .saturating_add(needle.len());
    let rest = line.get(start..).unwrap_or("").trim_start();
    let body = rest
        .strip_prefix('"')
        .ok_or_else(|| format!("`{key}` is not a string"))?;
    let mut out = String::new();
    let mut characters = body.chars();
    while let Some(character) = characters.next() {
        match character {
            '"' => return Ok(out),
            '\\' => match characters.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some(other) => out.push(other),
                None => return Err(format!("`{key}` ends in a backslash")),
            },
            other => out.push(other),
        }
    }
    Err(format!("`{key}` is unterminated"))
}

/// Extracts a JSON boolean field from one line.
fn extract_bool(line: &str, key: &str) -> Result<bool, String> {
    let raw = raw_field(line, key)?;
    if raw.starts_with("true") {
        return Ok(true);
    }
    if raw.starts_with("false") {
        return Ok(false);
    }
    Err(format!("`{key}` is not a boolean"))
}

/// Extracts a JSON number field from one line.
fn extract_number(line: &str, key: &str) -> Result<u64, String> {
    let raw = raw_field(line, key)?;
    let digits: String = raw
        .chars()
        .take_while(|character| character.is_ascii_digit())
        .collect();
    digits
        .parse()
        .map_err(|_| format!("`{key}` is not a number"))
}

/// Returns the text following a key, trimmed of leading whitespace.
fn raw_field<'a>(line: &'a str, key: &str) -> Result<&'a str, String> {
    let needle = format!("\"{key}\":");
    let start = line
        .find(&needle)
        .ok_or_else(|| format!("no `{key}` field"))?
        .saturating_add(needle.len());
    Ok(line.get(start..).unwrap_or("").trim_start())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A result must survive a write and read unchanged.
    #[test]
    fn a_result_round_trips() {
        let result = TestResult {
            test_id: "vfs.lock.shared-is-shared".to_string(),
            platform: "windows-x86_64".to_string(),
            passed: true,
            seed: 184_467,
            artifact_sha256: "abc123".to_string(),
        };
        let parsed = TestResult::parse(&result.to_json()).expect("the line parses");
        assert_eq!(parsed, result);
    }

    /// A line missing a field is refused rather than defaulted, because a
    /// defaulted `passed` would silently invent evidence.
    #[test]
    fn an_incomplete_line_is_refused() {
        let error = TestResult::parse("{\"test_id\": \"a\", \"platform\": \"windows-x86_64\"}")
            .expect_err("the line is incomplete");
        assert!(error.contains("passed"), "{error}");
    }

    /// Coverage is the intersection across a capability's tests: a capability
    /// is only as covered as its least covered test.
    #[test]
    fn coverage_is_the_intersection_across_tests() {
        let mut set = ResultSet::default();
        for (test, platform) in [
            ("a", "windows-x86_64"),
            ("a", "linux-x86_64"),
            ("b", "windows-x86_64"),
        ] {
            set.record(TestResult {
                test_id: test.to_string(),
                platform: platform.to_string(),
                passed: true,
                seed: 0,
                artifact_sha256: String::new(),
            });
        }
        assert_eq!(
            set.platforms_passing(&["a".to_string()]),
            vec!["linux-x86_64".to_string(), "windows-x86_64".to_string()]
        );
        assert_eq!(
            set.platforms_passing(&["a".to_string(), "b".to_string()]),
            vec!["windows-x86_64".to_string()]
        );
        assert!(set.platforms_passing(&["missing".to_string()]).is_empty());
    }

    /// A failure anywhere in a capability's tests must be visible.
    #[test]
    fn a_failure_is_visible() {
        let mut set = ResultSet::default();
        set.record(TestResult {
            test_id: "a".to_string(),
            platform: "linux-x86_64".to_string(),
            passed: false,
            seed: 3,
            artifact_sha256: String::new(),
        });
        assert!(set.any_failed(&["a".to_string()]));
        assert!(!set.any_failed(&["b".to_string()]));
    }
}
