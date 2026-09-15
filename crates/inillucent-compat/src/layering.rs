//! The dependency-direction check.
//!
//! Invariant: the layer graph in `docs/invariants/layering.toml` is what the
//! workspace actually looks like. The check walks every crate manifest, builds
//! the real graph, and refuses an edge the contract does not declare.
//!
//! Two of the charter's rules are impossible to enforce by review alone and are
//! enforced here instead: no production crate may depend on another database
//! engine or SQL parser, and no production crate may depend on the test-only
//! harness crates. Both are the kind of mistake that is one `cargo add` away
//! and invisible in a diff.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::toml_lite::{self, Value};

/// What a crate is for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CrateKind {
    /// Ships in the engine.
    Production,
    /// Exists only to test the engine.
    TestOnly,
}

/// One declared crate.
#[derive(Clone, Debug)]
pub struct CrateRule {
    /// The crate name.
    pub name: String,
    /// What the crate is for.
    pub kind: CrateKind,
    /// Its position in the layer graph, for reporting.
    pub layer: i64,
    /// Test-only crates this production crate may use as a dev-dependency.
    ///
    /// Ordinarily a production crate may not name one at all, because a
    /// harness in the engine's test surface is a harness that can drift into
    /// the engine. The exception is the simulator: the TDD's Phase 2 says
    /// "`inillucent-sim` becomes a dev-dependency of `inillucent-pool`, `inillucent-tree`,
    /// `inillucent-wal` and `inillucent-txn`", because a storage layer's fault
    /// campaigns have to run against the storage layer's own private types.
    /// Naming the crates one at a time keeps that an exception rather than a
    /// hole.
    pub may_test_with: Vec<String>,
    /// The internal crates it may depend on.
    pub may_depend_on: BTreeSet<String>,
}

/// One allowed third-party crate.
#[derive(Clone, Debug)]
pub struct ExternalRule {
    /// The crate name.
    pub name: String,
    /// Why it is infrastructure rather than delegated behaviour.
    pub category: String,
    /// The first-party crates that may depend on it.
    pub allowed_in: BTreeSet<String>,
}

/// One banned dependency pattern.
#[derive(Clone, Debug)]
pub struct ForbiddenRule {
    /// A substring that must not appear in a production dependency's name.
    pub pattern: String,
    /// Why it is banned.
    pub reason: String,
}

/// The whole contract.
#[derive(Clone, Debug, Default)]
pub struct Contract {
    /// Every declared crate, by name.
    pub crates: BTreeMap<String, CrateRule>,
    /// Every allowed third-party crate, by name.
    pub externals: BTreeMap<String, ExternalRule>,
    /// Every banned pattern.
    pub forbidden: Vec<ForbiddenRule>,
}

impl Contract {
    /// Reads the contract from disk.
    pub fn load(path: &Path) -> Result<Contract, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        Contract::parse(&text)
    }

    /// Parses the contract.
    pub fn parse(text: &str) -> Result<Contract, String> {
        let document = toml_lite::parse(text)?;
        let mut contract = Contract::default();
        for row in document.array("crate") {
            let name = row
                .get("name")
                .and_then(Value::as_str)
                .ok_or("a crate row has no name")?
                .to_string();
            let kind = match row.get("kind").and_then(Value::as_str) {
                Some("production") => CrateKind::Production,
                Some("test-only") => CrateKind::TestOnly,
                other => return Err(format!("crate `{name}` has unknown kind {other:?}")),
            };
            contract.crates.insert(
                name.clone(),
                CrateRule {
                    name,
                    kind,
                    layer: row.get("layer").and_then(Value::as_integer).unwrap_or(0),
                    may_test_with: row
                        .get("may_test_with")
                        .and_then(Value::as_list)
                        .map(<[String]>::to_vec)
                        .unwrap_or_default(),
                    may_depend_on: row
                        .get("may_depend_on")
                        .and_then(Value::as_list)
                        .map(|list| list.iter().cloned().collect())
                        .unwrap_or_default(),
                },
            );
        }
        for row in document.array("external") {
            let name = row
                .get("name")
                .and_then(Value::as_str)
                .ok_or("an external row has no name")?
                .to_string();
            contract.externals.insert(
                name.clone(),
                ExternalRule {
                    name,
                    category: row
                        .get("category")
                        .and_then(Value::as_str)
                        .unwrap_or("unclassified")
                        .to_string(),
                    allowed_in: row
                        .get("allowed_in")
                        .and_then(Value::as_list)
                        .map(|list| list.iter().cloned().collect())
                        .unwrap_or_default(),
                },
            );
        }
        for row in document.array("forbidden") {
            contract.forbidden.push(ForbiddenRule {
                pattern: row
                    .get("pattern")
                    .and_then(Value::as_str)
                    .ok_or("a forbidden row has no pattern")?
                    .to_string(),
                reason: row
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("no reason recorded")
                    .to_string(),
            });
        }
        Ok(contract)
    }
}

