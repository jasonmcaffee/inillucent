//! The matrix is used by every scenario, and it expands every quick arm.
//!
//! Invariant: **a file that tells an application's story runs that story at
//! every arm, and cannot quietly run it at one.** A story written as an
//! ordinary test would be a story at 32,768 byte pages, which is the
//! arrangement task-2033 hid behind: 3,169 tests, every one of them at one page
//! size, and an FTS5 index that refuses on row 42 at the size SQLite itself
//! defaults to.
//!
//! ## What this guard reads, and why it reads text
//!
//! There is no way to enumerate `#[test]` functions at run time, so this is a
//! scan of the source, the same shape as
//! `cli_commands::every_registry_command_has_a_subprocess_test`. It asks three
//! questions of the tree:
//!
//! 1. **Every scenario file invokes `scenario!`.** A file named `story_*.rs`
//!    that does not is a story that opted out of the matrix.
//! 2. **A scenario file holds no bare `#[test]`.** Every test in it comes from
//!    the macro, so every test in it runs at an arm. A file that mixed the two
//!    would have one story graded six ways and another graded once, and the
//!    difference would be invisible in the run's output.
//! 3. **The macro expands every quick arm.** `scenario!` names a constructor
//!    per arm, and `arms(Kind::Quick)` names three; if a constructor is dropped
//!    from the macro the three become two and every story silently stops being
//!    run at that configuration. This is the assertion the guard is named
//!    after.
//!
//! Rule 1.2: each scan asserts it found something before it asserts anything
//! about what it found. A walk that reads no files passes every "none of them
//! is wrong" test there is.

use std::path::{Path, PathBuf};

/// The directories that hold scenario suites.
///
/// Both are named in the TDD: the public facade's stories are in
/// `crates/inillucent/tests/` because that is where §2.1 of the testing
/// standard puts "what an application does with the public API", and the ones
/// that need the oracle, a subprocess or a fixture are in
/// `crates/inillucent-compat/tests/` beside the machinery they use.
const TEST_DIRECTORIES: [&str; 2] = ["crates/inillucent/tests", "crates/inillucent-compat/tests"];

/// A test file, read once.
struct Source {
    /// The path, relative to the workspace root, for a failure message.
    name: String,
    /// The file's text.
    text: String,
}

/// Reads every `.rs` file inside the test directories, and one level down.
///
/// One level down because `inillucent-compat`'s suites are modules of one
/// binary per tier, in `tests/<tier>/<suite>.rs`.
fn sources() -> Vec<Source> {
    let root = inillucent_compat::workspace_root();
    let mut found = Vec::new();
    for directory in TEST_DIRECTORIES {
        let path = root.join(directory);
        let Ok(entries) = std::fs::read_dir(&path) else {
            continue;
        };
        let mut files: Vec<PathBuf> = Vec::new();
        for entry in entries.flatten() {
            let file: PathBuf = entry.path();
            if file.is_dir() {
                if let Ok(inner) = std::fs::read_dir(&file) {
                    files.extend(inner.flatten().map(|entry| entry.path()));
                }
                continue;
            }
            files.push(file);
        }
        for file in files {
            if file.extension().map(|end| end != "rs").unwrap_or(true) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&file) else {
                continue;
            };
            let name = file
                .strip_prefix(&root)
                .map(|inside| inside.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|_| directory.to_string());
            found.push(Source { name, text });
        }
    }
    found
}

/// Whether a file invokes the macro: a line that *starts* with `scenario!(`.
///
/// **The column matters, and this guard is why.** Written as
/// `text.contains("scenario!(")` the first version of this file matched itself
/// - the string appears in the failure message below - and reported that the
/// guard was a scenario suite holding two tests the matrix had not expanded. An
/// invocation is a statement at the top level of a test file and so begins a
/// line; a mention inside a message or a comment is indented.
///
/// @param text - the file's text
fn invokes_the_macro(text: &str) -> bool {
    text.lines().any(|line| line.starts_with("scenario!("))
}

/// Whether a file is a scenario suite: it is named `story_*.rs`, or it uses the
/// macro.
///
/// Both halves matter. The name catches a story file that forgot the macro; the
/// use catches a file that is not named `story_` and is still a scenario suite,
/// which `crates/inillucent/tests/application.rs` is - it held the four stories
/// before the matrix existed and kept its name, because every comment in the
/// tree that cites a story cites that path.
///
/// @param source - the file
fn is_a_scenario_file(source: &Source) -> bool {
    Path::new(&source.name)
        .file_name()
        .map(|name| name.to_string_lossy().starts_with("story_"))
        .unwrap_or(false)
        || invokes_the_macro(&source.text)
}

