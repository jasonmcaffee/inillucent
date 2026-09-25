//! What `inillucent-testrun` builds for a selection, and the list of what it built.
//!
//! Invariant: **the build step names the packages and targets the selected rows
//! need and nothing wider, and a nested runner handed the artifact list runs the
//! executables the outer runner located, without starting cargo.**
//!
//! ## Why the build is narrowed
//!
//! The runner used to build with `cargo test --workspace --no-run --lib --tests`
//! whatever it had selected, so a one line change in `inillucent-cli` linked all
//! 227 test binaries, and every run compiled `inillucent-bench`, which pulls in
//! `ort`, `tokenizers` and the C compile of oniguruma. `--changed` narrowed what
//! ran and never what was built. [`test_arguments`] builds the argument list from
//! the selected rows instead: `-p` for each package, `--lib` when a library row
//! is selected, `--test` and `--bin` for each named target. cargo 1.95 accepts a
//! `--test` name that only one of the named packages has, and `--lib` on a
//! package with no library, which was checked before this was written, so one
//! invocation serves every package and cargo keeps its parallelism.
//!
//! `--lib` applies to every named package, so a selection that names one library
//! row builds the library harness of every other named package as well. That is
//! a few extra links and no extra run: the runner starts only the rows it
//! selected.
//!
//! ## Why the artifact list exists
//!
//! `gates_fail_closed` starts a nested runner in three of its cases. That runner
//! called `locate`, which asks cargo what it built, and cargo answers by
//! building. In the outer target directory that relinks executables a sibling
//! target is running, which Windows refuses, so the nested run built a whole
//! second workspace under `CARGO_TARGET_TMPDIR` instead, ran alone at the end of
//! the run, and took 36 of a 37 minute run. The outer runner now writes what it
//! located to [`ARTIFACTS_FILE`] and passes the path in [`ARTIFACTS_VARIABLE`],
//! and `inillucent-testrun --artifacts <file>` reads it and starts no cargo.

use std::path::{Path, PathBuf};

use inillucent_scalar::json::node::Node;
use inillucent_scalar::json::{parse, render};

use crate::selection::{Kind, Map, Row, Target};

/// The environment variable the outer runner sets on every child to the
/// artifact list's path.
pub const ARTIFACTS_VARIABLE: &str = "INILLUCENT_TESTRUN_ARTIFACTS";

/// Where the artifact list is written, relative to the target directory.
pub const ARTIFACTS_FILE: &str = "inillucent-testrun/artifacts.json";

/// The packages whose tests start a program this workspace builds.
///
/// `cliproc::program` finds `inillucent-shell`, `inillucent` and
/// `inillucent-mcp` in the target directory and, under the runner, builds
/// nothing itself. So the runner builds `inillucent-cli` and
/// `inillucent-driver-capi` whenever a selected row is in one of these
/// packages, or declares `shell` in `requires`. The selection suite's
/// `only_the_listed_packages_start_the_built_programs` fails when a test file
/// in another package starts to use `cliproc`.
pub const PROGRAM_PACKAGES: [&str; 1] = ["inillucent-compat"];

/// The gitignored file where a machine declares the prerequisites it lacks.
///
/// **A declared absence is not a hollow suite.** `--strict` fails a target that
/// ran nothing, or said it skipped, because its prerequisite was missing. On
/// the development box nine to eleven targets always did, for things nobody is
/// going to install there (a MySQL server, a live PostgreSQL, Go, openssl), so
/// `--strict` could never pass on it and every release shipped with its suite
/// switched off. A machine lists those here, a strict run reports the targets
/// they excuse under their own heading, and a skip that names anything else
/// still fails. CI has no such file, so a hollow suite there is still red.
pub const DECLARED_ABSENCES_FILE: &str = "tests/prerequisites.local.toml";

/// An environment variable naming a declaration file to read instead of
/// [`DECLARED_ABSENCES_FILE`].
///
/// For the cases in `gates_fail_closed` that check a declaration excuses what
/// it names and nothing else: they write a file of their own rather than touch
/// the one in the checkout another run may be reading.
pub const DECLARED_ABSENCES_VARIABLE: &str = "INILLUCENT_DECLARED_ABSENCES";

