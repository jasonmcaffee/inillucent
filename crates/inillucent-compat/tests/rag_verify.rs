//! The RAG example's own verification scripts, run.
//!
//! Invariant: **`examples/rag-agent/scripts/verify.sh` and `verify-indexed.sh`
//! are executed and their exit codes are the verdict.** Both are named by
//! `README.md`, by `AGENTS.md` and by `docs/closed-items.md`, three of the eight
//! agent skills point a reader at the example, and neither script was called by
//! anything (task-1969, 5.8).
//!
//! What they check is not something a Rust test should restate. `verify.sh`
//! holds ten questions, each with the Wikipedia article whose passages have to
//! come back in the top five, plus a vector width check and a full-text check.
//! `verify-indexed.sh` asks the same questions through a fresh process, which is
//! the shape task-1911's defect took: an HNSW index answered rows it had not
//! kept, and only a second process could see it. Rewriting either in Rust would
//! be a second copy of ten questions that could disagree with the first.
//!
//! **It needs the embedding weights.** The questions are semantic, so the
//! example's database has to be searchable by meaning, which means ONNX Runtime
//! and the nomic weights that `inillucent setup-embeddings all` installs. It
//! also needs a shell to run a `.sh` with. Both are declared on the row, and
//! both are announced as a skip rather than passed over.

use std::path::{Path, PathBuf};
use std::process::Command;

use inillucent_compat::cliproc::{program, run};
use inillucent_compat::workspace_root;

/// The two scripts, in the order the example's README runs them.
///
/// `verify.sh` first, because `verify-indexed.sh` asks the same questions in a
/// harder way: a failure in the first is a failure of the corpus and a failure
/// in only the second is a failure of the index's durability, and running them
/// the other way round would not tell them apart.
const SCRIPTS: [&str; 2] = ["verify.sh", "verify-indexed.sh"];

/// Returns the example's directory.
fn example() -> PathBuf {
    workspace_root().join("examples/rag-agent")
}

/// Returns a shell that can run a `.sh`, or nothing.
///
/// `bash` by name rather than `sh`: the scripts use `pipefail`, arrays and
/// `[[`, and a POSIX `sh` runs them wrongly rather than refusing them - which
/// would be a failure that is about the shell and reads as a failure of the
/// index.
fn shell() -> Option<String> {
    for name in ["bash", "sh"] {
        let answered = Command::new(name).arg("--version").output();
        if answered.is_ok_and(|output| output.status.success()) {
            return Some(name.to_string());
        }
    }
    None
}

/// Reports whether this build can answer `embed(TEXT)`.
///
/// Asked of the binary rather than of a cargo feature, because the feature and
/// the installed weights are two different things: a build with `embed` on and
/// no weights on the machine refuses at the first question, which is the state
/// every fresh clone is in.
///
/// @param binary - the built `inillucent`
fn can_embed(binary: &Path) -> bool {
    let ran = run(
        binary,
        &["--db", ":memory:", "query", "SELECT embed('a word')"],
    );
    ran.code == 0
}

/// Both of the example's scripts pass.
#[test]
fn the_example_answers_the_questions_it_documents() {
    let binary = program("inillucent");
    let Some(shell) = shell() else {
        inillucent_compat::differential::skipping(
            "no bash on PATH, and the example's checks are shell scripts",
        );
        return;
    };
    let directory = example();
    let database = directory.join("greek-philosophy.rdb");
    if !database.is_file() {
        inillucent_compat::differential::skipping(&format!(
            "{} is not in this checkout; run examples/rag-agent/scripts/build-database.sh",
            database.display()
        ));
        return;
    }
    if !can_embed(&binary) {
        inillucent_compat::differential::skipping(
            "this build cannot answer embed(TEXT); run `inillucent setup-embeddings all` \
             and build with --features inillucent-cli/embed",
        );
        return;
    }

    for script in SCRIPTS {
        let path = directory.join("scripts").join(script);
        assert!(
            path.is_file(),
            "{} is named by the example's README and is not there",
            path.display()
        );
        let output = Command::new(&shell)
            .arg(path.to_string_lossy().replace('\\', "/"))
            .current_dir(&directory)
            // The scripts read `INILLUCENT` for the binary to ask, which is what
            // lets this run the one this build produced rather than one
            // installed on the machine.
            .env("INILLUCENT", binary.to_string_lossy().replace('\\', "/"))
            .output()
            .unwrap_or_else(|error| panic!("{script} did not start: {error}"));
        let said = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.status.success(),
            "{script} reports the example does not answer its own questions:\n{said}"
        );
        // A script that found no questions to ask exits zero too. Both of them
        // print a line per case, so a run that asked nothing is a run with
        // almost no output.
        assert!(
            said.lines().count() >= 5,
            "{script} exited zero having printed {} line(s), so it asked almost nothing:\n{said}",
            said.lines().count()
        );
    }
}

/// The scripts the documents name are the scripts that are there.
///
/// A check that costs no shell and no weights, so a machine with neither still
/// fails when one of them is renamed - which is how a script nothing calls
/// stops existing without anybody noticing.
#[test]
fn every_script_the_documents_name_is_in_the_example() {
    let directory = example();
    for script in SCRIPTS {
        let path = directory.join("scripts").join(script);
        assert!(
            Path::new(&path).is_file(),
            "{} is named by README.md, AGENTS.md and docs/closed-items.md, and is not in \
             this checkout",
            path.display()
        );
    }
}
