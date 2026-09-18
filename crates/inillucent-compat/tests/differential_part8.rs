//! The 644 statements task-1979 asked both engines, run from the tree.
//!
//! Invariant: **every case in `corpora/differential-part8/` agrees with the
//! pinned SQLite 3.53.4 shell, unless its id is in `allow.list` with the defect
//! it belongs to - and an id in that list that now agrees fails too.** The
//! second half is what makes the first half worth having: a list that only
//! grows is a list of things nobody will ever take off it, and a fixed defect
//! left listed reads as coverage and is not (the rule from task-1969 section 4,
//! restated in task-1979 section 6.3).
//!
//! **Why the shell and not the `sqlite-oracle` binary `differential.rs`
//! drives.** These cases are whole scripts - a `CREATE TABLE`, some inserts,
//! then the question - and what they compare is the answer a person or an ORM
//! would see, rendered. The oracle protocol carries one operation at a time
//! with tagged values, which is the right tool for asking what a *value*
//! compares as and the wrong one for asking what a *statement* answers. The
//! reviewer's own runner drove both shells in JSON mode; this is that, in the
//! tree, over the same cases.
//!
//! **A run with no pinned shell records nothing rather than passing.** The
//! suite declares `requires = ["oracle"]` in `tests/selection.toml`, so a
//! machine without it skips visibly under `--strict`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use inillucent_compat::cliproc::program;
use inillucent_compat::workspace_root;

/// Where the cases live.
fn corpus() -> PathBuf {
    workspace_root().join("crates/inillucent-compat/tests/corpora/differential-part8")
}

/// Returns the pinned SQLite shell, if it has been downloaded.
fn pinned_shell() -> Option<PathBuf> {
    let path = workspace_root()
        .join(".sqlite-ref/3.53.4/shell")
        .join(format!("sqlite3{}", std::env::consts::EXE_SUFFIX));
    path.is_file().then_some(path)
}

/// One case: an id, the category its file is named after, and the statements.
struct Case {
    id: String,
    category: String,
    sql: String,
}

/// Reads every case file in the corpus.
///
/// The format is deliberately the crudest thing that carries these cases: one
/// per line, the id, a tab, and the statements with `\n` escaped. A format with
/// any structure would need a parser, and a parser is a second thing that can
/// disagree with what the reviewer ran.
fn cases() -> Vec<Case> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(corpus()) else {
        return found;
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|kind| kind == "cases"))
        .collect();
    files.sort();
    for file in files {
        let category = file
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default();
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        for line in text.lines() {
            if line.starts_with('#') || line.trim().is_empty() {
                continue;
            }
            let Some((id, sql)) = line.split_once('\t') else {
                panic!("{}: a case line has no tab: {line}", file.display());
            };
            found.push(Case {
                id: id.to_string(),
                category: category.clone(),
                sql: sql.replace("\\n", "\n").replace("\\\\", "\\"),
            });
        }
    }
    found
}

/// Reads the allow list: the ids that are known to disagree, and why.
fn allowed() -> BTreeMap<String, String> {
    let path = corpus().join("allow.list");
    let mut out = BTreeMap::new();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return out;
    };
    for line in text.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let Some((id, said)) = line.split_once('\t') else {
            panic!("{}: an allow line has no tab: {line}", path.display());
        };
        out.insert(id.trim().to_string(), said.trim().to_string());
    }
    out
}

/// What running one script through one shell produced.
struct Answer {
    code: i32,
    out: String,
    err: String,
}

/// Runs a script through a shell in JSON mode against an in-memory database.
///
/// `-bail` so that a script stops at its first failure in both engines, which
/// is what makes a comparison of the *whole* answer meaningful rather than a
/// comparison of how far each one got.
///
/// @param shell - the binary to run
/// @param arguments - the flags that put it in JSON mode
/// @param sql - the statements
fn run(shell: &Path, arguments: &[&str], sql: &str) -> Answer {
    use std::io::Write;
    let mut child = Command::new(shell)
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("{} did not start: {error}", shell.display()));
    if let Some(pipe) = child.stdin.as_mut() {
        let _ = pipe.write_all(sql.as_bytes());
        let _ = pipe.write_all(b"\n");
    }
    drop(child.stdin.take());
    let output = child
        .wait_with_output()
        .unwrap_or_else(|error| panic!("{} did not finish: {error}", shell.display()));
    Answer {
        code: output.status.code().unwrap_or(130),
        out: String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
        err: String::from_utf8_lossy(&output.stderr).replace("\r\n", "\n"),
    }
}

