//! Every released format, in both directions, against downloaded binaries.
//!
//! Invariant: **the current build reads every published release's file, and
//! every published release reads the current build's file or refuses it by
//! name.** The forward half is the one a format change breaks loudly; the
//! backward half is the one that costs somebody their afternoon, because an
//! application that upgrades one machine and not another has both builds
//! pointed at the same file. Since task-2074 moved the format to 2, every
//! release up to 0.1.7 refuses - see `assert_refused_by_format`.
//!
//! ## Why this is a separate target from `release_format.rs`
//!
//! It needs release binaries that are not in the repository. The archives are
//! downloaded by `tools/build-interop-fixture.ps1` into `tools/cross/bin/releases/`,
//! which is gitignored, so a fresh clone has none of them and every case here
//! would report success having asked nothing. That is the failure rule 1.2
//! names, so the suite skips visibly instead and its row in `tests/selection.toml`
//! says `requires = ["previous-release"]`, which `--strict` counts and names.
//!
//! The tier is `nightly` for the same reason `process_campaign`'s long form is:
//! it spawns a released binary once per question per release, which is minutes
//! rather than seconds.

use std::path::{Path, PathBuf};
use std::process::Command;

use inillucent_compat::differential::skipping;
use inillucent_compat::interop;
use inillucent_compat::matrix::default_arm;
use inillucent_compat::stories::{open, run};
use inillucent_compat::workspace_root;

/// What an older release cannot read out of a file this build wrote, and why.
///
/// **A recorded difference rather than an allow list, and the distinction is
/// the point.** An allow list says "this is wrong and a ticket will fix it".
/// Nothing can fix the answer a published binary gives: 0.1.1 is on people's
/// machines and its code will never change. So the row asserts the difference
/// **still happens**, and a change that made 0.1.1 read the new index would
/// turn this suite red and get the row deleted.
///
/// Each row is the release, the question in `verify.sql` it answers
/// differently, and the sentence that says why.
const KNOWN_GAPS: [(&str, &str, &str); 1] = [(
    "0.1.1",
    "fts.match",
    // **task-2053.** The FTS5 index layout changed in 0.1.2. 0.1.1 reads
    // everything else in the same file - the tables, the `WITHOUT ROWID`
    // entries, the blob over a page, the `inillucent_search` rows and the row
    // that exists only in the log - and it reads `count(*)` and `SELECT rowid,
    // title` out of `note_fts` itself. Only the `MATCH` comes back empty, and
    // it comes back empty rather than refused, because nothing in the index
    // said which layout wrote it. That silence is what task-2053 is about, and
    // the 0.1.1 half of it is history: an index written from that ticket on
    // carries a layout record and a reader that meets a layout it has not got
    // refuses by name, but 0.1.1 shipped before the record existed and will
    // never look for it.
    "the FTS5 index layout changed in 0.1.2, and 0.1.1 answers no rows rather than refusing \
     (task-2053)",
)];

/// The releases that report a file of a later format as damage rather than
/// refusing it by name.
///
/// **The named refusal - `this database is format version N and this build
/// reads version M; upgrade inillucent to open it`, with the status
/// `unsupported` - was added by task-1979 (E3), and 0.1.5 is the first release
/// that carries it.** These three answer `database disk image is malformed:
/// neither meta page is readable` for a file format 2 wrote. They refuse it,
/// which is the thing that matters - nothing they answer is a wrong value - and
/// the message is one a published binary can never be taught to say
/// differently, so it is recorded here rather than asserted away.
const FORMAT_REFUSED_AS_DAMAGE: [&str; 3] = ["0.1.1", "0.1.2", "0.1.3"];

/// Returns the format version the file a release wrote carries.
///
/// Byte 8 of the release's own fixture, which is the release's own answer to
/// which format it writes - and every release reads the format it writes and
/// the ones before it. Read from the checked-in file rather than kept in a
/// table, so a release that moves the number carries its own record of it.
///
/// @param version - the release
fn fixture_format(version: &str) -> u32 {
    let path = interop::directory().join(version).join("app.rdb");
    let bytes = std::fs::read(&path)
        .unwrap_or_else(|why| panic!("{} could not be read: {why}", path.display()));
    let mut format = [0u8; 4];
    format.copy_from_slice(
        bytes
            .get(8..12)
            .expect("a fixture is longer than its header"),
    );
    u32::from_le_bytes(format)
}