/// One crate's declared dependencies, read from its manifest.
#[derive(Clone, Debug)]
pub struct CrateManifest {
    /// The crate name.
    pub name: String,
    /// The crates it depends on in a normal (non-dev, non-build) build.
    pub normal: BTreeSet<String>,
    /// The crates it depends on only for its own tests.
    pub development: BTreeSet<String>,
}

/// Reads the `[workspace] members` list out of the root manifest.
///
/// This is the list `cargo` itself resolves before it compiles a line, so a
/// member naming a directory that is not in the repository is not a slow build
/// or a missing feature - it is `cargo metadata` exiting 101 on a fresh clone,
/// which is what `3969906` did when it added `drivers/` without committing it.
/// The list is read by hand rather than through `cargo metadata` for the same
/// reason `read_workspace` parses manifests: the check has to be able to run
/// when the workspace does *not* resolve, which is precisely the case it exists
/// to catch.
pub fn workspace_members(root: &Path) -> Result<Vec<String>, String> {
    let path = root.join("Cargo.toml");
    let text = std::fs::read_to_string(&path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let mut members = Vec::new();
    let mut section = String::new();
    let mut in_members = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') && !in_members {
            section = line
                .trim_start_matches('[')
                .trim_end_matches(']')
                .to_string();
            continue;
        }
        if section != "workspace" {
            continue;
        }
        if !in_members {
            let Some(rest) = line.strip_prefix("members") else {
                continue;
            };
            let Some(rest) = rest.trim_start().strip_prefix('=') else {
                continue;
            };
            let rest = rest.trim_start();
            if !rest.starts_with('[') {
                return Err("[workspace] members is not an inline array".to_string());
            }
            in_members = true;
            collect_member_entries(&rest[1..], &mut members);
            if rest.contains(']') {
                in_members = false;
            }
            continue;
        }
        collect_member_entries(line, &mut members);
        if line.contains(']') {
            in_members = false;
        }
    }
    if members.is_empty() {
        return Err(format!("{} declares no workspace members", path.display()));
    }
    Ok(members)
}

/// Pulls the quoted paths out of one line of a `members = [...]` array.
fn collect_member_entries(line: &str, members: &mut Vec<String>) {
    let line = line.split('#').next().unwrap_or("");
    let mut rest = line;
    while let Some(open) = rest.find('"') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('"') else {
            return;
        };
        members.push(after[..close].to_string());
        rest = &after[close + 1..];
    }
}

/// Reports every workspace member whose directory is missing or holds no
/// `Cargo.toml`.
///
/// An empty result is the invariant: the repository builds from a clone.
pub fn check_member_paths(root: &Path, members: &[String]) -> Vec<String> {
    let mut problems = Vec::new();
    for member in members {
        let directory = root.join(member);
        if !directory.is_dir() {
            problems.push(format!(
                "workspace member `{member}` names a directory that does not exist: {}",
                directory.display()
            ));
            continue;
        }
        if !directory.join("Cargo.toml").is_file() {
            problems.push(format!(
                "workspace member `{member}` has no Cargo.toml at {}",
                directory.join("Cargo.toml").display()
            ));
        }
    }
    problems
}