/// Reads the prerequisites a machine declares absent.
///
/// A missing file declares nothing. A file that is there and unreadable is an
/// error, because silently declaring nothing would turn a machine's excuses
/// into failures without saying why.
///
/// @param root - the workspace root
pub fn read_declared_absences(root: &Path) -> Result<Vec<String>, String> {
    let path = std::env::var_os(DECLARED_ABSENCES_VARIABLE)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join(DECLARED_ABSENCES_FILE));
    match std::fs::read_to_string(&path) {
        Ok(text) => parse_declared_absences(&text)
            .map_err(|reason| format!("{} is not readable: {reason}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(format!("cannot read {}: {error}", path.display())),
    }
}

/// Parses `absent = [...]` out of the declaration file.
///
/// @param text - the file's content
pub fn parse_declared_absences(text: &str) -> Result<Vec<String>, String> {
    let document = crate::toml_lite::parse(text)?;
    let mut absent: Vec<String> = document
        .top
        .get("absent")
        .and_then(crate::toml_lite::Value::as_list)
        .map(<[String]>::to_vec)
        .ok_or("it has no `absent = [...]` list")?;
    absent.sort();
    absent.dedup();
    Ok(absent)
}

/// One test executable cargo built, and where cargo would run it from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Artifact {
    /// The package, kind and name of the binary.
    pub target: Target,
    /// The executable.
    pub executable: PathBuf,
    /// The package directory, which is the working directory cargo gives it.
    pub directory: PathBuf,
}

/// Returns the rows the build has to make: the selected ones, and every row a
/// selected row names in `builds`.
///
/// `builds` is for a suite that runs other suites' executables itself.
/// `gates_fail_closed` runs the smoke tier and the two live database suites
/// through a nested runner, and those executables have to be in the artifact
/// list whether or not the outer run selected them.
///
/// @param map - the selection map
/// @param selected - the rows the run will start
pub fn build_set<'map>(map: &'map Map, selected: &[&'map Row]) -> Vec<&'map Row> {
    let mut rows: Vec<&Row> = selected.to_vec();
    for row in selected {
        for label in &row.builds {
            if let Some(extra) = map.rows.iter().find(|other| &other.target.label() == label) {
                if !rows.iter().any(|kept| kept.target == extra.target) {
                    rows.push(extra);
                }
            }
        }
    }
    rows
}

/// Returns the arguments that follow `cargo test --no-run` for a set of rows.
///
/// Each package once, `--lib` once when any row is a library, each test and
/// bin target once, and the features the rows ask for. Nothing else: no
/// `--workspace`, and no bare `--tests`, which would build every integration
/// file of every named package.
///
/// @param rows - the rows to build
pub fn test_arguments(rows: &[&Row]) -> Vec<String> {
    let mut packages: Vec<&str> = Vec::new();
    let mut tests: Vec<&str> = Vec::new();
    let mut bins: Vec<&str> = Vec::new();
    let mut library = false;
    for row in rows {
        push_once(&mut packages, &row.target.package);
        match row.target.kind {
            Kind::Lib => library = true,
            Kind::Test => push_once(&mut tests, &row.target.name),
            Kind::Bin => push_once(&mut bins, &row.target.name),
        }
    }
    let mut arguments = Vec::new();
    for package in packages {
        arguments.push("-p".to_string());
        arguments.push(package.to_string());
    }
    if library {
        arguments.push("--lib".to_string());
    }
    for test in tests {
        arguments.push("--test".to_string());
        arguments.push(test.to_string());
    }
    for bin in bins {
        arguments.push("--bin".to_string());
        arguments.push(bin.to_string());
    }
    let features = wanted_features(rows);
    if !features.is_empty() {
        arguments.push("--features".to_string());
        arguments.push(features.join(","));
    }
    arguments
}

/// Adds a name to a list unless it is there already, keeping first seen order.
///
/// @param list - the list
/// @param name - the name
fn push_once<'row>(list: &mut Vec<&'row str>, name: &'row str) {
    if !list.contains(&name) {
        list.push(name);
    }
}

/// Returns every feature the rows ask for, in one sorted list.
///
/// One list rather than one build per row: cargo unifies features across a
/// build anyway, so asking for them together is the same compilation.
///
/// @param rows - the rows to build
pub fn wanted_features(rows: &[&Row]) -> Vec<String> {
    let mut wanted: Vec<String> = Vec::new();
    for row in rows {
        for feature in &row.features {
            if !wanted.contains(feature) {
                wanted.push(feature.clone());
            }
        }
    }
    wanted.sort();
    wanted
}