/// Reports whether a release reads the format this build writes.
///
/// @param version - the release
fn reads_this_format(version: &str) -> bool {
    fixture_format(version) >= inillucent_pool::meta::FORMAT_VERSION
}

/// Asserts a release refused a file whose format it does not read, and refused
/// it the way it can.
///
/// **Refused, never answered.** task-2074 moved the format to 2 - a leaf's
/// delta area and the page checksum both changed - so a release that writes
/// format 1 cannot read a file this build wrote. What it must not do is read
/// it anyway and answer, and nothing here accepts an answer.
///
/// @param version - the release
/// @param question - what it was asked
/// @param asked - what it said
fn assert_refused_by_format(version: &str, question: &str, asked: &Result<String, String>) {
    let complaint = match asked {
        Ok(got) => panic!(
            "{version} writes format {} and this build writes format {}, and it answered \
             `{question}` with `{got}` instead of refusing the file",
            fixture_format(version),
            inillucent_pool::meta::FORMAT_VERSION
        ),
        Err(complaint) => complaint,
    };
    let named = format!("format version {}", inillucent_pool::meta::FORMAT_VERSION);
    if FORMAT_REFUSED_AS_DAMAGE.contains(&version) {
        assert!(
            complaint.contains("neither meta page is readable"),
            "{version} predates the named refusal and was expected to report the file as \
             unreadable; it said:\n  {complaint}"
        );
        return;
    }
    assert!(
        complaint.contains(&named) && complaint.contains("[unsupported]"),
        "{version} refused a file this build wrote without naming its format:\n  {complaint}"
    );
}

/// Returns the recorded gap for a release and a question, when there is one.
///
/// @param version - the release being asked
/// @param question - the label in `verify.sql`
fn known_gap(
    version: &str,
    question: &str,
) -> Option<&'static (&'static str, &'static str, &'static str)> {
    KNOWN_GAPS
        .iter()
        .find(|(release, name, _)| *release == version && *name == question)
}

/// Returns an empty scratch directory.
///
/// @param name - what to call it
fn scratch(name: &str) -> PathBuf {
    let path = workspace_root()
        .join("target")
        .join("scratch")
        .join("release-format-history")
        .join(name);
    let _ = std::fs::remove_dir_all(&path);
    let _ = std::fs::create_dir_all(&path);
    path
}

/// Asks a released binary one question and returns the single value it gives.
///
/// **`run` with `.mode list` and `.headers off` rather than `query --output
/// json`**, because every statement in `verify.sql` answers with one row of one
/// column and this prints that value and nothing else. The JSON was tried
/// first: it is pretty printed, so reading a scalar back out of it meant
/// matching on the layout of an older release's renderer, which is a thing this
/// suite has no business depending on.
///
/// A refusal comes back as the engine's own `Error [status]: message` line,
/// which is returned as the error rather than compared as a value.
///
/// @param exe - the released `inillucent`
/// @param database - the file to ask
/// @param sql - the statement, which answers with one row of one column
fn ask_release(exe: &Path, database: &Path, sql: &str) -> Result<String, String> {
    let input = format!(".headers off\n.mode list\n{};", sql.trim_end_matches(';'));
    let output = Command::new(exe)
        .arg("run")
        .arg(input)
        .arg("--db")
        .arg(database)
        .output()
        .map_err(|why| format!("{} would not run: {why}", exe.display()))?;
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    let complaint = String::from_utf8_lossy(&output.stderr).into_owned();
    if !output.status.success() || text.starts_with("Error [") || text.contains("Error [") {
        return Err(format!("{}{}", text.trim(), complaint.trim()));
    }
    Ok(text.trim().to_string())
}

/// Writes `build.sql` with the current build and returns the database.
///
/// The same statements every release was given, run through the library rather
/// than through a program, plus the row the fixtures write after a checkpoint -
/// so the file handed to an older binary has a log segment to replay as well as
/// pages to read.
///
/// @param into - a scratch directory
fn write_the_current_format(into: &Path) -> PathBuf {
    let path = into.join("app.rdb");
    let sql = std::fs::read_to_string(interop::build_sql())
        .unwrap_or_else(|why| panic!("tests/interop/build.sql could not be read: {why}"));
    let arm = default_arm();
    let database = open(&arm, &path);
    let connection = database.session();
    run(&connection, &sql);
    run(
        &connection,
        "INSERT INTO note (id, title, body, weight, tag) VALUES (9001, 'the row that only the \
         log holds', 'written after the checkpoint, so a reader that ignores the log cannot see \
         it', 9.5, 'Ledger')",
    );
    drop(database);
    path
}

