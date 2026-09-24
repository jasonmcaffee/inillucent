//! Every binding runs the whole conformance suite, or says which part it cannot.
//!
//! Invariant: **a runner that ran fewer cases than the suite holds has named,
//! in the suite file itself, the capability it lacks - and a runner that
//! declares no gap ran every case.**
//!
//! ## What this is for
//!
//! `drivers/conformance/suite.json` is what "the binding is correct" means.
//! Before task-2036 two things read it - the Rust driver and the Python
//! binding - while npm, Go and PHP each had a round trip of their own: three to
//! eight hand-written cases, agreeing with nothing. So "all the bindings pass
//! the suite" was three-fifths untrue, and nothing could say so.
//!
//! Each runner now writes `_agent_output/conformance/<language>.json` naming
//! the cases it ran and the ones it skipped, and this reads them. A runner that
//! quietly stopped running half the suite - because a case was renamed, or
//! because a `needs` was added and the runner started skipping it - shows up
//! here as a count that does not add up.
//!
//! ## The runners are driven, and a missing interpreter is a skip
//!
//! This used to read whatever records were on disk, and argued that driving the
//! runners "would mean this suite owning a Node, a Go toolchain and a PHP in
//! one place, and reporting a missing interpreter as a failure of the binding."
//! The second half of that is the part worth keeping, and it is kept: an
//! interpreter this machine does not have is a named skip, never a failure.
//!
//! **What reading leftovers cost was measured** (task-2066 §4.4.3).
//! `rust.json` and `python.json` are written by targets inside the Rust suite on
//! every strict run; npm, go and php are written only when somebody types their
//! command. So "some records and not others" - which this file used to call
//! "a runner stopped running, and that is a failure" - is the permanent state of
//! every machine that has run the suite once and not run those three by hand.
//! The first strict run on a clean machine skipped, because there were no
//! records at all. The second failed, on an unchanged tree, because the first
//! had written two of the five.
//!
//! It was also a green that evidenced nothing in the ordinary case: a developer
//! who has never run the three sees a skip, and a skip is what `--strict` is
//! supposed to make visible rather than what a release is cut on.
//!
//! **A record that is not there and cannot be made is a skip, not a pass.** A
//! runner whose interpreter is absent prints `; skipping` naming it, which is
//! what `--strict` counts. A runner whose interpreter is present is *run*, and
//! then its record is graded the way it always was.
//!
//! **A stale record cannot pass either.** Every record names the cases it ran
//! and the ones it skipped, and the two together have to be every case in the
//! suite. So a record made before a case was added fails the moment the suite
//! gains one, which is the only way a file on disk can be checked against a
//! file that changes.
//!
//! ## Why the runners are not a tier of their own
//!
//! The design asked for a row in `tests/selection.toml` per runner, so
//! `--strict` would count a binding suite that did not run. `inillucent-testrun`
//! is cargo-shaped end to end - it builds with `cargo test --no-run`, finds the
//! executables in cargo's own JSON, and reads libtest's summary - so a fourth
//! target kind for a shell command is a change to the runner's core, and this
//! ticket is tests.
//!
//! What it gets instead is the same guarantee from inside the cargo world: this
//! target has a row, it declares `requires`, it skips visibly when no runner has
//! been run, and it fails when a record is missing beside others or is older
//! than the suite. A run where the bindings did not run is loud either way.

use std::collections::BTreeSet;
use std::path::PathBuf;

use inillucent_compat::workspace_root;

/// The runners, and the command that produces each one's record.
///
/// Written out rather than discovered, because a runner that stopped being run
/// would otherwise stop being checked - which is the failure this whole file is
/// about, one level up.
const RUNNERS: [(&str, &str); 5] = [
    ("rust", "cargo test -p inillucent-driver --test conformance"),
    (
        "python",
        "python drivers/bindings/python/run_conformance.py",
    ),
    (
        "npm",
        "node --test packages/npm/inillucent/conformance.test.mjs",
    ),
    (
        "go",
        "go test -C packages/go -run TestConformanceSuite ./...",
    ),
    ("php", "php packages/php/tests/conformance.php"),
];

