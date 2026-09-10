//! The performance history, and what counts as a regression in it.
//!
//! Invariant: a regression is opened by a rule declared before the numbers, and
//! by two runs rather than one. A single run that came out slower is noise
//! until it happens again - benchmarking on a machine that also runs a browser
//! produces one bad interval fairly often - and a rule invented after looking
//! at a result is not a rule.
//!
//! The file is append-only, one JSON object per line, and every line carries
//! the label, the platform and the scale it belongs to. That is what makes a
//! comparison "against the last comparable run" mean something: a Windows small
//! run is compared with the previous Windows small run and with nothing else.
//!
//! What is compared is the *lower bound* of the paired interval, not the point
//! estimate. A workload whose interval widened but whose centre held has not
//! got slower; one whose lower bound fell has, whatever its centre says.

use std::collections::BTreeMap;
use std::path::Path;

use crate::report::json_string;

/// How much a lower bound may fall before it counts as a regression.
///
/// Five percent, which is the practical threshold the TDD names for p99 and is
/// the smallest movement worth waking anybody for on a machine that is not a
/// dedicated benchmark host.
pub const PRACTICAL_THRESHOLD: f64 = 0.05;

/// One recorded measurement.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    /// The label the run was given.
    pub label: String,
    /// The platform it ran on.
    pub platform: String,
    /// The scale it ran at.
    pub scale: String,
    /// The workload, or `*headline*` for the weighted geometric mean.
    pub workload: String,
    /// The family the workload is weighted under.
    pub family: String,
    /// The point estimate of the speed ratio, SQLite over inillucent.
    pub ratio: f64,
    /// The lower end of the 95% interval.
    pub low: f64,
    /// The upper end.
    pub high: f64,
    /// How many paired samples it came from.
    pub samples: usize,
    /// The optimizations that were switched off, or empty for the shipped
    /// engine.
    ///
    /// Part of the series key, not decoration. A run with a lever switched off
    /// is a different experiment, and comparing it against the run before it as
    /// though it were the next measurement of the same thing reports the arm
    /// itself as a regression - which it did, the first time three arms were
    /// recorded in a row.
    pub arm: String,
}

impl Entry {
    /// Reads one line of the history.
    pub fn parse(line: &str) -> Option<Entry> {
        let text = |key: &str| field(line, key).map(|value| value.to_string());
        let number = |key: &str| field(line, key).and_then(|value| value.parse::<f64>().ok());
        Some(Entry {
            label: text("label")?,
            platform: text("platform")?,
            scale: text("scale")?,
            workload: text("workload")?,
            family: text("family").unwrap_or_default(),
            ratio: number("ratio")?,
            low: number("low")?,
            high: number("high")?,
            samples: number("samples").unwrap_or(0.0) as usize,
            arm: text("arm").unwrap_or_default(),
        })
    }

    /// Renders the entry as one line.
    pub fn render(&self) -> String {
        format!(
            "{{\"label\": {}, \"platform\": {}, \"scale\": {}, \"workload\": {}, \"family\": {}, \
             \"ratio\": {:.6}, \"low\": {:.6}, \"high\": {:.6}, \"samples\": {}, \"arm\": {}}}",
            json_string(&self.label),
            json_string(&self.platform),
            json_string(&self.scale),
            json_string(&self.workload),
            json_string(&self.family),
            self.ratio,
            self.low,
            self.high,
            self.samples,
            json_string(&self.arm)
        )
    }

    /// Returns the key a run is compared within: one platform, one scale, one
    /// workload, one arm.
    pub fn series(&self) -> (String, String, String, String) {
        (
            self.platform.clone(),
            self.scale.clone(),
            self.workload.clone(),
            self.arm.clone(),
        )
    }
}

/// Returns one field of a flat JSON object, as text.
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("\"{key}\":");
    let start = line.find(&needle)?.saturating_add(needle.len());
    let rest = line.get(start..)?.trim_start();
    if let Some(quoted) = rest.strip_prefix('"') {
        return quoted.split('"').next();
    }
    Some(rest.split([',', '}']).next()?.trim())
}

/// Every recorded run, in the order they were written.
#[derive(Clone, Debug, Default)]
pub struct History {
    /// The entries, oldest first.
    pub entries: Vec<Entry>,
}

impl History {
    /// Reads a history file, treating a missing one as empty.
    pub fn load(path: &Path) -> History {
        let Ok(text) = std::fs::read_to_string(path) else {
            return History::default();
        };
        History {
            entries: text.lines().filter_map(Entry::parse).collect(),
        }
    }