/// Writes `build.sql` and then the searchable graph, and returns the database.
///
/// The same file as [`write_the_current_format`], plus the `vectors` table
/// `retrieval-build.sql` creates - which is why the two are separate functions
/// rather than one: `verify.sql`'s recorded answers count the rows of the
/// tables `build.sql` makes, and a case that asks those questions has to be
/// handed a file holding exactly those tables.
///
/// @param into - a scratch directory
fn write_the_current_format_with_a_graph(into: &Path) -> PathBuf {
    let path = write_the_current_format(into);
    let sql = std::fs::read_to_string(interop::retrieval_build_sql())
        .unwrap_or_else(|why| panic!("tests/interop/retrieval-build.sql could not be read: {why}"));
    let arm = default_arm();
    let database = open(&arm, &path);
    let connection = database.session();
    run(&connection, &sql);
    drop(database);
    path
}

/// The current build reads every published release's file, not only the newest.
///
/// `release_format.rs` reads the newest fixture in the `e2e` tier, because that
/// is the one a storage change breaks first and it costs a tenth of a second.
/// This is the whole history: six releases, every question, every time.
#[test]
fn the_current_build_reads_every_published_format() {
    let versions = interop::versions();
    assert!(
        versions.len() >= 6,
        "tests/interop holds {} fixtures and six releases have been published",
        versions.len()
    );
    let questions = interop::questions();
    let arm = default_arm();
    let mut compared = 0usize;

    for version in &versions {
        let area = scratch(version);
        let staged = interop::stage(version, &area);
        let database = open(&arm, &staged);
        let connection = database.session();
        let recorded = interop::expected(version);
        assert_eq!(
            recorded.len(),
            questions.len(),
            "{version}: expected.tsv records {} answers and verify.sql asks {}",
            recorded.len(),
            questions.len()
        );
        for (question, (name, wanted)) in questions.iter().zip(recorded.iter()) {
            assert_eq!(
                &question.name, name,
                "{version}: expected.tsv is out of order"
            );
            let got = inillucent_compat::stories::ask(&connection, &question.sql);
            assert_eq!(
                &got, wanted,
                "{version}: `{name}` answers differently under this build"
            );
            compared = compared.saturating_add(1);
        }
    }

    assert_eq!(
        compared,
        versions.len().saturating_mul(questions.len()),
        "some questions were not asked"
    );
}