/// The three runners this suite starts itself, and what each one needs.
///
/// `rust` and `python` are left out because they are their own targets in
/// `tests/selection.toml` and run on every strict run already; starting them a
/// second time here would double their cost and grade the same record twice.
///
/// Each entry is the record's language name, the interpreter to look for, the
/// arguments that ask it for its version, and the arguments that run the
/// suite. A program on the path that will not start is the same to this suite
/// as one that is not there, so the version call is how "is it here" is asked.
///
/// **The version call is per language and not `--version` for all three.**
/// `node --version` and `php --version` are right; Go's is `go version`, and
/// `go --version` exits non zero with `flag provided but not defined:
/// -version`. Probing Go the other way made a machine *with* Go report Go
/// absent, and an absent interpreter is a named skip - so the Go binding would
/// have stopped being graded and this suite would have said so in a sentence
/// that read like a fact about the machine. Go is not installed here, which is
/// why the wrong spelling reached the right answer and why this was found by
/// reading rather than by a red run; every GitHub runner has Go, so 4.4.1 is
/// where it would have shown.
const DRIVEN: [(&str, &str, &[&str], &[&str]); 3] = [
    (
        "npm",
        "node",
        &["--version"],
        &["--test", "packages/npm/inillucent/conformance.test.mjs"],
    ),
    (
        "go",
        "go",
        &["version"],
        &[
            "test",
            "-C",
            "packages/go",
            "-run",
            "TestConformanceSuite",
            "./...",
        ],
    ),
    (
        "php",
        "php",
        &["--version"],
        &["packages/php/tests/conformance.php"],
    ),
];

/// Runs the three runners this suite owns, and names the ones it could not.
///
/// A runner that ran is graded below like any other. A runner whose interpreter
/// is absent is named here and skipped there - which is the half of the old
/// argument worth keeping: this suite is about whether a binding runs the whole
/// conformance suite, and a machine with no Go toolchain says nothing about
/// that either way.
///
/// The runner's own exit code is deliberately ignored. A runner that ran and
/// failed a case writes that into its record, and the record is what this file
/// grades; treating the exit code as the answer as well would report the same
/// failure twice and in less detail.
///
/// @returns the languages whose interpreter this machine does not have
fn drive_the_runners() -> Vec<String> {
    let root = workspace_root();
    // **The runners need the binary, and none of them can build it**
    // (task-2066 §4.4.3). All three spawn `inillucent` and look for it through
    // `INILLUCENT_BIN` or on the path; without it they skip, write no record,
    // and this suite then reports them as runners that stopped running. The
    // npm runner said so in as many words - `no inillucent binary: set
    // INILLUCENT_BIN ...; skipping` - to a standard error nothing was reading.
    //
    // `cliproc::program` builds it into the same target directory the calling
    // test binary is in, which is what makes this work under a redirected
    // `CARGO_TARGET_DIR` and so in every worktree.
    let binary = inillucent_compat::cliproc::program("inillucent");
    let mut absent = Vec::new();
    for (language, program, version, arguments) in DRIVEN {
        let present = std::process::Command::new(program)
            .args(version)
            .current_dir(&root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if !present {
            absent.push(language.to_string());
            continue;
        }
        let _ = std::process::Command::new(program)
            .args(arguments)
            .current_dir(&root)
            .env("INILLUCENT_BIN", &binary)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    absent
}

/// One runner's record of what it did.
struct Record {
    /// The cases it graded.
    ran: BTreeSet<String>,
    /// The cases it skipped.
    skipped: BTreeSet<String>,
    /// What it says it cannot do.
    lacks: BTreeSet<String>,
    /// What did not hold.
    failures: Vec<String>,
}

/// Reads the list of strings under a key of a flat JSON object.
///
/// Hand written rather than taken from a reader, for the reason
/// `drivers/inillucent-driver/tests/conformance.rs` gives about the suite
/// itself: the whole of what is in these records is arrays of strings and a
/// language name, and a dependency for that is a dependency.
///
/// @param text - the record
/// @param key - the array's name
fn strings_under(text: &str, key: &str) -> Vec<String> {
    let marker = format!("\"{key}\"");
    let Some(at) = text.find(&marker) else {
        return Vec::new();
    };
    let rest = text.get(at + marker.len()..).unwrap_or_default();
    let Some(open) = rest.find('[') else {
        return Vec::new();
    };
    let Some(close) = rest.get(open..).and_then(|from| from.find(']')) else {
        return Vec::new();
    };
    let body = rest.get(open + 1..open + close).unwrap_or_default();
    let mut out = Vec::new();
    let mut current = String::new();
    let mut inside = false;
    let mut escaped = false;
    for letter in body.chars() {
        match (inside, escaped, letter) {
            (false, _, '"') => inside = true,
            (true, false, '\\') => escaped = true,
            (true, true, other) => {
                current.push(other);
                escaped = false;
            }
            (true, false, '"') => {
                out.push(std::mem::take(&mut current));
                inside = false;
            }
            (true, false, other) => current.push(other),
            _ => {}
        }
    }
    out
}

/// Reads the names out of a `skipped` array, whose items are objects.
///
/// @param text - the record
fn skipped_names(text: &str) -> BTreeSet<String> {
    let Some(at) = text.find("\"skipped\"") else {
        return BTreeSet::new();
    };
    let rest = text.get(at..).unwrap_or_default();
    // **To the *matching* bracket, not the first one.** Each skipped entry
    // carries its own `needs` array, so stopping at the first `]` stopped
    // inside the first entry - and the two runners whose records were read that
    // way came back accounting for 28 of 32 cases, which the staleness check
    // then reported as a stale record. It was the reader.
    let mut depth = 0usize;
    let mut close = rest.len();
    for (at, letter) in rest.char_indices() {
        match letter {
            '[' => depth = depth.saturating_add(1),
            ']' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    close = at;
                    break;
                }
            }
            _ => {}
        }
    }
    let body = rest.get(..close).unwrap_or_default();
    let mut out = BTreeSet::new();
    let mut looking = body;
    while let Some(found) = looking.find("\"name\":") {
        let after = looking.split_at(found + "\"name\":".len()).1;
        let Some(open) = after.find('"') else { break };
        let value = after.get(open + 1..).unwrap_or_default();
        if let Some(name) = value.split('"').next() {
            out.insert(name.to_string());
        }
        looking = value;
    }
    out
}