    /// Returns the labels in the order they were first written.
    pub fn labels(&self) -> Vec<String> {
        let mut found: Vec<String> = Vec::new();
        for entry in &self.entries {
            if !found.contains(&entry.label) {
                found.push(entry.label.clone());
            }
        }
        found
    }

    /// Returns one series' entries, oldest first.
    pub fn series(&self, platform: &str, scale: &str, workload: &str) -> Vec<&Entry> {
        self.entries
            .iter()
            .filter(|entry| {
                entry.platform == platform && entry.scale == scale && entry.workload == workload
            })
            .collect()
    }

    /// Returns the workloads that have got slower in the last two runs.
    ///
    /// Two consecutive comparable runs, both below the best lower bound the
    /// series had reached before them, by more than the practical threshold.
    /// One run is noise; the third-to-last is what "before them" means.
    pub fn regressions(&self, platform: &str) -> Vec<Regression> {
        let mut series: BTreeMap<(String, String, String, String), Vec<&Entry>> = BTreeMap::new();
        for entry in &self.entries {
            if entry.platform != platform {
                continue;
            }
            series.entry(entry.series()).or_default().push(entry);
        }
        let mut found = Vec::new();
        for ((_, scale, workload, arm), runs) in series {
            if runs.len() < 3 {
                continue;
            }
            let split = runs.len().saturating_sub(2);
            let Some(history) = runs.get(..split) else {
                continue;
            };
            let Some(recent) = runs.get(split..) else {
                continue;
            };
            let best = history
                .iter()
                .map(|entry| entry.low)
                .fold(f64::NEG_INFINITY, f64::max);
            if !best.is_finite() {
                continue;
            }
            let threshold = best * (1.0 - PRACTICAL_THRESHOLD);
            if recent.iter().all(|entry| entry.low < threshold) {
                let latest = recent.last().map(|entry| entry.low).unwrap_or(0.0);
                found.push(Regression {
                    scale,
                    workload,
                    arm,
                    best,
                    now: latest,
                    labels: recent.iter().map(|entry| entry.label.clone()).collect(),
                });
            }
        }
        found
    }
}

/// One workload that has got slower and stayed slower.
#[derive(Clone, Debug, PartialEq)]
pub struct Regression {
    /// The scale it regressed at.
    pub scale: String,
    /// The workload.
    pub workload: String,
    /// The arm it regressed under, or empty for the shipped engine.
    pub arm: String,
    /// The best lower bound the series had reached.
    pub best: f64,
    /// The lower bound it has now.
    pub now: f64,
    /// The two runs that are below it.
    pub labels: Vec<String>,
}

