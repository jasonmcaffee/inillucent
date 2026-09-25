//! Which tests a change has to run, and which it does not.
//!
//! Invariant: **the map in `tests/selection.toml` names every test target in
//! the workspace exactly once, and `discover` is what proves it.** A selector
//! whose map has drifted does not report an error - it silently runs less than
//! it should, and a suite that stops being selected stops being a test. So the
//! map is compared against the file system rather than trusted, by
//! `crates/inillucent-compat/tests/tooling/selection.rs`, and a target added without a
//! row fails that test on the machine that added it.
//!
//! ## Why a declared map rather than a derived one
//!
//! Two thirds of this workspace's assertions live in one crate. `inillucent-compat`
//! holds 74 integration suites, and every one of them depends on
//! `inillucent-compat`, so a graph walk would answer "a change to the tree
//! layer selects all 74" - which is the same as having no selector at all.
//!
//! Nor can the answer be read out of each suite's `use` statements. The suites
//! that matter most do not import the engine: `semantics`, `cli`, `pragma`,
//! `json` and `fts5` drive the *shell* over a pipe, so their imports name
//! `inillucent-compat` and nothing else while what they actually exercise is
//! the whole engine end to end. Deriving coverage from imports would drop
//! exactly the suites that cover the most.
//!
//! So coverage is **declared**, once, per target, and the two failure modes
//! that a declaration invites are both closed by a test: a target with no row
//! fails, and a row naming a package that does not exist fails.
//!
//! ## What selection is allowed to get wrong
//!
//! Selecting too much costs time. Selecting too little costs a defect that
//! reaches `main`, so every judgement call in here is deliberately biased
//! toward running more:
//!
//! - a changed file outside every crate selects **everything** unless a
//!   `[[path]]` row says narrower, so a new top-level directory is safe by
//!   default rather than invisible;
//! - the closure is over **reverse** dependencies, so a change to the base
//!   crate selects every crate above it;
//! - `dev-dependencies` count as edges, because a change to the simulator has
//!   to re-run the campaigns that inject faults through it.
//!
//! ## Cadence: when a tier runs
//!
//! Every tier declares a [`Cadence`]: `change`, `merge` or `nightly`. The
//! dependency closure answers "what can this change break", and the cadence
//! answers "is this the run that should find out". They are separate because
//! the closure alone selected 178 of 231 targets for a four file change in
//! `inillucent-sql`, one of them a 3,991 s nightly story, and it ran the crash
//! suites on a parser edit because 23 of them named `inillucent-engine`, which
//! the closure reaches from almost anywhere.
//!
//! - A `change` tier row is selected by the closure, as it always was.
//! - A `merge` tier row is selected in a change run only when a package that
//!   actually changed (a **seed**, before the closure) is one it covers, or its
//!   own crate or its own file changed. CI's merge run and the nightly select it
//!   by the closure. So a crash suite runs for the ticket that edits the log,
//!   and on every merge, and not for the ticket that edits the parser.
//! - A `nightly` tier row is never selected by a change run. It runs on the
//!   schedule, or when it is named with `--tier` or `--target`.
//!
//! What this costs is written down in `tasks/task-2114-inillucent-build-times-tdd.md`
//! section 6.2: a merge row whose `covers` is too narrow is caught by the merge
//! run in CI and by the nightly, hours later rather than minutes. A change run
//! was never the only run.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::layering::CrateManifest;
use crate::toml_lite::{self, Value};

/// What kind of target carries the tests.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Kind {
    /// The crate's own `#[cfg(test)]` modules, compiled into the lib harness.
    Lib,
    /// One file under the crate's `tests/` directory.
    Test,
    /// A binary's own `#[cfg(test)]` modules.
    ///
    /// Most binaries here hold none - they are benchmark and profiling
    /// instruments - but two crates in this workspace have **no library at
    /// all** and keep every assertion they own in a binary: `inillucent-bench`
    /// holds 160 tests and `inillucent-cli` holds 19. A runner that skipped bin
    /// targets on the reasonable-sounding grounds that binaries do not hold
    /// tests would drop those 179 silently, which is why the selection suite
    /// checks the claim per file instead of believing it.
    Bin,
}

impl Kind {
    /// Returns the word the map spells this kind with.
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Lib => "lib",
            Kind::Test => "test",
            Kind::Bin => "bin",
        }
    }

    /// Reads a kind from the map's spelling.
    ///
    /// @param text - the word in the map
    pub fn parse(text: &str) -> Option<Kind> {
        match text {
            "lib" => Some(Kind::Lib),
            "test" => Some(Kind::Test),
            "bin" => Some(Kind::Bin),
            _ => None,
        }
    }
}

/// One test target: a binary `cargo test` builds and runs.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Target {
    /// The crate the target belongs to.
    pub package: String,
    /// Whether it is the lib harness or an integration file.
    pub kind: Kind,
    /// The target name, which for [`Kind::Test`] is the file stem, and the
    /// name of the binary cargo builds.
    pub name: String,
    /// The module inside that binary, when the binary holds one suite per
    /// module.
    ///
    /// `inillucent-compat`'s integration tests are one binary per tier, and
    /// each former file is a module of it: `tests/differential.rs` declares
    /// `mod semantics;` and the suite is `tests/differential/semantics.rs`. A
    /// target is still one suite, so the runner starts the binary once per
    /// module with that module's test names and `--exact`, and the suite keeps
    /// its own process, timing row and kill budget. One binary instead of 149
    /// is one link of the compat library and its 23 crates instead of 149.
    pub module: Option<String>,
}

impl Target {
    /// Returns the `package::name` form used in reports and on the command
    /// line, or `package::name::module` for a module target.
    pub fn label(&self) -> String {
        match &self.module {
            Some(module) => format!("{}::{}::{module}", self.package, self.name),
            None => format!("{}::{}", self.package, self.name),
        }
    }

    /// Returns the binary this target runs in: itself with no module.
    pub fn binary(&self) -> Target {
        Target {
            package: self.package.clone(),
            kind: self.kind,
            name: self.name.clone(),
            module: None,
        }
    }
}