/// Strips whitespace that is formatting rather than an answer.
///
/// @param text - what a shell printed
fn flattened(text: &str) -> String {
    text.lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<&str>>()
        .join("\n")
}

/// How the two engines answered one case.
#[derive(Debug, Eq, PartialEq)]
enum Verdict {
    /// The same rows, or both refused.
    Agree,
    /// Different rows.
    RowsDiffer,
    /// This engine refused where SQLite answered, because the construct is not
    /// built. Exit code 3, which is the engine's own "not yet" and is a
    /// documented answer rather than a wrong one.
    NotBuilt,
    /// This engine refused where SQLite answered, for some other reason.
    RefusedOnly,
    /// SQLite refused where this engine answered.
    AcceptedOnly,
}

/// Compares two answers.
///
/// @param oracle - what the pinned shell said
/// @param ours - what this engine said
fn verdict(oracle: &Answer, ours: &Answer) -> Verdict {
    match (oracle.code != 0, ours.code != 0) {
        (true, true) => Verdict::Agree,
        (true, false) => Verdict::AcceptedOnly,
        (false, true) => match ours.code == 3
            || ours.err.to_ascii_lowercase().contains("not built")
            || ours.err.to_ascii_lowercase().contains("unsupported")
        {
            true => Verdict::NotBuilt,
            false => Verdict::RefusedOnly,
        },
        (false, false) => match flattened(&oracle.out) == flattened(&ours.out) {
            true => Verdict::Agree,
            false => Verdict::RowsDiffer,
        },
    }
}

/// Every case agrees with SQLite, except the ones the allow list names - and
/// every id the allow list names still disagrees.
#[test]
fn the_corpus_agrees_with_sqlite_outside_the_allow_list() {
    let Some(oracle) = pinned_shell() else {
        inillucent_base::testing::skipping("the pinned SQLite 3.53.4 shell is not downloaded");
        return;
    };
    let Some(shell) = program("inillucent-shell") else {
        return;
    };
    let cases = cases();
    assert!(
        cases.len() >= 600,
        "the corpus holds {} cases, which means it is not the one task-1979 ran",
        cases.len()
    );
    let allowed = allowed();

    let mut unexpected: Vec<String> = Vec::new();
    let mut stale: Vec<String> = Vec::new();
    for case in &cases {
        let theirs = run(
            &oracle,
            &["-cmd", ".mode json", "-bail", ":memory:"],
            &case.sql,
        );
        let ours = run(&shell, &["-json", "-bail", ":memory:"], &case.sql);
        let said = verdict(&theirs, &ours);
        let listed = allowed.get(&case.id);
        match (said == Verdict::Agree, listed) {
            (true, Some(defect)) => stale.push(format!("{} ({defect})", case.id)),
            (false, None) => unexpected.push(format!(
                "{} [{}] {:?}\n    sql:    {}\n    sqlite: {}\n    ours:   {}{}",
                case.id,
                case.category,
                said,
                case.sql,
                flattened(&theirs.out).replace('\n', " | "),
                flattened(&ours.out).replace('\n', " | "),
                match ours.err.trim().is_empty() {
                    true => String::new(),
                    false => format!("\n    said:   {}", ours.err.trim()),
                }
            )),
            _ => {}
        }
    }

    assert!(
        stale.is_empty(),
        "these ids are in `corpora/differential-part8/allow.list` and now agree with SQLite. \
         Take them off the list; a defect left listed reads as coverage and is not:\n  {}",
        stale.join("\n  ")
    );
    assert!(
        unexpected.is_empty(),
        "{} case(s) disagree with SQLite and are not in \
         `corpora/differential-part8/allow.list`:\n  {}",
        unexpected.len(),
        unexpected.join("\n  ")
    );
}

/// The allow list names no case the corpus does not hold.
///
/// A line left behind after its case was renamed or removed is a rule about
/// nothing, and it would quietly stop the case above from ever checking it.
#[test]
fn the_allow_list_names_only_cases_the_corpus_holds() {
    let cases = cases();
    if cases.is_empty() {
        inillucent_base::testing::skipping("the differential-part8 corpus is not in the tree");
        return;
    }
    let listed = allowed();
    let mut missing: Vec<&str> = Vec::new();
    for id in listed.keys() {
        if !cases.iter().any(|case| &case.id == id) {
            missing.push(id);
        }
    }
    assert!(
        missing.is_empty(),
        "these ids are in the allow list and in no case file:\n  {}",
        missing.join("\n  ")
    );
}