/// Renders the dashboard: every workload's ratio across every recorded run.
///
/// One table per scale, one row per workload, one column per label in the order
/// they were run. It is the artifact that answers "did that change help", which
/// a single scorecard cannot.
/// @param history - the recorded runs
/// @param platform - the platform to render
pub fn dashboard(history: &History, platform: &str) -> String {
    let labels = history.labels();
    let mut out = String::new();
    out.push_str("# inillucent performance dashboard\n\n");
    out.push_str(&format!(
        "Platform `{platform}`. Every number is the paired speed ratio, SQLite over inillucent, so \
         above one is faster than the reference. Columns are runs in the order they were taken.\n\n"
    ));
    let mut scales: Vec<String> = Vec::new();
    for entry in &history.entries {
        if entry.platform == platform && !scales.contains(&entry.scale) {
            scales.push(entry.scale.clone());
        }
    }
    for scale in &scales {
        out.push_str(&format!("## Scale `{scale}`\n\n| workload |"));
        for label in &labels {
            out.push_str(&format!(" {label} |"));
        }
        out.push_str("\n|---|");
        for _ in &labels {
            out.push_str("---:|");
        }
        out.push('\n');
        let mut workloads: Vec<String> = Vec::new();
        for entry in &history.entries {
            if entry.platform == platform
                && entry.scale == *scale
                && !workloads.contains(&entry.workload)
            {
                workloads.push(entry.workload.clone());
            }
        }
        for workload in workloads {
            out.push_str(&format!("| `{workload}` |"));
            for label in &labels {
                let found = history.entries.iter().rfind(|entry| {
                    entry.platform == platform
                        && entry.scale == *scale
                        && entry.workload == workload
                        && entry.label == *label
                });
                match found {
                    Some(entry) => out.push_str(&format!(" {:.3}x |", entry.ratio)),
                    None => out.push_str(" - |"),
                }
            }
            out.push('\n');
        }
        out.push('\n');
    }
    let regressions = history.regressions(platform);
    out.push_str("## Regressions\n\n");
    if regressions.is_empty() {
        out.push_str(
            "None open. A regression opens when a workload's lower confidence bound sits more \
             than five percent below the best it had reached, for two consecutive comparable \
             runs - one run is noise.\n",
        );
        return out;
    }
    out.push_str(
        "| scale | workload | arm | best lower bound | now | runs |\n\
         |---|---|---|---:|---:|---|\n",
    );
    for regression in regressions {
        out.push_str(&format!(
            "| `{}` | `{}` | {} | {:.3}x | {:.3}x | {} |\n",
            regression.scale,
            regression.workload,
            if regression.arm.is_empty() {
                "shipped".to_string()
            } else {
                format!("no `{}`", regression.arm)
            },
            regression.best,
            regression.now,
            regression.labels.join(", ")
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(label: &str, workload: &str, low: f64) -> Entry {
        Entry {
            label: label.to_string(),
            platform: "windows-x86_64".to_string(),
            scale: "small".to_string(),
            workload: workload.to_string(),
            family: "read.point".to_string(),
            ratio: low + 0.1,
            low,
            high: low + 0.2,
            samples: 30,
            arm: String::new(),
        }
    }

    /// An entry survives the round trip through its line.
    #[test]
    fn an_entry_round_trips() {
        let original = entry("baseline", "point.rowid", 1.25);
        let parsed = Entry::parse(&original.render()).expect("it parses");
        assert_eq!(parsed.label, "baseline");
        assert_eq!(parsed.workload, "point.rowid");
        assert!((parsed.low - 1.25).abs() < 1.0e-6);
        assert_eq!(parsed.samples, 30);
    }

    /// One slow run is noise; two in a row is a regression.
    #[test]
    fn a_regression_needs_two_runs() {
        let mut history = History::default();
        history.entries.push(entry("a", "point.rowid", 1.00));
        history.entries.push(entry("b", "point.rowid", 1.00));
        history.entries.push(entry("c", "point.rowid", 0.80));
        assert!(
            history.regressions("windows-x86_64").is_empty(),
            "one slow run is not a regression"
        );
        history.entries.push(entry("d", "point.rowid", 0.80));
        let found = history.regressions("windows-x86_64");
        assert_eq!(found.len(), 1);
        assert_eq!(
            found.first().map(|entry| entry.workload.as_str()),
            Some("point.rowid")
        );
    }

    /// A movement inside the practical threshold is not a regression.
    #[test]
    fn a_small_movement_is_not_a_regression() {
        let mut history = History::default();
        history.entries.push(entry("a", "point.rowid", 1.00));
        history.entries.push(entry("b", "point.rowid", 1.00));
        history.entries.push(entry("c", "point.rowid", 0.97));
        history.entries.push(entry("d", "point.rowid", 0.97));
        assert!(history.regressions("windows-x86_64").is_empty());
    }

    /// A series on another platform is not compared with this one.
    #[test]
    fn platforms_are_compared_separately() {
        let mut history = History::default();
        for label in ["a", "b", "c", "d"] {
            let mut other = entry(label, "point.rowid", 0.10);
            other.platform = "linux-x86_64".to_string();
            history.entries.push(other);
        }
        assert!(history.regressions("windows-x86_64").is_empty());
        assert_eq!(history.regressions("linux-x86_64").len(), 0);
    }

    /// A run with a lever switched off is a different experiment.
    ///
    /// Three arms recorded in a row are three measurements of three different
    /// things. Comparing them as one series reported the arm itself as a
    /// regression, which is what this pins.
    #[test]
    fn an_arm_is_its_own_series() {
        let mut history = History::default();
        for label in ["a", "b", "c", "d"] {
            history.entries.push(entry(label, "point.rowid", 1.00));
        }
        for label in ["e", "f"] {
            let mut arm = entry(label, "point.rowid", 0.40);
            arm.arm = "covering-index".to_string();
            history.entries.push(arm);
        }
        assert!(
            history.regressions("windows-x86_64").is_empty(),
            "the arm is not a slower measurement of the shipped engine"
        );
    }

    /// The dashboard names every run it has, and says so when nothing regressed.
    #[test]
    fn the_dashboard_lists_every_run() {
        let mut history = History::default();
        history.entries.push(entry("baseline", "point.rowid", 1.0));
        history.entries.push(entry("levers", "point.rowid", 1.4));
        let rendered = dashboard(&history, "windows-x86_64");
        assert!(rendered.contains("| baseline | levers |"), "{rendered}");
        assert!(rendered.contains("`point.rowid`"));
        assert!(rendered.contains("None open."));
    }
}