/// One row of the map: a target, the tier it runs in, and what it covers.
#[derive(Clone, Debug)]
pub struct Row {
    /// The target this row is about.
    pub target: Target,
    /// The tier it belongs to.
    pub tier: String,
    /// The packages whose change must re-run it.
    pub covers: BTreeSet<String>,
    /// External prerequisites without which it evidences nothing.
    pub requires: Vec<String>,
    /// The cargo features this target is built and run with.
    ///
    /// **A test behind a feature the build does not turn on is in no binary,
    /// and it reads as coverage (task-1913).** `cargo test --workspace` builds
    /// with default features, so `inillucent-core`'s twenty-seven `onnx` tests
    /// and `inillucent-search`'s three `embed` tests were in the source, were
    /// counted by nobody, and had never run. `inillucent-search`'s own
    /// `lib.rs` already says why that is worse than a missing test - it pulled
    /// `embed_refusal` out from behind the feature for exactly this reason -
    /// and this is the field that lets the runner build the rest of them.
    ///
    /// Each entry is written the way cargo takes it in a workspace build:
    /// `package/feature`. Empty for every target that needs none, which is all
    /// but two of them.
    pub features: Vec<String>,
    /// Whether this one target needs the run to itself.
    ///
    /// **`Tier::exclusive` is the same idea one level up, and the level is what
    /// makes it the wrong tool here** (task-2066 §4.4.16). A tier that measures
    /// time cannot share a machine, so every target in it runs alone. A target
    /// that starts a *nested* run has a narrower problem: it asks cargo what it
    /// built, cargo answers by building, and Windows will not replace an image
    /// a sibling target is executing. `gates_fail_closed` is the only target in
    /// the tree that does this, and it sits in `tooling` beside forty targets
    /// that have no reason to run one at a time.
    ///
    /// `gates_fail_closed` used to set it, until its nested runner stopped
    /// starting cargo: it now reads the outer run's artifact list (see
    /// `testplan`), so there is no relink to race and it runs beside the rest.
    /// `bindings` sets it for a different reason: it grades records two other
    /// targets write, so it has to run after them.
    pub alone: bool,
    /// Other targets, by label, whose executables this suite starts itself.
    ///
    /// `gates_fail_closed` runs a nested `inillucent-testrun` over the smoke
    /// tier and the two live database suites. The nested runner reads the
    /// outer runner's artifact list and starts no cargo, so those executables
    /// have to be built by the outer run whether or not it selected them.
    /// `testplan::build_set` adds them to the build and not to the run.
    pub builds: Vec<String>,
}

/// When a tier runs.
///
/// Ordered: a run at one cadence includes every tier at that cadence and below
/// it, so `Merge` includes the `change` tiers and `Nightly` includes everything.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Cadence {
    /// On every change, in the ticket loop: `inillucent-testrun --changed`.
    Change,
    /// On every merge, in CI; in a change run only for a seed the row covers.
    Merge,
    /// On the schedule only, or when named.
    Nightly,
}

impl Cadence {
    /// Returns the word the map and the command line spell this cadence with.
    pub fn as_str(self) -> &'static str {
        match self {
            Cadence::Change => "change",
            Cadence::Merge => "merge",
            Cadence::Nightly => "nightly",
        }
    }

    /// Reads a cadence from the map's or the command line's spelling.
    ///
    /// @param text - the word
    pub fn parse(text: &str) -> Option<Cadence> {
        match text {
            "change" => Some(Cadence::Change),
            "merge" => Some(Cadence::Merge),
            "nightly" => Some(Cadence::Nightly),
            _ => None,
        }
    }
}

/// One tier: a named reason to run a subset.
#[derive(Clone, Debug)]
pub struct Tier {
    /// The tier name, as `--tier` takes it.
    pub name: String,
    /// What the tier is for, printed by `--list-tiers`.
    pub purpose: String,
    /// Whether this tier needs the machine to itself.
    ///
    /// A tier that measures *time* cannot share a machine with a hundred other
    /// test binaries, and the failure is not a flake to be tuned away - it is
    /// the measurement being of the wrong thing. The `perf` tier's
    /// "one transaction beats many" guard reads 40x on an idle machine, 3.5x
    /// with twenty-four binaries in flight, and **1.3x** under a full run,
    /// because at saturation both arms are dominated by scheduling rather than
    /// by commits. No threshold distinguishes "batching works" from "batching
    /// was removed" in that state.
    ///
    /// So the runner finishes everything else first and then runs an exclusive
    /// tier on its own, one binary at a time **and one thread inside each
    /// binary**. The second half was missing until task-1886: the tier got the
    /// machine to itself among the binaries and then ran its own six guards two
    /// at a time against each other, which is the same contention at a smaller
    /// scale. It costs the length of that tier - about half a minute here.
    ///
    /// **What it cannot do is give the tier the box.** Four agent terminals and
    /// a training run are outside this runner's reach, and that is the load that
    /// actually broke `inillucent::budget` twice. Exclusivity is worth having
    /// and it is not a fix; the fix was to stop asserting on a clock at all, and
    /// `crates/inillucent/tests/budget.rs` now asserts on counts. This flag
    /// protects the one ceiling left in that file.
    pub exclusive: bool,
    /// When the tier runs: see [`Cadence`] and this module's documentation.
    ///
    /// Required in the map. A tier with no cadence would have to default to
    /// one, and either default is wrong for some tier: `change` puts a nightly
    /// story back into every ticket, and `nightly` stops a new tier running on
    /// a change without anybody deciding that it should not.
    pub cadence: Cadence,
}

/// One non-crate path prefix, and what changing it selects.
#[derive(Clone, Debug)]
pub struct PathRule {
    /// The repository-relative prefix.
    pub prefix: String,
    /// The packages a change under it selects; `*` means every package.
    pub packages: BTreeSet<String>,
    /// Why the rule is what it is.
    pub reason: String,
}

/// The whole map.
#[derive(Clone, Debug, Default)]
pub struct Map {
    /// Every declared tier, in the order the file lists them.
    pub tiers: Vec<Tier>,
    /// Every declared target.
    pub rows: Vec<Row>,
    /// Every declared non-crate path rule.
    pub paths: Vec<PathRule>,
}