/// Reads every workspace member's manifest.
///
/// The manifests are parsed rather than `cargo metadata` being invoked because
/// the check has to run without a network and without a resolved lockfile, and
/// because the declared edge is what the contract is about: a transitive edge
/// through an allowed crate is not a violation.
pub fn read_workspace(root: &Path) -> Result<Vec<CrateManifest>, String> {
    let crates_dir = root.join("crates");
    let entries = std::fs::read_dir(&crates_dir)
        .map_err(|error| format!("cannot read {}: {error}", crates_dir.display()))?;
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.join("Cargo.toml").is_file())
        .collect();
    paths.sort();
    let mut manifests = Vec::new();
    for path in paths {
        manifests.push(read_manifest(&path.join("Cargo.toml"))?);
    }
    Ok(manifests)
}

/// Reads the manifest of every crate the root `[workspace] members` list names,
/// wherever in the repository it lives.
///
/// [`read_workspace`] walks `crates/` and is what the layering contract is
/// checked against, because the contract is about that directory. Test
/// selection is a different question: it needs the graph the *workspace* has,
/// and that now includes `drivers/`, whose two crates `read_workspace`
/// cannot see. A driver change that selected no tests would be the selector
/// failing silently, which is the one way a selector must never fail.
///
/// @param root - the workspace root
/// @param members - the member paths, as [`workspace_members`] returned them
pub fn read_members(root: &Path, members: &[String]) -> Result<Vec<CrateManifest>, String> {
    let mut manifests = Vec::new();
    for member in members {
        let manifest = root.join(member).join("Cargo.toml");
        if !manifest.is_file() {
            return Err(format!(
                "workspace member `{member}` has no Cargo.toml at {}",
                manifest.display()
            ));
        }
        manifests.push(read_manifest(&manifest)?);
    }
    // **In member order, deliberately not sorted.** `selection::seeds_of` pairs
    // this list with the member paths positionally to learn which directory
    // holds which package - the two are not the same string, because
    // `crates/inillucent-cli` builds a binary called `inillucent-shell` and the
    // drivers live outside `crates/`. Sorting here silently scrambled that
    // pairing, and the symptom was a selector that answered backwards: a change
    // to the base crate selected 8 targets and a change to the leaf selected 65.
    Ok(manifests)
}

/// Reads one crate manifest's dependency sections.
///
/// This is a deliberately small reader for the shapes the workspace uses:
/// `[dependencies]`, `[dev-dependencies]`, and target-specific variants of
/// both. Anything else is reported rather than ignored.
pub fn read_manifest(path: &Path) -> Result<CrateManifest, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let mut name = String::new();
    let mut normal = BTreeSet::new();
    let mut development = BTreeSet::new();
    let mut section = String::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            section = line
                .trim_matches(|character| character == '[' || character == ']')
                .to_string();
            continue;
        }
        let Some((key, _)) = line.split_once('=') else {
            continue;
        };
        // `serde.workspace = true` names the crate `serde`; the dotted suffix
        // is cargo syntax, not part of the dependency's name.
        let key = key
            .trim()
            .split('.')
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        if section == "package" && key == "name" {
            name = line
                .split('"')
                .nth(1)
                .ok_or_else(|| format!("{}: package name is not a string", path.display()))?
                .to_string();
            continue;
        }
        if section.ends_with("dev-dependencies") {
            development.insert(key);
        } else if section.ends_with("dependencies") {
            normal.insert(key);
        }
    }
    if name.is_empty() {
        return Err(format!("{}: no package name", path.display()));
    }
    Ok(CrateManifest {
        name,
        normal,
        development,
    })
}