/// Reads one runner's record, or nothing when it has not been produced.
///
/// @param language - the runner's name
fn record(language: &str) -> Option<Record> {
    let path = workspace_root()
        .join("_agent_output/conformance")
        .join(format!("{language}.json"));
    let text = std::fs::read_to_string(path).ok()?;
    Some(Record {
        ran: strings_under(&text, "ran").into_iter().collect(),
        skipped: skipped_names(&text),
        lacks: strings_under(&text, "lacks").into_iter().collect(),
        failures: strings_under(&text, "failures"),
    })
}

/// The suite's case names, and what each one needs.
///
/// @returns every case's name, paired with the capabilities it needs
fn cases() -> Vec<(String, BTreeSet<String>)> {
    let path: PathBuf = workspace_root().join("drivers/conformance/suite.json");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|why| panic!("{}: {why}", path.display()))
        // **Line endings normalised before anything looks for a newline.** The
        // scan below cuts the file on a newline, two spaces and a brace, and a
        // checkout with CRLF endings holds a carriage return before each one -
        // so it found no case at all and this guard failed saying the suite was
        // empty. It is the same file either way; git hands out whichever the
        // platform asks for.
        .replace("\r\n", "\n");
    let mut out = Vec::new();
    // One case per `"name":` that is a case's own, which is every one at the
    // indentation the file is written at. A `"name"` inside a column list is
    // deeper, so the prefix is what tells them apart.
    for block in text.split("\n    {\n").skip(1) {
        let Some(name) = block
            .find("\"name\": \"")
            .and_then(|at| block.get(at + "\"name\": \"".len()..))
            .and_then(|rest| rest.split('"').next())
        else {
            continue;
        };
        // **The whole block, not the part before `"steps"`.** The first version
        // cut the block at `"steps"` on the assumption that a case's own keys
        // come first; they do not - `group` and `needs` are written after it -
        // so every case came back needing nothing and three runners were
        // reported as skipping cases they had said they could not run. Nothing
        // inside a step is called `needs`, so the whole block is safe to scan.
        let needs: BTreeSet<String> = strings_under(block, "needs").into_iter().collect();
        out.push((name.to_string(), needs));
    }
    out
}