/// The body of `macro_rules! scenario`, and nothing after it.
///
/// **Bounded by counting braces, not by matching a line.** Reading everything
/// after the definition swept in `matrix.rs`'s own `#[cfg(test)] mod tests`, so
/// the count of expanded arms came back as eleven against six declared - the
/// five extra being unit test names. The first fix for that was to stop at the
/// first `"\n}\n"`, a closing brace in the first column, and it worked until the
/// file was checked out with CRLF line endings: the pattern then matched
/// nowhere and the sweep came back. A scan that counts `{` and `}` cannot be
/// broken by either, and the macro's body is balanced by the compiler's own
/// rules.
///
/// @param source - the text of `matrix.rs`
fn macro_body(source: &str) -> Option<&str> {
    let at = source.find("macro_rules! scenario")?;
    let rest = source.get(at..)?;
    let opens = rest.find('{')?;
    let mut depth = 0usize;
    for (offset, character) in rest.char_indices().skip(opens) {
        match character {
            '{' => depth = depth.saturating_add(1),
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return rest.get(opens..=offset);
                }
            }
            _ => {}
        }
    }
    None
}

/// What a story file must say to run at fewer than every arm.
///
/// **An opt-out that the file itself has to carry, in words a reviewer sees.**
/// Two stories cannot run at six arms and should still be called stories:
/// `story_ledger_day` issues eight hundred transactions and costs five seconds
/// an arm, so six of them would be most of the e2e tier's fifteen second
/// budget, and `story_ledger_day_nightly` issues a hundred thousand. The answer
/// is not to let a file opt out by saying nothing - that is the failure this
/// guard exists for - but to make opting out a sentence in the file, beside the
/// arms it does run at.
const OPT_OUT: &str = "**Not a matrix story.**";

/// Counts the bare `#[test]` attributes in a file.
///
/// A `scenario!` expansion carries its own, so a scenario file's source has
/// none of its own: the count is exactly the number of tests that are *not*
/// run at every arm.
///
/// @param text - the file's text
fn bare_tests(text: &str) -> usize {
    text.lines().filter(|line| line.trim() == "#[test]").count()
}

