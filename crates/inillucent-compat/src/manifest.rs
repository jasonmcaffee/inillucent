//! The parity manifest and the reference metadata.
//!
//! Invariant: a documentation claim cannot exist without a test link. The
//! manifest is the only place a capability is declared, the report is generated
//! from it, and the generator refuses a manifest that claims more than its
//! evidence supports.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::toml_lite::{self, Table, Value};

/// How complete a capability is.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum Status {
    /// Nothing is implemented.
    Missing,
    /// Something is implemented and its tests are incomplete or red.
    Partial,
    /// Implemented, tested, and evidenced on every required platform.
    Pass,
    /// Deliberately different from the reference, with a recorded reason.
    IntentionalDeviation,
}

impl Status {
    /// Parses the manifest spelling of a status.
    pub fn parse(text: &str) -> Option<Status> {
        match text {
            "missing" => Some(Status::Missing),
            "partial" => Some(Status::Partial),
            "pass" => Some(Status::Pass),
            "intentional-deviation" => Some(Status::IntentionalDeviation),
            _ => None,
        }
    }

    /// Returns the manifest spelling of a status.
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Missing => "missing",
            Status::Partial => "partial",
            Status::Pass => "pass",
            Status::IntentionalDeviation => "intentional-deviation",
        }
    }
}

/// One capability row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Capability {
    /// The stable identifier a test and a report both cite.
    pub id: String,
    /// The documentation page the requirement comes from.
    pub source: String,
    /// Which profile the row belongs to.
    pub profile: String,
    /// The implementation phase that owns the row.
    pub phase: String,
    /// How complete the capability is.
    pub status: Status,
    /// The behaviour expected of a negative-parity row.
    pub expected: Option<String>,
    /// The test identifiers that evidence the row.
    pub tests: Vec<String>,
}

/// The whole parity manifest.
#[derive(Clone, Debug)]
pub struct Manifest {
    /// The SQLite release the manifest is measured against.
    pub reference: String,
    /// Every capability row, in file order.
    pub capabilities: Vec<Capability>,
}

impl Manifest {
    /// Reads a manifest from disk.
    pub fn load(path: &Path) -> Result<Manifest, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        Manifest::parse(&text)
    }

    /// Parses a manifest.
    pub fn parse(text: &str) -> Result<Manifest, String> {
        let document = toml_lite::parse(text)?;
        let reference = document.require_str("reference")?.to_string();
        let mut capabilities = Vec::new();
        for (index, row) in document.array("capability").iter().enumerate() {
            capabilities.push(parse_capability(row, index)?);
        }
        Ok(Manifest {
            reference,
            capabilities,
        })
    }

    /// Returns how many rows hold each status.
    pub fn counts(&self) -> BTreeMap<&'static str, usize> {
        let mut counts = BTreeMap::new();
        for capability in &self.capabilities {
            *counts.entry(capability.status.as_str()).or_insert(0) += 1;
        }
        counts
    }
}

/// Parses one capability row, naming the row when a field is wrong.
fn parse_capability(row: &Table, index: usize) -> Result<Capability, String> {
    let text = |key: &str| -> Result<String, String> {
        row.get(key)
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| format!("capability {index}: missing string `{key}`"))
    };
    let id = text("id")?;
    let status_text = text("status")?;
    let status = Status::parse(&status_text)
        .ok_or_else(|| format!("capability `{id}`: unknown status `{status_text}`"))?;
    let tests = row
        .get("tests")
        .and_then(Value::as_list)
        .map(|list| list.to_vec())
        .ok_or_else(|| format!("capability `{id}`: missing list `tests`"))?;
    Ok(Capability {
        id,
        source: text("source")?,
        profile: text("profile")?,
        phase: text("phase")?,
        status,
        expected: row
            .get("expected")
            .and_then(Value::as_str)
            .map(str::to_string),
        tests,
    })
}

/// The documentation pages a capability row may cite.
#[derive(Clone, Debug)]
pub struct SourceRegister {
    /// Every allowed URL.
    pub urls: BTreeSet<String>,
}

impl SourceRegister {
    /// Reads the register from disk.
    pub fn load(path: &Path) -> Result<SourceRegister, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        SourceRegister::parse(&text)
    }

    /// Parses the register.
    pub fn parse(text: &str) -> Result<SourceRegister, String> {
        let document = toml_lite::parse(text)?;
        let mut urls = BTreeSet::new();
        for row in document.array("source") {
            let url = row
                .get("url")
                .and_then(Value::as_str)
                .ok_or_else(|| "a source row has no url".to_string())?;
            urls.insert(url.to_string());
        }
        Ok(SourceRegister { urls })
    }
}