impl Map {
    /// Reads the map from disk.
    ///
    /// @param path - the `tests/selection.toml` file
    pub fn load(path: &Path) -> Result<Map, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        Map::parse(&text)
    }

    /// Parses the map, reporting anything it does not understand.
    ///
    /// @param text - the map's text
    pub fn parse(text: &str) -> Result<Map, String> {
        let document = toml_lite::parse(text)?;
        let mut map = Map::default();
        for row in document.array("tier") {
            let name = row
                .get("name")
                .and_then(Value::as_str)
                .ok_or("a tier row has no name")?
                .to_string();
            let purpose = row
                .get("purpose")
                .and_then(Value::as_str)
                .unwrap_or("no purpose recorded")
                .to_string();
            let exclusive = row
                .get("exclusive")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let cadence_text = row
                .get("cadence")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("tier `{name}` has no cadence"))?;
            let cadence = Cadence::parse(cadence_text).ok_or_else(|| {
                format!("tier `{name}` has cadence `{cadence_text}`; it must be change, merge or nightly")
            })?;
            map.tiers.push(Tier {
                name,
                purpose,
                exclusive,
                cadence,
            });
        }
        for row in document.array("target") {
            let package = row
                .get("package")
                .and_then(Value::as_str)
                .ok_or("a target row has no package")?
                .to_string();
            let kind_text = row
                .get("kind")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("target row for `{package}` has no kind"))?;
            let kind = Kind::parse(kind_text).ok_or_else(|| {
                format!("target row for `{package}` has unknown kind `{kind_text}`")
            })?;
            let name = match kind {
                Kind::Lib => "lib".to_string(),
                Kind::Test | Kind::Bin => row
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| format!("{kind_text} row for `{package}` has no name"))?
                    .to_string(),
            };
            let tier = row
                .get("tier")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("target row for `{package}::{name}` has no tier"))?
                .to_string();
            let covers: BTreeSet<String> = row
                .get("covers")
                .and_then(Value::as_list)
                .map(|list| list.iter().cloned().collect())
                .unwrap_or_else(|| {
                    let mut only = BTreeSet::new();
                    only.insert(package.clone());
                    only
                });
            let requires = row
                .get("requires")
                .and_then(Value::as_list)
                .map(<[String]>::to_vec)
                .unwrap_or_default();
            let features = row
                .get("features")
                .and_then(Value::as_list)
                .map(<[String]>::to_vec)
                .unwrap_or_default();
            let alone = row.get("alone").and_then(Value::as_bool).unwrap_or(false);
            let builds = row
                .get("builds")
                .and_then(Value::as_list)
                .map(<[String]>::to_vec)
                .unwrap_or_default();
            let module = row
                .get("module")
                .and_then(Value::as_str)
                .map(str::to_string);
            if module.is_some() && kind != Kind::Test {
                return Err(format!(
                    "{kind_text} row for `{package}::{name}` has a module; only a test binary holds modules"
                ));
            }
            map.rows.push(Row {
                target: Target {
                    package,
                    kind,
                    name,
                    module,
                },
                tier,
                covers,
                requires,
                features,
                alone,
                builds,
            });
        }
        for row in document.array("path") {
            let prefix = row
                .get("prefix")
                .and_then(Value::as_str)
                .ok_or("a path row has no prefix")?
                .to_string();
            let packages = row
                .get("packages")
                .and_then(Value::as_list)
                .map(|list| list.iter().cloned().collect())
                .unwrap_or_default();
            let reason = row
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("no reason recorded")
                .to_string();
            map.paths.push(PathRule {
                prefix,
                packages,
                reason,
            });
        }
        if map.rows.is_empty() {
            return Err("the map declares no targets".to_string());
        }
        Ok(map)
    }

    /// Returns the row for one target, if the map has one.
    ///
    /// @param target - the target to look up
    pub fn row(&self, target: &Target) -> Option<&Row> {
        self.rows.iter().find(|row| &row.target == target)
    }

    /// Returns the cadence of a tier by name.
    ///
    /// A tier the map does not declare counts as `change`, so a row in an
    /// undeclared tier is still selected by the closure. `every_tier_is_declared`
    /// in the selection suite fails on such a row anyway.
    ///
    /// @param tier - the tier name
    pub fn cadence_of(&self, tier: &str) -> Cadence {
        self.tiers
            .iter()
            .find(|declared| declared.name == tier)
            .map(|declared| declared.cadence)
            .unwrap_or(Cadence::Change)
    }

    /// Returns every row whose tier runs at a cadence or below it.
    ///
    /// This is the selection for a run with no `--changed`: `Merge` is every
    /// tier except `nightly`, and `Nightly` is the whole map.
    ///
    /// @param cadence - the highest cadence to include
    pub fn rows_up_to(&self, cadence: Cadence) -> Vec<&Row> {
        self.rows
            .iter()
            .filter(|row| self.cadence_of(&row.tier) <= cadence)
            .collect()
    }

    /// Returns every tier name the rows use, whether or not it is declared.
    pub fn tiers_used(&self) -> BTreeSet<String> {
        self.rows.iter().map(|row| row.tier.clone()).collect()
    }

    /// Returns every prerequisite name the rows declare in `requires`.
    ///
    /// This is the full list `inillucent-testrun --absent` accepts. A name
    /// outside it is refused, so a misspelt `--absent postgress` cannot excuse
    /// nothing while looking as if it excused something.
    pub fn prerequisites(&self) -> BTreeSet<String> {
        self.rows
            .iter()
            .flat_map(|row| row.requires.iter().cloned())
            .collect()
    }
}

impl Row {
    /// Reports whether this row requires one of the prerequisites a machine
    /// has said it does not have.
    ///
    /// The match is on the row, not on the suite's own skip sentence, because
    /// the sentence is prose and the row is a name. The cost is that a suite
    /// requiring two things, one of them declared absent, is excused whichever
    /// of the two it was missing. That is why `--absent` is for things a
    /// machine cannot have, such as a private checkout, and not for things a
    /// setup step forgot to build.
    ///
    /// @param absent - the prerequisites declared absent with `--absent`
    pub fn needs_any_of(&self, absent: &[String]) -> bool {
        self.requires.iter().any(|name| absent.contains(name))
    }
}

/// Finds every test target `cargo test` would build, by walking the repository.
///
/// This is the half of the contract that cannot be declared: it is what cargo
/// will actually build, read off the file system, so that the map can be
/// compared against reality rather than against itself.
///
/// All three kinds come back, **including binaries**. That is not a detail: two
/// crates here have no library at all and keep every test they own in a bin
/// target, so a discovery that returned only libraries and `tests/` files would
/// have declared the map complete while 179 tests sat outside it.
///
/// @param root - the workspace root
/// @param members - the member paths, as `layering::workspace_members` returned them
pub fn discover(root: &Path, members: &[String]) -> Result<Vec<Target>, String> {
    let mut targets = Vec::new();
    for member in members {
        let directory = root.join(member);
        let package = read_package_name(&directory.join("Cargo.toml"))?;
        if directory.join("src/lib.rs").is_file() {
            targets.push(Target {
                package: package.clone(),
                kind: Kind::Lib,
                name: "lib".to_string(),
                module: None,
            });
        }
        for name in binary_names(&directory, &package)? {
            targets.push(Target {
                package: package.clone(),
                kind: Kind::Bin,
                name,
                module: None,
            });
        }
        for (target, _) in integration_targets(&directory, &package)? {
            targets.push(target);
        }
    }
    targets.sort();
    Ok(targets)
}

/// Returns one crate's integration targets, each with the file that holds it.
///
/// A `tests/<name>.rs` is one target, unless it is a binary of suite modules:
/// then each module is a target and its file is `tests/<name>/<module>.rs`.
///
/// @param directory - the crate directory
/// @param package - the crate's package name
pub fn integration_targets(
    directory: &Path,
    package: &str,
) -> Result<Vec<(Target, PathBuf)>, String> {
    let mut found = Vec::new();
    for name in integration_names(directory)? {
        let root = integration_root(directory, &name);
        let modules = suite_modules(directory, &name, &root)?;
        if modules.is_empty() {
            found.push((
                Target {
                    package: package.to_string(),
                    kind: Kind::Test,
                    name,
                    module: None,
                },
                root,
            ));
            continue;
        }
        for module in modules {
            let file = directory
                .join("tests")
                .join(&name)
                .join(format!("{module}.rs"));
            found.push((
                Target {
                    package: package.to_string(),
                    kind: Kind::Test,
                    name: name.clone(),
                    module: Some(module),
                },
                file,
            ));
        }
    }
    Ok(found)
}