/// Returns whether the rows need `inillucent-cli` and `inillucent-driver-capi`
/// built as programs.
///
/// @param rows - the rows to build
pub fn needs_programs(rows: &[&Row]) -> bool {
    rows.iter().any(|row| {
        PROGRAM_PACKAGES.contains(&row.target.package.as_str())
            || row
                .requires
                .iter()
                .any(|need| need == "shell" || need == "programs")
    })
}

/// How long an `--exact` name list may get before it is split across more than
/// one run of the same binary.
///
/// Windows refuses a command line over 32,767 characters and libtest reads no
/// response file, so a module with enough tests would fail to start. 24,000
/// leaves room for the executable's path and the other arguments.
pub const EXACT_LIST_LIMIT: usize = 24_000;

/// Returns a module's test names out of a binary's `--list --format terse`.
///
/// libtest names a test by its module path inside the binary, so every test in
/// `tests/differential/semantics.rs` is `semantics::...`. The first path
/// segment is compared whole: a prefix match on `semantics` would also take
/// `dml_semantics::...`, which is why the runner passes `--exact` names rather
/// than a filter. A `--filter` from the command line keeps only the names that
/// contain it, which is what libtest's own filter would have done.
///
/// @param listing - what the binary printed for `--list --format terse`
/// @param module - the module to take
/// @param filter - the runner's `--filter`, if any
pub fn module_tests(listing: &str, module: &str, filter: Option<&str>) -> Vec<String> {
    listing
        .lines()
        .filter_map(|line| line.trim_end().strip_suffix(": test"))
        .filter(|name| name.split("::").next() == Some(module))
        .filter(|name| filter.is_none_or(|text| name.contains(text)))
        .map(str::to_string)
        .collect()
}

/// Splits `--exact` names into lists that each fit on one command line.
///
/// An empty list comes back as one list holding a name no test has, so the
/// binary still starts, runs nothing, and reports `0 passed`: a module whose
/// every test the filter excluded is a target that graded nothing, and the
/// runner already has an answer for that.
///
/// @param names - the module's test names
/// @param limit - the most characters one list may take
pub fn exact_chunks(names: &[String], limit: usize) -> Vec<Vec<String>> {
    if names.is_empty() {
        return vec![vec!["no-test-is-named-this".to_string()]];
    }
    let mut chunks: Vec<Vec<String>> = Vec::new();
    let mut current: Vec<String> = Vec::new();
    let mut length = 0usize;
    for name in names {
        let cost = name.len() + 1;
        if !current.is_empty() && length + cost > limit {
            chunks.push(std::mem::take(&mut current));
            length = 0;
        }
        current.push(name.clone());
        length += cost;
    }
    chunks.push(current);
    chunks
}

/// Returns where the artifact list lives for a target directory.
///
/// @param target_directory - cargo's target directory
pub fn artifacts_path(target_directory: &Path) -> PathBuf {
    target_directory.join(ARTIFACTS_FILE)
}

/// Writes an artifact list as JSON.
///
/// @param artifacts - what cargo built
pub fn render_artifacts(artifacts: &[Artifact]) -> String {
    let mut text = String::from("[\n");
    for (index, artifact) in artifacts.iter().enumerate() {
        text.push_str(&format!(
            "  {{\"package\": {}, \"kind\": {}, \"name\": {}, \"executable\": {}, \"directory\": {}}}",
            json_string(&artifact.target.package),
            json_string(artifact.target.kind.as_str()),
            json_string(&artifact.target.name),
            json_string(&artifact.executable.to_string_lossy()),
            json_string(&artifact.directory.to_string_lossy())
        ));
        text.push_str(if index + 1 < artifacts.len() {
            ",\n"
        } else {
            "\n"
        });
    }
    text.push_str("]\n");
    text
}

/// Returns a JSON string literal for a text.
///
/// Only the characters a path or a crate name can hold need an escape here:
/// the backslash every Windows path is full of, the quote, and control
/// characters, which no path on this machine has but which would otherwise
/// produce a file the reader refuses.
///
/// @param text - the text
pub fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            control if (control as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", control as u32));
            }
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// Reads an artifact list written by [`render_artifacts`].
///
/// @param text - the file's content
pub fn parse_artifacts(text: &str) -> Result<Vec<Artifact>, String> {
    let parsed = parse::parse(text).map_err(|error| format!("not JSON: {error:?}"))?;
    let Node::Array(items) = parsed.node else {
        return Err("the artifact list is not a JSON array".to_string());
    };
    let mut artifacts = Vec::new();
    for item in &items {
        let text_at = |name: &str| -> Result<String, String> {
            json_field(item, name)
                .and_then(json_text)
                .ok_or_else(|| format!("an artifact has no `{name}`"))
        };
        let kind_text = text_at("kind")?;
        let kind =
            Kind::parse(&kind_text).ok_or_else(|| format!("an artifact has kind `{kind_text}`"))?;
        artifacts.push(Artifact {
            target: Target {
                package: text_at("package")?,
                kind,
                name: text_at("name")?,
                module: None,
            },
            executable: PathBuf::from(text_at("executable")?),
            directory: PathBuf::from(text_at("directory")?),
        });
    }
    Ok(artifacts)
}