/// Checks the real workspace against the contract, returning every violation.
pub fn check(contract: &Contract, manifests: &[CrateManifest]) -> Vec<String> {
    let mut violations = Vec::new();
    let known: BTreeSet<&str> = contract.crates.keys().map(String::as_str).collect();

    for manifest in manifests {
        let Some(rule) = contract.crates.get(&manifest.name) else {
            violations.push(format!(
                "crate `{}` is not declared in docs/invariants/layering.toml",
                manifest.name
            ));
            continue;
        };
        for dependency in &manifest.normal {
            if known.contains(dependency.as_str()) {
                check_internal_edge(contract, rule, dependency, &mut violations);
                continue;
            }
            check_external_edge(contract, rule, dependency, &mut violations);
        }
        for dependency in manifest
            .development
            .iter()
            .filter(|name| known.contains(name.as_str()))
        {
            let Some(target) = contract.crates.get(dependency) else {
                continue;
            };
            if rule.kind == CrateKind::Production
                && target.kind == CrateKind::TestOnly
                && !rule.may_test_with.iter().any(|name| name == dependency)
            {
                violations.push(format!(
                    "production crate `{}` uses test-only crate `{dependency}` even as a dev-dependency, which puts harness code in the engine's test surface",
                    rule.name
                ));
            }
            check_development_edge(rule, target, &mut violations);
        }
        unused_allowances(rule, manifest, &mut violations);
    }
    violations.extend(find_cycles(contract));
    violations
}

/// Reports an allowance the crate does not use.
///
/// **A contract that over-describes is not true (task-1962, A13).** The check
/// reported an edge the contract forbids and never an edge the contract allows
/// that nobody takes, so `inillucent-catalog`'s rule could name
/// `inillucent-transaction` - a dependency it does not have - and the file still
/// read as a description of the workspace. A reader deciding whether a new call
/// is allowed reads the rule, not the manifest, so a row that is there for no
/// reason is a row that permits something nobody decided to permit.
///
/// A crate's own name is skipped: a rule naming itself is how a crate with no
/// first-party dependencies is written.
///
/// @param rule - the crate's layering rule
/// @param manifest - what its `Cargo.toml` actually declares
/// @param violations - where a finding is recorded
fn unused_allowances(rule: &CrateRule, manifest: &CrateManifest, violations: &mut Vec<String>) {
    for allowed in &rule.may_depend_on {
        if allowed == &rule.name
            || manifest.normal.contains(allowed)
            || manifest.development.contains(allowed)
        {
            continue;
        }
        violations.push(format!(
            "crate `{}` is allowed to depend on `{allowed}` and does not, so remove the row from docs/invariants/layering.toml",
            rule.name
        ));
    }
}

/// Checks one dev-dependency edge between two first-party crates.
///
/// **The layer rule is relaxed and the direction rule is kept (task-1962,
/// A13).** A test may reach for a crate its production code does not, which is
/// why a dev edge is not held to `may_depend_on` - `inillucent-exec` and
/// `inillucent-tree` each build a database over `inillucent-vfs` in a test and
/// neither depends on it otherwise. What it may not do is reach *upward*: a
/// crate whose tests depend on one above it is a cycle in everything but the
/// production graph, and it makes the lower crate impossible to test in
/// isolation, which is the property the layering exists for.
///
/// A test-only crate is skipped: it is not in the production graph at all, so
/// the layer numbers do not order it against a production crate, and the edge
/// to one is what `may_test_with` above governs. `inillucent-pool`,
/// `inillucent-txn` and `inillucent-wal` each drive `inillucent-sim` in a test
/// for exactly that reason.
///
/// @param rule - the depending crate's layering rule
/// @param target - the rule of the crate it depends on
/// @param violations - where a finding is recorded
fn check_development_edge(rule: &CrateRule, target: &CrateRule, violations: &mut Vec<String>) {
    if target.kind == CrateKind::TestOnly {
        return;
    }
    if target.layer >= rule.layer && rule.name != target.name {
        violations.push(format!(
            "crate `{}` (layer {}) has a dev-dependency on `{}` (layer {}), which is not below it",
            rule.name, rule.layer, target.name, target.layer
        ));
    }
}