/// Returns the suite modules a test binary's root file declares.
///
/// A binary of suite modules holds no test of its own and declares
/// `mod <module>;` for each `tests/<name>/<module>.rs`. A root that holds a
/// test is an ordinary suite, and any `mod` it declares is a helper it
/// includes, not a target.
///
/// @param directory - the crate directory
/// @param name - the binary's name, which is the root file's stem
/// @param root - the root file
fn suite_modules(directory: &Path, name: &str, root: &Path) -> Result<Vec<String>, String> {
    if holds_a_test(root)? {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(root)
        .map_err(|error| format!("cannot read {}: {error}", root.display()))?;
    let mut modules = Vec::new();
    for line in text.lines() {
        let Some(module) = line
            .trim()
            .strip_prefix("mod ")
            .and_then(|rest| rest.strip_suffix(';'))
        else {
            continue;
        };
        if directory
            .join("tests")
            .join(name)
            .join(format!("{module}.rs"))
            .is_file()
        {
            modules.push(module.to_string());
        }
    }
    modules.sort();
    Ok(modules)
}

/// Returns one crate's integration target names, sorted: the stem of each
/// `tests/*.rs`, and the directory name of each `tests/*/main.rs`.
///
/// @param directory - the crate directory
fn integration_names(directory: &Path) -> Result<Vec<String>, String> {
    let tests = directory.join("tests");
    if !tests.is_dir() {
        return Ok(Vec::new());
    }
    let mut names = Vec::new();
    for entry in std::fs::read_dir(&tests)
        .map_err(|error| format!("cannot read {}: {error}", tests.display()))?
    {
        let path = entry
            .map_err(|error| format!("cannot read {}: {error}", tests.display()))?
            .path();
        // cargo takes `tests/<name>/main.rs` as a test target called <name>,
        // which is how a binary of suite modules is laid out.
        if path.is_dir() && path.join("main.rs").is_file() {
            if let Some(name) = path.file_name().and_then(|text| text.to_str()) {
                names.push(name.to_string());
            }
            continue;
        }
        if path.extension().and_then(|text| text.to_str()) != Some("rs") {
            continue;
        }
        if let Some(stem) = path.file_stem().and_then(|text| text.to_str()) {
            names.push(stem.to_string());
        }
    }
    names.sort();
    Ok(names)
}

/// Returns the file cargo compiles as the root of an integration target.
///
/// @param directory - the crate directory
/// @param name - the target name
fn integration_root(directory: &Path, name: &str) -> PathBuf {
    let flat = directory.join("tests").join(format!("{name}.rs"));
    if flat.is_file() {
        return flat;
    }
    directory.join("tests").join(name).join("main.rs")
}

/// Returns the names of one crate's bin targets, sorted.
///
/// Cargo's rules, in the order it applies them: a `[[bin]]` section names its
/// own target, `src/main.rs` is a target named after the package unless a
/// section already claimed that path, and each `src/bin/*.rs` is a target named
/// after its file unless a section already claimed it. The names matter because
/// they are what the map's rows are matched against - `inillucent-cli`'s binary
/// is called `inillucent-shell`, and a map that guessed the file stem would
/// declare a target that does not exist.
///
/// @param directory - the crate directory
/// @param package - the crate's package name
fn binary_names(directory: &Path, package: &str) -> Result<Vec<String>, String> {
    let declared = declared_binaries(directory)?;
    let claimed: BTreeSet<String> = declared.iter().map(|(_, path)| path.clone()).collect();
    let mut names: BTreeSet<String> = declared.iter().map(|(name, _)| name.clone()).collect();
    if directory.join("src/main.rs").is_file() && !claimed.contains("src/main.rs") {
        names.insert(package.to_string());
    }
    let bin = directory.join("src/bin");
    if bin.is_dir() {
        for entry in std::fs::read_dir(&bin)
            .map_err(|error| format!("cannot read {}: {error}", bin.display()))?
        {
            let file = entry
                .map_err(|error| format!("cannot read {}: {error}", bin.display()))?
                .path();
            if file.extension().and_then(|text| text.to_str()) != Some("rs") {
                continue;
            }
            let Some(stem) = file.file_stem().and_then(|text| text.to_str()) else {
                continue;
            };
            if claimed.contains(&format!("src/bin/{stem}.rs")) {
                continue;
            }
            names.insert(stem.to_string());
        }
    }
    Ok(names.into_iter().collect())
}

/// Reads every `[[bin]]` section as a name and the path it claims.
///
/// @param directory - the crate directory
fn declared_binaries(directory: &Path) -> Result<Vec<(String, String)>, String> {
    let manifest = directory.join("Cargo.toml");
    let text = std::fs::read_to_string(&manifest)
        .map_err(|error| format!("cannot read {}: {error}", manifest.display()))?;
    let mut declared = Vec::new();
    let mut in_bin = false;
    let mut name = String::new();
    let mut path = String::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            if in_bin && !name.is_empty() {
                declared.push((name.clone(), path.clone()));
            }
            in_bin = line == "[[bin]]";
            name.clear();
            path.clear();
            continue;
        }
        if !in_bin {
            continue;
        }
        if let Some((key, body)) = line.split_once('=') {
            let body = body.trim().trim_matches('"').replace('\\', "/");
            match key.trim() {
                "name" => name = body,
                "path" => path = body,
                _ => {}
            }
        }
    }
    if in_bin && !name.is_empty() {
        declared.push((name, path));
    }
    Ok(declared)
}