/// Returns one member of a JSON object.
///
/// @param node - the object
/// @param name - the member's label
pub fn json_field<'tree>(node: &'tree Node, name: &str) -> Option<&'tree Node> {
    match node {
        Node::Object(members) => members
            .iter()
            .find(|(label, _)| render::unescape(label) == name)
            .map(|(_, value)| value),
        _ => None,
    }
}

/// Returns a JSON string's content, with its escapes resolved.
///
/// The escapes matter: every path cargo reports on Windows arrives with doubled
/// backslashes.
///
/// @param node - the string node
pub fn json_text(node: &Node) -> Option<String> {
    match node {
        Node::Text(_) | Node::TextJ(_) | Node::Text5(_) | Node::TextRaw(_) => {
            Some(render::unescape(node))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parses a map for the argument tests.
    fn map(text: &str) -> Map {
        Map::parse(text).expect("the map parses")
    }

    /// Five rows across three packages give each package, each test and each
    /// bin once, `--lib` once, the features sorted, and nothing else.
    #[test]
    fn the_arguments_name_each_thing_once_and_nothing_else() {
        let map = map("[[target]]\npackage = \"a\"\nkind = \"lib\"\ntier = \"unit\"\nfeatures = [\"a/x\"]\n\
             [[target]]\npackage = \"b\"\nkind = \"lib\"\ntier = \"unit\"\n\
             [[target]]\npackage = \"b\"\nkind = \"test\"\nname = \"one\"\ntier = \"engine\"\n\
             [[target]]\npackage = \"c\"\nkind = \"test\"\nname = \"one\"\ntier = \"engine\"\n\
             [[target]]\npackage = \"c\"\nkind = \"bin\"\nname = \"tool\"\ntier = \"unit\"\nfeatures = [\"c/y\", \"a/x\"]\n");
        let rows: Vec<&Row> = map.rows.iter().collect();
        assert_eq!(
            test_arguments(&rows),
            [
                "-p",
                "a",
                "-p",
                "b",
                "-p",
                "c",
                "--lib",
                "--test",
                "one",
                "--bin",
                "tool",
                "--features",
                "a/x,c/y"
            ]
            .map(String::from)
            .to_vec()
        );
    }

    /// No library row, no `--lib`: a selection of one integration file builds
    /// that file and not the crate's library harness.
    #[test]
    fn no_library_row_means_no_lib_flag() {
        let map = map(
            "[[target]]\npackage = \"a\"\nkind = \"test\"\nname = \"one\"\ntier = \"engine\"\n",
        );
        let rows: Vec<&Row> = map.rows.iter().collect();
        assert_eq!(
            test_arguments(&rows),
            ["-p", "a", "--test", "one"].map(String::from).to_vec()
        );
    }

    /// The programs are built for a row in a package that starts them, or a
    /// row that says it needs the shell, and for nothing else.
    #[test]
    fn the_programs_are_built_only_when_a_row_starts_one() {
        let map = map("[[target]]\npackage = \"inillucent-wal\"\nkind = \"lib\"\ntier = \"unit\"\n\
             [[target]]\npackage = \"inillucent-compat\"\nkind = \"test\"\nname = \"x\"\ntier = \"engine\"\n\
             [[target]]\npackage = \"other\"\nkind = \"test\"\nname = \"y\"\ntier = \"engine\"\nrequires = [\"shell\"]\n");
        let wal: Vec<&Row> = map.rows.iter().take(1).collect();
        assert!(!needs_programs(&wal));
        let compat: Vec<&Row> = map.rows.iter().skip(1).take(1).collect();
        assert!(needs_programs(&compat));
        let shell: Vec<&Row> = map.rows.iter().skip(2).collect();
        assert!(needs_programs(&shell));
    }

    /// A row's `builds` adds the rows it names to the build, once, and only
    /// when that row is itself selected.
    #[test]
    fn a_row_that_runs_other_suites_builds_them() {
        let map = map("[[target]]\npackage = \"a\"\nkind = \"test\"\nname = \"nested\"\ntier = \"tooling\"\nbuilds = [\"b::smoke\", \"b::smoke\"]\n\
             [[target]]\npackage = \"b\"\nkind = \"test\"\nname = \"smoke\"\ntier = \"smoke\"\n\
             [[target]]\npackage = \"c\"\nkind = \"test\"\nname = \"other\"\ntier = \"engine\"\n");
        let nested: Vec<&Row> = map.rows.iter().take(1).collect();
        let built: Vec<String> = build_set(&map, &nested)
            .iter()
            .map(|row| row.target.label())
            .collect();
        assert_eq!(built, vec!["a::nested".to_string(), "b::smoke".to_string()]);
        let other: Vec<&Row> = map.rows.iter().skip(2).collect();
        assert_eq!(build_set(&map, &other).len(), 1);
    }

    /// The artifact list reads back as it was written, including the
    /// backslashes of a Windows path and a quote nobody should put in a path.
    #[test]
    fn the_artifact_list_round_trips() {
        let written = vec![
            Artifact {
                target: Target {
                    package: "inillucent".to_string(),
                    kind: Kind::Test,
                    name: "smoke".to_string(),
                    module: None,
                },
                executable: PathBuf::from("D:\\target\\debug\\deps\\smoke-1.exe"),
                directory: PathBuf::from("C:\\repo\\crates\\inillucent"),
            },
            Artifact {
                target: Target {
                    package: "inillucent-wal".to_string(),
                    kind: Kind::Lib,
                    name: "lib".to_string(),
                    module: None,
                },
                executable: PathBuf::from("/tmp/a \"quoted\" dir/wal"),
                directory: PathBuf::from("/repo/crates/inillucent-wal"),
            },
        ];
        let read = parse_artifacts(&render_artifacts(&written)).expect("the list reads back");
        assert_eq!(read, written);
    }

    /// A module's names are the ones whose first path segment is the module,
    /// whole, and a filter narrows them further.
    #[test]
    fn a_module_takes_its_own_names_and_no_others() {
        let listing = "semantics::a: test\nsemantics::inner::b: test\n\
                       dml_semantics::c: test\nsemantics::bench: benchmark\nother::d: test\n";
        assert_eq!(
            module_tests(listing, "semantics", None),
            vec![
                "semantics::a".to_string(),
                "semantics::inner::b".to_string()
            ]
        );
        assert_eq!(
            module_tests(listing, "semantics", Some("inner")),
            vec!["semantics::inner::b".to_string()]
        );
        assert!(module_tests(listing, "missing", None).is_empty());
    }

    /// A long list splits so no part passes the limit, keeps every name once
    /// and in order, and an empty list is one list naming nothing real.
    #[test]
    fn the_exact_names_split_at_the_limit() {
        let names: Vec<String> = (0..10).map(|index| format!("module::t{index}")).collect();
        let chunks = exact_chunks(&names, 40);
        assert!(chunks.len() > 1, "{chunks:?}");
        assert!(chunks
            .iter()
            .all(|chunk| chunk.iter().map(|name| name.len() + 1).sum::<usize>() <= 40));
        assert_eq!(chunks.concat(), names);
        assert_eq!(exact_chunks(&names, EXACT_LIST_LIMIT).len(), 1);
        assert_eq!(
            exact_chunks(&[], 40),
            vec![vec!["no-test-is-named-this".to_string()]]
        );
    }

    /// The declaration file is a sorted list; a file without the list is
    /// refused rather than read as declaring nothing.
    #[test]
    fn the_declaration_file_is_read_or_refused() {
        assert_eq!(
            parse_declared_absences("absent = [\"postgres\", \"mysql\", \"mysql\"]\n"),
            Ok(vec!["mysql".to_string(), "postgres".to_string()])
        );
        assert!(parse_declared_absences("present = [\"mysql\"]\n").is_err());
    }

    /// An empty list is a list, and text that is not one is refused with a
    /// sentence rather than read as empty.
    #[test]
    fn the_artifact_reader_refuses_what_is_not_a_list() {
        assert_eq!(parse_artifacts(&render_artifacts(&[])), Ok(Vec::new()));
        assert!(parse_artifacts("{\"package\": \"a\"}").is_err());
        assert!(parse_artifacts("[{\"package\": \"a\"}]").is_err());
    }
}