/// Every runner ran every case its capabilities allow.
///
/// **Delete a case from a runner's record and this fails**, which is rule 1.5:
/// the guard is checked by taking away the thing it guards. It is also what
/// turns "all four bindings pass the suite" from a sentence into a count.
#[test]
fn every_binding_runs_the_whole_suite() {
    let cases = cases();
    assert!(
        cases.len() >= 30,
        "read {} cases out of drivers/conformance/suite.json, so the scan is wrong rather than \
         the suite short",
        cases.len()
    );

    // The three this suite owns are run first, so what is graded below is this
    // run's answer rather than whatever a previous one left on disk.
    let absent = drive_the_runners();

    let mut missing_records: Vec<String> = Vec::new();
    let mut wrong: Vec<String> = Vec::new();
    let mut found = 0usize;
    for (language, how) in RUNNERS {
        let Some(record) = record(language) else {
            // An interpreter this machine does not have is not a binding that
            // stopped running. It is named in the skip below instead.
            if !absent.iter().any(|named| named == language) {
                missing_records.push(format!("{language}: produce it with `{how}`"));
            }
            continue;
        };
        found = found.saturating_add(1);

        assert!(
            record.failures.is_empty(),
            "the {language} runner's record says {} of the suite's assertions did not hold:\n  \
             {}",
            record.failures.len(),
            record.failures.join("\n  ")
        );

        let mut should_have_run: Vec<&str> = Vec::new();
        let mut should_not_have_skipped: Vec<&str> = Vec::new();
        for (name, needs) in &cases {
            let blocked = needs.iter().any(|needed| record.lacks.contains(needed));
            match (
                blocked,
                record.ran.contains(name),
                record.skipped.contains(name),
            ) {
                // It can run it and did not.
                (false, false, _) => should_have_run.push(name),
                // It skipped a case it has the capabilities for.
                (false, true, true) => should_not_have_skipped.push(name),
                _ => {}
            }
        }
        if !should_have_run.is_empty() {
            wrong.push(format!(
                "{language} declares it lacks {:?} and did not run these cases, which need \
                 nothing it lacks:\n    {}",
                record.lacks,
                should_have_run.join("\n    ")
            ));
        }
        if !should_not_have_skipped.is_empty() {
            wrong.push(format!(
                "{language} recorded these as skipped and has the capabilities for them:\n    {}",
                should_not_have_skipped.join("\n    ")
            ));
        }

        // **The record has to be about this suite.** A record is a file on
        // disk and the suite is a file that changes, so the only thing that
        // can tell a current record from last week's is whether it accounts
        // for every case the suite holds now.
        let accounted: BTreeSet<&String> = record.ran.union(&record.skipped).collect();
        let unaccounted: Vec<&str> = cases
            .iter()
            .map(|(name, _)| name.as_str())
            .filter(|name| !accounted.contains(&name.to_string()))
            .collect();
        if !unaccounted.is_empty() {
            wrong.push(format!(
                "{language}'s record accounts for {} of the suite's {} cases, so it was made \
                 against an older suite. Run it again: `{how}`. It has never seen:\n    {}",
                accounted.len(),
                cases.len(),
                unaccounted.join("\n    ")
            ));
        }
    }

    // **An interpreter that is not installed is a skip, and it is the only
    // skip left** (task-2066 §4.4.3). It used to be "no records at all", which
    // made the first strict run on a machine skip and the second fail, because
    // the first had written two of the five itself.
    if !absent.is_empty() {
        // Built first, so the announcement and the early return sit next to
        // each other: `every_early_return_in_a_test_says_why` reads the lines
        // around a `return` looking for the helper, and a three-line `format!`
        // between them puts it out of reach.
        let said = format!(
            "this machine has no {}, so {} of the five conformance runners could not be run",
            absent.join(", no "),
            absent.len()
        );
        inillucent_compat::differential::skipping(&said);
        return;
    }
    if found == 0 {
        inillucent_compat::differential::skipping(
            "no conformance runner produced a record, so there is nothing to grade",
        );
        return;
    }

    assert!(
        missing_records.is_empty(),
        "these runners have produced no record, so whether they run the suite is not something \
         this machine can say:\n  {}\n\
         A record is written by the runner itself into _agent_output/conformance/.",
        missing_records.join("\n  ")
    );
    assert!(
        wrong.is_empty(),
        "the conformance suite is not being run whole:\n  {}\n\
         A runner that cannot do something names the capability in \
         drivers/conformance/suite.json's `skipped_by`, with the reason. A runner that skips \
         a case it could run is skipping it for no recorded reason at all.",
        wrong.join("\n  ")
    );
}