/// Returns every target that owns at least one source file holding a `#[test]`.
///
/// This is what makes "the map is complete" a checkable claim rather than a
/// convention. A file is attributed to the target that compiles it:
/// `src/bin/x.rs` to the binary it is, a `tests/` file to itself, and anything
/// else to the library when the crate has one and to the crate's binaries when
/// it does not. Attributing a shared module to the library is the safe
/// direction - the library's row exists anyway - and the case it is protecting
/// against is the opposite one, a crate with no library whose tests would
/// otherwise belong to nothing.
///
/// @param root - the workspace root
/// @param members - the member paths
pub fn targets_holding_tests(root: &Path, members: &[String]) -> Result<BTreeSet<Target>, String> {
    let mut holding = BTreeSet::new();
    for member in members {
        let directory = root.join(member);
        let package = read_package_name(&directory.join("Cargo.toml"))?;
        let has_library = directory.join("src/lib.rs").is_file();
        let declared = declared_binaries(&directory)?;
        for file in rust_sources(&directory.join("src"))? {
            if !holds_a_test(&file)? {
                continue;
            }
            let relative = file
                .strip_prefix(&directory)
                .map(|path| path.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            if relative.starts_with("src/bin/") {
                let stem = relative
                    .trim_start_matches("src/bin/")
                    .trim_end_matches(".rs")
                    .to_string();
                let name = declared
                    .iter()
                    .find(|(_, path)| path == &format!("src/bin/{stem}.rs"))
                    .map(|(name, _)| name.clone())
                    .unwrap_or(stem);
                holding.insert(Target {
                    package: package.clone(),
                    kind: Kind::Bin,
                    name,
                    module: None,
                });
                continue;
            }
            if has_library {
                holding.insert(Target {
                    package: package.clone(),
                    kind: Kind::Lib,
                    name: "lib".to_string(),
                    module: None,
                });
            } else {
                for name in binary_names(&directory, &package)? {
                    holding.insert(Target {
                        package: package.clone(),
                        kind: Kind::Bin,
                        name,
                        module: None,
                    });
                }
            }
        }
        for (target, file) in integration_targets(&directory, &package)? {
            if holds_a_test(&file)? {
                holding.insert(target);
            }
        }
    }
    Ok(holding)
}

/// Returns every `.rs` file under a directory, recursively.
///
/// @param directory - where to start; a missing directory is empty, not an error
fn rust_sources(directory: &Path) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    if !directory.is_dir() {
        return Ok(files);
    }
    let mut pending = vec![directory.to_path_buf()];
    while let Some(next) = pending.pop() {
        for entry in std::fs::read_dir(&next)
            .map_err(|error| format!("cannot read {}: {error}", next.display()))?
        {
            let path = entry
                .map_err(|error| format!("cannot read {}: {error}", next.display()))?
                .path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().and_then(|text| text.to_str()) == Some("rs") {
                files.push(path);
            }
        }
    }
    files.sort();
    Ok(files)
}

/// Reports whether a source file declares a test.
///
/// Reading the text is enough because the question is only ever "could a test
/// be hiding here": a false yes costs one row in the map, and a false no costs
/// a suite nobody runs.
///
/// @param path - the source file
fn holds_a_test(path: &Path) -> Result<bool, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    Ok(text
        .lines()
        .map(str::trim)
        .any(|line| line.starts_with("#[test]")))
}

/// Reads the `name` out of a `[package]` section.
///
/// @param path - the manifest
fn read_package_name(path: &Path) -> Result<String, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let mut in_package = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_package = line == "[package]";
            continue;
        }
        if !in_package {
            continue;
        }
        let Some((key, body)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != "name" {
            continue;
        }
        return Ok(body.trim().trim_matches('"').to_string());
    }
    Err(format!("{} declares no package name", path.display()))
}

/// Builds the reverse-dependency graph: for each package, who depends on it.
///
/// Development dependencies are edges here even though they are not edges in a
/// release build, because a change to a crate that exists only to test another
/// one must re-run that other one's tests. The simulator is the case that
/// matters: it is a dev-dependency of the pool, the tree, the log and the
/// transaction engine, and a fault it stops injecting is a campaign that stops
/// testing anything.
///
/// @param manifests - every workspace member's manifest
pub fn dependents(manifests: &[CrateManifest]) -> BTreeMap<String, BTreeSet<String>> {
    let mut graph: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for manifest in manifests {
        graph.entry(manifest.name.clone()).or_default();
    }
    for manifest in manifests {
        for dependency in manifest.normal.iter().chain(manifest.development.iter()) {
            graph
                .entry(dependency.clone())
                .or_default()
                .insert(manifest.name.clone());
        }
    }
    graph
}

/// Closes a seed set of packages over the reverse-dependency graph.
///
/// @param graph - the graph [`dependents`] built
/// @param seeds - the packages a change touched
pub fn affected(
    graph: &BTreeMap<String, BTreeSet<String>>,
    seeds: &BTreeSet<String>,
) -> BTreeSet<String> {
    let mut reached: BTreeSet<String> = BTreeSet::new();
    let mut pending: Vec<String> = seeds.iter().cloned().collect();
    while let Some(package) = pending.pop() {
        if !reached.insert(package.clone()) {
            continue;
        }
        if let Some(above) = graph.get(&package) {
            for dependent in above {
                if !reached.contains(dependent) {
                    pending.push(dependent.clone());
                }
            }
        }
    }
    reached
}

/// What a set of changed paths selects.
#[derive(Clone, Debug, Default)]
pub struct Choice {
    /// The packages the changed paths belong to, before the closure.
    pub seeds: BTreeSet<String>,
    /// Integration suites the change edited directly.
    ///
    /// Editing `tests/wal_crash.rs` is a request to run `wal_crash`, not a
    /// reason to run the other seventy-one suites in the same crate. Seeding
    /// the package would do the second, because every suite in a crate shares
    /// its package name.
    pub direct: BTreeSet<Target>,
    /// Whether a path fell outside every rule, so everything is selected.
    pub selects_everything: bool,
    /// The paths that caused that, for the report.
    pub unmatched: Vec<String>,
}

/// Turns changed repository paths into the packages whose tests must run.
///
/// A path inside a member directory belongs to that member. A path outside
/// every member is looked up in the map's `[[path]]` rules, and a path matching
/// none of them selects everything - the safe answer, and the one that makes a
/// new top-level directory loud rather than invisible.
///
/// @param map - the selection map, for its path rules
/// @param members - the member paths, longest first is not required
/// @param manifests - every member's manifest, for the member-to-package name
/// @param changed - repository-relative paths, with forward slashes
pub fn seeds_of(
    map: &Map,
    members: &[String],
    manifests: &[CrateManifest],
    changed: &[String],
) -> Choice {
    // A member path such as `crates/inillucent-tree` maps to the package name
    // its manifest declares, which is not always the directory name: the
    // drivers live in `drivers/inillucent-driver` and are named for the crate.
    let mut by_directory: Vec<(String, String)> = Vec::new();
    for (member, manifest) in members.iter().zip(manifests.iter()) {
        by_directory.push((format!("{member}/"), manifest.name.clone()));
    }
    // Longest prefix first, so `drivers/inillucent-driver-capi/` is not eaten
    // by `drivers/inillucent-driver/`.
    by_directory.sort_by_key(|(directory, _)| std::cmp::Reverse(directory.len()));

    let mut choice = Choice::default();
    for path in changed {
        let path = path.replace('\\', "/");
        if let Some((prefix, package)) = by_directory
            .iter()
            .find(|(prefix, _)| path.starts_with(prefix.as_str()))
        {
            match integration_target(map, &path, prefix, package) {
                Some(target) => {
                    choice.direct.insert(target);
                }
                None => {
                    choice.seeds.insert(package.clone());
                }
            }
            continue;
        }
        let rule = map
            .paths
            .iter()
            .filter(|rule| path.starts_with(&rule.prefix))
            .max_by_key(|rule| rule.prefix.len());
        match rule {
            Some(rule) if rule.packages.iter().any(|name| name == "*") => {
                choice.selects_everything = true;
            }
            Some(rule) => {
                for package in &rule.packages {
                    choice.seeds.insert(package.clone());
                }
            }
            None => {
                choice.selects_everything = true;
                choice.unmatched.push(path);
            }
        }
    }
    choice
}