/// Checks one edge between two first-party crates.
fn check_internal_edge(
    contract: &Contract,
    rule: &CrateRule,
    dependency: &str,
    violations: &mut Vec<String>,
) {
    if !rule.may_depend_on.contains(dependency) {
        violations.push(format!(
            "crate `{}` depends on `{dependency}`, which its layering rule does not allow",
            rule.name
        ));
        return;
    }
    let Some(target) = contract.crates.get(dependency) else {
        return;
    };
    if rule.kind == CrateKind::Production && target.kind == CrateKind::TestOnly {
        violations.push(format!(
            "production crate `{}` depends on test-only crate `{dependency}`",
            rule.name
        ));
    }
    if target.layer >= rule.layer && rule.name != dependency {
        violations.push(format!(
            "crate `{}` (layer {}) depends on `{dependency}` (layer {}), which is not below it",
            rule.name, rule.layer, target.layer
        ));
    }
}

/// Checks one edge to a third-party crate.
fn check_external_edge(
    contract: &Contract,
    rule: &CrateRule,
    dependency: &str,
    violations: &mut Vec<String>,
) {
    let lowered = dependency.to_ascii_lowercase();
    for forbidden in &contract.forbidden {
        if lowered.contains(&forbidden.pattern) {
            violations.push(format!(
                "crate `{}` depends on `{dependency}`, which is forbidden: {}",
                rule.name, forbidden.reason
            ));
            return;
        }
    }
    let Some(external) = contract.externals.get(dependency) else {
        violations.push(format!(
            "crate `{}` depends on third-party crate `{dependency}`, which docs/dependency-policy.md does not approve",
            rule.name
        ));
        return;
    };
    if !external.allowed_in.contains(&rule.name) {
        violations.push(format!(
            "crate `{}` depends on `{dependency}`, which is approved only for {:?}",
            rule.name, external.allowed_in
        ));
    }
}