/// Every capability a case needs is one the suite declares, and every runner's
/// `lacks` names a capability that exists.
///
/// The other direction. A `needs: ["sesion"]` with a typo in it would be a case
/// no runner ever skips and every runner is expected to run - which is a
/// requirement nobody can satisfy and nothing would report.
#[test]
fn every_capability_a_case_needs_is_one_the_suite_declares() {
    let path = workspace_root().join("drivers/conformance/suite.json");
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|why| panic!("{}: {why}", path.display()));
    let Some(block) = text.split("\"capabilities\"").nth(1) else {
        panic!("{} declares no `capabilities`", path.display());
    };
    let declared: BTreeSet<String> = block
        .split('}')
        .next()
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.trim().strip_prefix('"'))
        .filter_map(|rest| rest.split('"').next())
        .map(str::to_string)
        .collect();
    assert!(
        !declared.is_empty(),
        "{} declares `capabilities` and names none",
        path.display()
    );

    let mut unknown: Vec<String> = Vec::new();
    for (name, needs) in cases() {
        for needed in needs {
            if !declared.contains(&needed) {
                unknown.push(format!("{name} needs `{needed}`"));
            }
        }
    }
    for (language, _) in RUNNERS {
        if let Some(record) = record(language) {
            for lacked in record.lacks {
                if !declared.contains(&lacked) {
                    unknown.push(format!("the {language} runner says it lacks `{lacked}`"));
                }
            }
        }
    }
    assert!(
        unknown.is_empty(),
        "these name a capability the suite does not declare, so nothing can satisfy them and \
         nothing reports it:\n  {}\n  declared: {declared:?}",
        unknown.join("\n  ")
    );
}

/// The directories whose test files `packages/package-tests.toml` has to name.
///
/// `drivers/bindings/` is here as well as `packages/` because the reference
/// Python binding lives there and runs the conformance suite: it is a language
/// package by everything except its path.
const PACKAGE_TEST_ROOTS: [&str; 2] = ["packages", "drivers/bindings"];

/// Whether a file under those roots is a test.
///
/// By name, the way each language names one: `*_test.go`, `*.test.mjs`,
/// `test_*.py` or `run_*.py`, and any `.php` under a `tests/` directory. A
/// rule per language rather than one rule, because there is no rule they share
/// and a rule that tried would either miss a file or claim a source file was a
/// test.
///
/// @param relative - the path, relative to the workspace root, with forward slashes
fn is_a_package_test(relative: &str) -> bool {
    let Some(name) = relative.rsplit('/').next() else {
        return false;
    };
    name.ends_with("_test.go")
        || name.ends_with(".test.mjs")
        || name.starts_with("test_") && name.ends_with(".py")
        || name.starts_with("run_") && name.ends_with(".py")
        || name.ends_with(".php") && relative.contains("/tests/")
}

/// Every `.rs`-less test file under the package roots.
fn package_test_files() -> BTreeSet<String> {
    let root = workspace_root();
    let mut found = BTreeSet::new();
    for start in PACKAGE_TEST_ROOTS {
        let mut pending = vec![root.join(start)];
        while let Some(directory) = pending.pop() {
            let Ok(entries) = std::fs::read_dir(&directory) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().to_string();
                if path.is_dir() {
                    if matches!(name.as_str(), "node_modules" | "target" | "dist" | "build") {
                        continue;
                    }
                    pending.push(path);
                    continue;
                }
                let relative = path
                    .strip_prefix(&root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                if is_a_package_test(&relative) {
                    found.insert(relative);
                }
            }
        }
    }
    found
}

/// The paths `packages/package-tests.toml` names.
fn manifest_paths() -> BTreeSet<String> {
    let path = workspace_root().join("packages/package-tests.toml");
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|why| panic!("{}: {why}", path.display()));
    let document = inillucent_compat::toml_lite::parse(&text)
        .unwrap_or_else(|why| panic!("{}: {why}", path.display()));
    let rows = document.array("test");
    assert!(
        rows.len() >= 8,
        "{} holds {} rows, which is fewer than the language packages have test files",
        path.display(),
        rows.len()
    );
    rows.iter()
        .map(|row| {
            row.get("path")
                .and_then(inillucent_compat::toml_lite::Value::as_str)
                .unwrap_or_else(|| panic!("a row of {} has no `path`", path.display()))
                .to_string()
        })
        .collect()
}