/// Returns the integration target a changed path *is*, when it is one.
///
/// A file directly under `tests/` is its own target, unless it is the root of a
/// binary of suite modules: that file is the list of `mod` lines, and changing
/// it seeds the whole package. A file one directory down is a suite module when
/// the map has a row for it, which is how `inillucent-compat`'s suites are laid
/// out. Anything else in a subdirectory of `tests/` is a shared helper several
/// suites include, so changing it seeds the whole package rather than naming
/// one suite.
///
/// @param map - the selection map, to tell a suite module from a helper
/// @param path - the changed path, with forward slashes
/// @param prefix - the member directory it is inside, ending in a slash
/// @param package - that member's package name
fn integration_target(map: &Map, path: &str, prefix: &str, package: &str) -> Option<Target> {
    let inside = path.strip_prefix(prefix)?;
    let name = inside.strip_prefix("tests/")?.strip_suffix(".rs")?;
    let (name, module) = match name.split_once('/') {
        Some((binary, module)) if !module.contains('/') => (binary, Some(module.to_string())),
        Some(_) => return None,
        None => (name, None),
    };
    let target = Target {
        package: package.to_string(),
        kind: Kind::Test,
        name: name.to_string(),
        module,
    };
    let holds_modules = map
        .rows
        .iter()
        .any(|row| row.target.binary() == target.binary() && row.target.module.is_some());
    match (&target.module, holds_modules) {
        (Some(_), _) if map.row(&target).is_none() => None,
        (None, true) => None,
        _ => Some(target),
    }
}

/// Chooses every target a change can break, whatever its tier's cadence.
///
/// This is the closure alone, which is what a nightly run asks. A change run
/// asks [`select_at`] with [`Cadence::Change`] instead.
///
/// @param map - the selection map
/// @param choice - what the changed paths selected
/// @param graph - the reverse-dependency graph
pub fn select<'map>(
    map: &'map Map,
    choice: &Choice,
    graph: &BTreeMap<String, BTreeSet<String>>,
) -> Vec<&'map Row> {
    select_at(map, choice, graph, Cadence::Nightly)
}