/// Every released binary reads a file this build wrote, or refuses it by name.
///
/// **The direction that costs an afternoon.** An application on two machines
/// upgrades one of them; the older build then opens a file the newer one wrote.
/// A format that only moves forward is one where that loses the database, and
/// nothing else in the suite asks the question, because nothing else has an
/// older binary to ask it with.
///
/// **Since task-2074 every published release refuses.** This build writes
/// format 2 and every release before it writes format 1, so for each of them
/// the case asserts a refusal, and asserts nothing answered. A release that
/// writes format 2 itself - the first one to ship this change, and every one
/// after - is graded on its answers as before, with no row to add: which of the
/// two a release gets is read off the format of the fixture it wrote.
///
/// A release whose binary is not downloaded is skipped by name. When none of
/// them is, the whole case skips - and `--strict` counts it, so a green with
/// nothing downloaded cannot be read as a green with everything checked.
#[test]
fn every_released_binary_reads_what_this_build_writes() {
    let versions = interop::versions();
    let available: Vec<(String, PathBuf)> = versions
        .iter()
        .filter_map(|version| interop::release_binary(version).map(|exe| (version.clone(), exe)))
        .collect();
    if available.is_empty() {
        skipping(
            "no released binary is on disk; run `pwsh tools/build-interop-fixture.ps1 -Version \
             <version>` for a published release",
        );
        return;
    }

    let area = scratch("backward");
    let database = write_the_current_format(&area);
    let questions = interop::questions();

    // **This build's own answers first, so a failure below is about the older
    // binary.** Without it, a mistake in writing the file reads as every
    // release having lost the ability to read one - six identical failures
    // naming the wrong side.
    {
        let arm = default_arm();
        let handle = open(&arm, &database);
        let connection = handle.session();
        let Some(reference) = versions.last().map(|version| interop::expected(version)) else {
            panic!("tests/interop holds no fixtures");
        };
        for (question, (name, wanted)) in questions.iter().zip(reference.iter()) {
            assert_eq!(
                &inillucent_compat::stories::ask(&connection, &question.sql),
                wanted,
                "this build wrote build.sql and then answered `{name}` differently from every \
                 released fixture, so the file handed to the older binaries is not the one this \
                 case is about"
            );
        }
    }
    let Some(reference) = versions.last().map(|version| interop::expected(version)) else {
        panic!("tests/interop holds no fixtures");
    };

    let mut checked = 0usize;
    let mut gaps_seen = 0usize;
    for (version, exe) in &available {
        for (question, (name, wanted)) in questions.iter().zip(reference.iter()) {
            assert_eq!(&question.name, name, "expected.tsv is out of order");
            if !reads_this_format(version) {
                let asked = ask_release(exe, &database, &question.sql);
                assert_refused_by_format(version, name, &asked);
                checked = checked.saturating_add(1);
                continue;
            }
            let got = match ask_release(exe, &database, &question.sql) {
                Ok(got) => got,
                Err(complaint) => panic!(
                    "{version} could not answer `{name}` against a database this build wrote:\n  \
                     {complaint}"
                ),
            };
            checked = checked.saturating_add(1);

            if let Some((_, _, why)) = known_gap(version, name) {
                assert_ne!(
                    &got, wanted,
                    "{version} now reads `{name}` out of a file this build wrote, and \
                     KNOWN_GAPS still says it cannot. Delete that row: {why}"
                );
                gaps_seen = gaps_seen.saturating_add(1);
                continue;
            }

            assert_eq!(
                &got, wanted,
                "{version} reads `{name}` out of a file this build wrote as `{got}`, where \
                 every release's own fixture says `{wanted}`. If this is a format change that \
                 cannot be undone, it belongs in KNOWN_GAPS with the ticket that records it - \
                 not in an allow list, because an older release's answer can never be fixed."
            );
        }
    }

    assert_eq!(
        checked,
        available.len().saturating_mul(questions.len()),
        "{} releases were on disk and {checked} of {} questions were answered",
        available.len(),
        available.len().saturating_mul(questions.len())
    );

    // Rule 1.3, the other direction: a recorded difference that no longer
    // happens is a row nobody has deleted, which is how a ledger becomes
    // decoration. Only gaps whose release is actually on disk, and reads the
    // format this build writes, are counted: a release that refuses the whole
    // file cannot show a difference in one answer.
    let expected_gaps = KNOWN_GAPS
        .iter()
        .filter(|(version, _, _)| available.iter().any(|(had, _)| had == version))
        .filter(|(version, _, _)| reads_this_format(version))
        .count();
    assert_eq!(
        gaps_seen, expected_gaps,
        "KNOWN_GAPS names {expected_gaps} differences for the releases on disk and {gaps_seen} \
         happened"
    );
}

/// What an older release cannot ask of a search index this build wrote.
///
/// The retrieval half's own version of [`KNOWN_GAPS`], and kept separate
/// because the two answer different questions: that one is about `verify.sql`'s
/// recorded answers, this one about the graph `retrieval-build.sql` writes.
/// The rule is the same. A published binary's answer can never be fixed, so a
/// row asserts the difference **still happens**, and a change that made the
/// release answer correctly turns this suite red and gets the row deleted.
///
/// Each row is the release, the question in `retrieval.sql`, and why.
const KNOWN_RETRIEVAL_GAPS: [(&str, &str, &str); 0] = [];

/// Returns the recorded retrieval gap for a release and a question.
///
/// @param version - the release being asked
/// @param question - the label in `retrieval.sql`
fn known_retrieval_gap(
    version: &str,
    question: &str,
) -> Option<&'static (&'static str, &'static str, &'static str)> {
    KNOWN_RETRIEVAL_GAPS
        .iter()
        .find(|(release, name, _)| *release == version && *name == question)
}