/// One artifact of the pinned reference build.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReferenceArtifact {
    /// The file name as published.
    pub name: String,
    /// Where it is downloaded from.
    pub url: String,
    /// Its published size in bytes.
    pub bytes: u64,
    /// Its published SHA3-256 sum.
    pub sha3_256: String,
    /// What the harness uses it for.
    pub used_for: String,
}

/// The pinned reference build's metadata.
#[derive(Clone, Debug)]
pub struct Reference {
    /// The SQLite version.
    pub version: String,
    /// The release identifier used in artifact names.
    pub release_id: String,
    /// The compile options the oracle is built with.
    pub compile_options: String,
    /// The run-time settings a comparison uses on both sides.
    pub settings: BTreeMap<String, String>,
    /// The published artifacts.
    pub artifacts: Vec<ReferenceArtifact>,
}

impl Reference {
    /// Reads the reference metadata from disk.
    pub fn load(path: &Path) -> Result<Reference, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        Reference::parse(&text)
    }

    /// Parses the reference metadata.
    pub fn parse(text: &str) -> Result<Reference, String> {
        let document = toml_lite::parse(text)?;
        let mut settings = BTreeMap::new();
        for key in [
            "page_size",
            "journal_mode",
            "synchronous",
            "cache_size",
            "temp_store",
            "mmap_size",
            "foreign_keys",
            "profile",
        ] {
            if let Some(value) = document.top.get(key) {
                settings.insert(key.to_string(), render_scalar(value));
            }
        }
        let mut artifacts = Vec::new();
        for row in document.array("artifact") {
            artifacts.push(ReferenceArtifact {
                name: required_text(row, "name")?,
                url: required_text(row, "url")?,
                bytes: row
                    .get("bytes")
                    .and_then(Value::as_integer)
                    .ok_or_else(|| "an artifact row has no byte count".to_string())?
                    .max(0) as u64,
                sha3_256: required_text(row, "sha3_256")?,
                used_for: required_text(row, "used_for")?,
            });
        }
        Ok(Reference {
            version: document.require_str("version")?.to_string(),
            release_id: document.require_str("release_id")?.to_string(),
            compile_options: document.require_str("compile_options")?.to_string(),
            settings,
            artifacts,
        })
    }

    /// Returns the artifact with the given name.
    pub fn artifact(&self, name: &str) -> Option<&ReferenceArtifact> {
        self.artifacts.iter().find(|artifact| artifact.name == name)
    }
}

/// Returns a required string field, naming the field when it is absent.
fn required_text(row: &Table, key: &str) -> Result<String, String> {
    row.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("a row is missing string `{key}`"))
}

/// Renders a scalar for the settings table.
fn render_scalar(value: &Value) -> String {
    match value {
        Value::Text(text) => text.clone(),
        Value::Integer(number) => number.to_string(),
        Value::Boolean(flag) => flag.to_string(),
        Value::List(items) => items.join(","),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A manifest row must carry every field the report needs.
    #[test]
    fn a_capability_row_parses() {
        let manifest = Manifest::parse(
            "reference = \"sqlite-3.53.4\"\n\n\
             [[capability]]\n\
             id = \"sql.select.basic\"\n\
             source = \"https://sqlite.org/lang_select.html\"\n\
             profile = \"default\"\n\
             phase = \"phase 6\"\n\
             status = \"partial\"\n\
             tests = [\"select.basic\"]\n",
        )
        .expect("the row parses");
        assert_eq!(manifest.reference, "sqlite-3.53.4");
        assert_eq!(manifest.capabilities.len(), 1);
        let capability = manifest.capabilities.first().expect("one row");
        assert_eq!(capability.status, Status::Partial);
        assert_eq!(capability.tests, vec!["select.basic".to_string()]);
    }

    /// A status the manifest does not define must be refused, not defaulted.
    #[test]
    fn an_unknown_status_is_refused() {
        let error = Manifest::parse(
            "reference = \"x\"\n\n[[capability]]\nid = \"a\"\nsource = \"s\"\nprofile = \"default\"\nphase = \"p\"\nstatus = \"probably\"\ntests = []\n",
        )
        .expect_err("an unknown status is refused");
        assert!(error.contains("unknown status"), "{error}");
    }

    /// A row with no `tests` list at all is a mistake, distinct from a row with
    /// an empty one.
    #[test]
    fn a_missing_tests_list_is_refused() {
        let error = Manifest::parse(
            "reference = \"x\"\n\n[[capability]]\nid = \"a\"\nsource = \"s\"\nprofile = \"default\"\nphase = \"p\"\nstatus = \"missing\"\n",
        )
        .expect_err("a missing list is refused");
        assert!(error.contains("missing list `tests`"), "{error}");
    }
}