/// Every test the language packages own is named in the manifest, and every row
/// names a file that is there.
///
/// **`packages/python/tests/test_wheel.py` was referenced by nothing at all.**
/// Not a selection row, not a script, not the release - so a wheel could ship
/// with its native half missing and that file would sit there passing on
/// nobody's machine. This is the check that makes a file under `packages/`
/// impossible to add without saying how it runs, and impossible to delete
/// without the manifest noticing.
///
/// **Add a test file under `packages/` and this fails**, which is rule 1.5.
#[test]
fn every_package_test_is_named_here() {
    let on_disk = package_test_files();
    let named = manifest_paths();
    assert!(
        on_disk.len() >= 8,
        "found {} test files under {PACKAGE_TEST_ROOTS:?}, so the walk is wrong rather than the \
         packages being nearly empty",
        on_disk.len()
    );

    let unnamed: Vec<&String> = on_disk.difference(&named).collect();
    assert!(
        unnamed.is_empty(),
        "these test files are in the tree and packages/package-tests.toml does not name them, \
         so nothing says how they run or whether they ever do:\n  {}",
        unnamed
            .iter()
            .map(|one| one.as_str())
            .collect::<Vec<&str>>()
            .join("\n  ")
    );

    let gone: Vec<&String> = named.difference(&on_disk).collect();
    assert!(
        gone.is_empty(),
        "packages/package-tests.toml names these and they are not there:\n  {}",
        gone.iter()
            .map(|one| one.as_str())
            .collect::<Vec<&str>>()
            .join("\n  ")
    );
}

/// Returns the directories on `PATH`, in order.
fn path_entries() -> Vec<PathBuf> {
    let Ok(path) = std::env::var("PATH") else {
        return Vec::new();
    };
    std::env::split_paths(&path).collect()
}

/// Whether `PATH` holds a program of that name, without running it.
///
/// The independent half of the test below: `PATH` is asked whether the program
/// is installed, and the probe in `DRIVEN` is then asked whether it works. A
/// probe that decided both questions could not be wrong about either.
///
/// @param program - the program's name, without an extension
fn is_on_the_path(program: &str) -> bool {
    let suffixes: Vec<&str> = if std::env::consts::EXE_SUFFIX.is_empty() {
        vec![""]
    } else {
        // Windows runs `go.exe` for `go`, and a Go installation puts `go.exe`
        // on the path and nothing called `go`. `PATHEXT` holds more than these
        // two; a `.bat` or `.cmd` shim is how several toolchains install, and
        // `node` on Windows is frequently `node.exe` beside a `node.cmd`.
        vec![".exe", ".cmd", ".bat", ""]
    };
    path_entries().iter().any(|directory| {
        suffixes
            .iter()
            .any(|suffix| directory.join(format!("{program}{suffix}")).is_file())
    })
}

/// Every runner's version probe is a spelling that runner accepts.
///
/// **`go --version` is not a Go command** (task-2066 §4.4.3). Every probe in
/// `DRIVEN` used to be `--version`, which is right for `node` and `php` and
/// wrong for `go`: Go answers `flag provided but not defined: -version` and
/// exits non zero. The suite reads a failed probe as "this machine does not
/// have Go", and an absent interpreter is a named skip - so on a machine that
/// *has* Go, the Go binding would have stopped being graded and the suite would
/// have said so in a sentence that reads like a fact about the machine.
///
/// The two halves come from different places on purpose. `PATH` says whether
/// the program is installed; the probe says whether it runs. A test that asked
/// the probe both questions could not be wrong about either, which is the shape
/// that let the defect sit here in the first place.
///
/// A program this machine does not have is not evidence either way and is
/// counted rather than asserted about, so the failure message can say how much
/// of the table this run actually graded.
#[test]
fn every_version_probe_is_a_spelling_that_program_accepts() {
    let root = workspace_root();
    let mut refused: Vec<String> = Vec::new();
    let mut graded = 0usize;
    let mut uninstalled: Vec<&str> = Vec::new();
    for (language, program, version, _) in DRIVEN {
        if !is_on_the_path(program) {
            uninstalled.push(program);
            continue;
        }
        graded = graded.saturating_add(1);
        let output = std::process::Command::new(program)
            .args(version)
            .current_dir(&root)
            .output();
        match output {
            Ok(output) if output.status.success() => {}
            Ok(output) => refused.push(format!(
                "{language}: `{program} {}` exited {} and printed:\n    {}",
                version.join(" "),
                output
                    .status
                    .code()
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "on a signal".to_string()),
                String::from_utf8_lossy(&output.stderr)
                    .trim()
                    .replace('\n', "\n    ")
            )),
            Err(why) => refused.push(format!(
                "{language}: `{program} {}` did not start: {why}",
                version.join(" ")
            )),
        }
    }
    assert!(
        refused.is_empty(),
        "these version probes are on PATH and refuse the arguments DRIVEN asks them for, so \
         `drive_the_runners` reads them as interpreters this machine does not have and skips \
         the binding without grading it:\n  {}",
        refused.join("\n  ")
    );
    if graded == 0 {
        inillucent_compat::differential::skipping(&format!(
            "none of {:?} is on PATH, so no version probe could be graded",
            uninstalled
        ));
    }
}
