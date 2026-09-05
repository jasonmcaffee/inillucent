//! The syntax obligation register, and the report it renders.
//!
//! Invariant: a production with no row cannot be forgotten, and a row with no
//! example cannot claim coverage. The register is the denominator for the
//! parser the way `compat/sqlite-3.53.4.toml` is the denominator for the
//! engine: it lists every syntax diagram SQLite publishes, and the test that
//! reads it refuses a row that carries no evidence.

use std::path::Path;

use crate::toml_lite::{self, Value};

/// Whether inillucent accepts a production.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyntaxStatus {
    /// The parser accepts it and the AST records it.
    Parsed,
    /// Deliberately not accepted yet.
    Omitted,
}

impl SyntaxStatus {
    /// Parses a status word.
    pub fn parse(text: &str) -> Option<SyntaxStatus> {
        match text {
            "parsed" => Some(SyntaxStatus::Parsed),
            "omitted" => Some(SyntaxStatus::Omitted),
            _ => None,
        }
    }

    /// Returns the word this status is written as.
    pub fn as_str(self) -> &'static str {
        match self {
            SyntaxStatus::Parsed => "parsed",
            SyntaxStatus::Omitted => "omitted",
        }
    }
}

/// One published syntax production.
#[derive(Clone, Debug)]
pub struct Production {
    /// The production's name on the SQLite site.
    pub name: String,
    /// Where it is published.
    pub source: String,
    /// Whether inillucent accepts it.
    pub status: SyntaxStatus,
    /// Statements that must parse.
    pub positive: Vec<String>,
    /// Statements that must be refused.
    pub negative: Vec<String>,
    /// Why the pinned build cannot be compared against this production.
    ///
    /// Some productions exist in SQLite's grammar but are behind a compile
    /// option the reference build does not set - `ORDER BY` and `LIMIT` on
    /// `DELETE` need `SQLITE_ENABLE_UPDATE_DELETE_LIMIT`. Comparing against a
    /// build that was not compiled with them would report a grammar difference
    /// that is really a build difference.
    pub oracle: Option<String>,
    /// Why it is not accepted, when it is omitted.
    pub reason: Option<String>,
    /// The phase that will accept it, when it is omitted.
    pub phase: Option<String>,
}

/// The whole register.
#[derive(Clone, Debug)]
pub struct SyntaxRegister {
    /// The release the register describes.
    pub reference: String,
    /// Every production, in file order.
    pub productions: Vec<Production>,
}

impl SyntaxRegister {
    /// Loads the register from a file.
    pub fn load(path: &Path) -> Result<SyntaxRegister, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|reason| format!("{}: {reason}", path.display()))?;
        SyntaxRegister::parse(&text)
    }

    /// Parses the register.
    pub fn parse(text: &str) -> Result<SyntaxRegister, String> {
        let document = toml_lite::parse(text)?;
        let reference = document.require_str("reference")?.to_string();
        let mut productions = Vec::new();
        for row in document.array("production") {
            let name = row
                .get("name")
                .and_then(Value::as_str)
                .ok_or("a production with no name")?
                .to_string();
            let source = row
                .get("source")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("`{name}` has no source"))?
                .to_string();
            let status_text = row
                .get("status")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("`{name}` has no status"))?;
            let status = SyntaxStatus::parse(status_text)
                .ok_or_else(|| format!("`{name}` has unknown status `{status_text}`"))?;
            let positive = row
                .get("positive")
                .and_then(Value::as_list)
                .map(<[String]>::to_vec)
                .unwrap_or_default();
            let negative = row
                .get("negative")
                .and_then(Value::as_list)
                .map(<[String]>::to_vec)
                .unwrap_or_default();
            productions.push(Production {
                name,
                source,
                status,
                positive,
                negative,
                oracle: row
                    .get("oracle")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                reason: row
                    .get("reason")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                phase: row.get("phase").and_then(Value::as_str).map(str::to_string),
            });
        }
        Ok(SyntaxRegister {
            reference,
            productions,
        })
    }

    /// Returns every structural problem with the register.
    ///
    /// A parsed production with no positive example is the one that matters: it
    /// is a claim of coverage with nothing behind it, which is exactly what the
    /// register exists to prevent.
    pub fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        let mut seen: Vec<&str> = Vec::new();
        for production in &self.productions {
            if seen.contains(&production.name.as_str()) {
                problems.push(format!("`{}` appears twice", production.name));
            }
            seen.push(&production.name);
            match production.status {
                SyntaxStatus::Parsed if production.positive.is_empty() => problems.push(format!(
                    "`{}` claims to be parsed but has no example",
                    production.name
                )),
                SyntaxStatus::Omitted if production.reason.is_none() => {
                    problems.push(format!("`{}` is omitted with no reason", production.name))
                }
                _ => {}
            }
        }
        problems
    }

    /// Returns whether a production can be compared against the pinned build.
    pub fn comparable(production: &Production) -> bool {
        production.oracle.is_none()
    }

    /// Returns how many examples the register carries.
    pub fn example_count(&self) -> usize {
        self.productions
            .iter()
            .map(|production| production.positive.len() + production.negative.len())
            .sum()
    }
}