/// Every released binary still searches a graph this build wrote.
///
/// **The question nothing in the suite asked** (task-2053). `verify.sql` asks
/// an `inillucent_search` table for its rows and its content, which is a read
/// of `%_content`; it never asks it to *search*, so no term query, no ranked
/// query and no nearest-neighbour query had ever been run by an older binary
/// against a graph a newer build wrote. The half of the file SQLite has no
/// equivalent of is the half a format change is most likely to move, and it was
/// the half nothing was checking.
///
/// A release that cannot search the graph must **refuse**. That is the whole of
/// what task-2053 is about: an empty result set is a legitimate answer to a
/// search, so a build that answers one for an index it cannot read has told the
/// application the documents do not exist.
#[test]
fn every_released_binary_searches_what_this_build_writes() {
    let versions = interop::versions();
    let available: Vec<(String, PathBuf)> = versions
        .iter()
        .filter_map(|version| interop::release_binary(version).map(|exe| (version.clone(), exe)))
        .collect();
    if available.is_empty() {
        skipping(
            "no released binary is on disk; run `pwsh tools/build-interop-fixture.ps1 -Version \
             <version>` for a published release",
        );
        return;
    }

    let area = scratch("retrieval");
    let database = write_the_current_format_with_a_graph(&area);
    let questions = interop::retrieval_questions();
    assert!(
        !questions.is_empty(),
        "tests/interop/retrieval.sql asks nothing, so this case would compare nothing"
    );

    // **This build's own answers are the reference.** There is no recorded one:
    // the graph is written here rather than checked in, for the reason
    // `retrieval.sql` gives. So the comparison is between two builds reading
    // one file, which is what the case is about.
    let reference: Vec<String> = {
        let arm = default_arm();
        let handle = open(&arm, &database);
        let connection = handle.session();
        questions
            .iter()
            .map(|question| inillucent_compat::stories::ask(&connection, &question.sql))
            .collect()
    };
    for (question, answer) in questions.iter().zip(reference.iter()) {
        assert!(
            !answer.is_empty(),
            "this build answered `{}` with nothing, so the file handed to the older binaries \
             does not hold the graph this case is about",
            question.name
        );
    }

    let mut checked = 0usize;
    let mut gaps_seen = 0usize;
    for (version, exe) in &available {
        for (question, wanted) in questions.iter().zip(reference.iter()) {
            let asked = ask_release(exe, &database, &question.sql);
            checked = checked.saturating_add(1);
            // A release of an older format refuses the whole file; see
            // `assert_refused_by_format`.
            if !reads_this_format(version) {
                assert_refused_by_format(version, &question.name, &asked);
                continue;
            }

            if let Some((_, _, why)) = known_retrieval_gap(version, &question.name) {
                assert!(
                    asked.as_ref().is_ok_and(|got| got != wanted) || asked.is_err(),
                    "{version} now answers `{}` the way this build does, and \
                     KNOWN_RETRIEVAL_GAPS still says it cannot. Delete that row: {why}",
                    question.name
                );
                gaps_seen = gaps_seen.saturating_add(1);
                continue;
            }

            let got = match asked {
                Ok(got) => got,
                Err(complaint) => panic!(
                    "{version} could not answer `{}` against a graph this build wrote:\n  \
                     {complaint}",
                    question.name
                ),
            };
            assert_eq!(
                &got, wanted,
                "{version} answers `{}` as `{got}` where this build answers `{wanted}` on the \
                 same file. If this is a format change that cannot be undone it belongs in \
                 KNOWN_RETRIEVAL_GAPS with the ticket that records it - and the release has to \
                 *refuse*, because an empty answer to a search is one an application cannot \
                 tell from a correct one.",
                question.name
            );
        }
    }

    assert_eq!(
        checked,
        available.len().saturating_mul(questions.len()),
        "{} releases were on disk and {checked} of {} questions were asked",
        available.len(),
        available.len().saturating_mul(questions.len())
    );
    let expected_gaps = KNOWN_RETRIEVAL_GAPS
        .iter()
        .filter(|(version, _, _)| available.iter().any(|(had, _)| had == version))
        .filter(|(version, _, _)| reads_this_format(version))
        .count();
    assert_eq!(
        gaps_seen, expected_gaps,
        "KNOWN_RETRIEVAL_GAPS names {expected_gaps} differences for the releases on disk and \
         {gaps_seen} happened"
    );
}