/// Reports any cycle in the declared graph.
///
/// The layer numbers already forbid a cycle, but they are a human-maintained
/// field; walking the graph catches a cycle introduced by two rules that were
/// each individually plausible.
fn find_cycles(contract: &Contract) -> Vec<String> {
    let mut violations = Vec::new();
    for start in contract.crates.keys() {
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        let mut frontier: Vec<&str> = contract
            .crates
            .get(start)
            .map(|rule| rule.may_depend_on.iter().map(String::as_str).collect())
            .unwrap_or_default();
        while let Some(next) = frontier.pop() {
            if next == start.as_str() {
                violations.push(format!("crate `{start}` is in a dependency cycle"));
                break;
            }
            if !seen.insert(next) {
                continue;
            }
            if let Some(rule) = contract.crates.get(next) {
                frontier.extend(rule.may_depend_on.iter().map(String::as_str));
            }
        }
    }
    violations
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a two-crate contract for a test.
    fn contract() -> Contract {
        Contract::parse(
            "[[crate]]\nname = \"low\"\nkind = \"production\"\nlayer = 0\nmay_depend_on = []\n\n\
             [[crate]]\nname = \"high\"\nkind = \"production\"\nlayer = 1\nmay_depend_on = [\"low\"]\n\n\
             [[crate]]\nname = \"harness\"\nkind = \"test-only\"\nlayer = 9\nmay_depend_on = [\"low\", \"high\"]\n\n\
             [[external]]\nname = \"libc\"\ncategory = \"os-boundary\"\nallowed_in = [\"low\"]\n\n\
             [[forbidden]]\npattern = \"sqlite\"\nreason = \"another engine\"\n",
        )
        .expect("the contract parses")
    }

    /// Builds a manifest for a test.
    fn manifest(name: &str, normal: &[&str]) -> CrateManifest {
        CrateManifest {
            name: name.to_string(),
            normal: normal.iter().map(|item| item.to_string()).collect(),
            development: BTreeSet::new(),
        }
    }

    /// The declared graph must pass its own check.
    #[test]
    fn a_conforming_workspace_passes() {
        let violations = check(
            &contract(),
            &[
                manifest("low", &["libc"]),
                manifest("high", &["low"]),
                manifest("harness", &["low", "high"]),
            ],
        );
        assert!(violations.is_empty(), "{violations:?}");
    }

    /// An edge that points upward is what the whole contract exists to stop.
    #[test]
    fn an_upward_edge_is_refused() {
        let violations = check(&contract(), &[manifest("low", &["high"])]);
        assert!(violations
            .iter()
            .any(|violation| violation.contains("does not allow")));
    }

    /// A production crate reaching into the harness is refused, including as a
    /// dev-dependency.
    #[test]
    fn a_production_crate_may_not_use_the_harness() {
        let mut low = manifest("low", &[]);
        low.development.insert("harness".to_string());
        let violations = check(&contract(), &[low]);
        assert!(violations
            .iter()
            .any(|violation| violation.contains("test-only")));
    }

    /// Another database engine is refused by name, whatever it is called.
    #[test]
    fn another_engine_is_refused() {
        let violations = check(&contract(), &[manifest("low", &["rusqlite"])]);
        assert!(violations
            .iter()
            .any(|violation| violation.contains("forbidden")));
    }

    /// An unapproved third-party crate is refused even when it is harmless,
    /// because the policy is an allow-list rather than a deny-list.
    #[test]
    fn an_unapproved_dependency_is_refused() {
        let violations = check(&contract(), &[manifest("low", &["itertools"])]);
        assert!(violations
            .iter()
            .any(|violation| violation.contains("does not approve")));
    }

    /// An approved crate in the wrong place is still refused: `libc` belongs to
    /// the VFS layer and nowhere else.
    #[test]
    fn an_approved_dependency_in_the_wrong_crate_is_refused() {
        let violations = check(&contract(), &[manifest("high", &["low", "libc"])]);
        assert!(violations
            .iter()
            .any(|violation| violation.contains("approved only for")));
    }

    /// A cycle in the declared rules must be reported even when each rule looks
    /// reasonable on its own.
    #[test]
    fn a_cycle_is_reported() {
        let cyclic = Contract::parse(
            "[[crate]]\nname = \"a\"\nkind = \"production\"\nlayer = 0\nmay_depend_on = [\"b\"]\n\n\
             [[crate]]\nname = \"b\"\nkind = \"production\"\nlayer = 1\nmay_depend_on = [\"a\"]\n",
        )
        .expect("the contract parses");
        let violations = find_cycles(&cyclic);
        assert!(!violations.is_empty(), "the cycle was not reported");
    }

    /// The members list is read out of the real root manifest, including the
    /// multi-line, comment-interleaved form the workspace actually uses.
    #[test]
    fn the_members_list_reads_from_the_root_manifest() {
        let members = workspace_members(&crate::workspace_root()).expect("the manifest parses");
        assert!(members
            .iter()
            .any(|member| member == "crates/inillucent-base"));
        assert!(members
            .iter()
            .any(|member| member == "crates/inillucent-compat"));
        assert!(
            !members.iter().any(|member| member.contains('#')),
            "a comment leaked into the members list: {members:?}"
        );
    }

    /// A member naming a directory that is not in the repository is what makes
    /// `cargo metadata` exit 101 on a clone, so it has to be reported.
    #[test]
    fn a_member_whose_directory_is_absent_is_reported() {
        let root = crate::workspace_root();
        let problems = check_member_paths(
            &root,
            &[
                "crates/inillucent-base".to_string(),
                "drivers/there-is-no-such-crate".to_string(),
            ],
        );
        assert_eq!(problems.len(), 1, "{problems:#?}");
        assert!(problems[0].contains("there-is-no-such-crate"));
    }

    /// A directory that exists but holds no manifest fails the same way cargo
    /// does, and has to be reported separately from an absent one.
    #[test]
    fn a_member_directory_without_a_manifest_is_reported() {
        let root = crate::workspace_root();
        let problems = check_member_paths(&root, &["crates".to_string()]);
        assert_eq!(problems.len(), 1, "{problems:#?}");
        assert!(problems[0].contains("no Cargo.toml"));
    }
}