/// Chooses the targets a change has to run at one cadence.
///
/// A row whose tier runs at `cadence` or below is selected by the closure. A
/// `merge` row in a `change` run is selected only by a seed it covers, its own
/// crate or its own file. A row whose tier runs above that is not selected at
/// all. The module documentation says why.
///
/// A change that selects everything (a path no rule covers, or a rule that says
/// `*`) still honours the cadence: it is every row up to `cadence`, and at
/// least every `merge` row, because a change nobody can place is exactly the
/// case the crash suites are for.
///
/// @param map - the selection map
/// @param choice - what the changed paths selected
/// @param graph - the reverse-dependency graph
/// @param cadence - the cadence of the run
pub fn select_at<'map>(
    map: &'map Map,
    choice: &Choice,
    graph: &BTreeMap<String, BTreeSet<String>>,
    cadence: Cadence,
) -> Vec<&'map Row> {
    if choice.selects_everything {
        return map.rows_up_to(cadence.max(Cadence::Merge));
    }
    let reached = affected(graph, &choice.seeds);
    map.rows
        .iter()
        .filter(|row| {
            let tier = map.cadence_of(&row.tier);
            let own =
                choice.seeds.contains(&row.target.package) || choice.direct.contains(&row.target);
            if tier > cadence {
                // Only a `merge` row in a `change` run is still selectable
                // here: it runs when the change is in something it says it
                // exercises, and never because the closure walked up to it.
                return tier == Cadence::Merge
                    && cadence == Cadence::Change
                    && (own
                        || row
                            .covers
                            .iter()
                            .any(|package| choice.seeds.contains(package)));
            }
            // Three independent reasons to run a suite, and a row needs only
            // one of them.
            //
            // The first is the interesting one. `covers` names the packages a
            // suite *drives* - the top of its stack, not everything underneath
            // it - because the closure has already walked upward from what
            // changed. A suite that drives the shell declares `inillucent-cli`,
            // and a change to the page pool selects it because the pool is
            // below the shell.
            //
            // The second is what keeps a suite's own crate honest: editing
            // `inillucent-wal`'s source runs `inillucent-wal`'s tests even
            // though no `covers` list needs to say so.
            //
            // `covers` deliberately does **not** fall back to the owning
            // package here. `inillucent-compat` sits above the whole engine, so
            // any engine change puts it in the closure - and a rule that read
            // the owning package out of the closure would select all 74 of its
            // suites for every change, which is the same as having no selector.
            row.covers.iter().any(|package| reached.contains(package)) || own
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a manifest with the given edges, for the graph tests.
    fn manifest(name: &str, normal: &[&str], development: &[&str]) -> CrateManifest {
        CrateManifest {
            name: name.to_string(),
            normal: normal.iter().map(|text| text.to_string()).collect(),
            development: development.iter().map(|text| text.to_string()).collect(),
        }
    }

    /// A change to a low crate must reach every crate above it, not only its
    /// immediate dependents - which is the whole reason the walk is a closure.
    #[test]
    fn the_closure_reaches_transitively() {
        let manifests = vec![
            manifest("base", &[], &[]),
            manifest("middle", &["base"], &[]),
            manifest("top", &["middle"], &[]),
            manifest("aside", &[], &[]),
        ];
        let graph = dependents(&manifests);
        let mut seeds = BTreeSet::new();
        seeds.insert("base".to_string());
        let reached = affected(&graph, &seeds);
        assert!(reached.contains("top"), "{reached:?}");
        assert!(!reached.contains("aside"), "{reached:?}");
    }

    /// A development edge counts, because a harness that stops injecting a
    /// fault is a campaign that stops testing anything.
    #[test]
    fn a_development_edge_is_an_edge() {
        let manifests = vec![manifest("sim", &[], &[]), manifest("pool", &[], &["sim"])];
        let graph = dependents(&manifests);
        let mut seeds = BTreeSet::new();
        seeds.insert("sim".to_string());
        assert!(affected(&graph, &seeds).contains("pool"));
    }

    /// A cycle must not spin. The workspace has none, but the walk is the
    /// wrong place to rely on that.
    #[test]
    fn a_cycle_terminates() {
        let manifests = vec![manifest("a", &["b"], &[]), manifest("b", &["a"], &[])];
        let graph = dependents(&manifests);
        let mut seeds = BTreeSet::new();
        seeds.insert("a".to_string());
        assert_eq!(affected(&graph, &seeds).len(), 2);
    }

    /// A path nobody declared selects everything, and says which path did it.
    /// The alternative - selecting nothing - is a new directory whose changes
    /// silently run no tests.
    #[test]
    fn an_undeclared_path_selects_everything() {
        let map = Map::parse("[[target]]\npackage = \"a\"\nkind = \"lib\"\ntier = \"unit\"\n")
            .expect("the map parses");
        let members = vec!["crates/a".to_string()];
        let manifests = vec![manifest("a", &[], &[])];
        let choice = seeds_of(&map, &members, &manifests, &["newthing/x.rs".to_string()]);
        assert!(choice.selects_everything);
        assert_eq!(choice.unmatched, vec!["newthing/x.rs".to_string()]);
    }

    /// A declared path rule narrows instead, and the longest matching prefix
    /// is the one that applies.
    #[test]
    fn the_longest_path_rule_wins() {
        let map = Map::parse(
            "[[target]]\npackage = \"a\"\nkind = \"lib\"\ntier = \"unit\"\n\
             [[path]]\nprefix = \"docs/\"\npackages = [\"*\"]\n\
             [[path]]\nprefix = \"docs/notes/\"\npackages = [\"a\"]\n",
        )
        .expect("the map parses");
        let members = vec!["crates/a".to_string()];
        let manifests = vec![manifest("a", &[], &[])];
        let choice = seeds_of(&map, &members, &manifests, &["docs/notes/x.md".to_string()]);
        assert!(!choice.selects_everything);
        assert!(choice.seeds.contains("a"));
    }

    /// Editing one suite runs that suite, not every suite its crate holds.
    /// This is the whole reason `direct` exists: `inillucent-compat` owns 74
    /// of them, and seeding the package would run all 74 for a one-line edit.
    #[test]
    fn editing_one_suite_names_only_that_suite() {
        let map = Map::parse(
            "[[target]]\npackage = \"a\"\nkind = \"test\"\nname = \"one\"\ntier = \"engine\"\ncovers = [\"z\"]\n\
             [[target]]\npackage = \"a\"\nkind = \"test\"\nname = \"two\"\ntier = \"engine\"\ncovers = [\"z\"]\n",
        )
        .expect("the map parses");
        let members = vec!["crates/a".to_string()];
        let manifests = vec![manifest("a", &[], &[])];
        let choice = seeds_of(
            &map,
            &members,
            &manifests,
            &["crates/a/tests/one.rs".to_string()],
        );
        assert!(choice.seeds.is_empty(), "{choice:?}");
        let graph = dependents(&manifests);
        let chosen = select(&map, &choice, &graph);
        assert_eq!(chosen.len(), 1, "{chosen:?}");
        assert_eq!(
            chosen.first().map(|row| row.target.name.clone()),
            Some("one".to_string())
        );
    }

    /// Editing a crate's own source runs that crate's own tests, with no
    /// `covers` list needing to mention it.
    #[test]
    fn editing_a_crate_runs_its_own_tests() {
        let map = Map::parse(
            "[[target]]\npackage = \"a\"\nkind = \"test\"\nname = \"one\"\ntier = \"engine\"\ncovers = [\"z\"]\n",
        )
        .expect("the map parses");
        let members = vec!["crates/a".to_string()];
        let manifests = vec![manifest("a", &[], &[])];
        let choice = seeds_of(
            &map,
            &members,
            &manifests,
            &["crates/a/src/lib.rs".to_string()],
        );
        let graph = dependents(&manifests);
        assert_eq!(select(&map, &choice, &graph).len(), 1);
    }

    /// A suite declares the top of its stack, and a change *below* that top
    /// selects it. This is what lets `covers = ["inillucent-cli"]` stand for
    /// "everything the shell is built on".
    #[test]
    fn covering_the_top_of_a_stack_catches_a_change_below_it() {
        let map = Map::parse(
            "[[target]]\npackage = \"harness\"\nkind = \"test\"\nname = \"shell\"\ntier = \"engine\"\ncovers = [\"top\"]\n",
        )
        .expect("the map parses");
        let members = vec![
            "crates/bottom".to_string(),
            "crates/top".to_string(),
            "crates/harness".to_string(),
        ];
        let manifests = vec![
            manifest("bottom", &[], &[]),
            manifest("top", &["bottom"], &[]),
            manifest("harness", &["top"], &[]),
        ];
        let choice = seeds_of(
            &map,
            &members,
            &manifests,
            &["crates/bottom/src/lib.rs".to_string()],
        );
        let graph = dependents(&manifests);
        assert_eq!(select(&map, &choice, &graph).len(), 1);
    }

    /// And a change in a crate the suite does not sit above selects nothing,
    /// which is the half that makes the selector worth having.
    #[test]
    fn an_unrelated_crate_selects_nothing() {
        let map = Map::parse(
            "[[target]]\npackage = \"harness\"\nkind = \"test\"\nname = \"shell\"\ntier = \"engine\"\ncovers = [\"top\"]\n",
        )
        .expect("the map parses");
        let members = vec![
            "crates/aside".to_string(),
            "crates/top".to_string(),
            "crates/harness".to_string(),
        ];
        let manifests = vec![
            manifest("aside", &[], &[]),
            manifest("top", &[], &[]),
            manifest("harness", &["top", "aside"], &[]),
        ];
        let choice = seeds_of(
            &map,
            &members,
            &manifests,
            &["crates/aside/src/lib.rs".to_string()],
        );
        let graph = dependents(&manifests);
        assert!(select(&map, &choice, &graph).is_empty());
    }

    /// A file inside a crate belongs to that crate, and the longer directory
    /// prefix wins so the driver's C ABI is not read as the driver.
    #[test]
    fn the_longer_member_prefix_wins() {
        let map = Map::parse("[[target]]\npackage = \"a\"\nkind = \"lib\"\ntier = \"unit\"\n")
            .expect("the map parses");
        let members = vec![
            "drivers/inillucent-driver".to_string(),
            "drivers/inillucent-driver-capi".to_string(),
        ];
        let manifests = vec![
            manifest("inillucent-driver", &[], &[]),
            manifest("inillucent-driver-capi", &[], &[]),
        ];
        let choice = seeds_of(
            &map,
            &members,
            &manifests,
            &["drivers/inillucent-driver-capi/src/lib.rs".to_string()],
        );
        assert!(
            choice.seeds.contains("inillucent-driver-capi"),
            "{choice:?}"
        );
        assert!(!choice.seeds.contains("inillucent-driver"), "{choice:?}");
    }

    /// The map the cadence tests share: one tier at each cadence, and one
    /// suite in each, all covering `top`, which sits above `bottom`.
    fn cadenced() -> (Map, Vec<String>, Vec<CrateManifest>) {
        let map = Map::parse(
            "[[tier]]\nname = \"engine\"\ncadence = \"change\"\n\
             [[tier]]\nname = \"durability\"\ncadence = \"merge\"\n\
             [[tier]]\nname = \"nightly\"\ncadence = \"nightly\"\n\
             [[target]]\npackage = \"harness\"\nkind = \"test\"\nname = \"quick\"\ntier = \"engine\"\ncovers = [\"top\"]\n\
             [[target]]\npackage = \"harness\"\nkind = \"test\"\nname = \"crash\"\ntier = \"durability\"\ncovers = [\"top\"]\n\
             [[target]]\npackage = \"harness\"\nkind = \"test\"\nname = \"story\"\ntier = \"nightly\"\ncovers = [\"top\"]\n",
        )
        .expect("the map parses");
        let members = vec![
            "crates/bottom".to_string(),
            "crates/top".to_string(),
            "crates/harness".to_string(),
        ];
        let manifests = vec![
            manifest("bottom", &[], &[]),
            manifest("top", &["bottom"], &[]),
            manifest("harness", &["top"], &[]),
        ];
        (map, members, manifests)
    }

    /// Returns the names a change to one file selects at one cadence.
    fn names_at(path: &str, cadence: Cadence) -> Vec<String> {
        let (map, members, manifests) = cadenced();
        let choice = seeds_of(&map, &members, &manifests, &[path.to_string()]);
        let graph = dependents(&manifests);
        select_at(&map, &choice, &graph, cadence)
            .iter()
            .map(|row| row.target.name.clone())
            .collect()
    }

    /// A change below a `merge` row's cover reaches it by the closure, and a
    /// change run does not run it for that. The merge run does.
    #[test]
    fn a_merge_row_needs_a_seed_in_a_change_run() {
        assert_eq!(
            names_at("crates/bottom/src/lib.rs", Cadence::Change),
            vec!["quick".to_string()]
        );
        assert_eq!(
            names_at("crates/bottom/src/lib.rs", Cadence::Merge),
            vec!["quick".to_string(), "crash".to_string()]
        );
    }

    /// A change in the package a `merge` row covers selects it in a change run.
    #[test]
    fn a_merge_row_runs_when_its_cover_changed() {
        assert_eq!(
            names_at("crates/top/src/lib.rs", Cadence::Change),
            vec!["quick".to_string(), "crash".to_string()]
        );
    }

    /// A `nightly` row is never selected by a change or a merge run, even by
    /// its own file, and the nightly run selects it by the closure.
    #[test]
    fn a_nightly_row_runs_only_at_the_nightly_cadence() {
        assert!(!names_at("crates/top/src/lib.rs", Cadence::Merge).contains(&"story".to_string()));
        assert!(!names_at("crates/harness/tests/story.rs", Cadence::Change)
            .contains(&"story".to_string()));
        assert!(
            names_at("crates/bottom/src/lib.rs", Cadence::Nightly).contains(&"story".to_string())
        );
    }

    /// A path no rule covers selects every `change` and `merge` row, and no
    /// `nightly` row, unless the run is the nightly one.
    #[test]
    fn selecting_everything_still_leaves_the_nightly_rows_to_the_nightly() {
        assert_eq!(
            names_at("newthing/x.rs", Cadence::Change),
            vec!["quick".to_string(), "crash".to_string()]
        );
        assert_eq!(names_at("newthing/x.rs", Cadence::Nightly).len(), 3);
    }

    /// A tier with no cadence is refused, so nobody has to guess which default
    /// was meant.
    #[test]
    fn a_tier_without_a_cadence_is_refused() {
        let refused = Map::parse(
            "[[tier]]\nname = \"engine\"\n\
             [[target]]\npackage = \"a\"\nkind = \"lib\"\ntier = \"engine\"\n",
        );
        assert_eq!(
            refused.err(),
            Some("tier `engine` has no cadence".to_string())
        );
    }

    /// Editing a suite module runs that module's target, editing the binary's
    /// list of modules seeds the package, and a helper one directory down that
    /// is not a suite seeds the package too.
    #[test]
    fn a_suite_module_is_its_own_target() {
        let map = Map::parse(
            "[[target]]\npackage = \"a\"\nkind = \"test\"\nname = \"engine\"\nmodule = \"one\"\ntier = \"engine\"\ncovers = [\"z\"]\n\
             [[target]]\npackage = \"a\"\nkind = \"test\"\nname = \"engine\"\nmodule = \"two\"\ntier = \"engine\"\ncovers = [\"z\"]\n",
        )
        .expect("the map parses");
        let members = vec!["crates/a".to_string()];
        let manifests = vec![manifest("a", &[], &[])];
        let module = seeds_of(
            &map,
            &members,
            &manifests,
            &["crates/a/tests/engine/one.rs".to_string()],
        );
        assert!(module.seeds.is_empty(), "{module:?}");
        let labels: Vec<String> = module.direct.iter().map(Target::label).collect();
        assert_eq!(labels, vec!["a::engine::one".to_string()]);

        let list = seeds_of(
            &map,
            &members,
            &manifests,
            &["crates/a/tests/engine.rs".to_string()],
        );
        assert!(
            list.seeds.contains("a") && list.direct.is_empty(),
            "{list:?}"
        );

        let helper = seeds_of(
            &map,
            &members,
            &manifests,
            &["crates/a/tests/engine/common.rs".to_string()],
        );
        assert!(
            helper.seeds.contains("a") && helper.direct.is_empty(),
            "{helper:?}"
        );
    }

    /// A module row's label names the binary and the module, and only a test
    /// row may have one.
    #[test]
    fn a_module_row_is_labelled_by_binary_and_module() {
        let map = Map::parse(
            "[[target]]\npackage = \"a\"\nkind = \"test\"\nname = \"engine\"\nmodule = \"one\"\ntier = \"engine\"\n",
        )
        .expect("the map parses");
        let row = map.rows.first().expect("one row");
        assert_eq!(row.target.label(), "a::engine::one");
        assert_eq!(row.target.binary().label(), "a::engine");
        assert!(Map::parse(
            "[[target]]\npackage = \"a\"\nkind = \"lib\"\nmodule = \"one\"\ntier = \"unit\"\n"
        )
        .is_err());
    }

    /// A row with no `covers` covers its own package, which is what makes the
    /// map bearable for the twenty crates that hold their own tests.
    #[test]
    fn covers_defaults_to_the_owning_package() {
        let map = Map::parse("[[target]]\npackage = \"a\"\nkind = \"lib\"\ntier = \"unit\"\n")
            .expect("the map parses");
        let row = map.rows.first().expect("one row");
        assert!(row.covers.contains("a"));
    }
}