/// Every scenario file runs its tests through `scenario!`, and the macro
/// expands every quick arm.
///
/// **Remove `sqlite_page` from the macro in `matrix.rs` and this fails**, which
/// is rule 1.5: the guard is checked by taking away the thing it guards. It is
/// also what makes "we run at SQLite's page size" a fact the build verifies
/// rather than a sentence in a design document.
#[test]
fn scenarios_run_every_quick_arm() {
    let sources = sources();
    assert!(
        sources.len() >= 20,
        "read {} test files out of {TEST_DIRECTORIES:?}, which means the scan is looking in the \
         wrong place rather than that the tree is nearly empty",
        sources.len()
    );

    let scenario_files: Vec<&Source> = sources
        .iter()
        .filter(|one| is_a_scenario_file(one))
        .collect();
    assert!(
        !scenario_files.is_empty(),
        "no file in {TEST_DIRECTORIES:?} is a scenario suite, so this guard asserted nothing"
    );

    let mut without_the_macro: Vec<&str> = Vec::new();
    let mut with_a_bare_test: Vec<String> = Vec::new();
    let mut opted_out: Vec<&str> = Vec::new();
    let mut opted_out_badly: Vec<String> = Vec::new();
    for file in &scenario_files {
        if file.text.contains(OPT_OUT) {
            // A file that opts out has to name the arms it does run at, so the
            // configuration is still a choice somebody made rather than
            // whatever `Database::open` happens to give.
            opted_out.push(&file.name);
            if !file.text.contains("_arm()") {
                opted_out_badly.push(format!("{} (says `{OPT_OUT}` and names no arm)", file.name));
            }
            if invokes_the_macro(&file.text) {
                opted_out_badly.push(format!(
                    "{} (says `{OPT_OUT}` and invokes `scenario!` anyway)",
                    file.name
                ));
            }
            continue;
        }
        if !invokes_the_macro(&file.text) {
            without_the_macro.push(&file.name);
        }
        let bare = bare_tests(&file.text);
        if bare > 0 {
            with_a_bare_test.push(format!("{} ({bare})", file.name));
        }
    }
    assert!(
        opted_out_badly.is_empty(),
        "these files opt out of the matrix and do not say what they run at instead:\n  {}",
        opted_out_badly.join("\n  ")
    );
    // **Three, and the third argued for itself** (task-2066 section 4.4.5). The
    // bound was two and the message asked a third to make its case rather than
    // join a list, which is what `story_large_table_nightly.rs` does in its
    // module documentation: each arm's pool is `frames * page_size`, so ten
    // times it is 1.25 GiB at `default` and at `truncate-journal`, 160 MiB at
    // three more, and 2.5 MiB at `small-pool`. Narrowing the pool at every arm
    // instead is 520 seconds apiece, fifty two minutes for one target against a
    // whole gate of thirty one. The bound moves when a story has done that
    // arithmetic in public, and not otherwise.
    assert!(
        opted_out.len() <= 3,
        "{} story files opt out of the matrix, and the exemption was written for the two ledger \
         soaks and the large-table nightly. A fourth is a story that should be arguing for \
         itself rather than joining a list:\n  {}",
        opted_out.len(),
        opted_out.join("\n  ")
    );
    assert!(
        without_the_macro.is_empty(),
        "these files are named `story_*.rs` and do not invoke `scenario!`, so whatever they test \
         runs at one configuration:\n  {}\n\
         Write each test as `fn story(arm: &Arm, area: &Path)` and add `scenario!(name);`. If \
         the story genuinely cannot run six times - a soak, a long form - say `{OPT_OUT}` in its \
         module documentation, with the reason and the arms it does run at.",
        without_the_macro.join("\n  ")
    );
    assert!(
        with_a_bare_test.is_empty(),
        "these scenario files hold a `#[test]` the matrix did not expand, so the file grades one \
         story six ways and another once and the run's output cannot tell them apart:\n  {}\n\
         Write the test as `fn story(arm: &Arm, area: &Path)` and add `scenario!(name, story);`.",
        with_a_bare_test.join("\n  ")
    );

    // The assertion the guard is named after: the macro names a constructor per
    // arm, and every quick arm's constructor is one of them.
    let macro_source = std::fs::read_to_string(
        inillucent_compat::workspace_root().join("crates/inillucent-compat/src/matrix.rs"),
    )
    .expect("matrix.rs is readable");
    let Some(body) = macro_body(&macro_source) else {
        panic!("matrix.rs no longer defines `scenario!`, so no scenario file can be expanding it");
    };
    let mut missing: Vec<String> = Vec::new();
    for arm in inillucent_compat::matrix::arms(inillucent_compat::matrix::Kind::Quick) {
        let constructor = format!("{}_arm()", arm.test_name());
        let test = format!("fn {}()", arm.test_name());
        if !body.contains(&constructor) || !body.contains(&test) {
            missing.push(format!(
                "{} (wanted `{constructor}` and `{test}`)",
                arm.name
            ));
        }
    }
    assert!(
        missing.is_empty(),
        "`scenario!` does not expand a test for every quick arm, so every story in the tree has \
         silently stopped running at:\n  {}",
        missing.join("\n  ")
    );
}

/// Every arm the macro expands is an arm the matrix declares.
///
/// The other direction of the same rule. A test called `::sqlite_page` that no
/// longer corresponds to a row in `arms(Kind::Full)` is a test whose name says
/// it runs at a configuration the matrix has stopped describing, and a reader
/// of the run's output has no way to know.
#[test]
fn every_expanded_arm_is_one_the_matrix_declares() {
    let macro_source = std::fs::read_to_string(
        inillucent_compat::workspace_root().join("crates/inillucent-compat/src/matrix.rs"),
    )
    .expect("matrix.rs is readable");
    let Some(body) = macro_body(&macro_source) else {
        panic!("matrix.rs no longer defines `scenario!`");
    };
    let declared: Vec<String> =
        inillucent_compat::matrix::arms(inillucent_compat::matrix::Kind::Full)
            .iter()
            .map(|arm| arm.test_name())
            .collect();
    let expanded: Vec<String> = body
        .lines()
        .filter_map(|line| line.trim().strip_prefix("fn "))
        .filter_map(|rest| rest.split('(').next())
        .map(str::to_string)
        .collect();
    assert_eq!(
        expanded.len(),
        declared.len(),
        "`scenario!` expands {} tests and the matrix declares {} arms:\n  expanded {expanded:?}\n  \
         declared {declared:?}",
        expanded.len(),
        declared.len()
    );
    for name in &expanded {
        assert!(
            declared.contains(name),
            "`scenario!` expands `{name}`, which is not an arm the matrix declares: {declared:?}"
        );
    }
}