/// Renders the register as a report.
pub fn report(register: &SyntaxRegister) -> String {
    let parsed = register
        .productions
        .iter()
        .filter(|production| production.status == SyntaxStatus::Parsed)
        .count();
    let omitted = register.productions.len().saturating_sub(parsed);
    let mut out = String::new();
    out.push_str("# inillucent syntax obligations\n\n");
    out.push_str(&format!(
        "Measured against {}. {} productions: {parsed} parsed, {omitted} omitted, \
         {} examples.\n\n",
        register.reference,
        register.productions.len(),
        register.example_count()
    ));
    out.push_str(
        "A production is `parsed` when the parser accepts every positive example and refuses \
         every negative one. The examples are the evidence; a row with none cannot claim \
         coverage.\n\n",
    );
    out.push_str("| Production | Status | Positive | Negative | Source |\n");
    out.push_str("|---|---|---:|---:|---|\n");
    for production in &register.productions {
        out.push_str(&format!(
            "| `{}` | {} | {} | {} | [{}]({}) |\n",
            production.name,
            production.status.as_str(),
            production.positive.len(),
            production.negative.len(),
            short_source(&production.source),
            production.source
        ));
    }
    let uncompared: Vec<&Production> = register
        .productions
        .iter()
        .filter(|production| production.oracle.is_some())
        .collect();
    if !uncompared.is_empty() {
        out.push_str("\n## Not compared against the pinned build\n\n");
        for production in uncompared {
            out.push_str(&format!(
                "- `{}` - {}\n",
                production.name,
                production.oracle.clone().unwrap_or_default()
            ));
        }
    }
    let omissions: Vec<&Production> = register
        .productions
        .iter()
        .filter(|production| production.status == SyntaxStatus::Omitted)
        .collect();
    if !omissions.is_empty() {
        out.push_str("\n## Documented omissions\n\n");
        for production in omissions {
            out.push_str(&format!(
                "- `{}` - {} (arrives in {})\n",
                production.name,
                production.reason.clone().unwrap_or_default(),
                production
                    .phase
                    .clone()
                    .unwrap_or_else(|| "a later phase".to_string())
            ));
        }
    }
    out
}

/// Shortens a documentation URL to its page name, for the report's table.
fn short_source(source: &str) -> String {
    source
        .rsplit('/')
        .next()
        .unwrap_or(source)
        .trim_end_matches(".html")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A production that claims to be parsed with no example is a claim with
    /// nothing behind it, and the register refuses it.
    #[test]
    fn a_parsed_production_with_no_example_is_a_problem() {
        let register = SyntaxRegister::parse(
            "reference = \"x\"\n\n[[production]]\nname = \"p\"\nsource = \"s\"\n\
             status = \"parsed\"\npositive = []\n",
        )
        .expect("it parses");
        assert_eq!(register.problems().len(), 1);
    }

    /// An omission has to say why.
    #[test]
    fn an_omission_must_give_a_reason() {
        let register = SyntaxRegister::parse(
            "reference = \"x\"\n\n[[production]]\nname = \"p\"\nsource = \"s\"\n\
             status = \"omitted\"\n",
        )
        .expect("it parses");
        assert_eq!(register.problems().len(), 1);

        let excused = SyntaxRegister::parse(
            "reference = \"x\"\n\n[[production]]\nname = \"p\"\nsource = \"s\"\n\
             status = \"omitted\"\nreason = \"later\"\n",
        )
        .expect("it parses");
        assert!(excused.problems().is_empty());
    }

    /// A duplicated production is caught, because two rows for one diagram let
    /// one of them rot unnoticed.
    #[test]
    fn a_duplicate_production_is_a_problem() {
        let register = SyntaxRegister::parse(
            "reference = \"x\"\n\n[[production]]\nname = \"p\"\nsource = \"s\"\n\
             status = \"parsed\"\npositive = [\"SELECT 1\"]\n\n\
             [[production]]\nname = \"p\"\nsource = \"s\"\nstatus = \"parsed\"\n\
             positive = [\"SELECT 2\"]\n",
        )
        .expect("it parses");
        assert_eq!(register.problems().len(), 1);
    }
}
