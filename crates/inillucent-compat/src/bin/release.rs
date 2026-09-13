//! Assembles the release candidate, and says plainly whether it passes.
//!
//! Invariant: this reports, it does not decide. The gates were written down
//! before the numbers - the capability manifest, the performance contract's
//! headline bound and family floors, the platform matrix - and this program
//! reads what the runs recorded and states the verdict each gate reaches. A
//! release that does not meet its bar says so on the first line of its own
//! report; there is no argument in this file for why a number is acceptable.
//!
//! What it gathers, which is the TDD's release-artifact list:
//!
//! - the crates, the C library, the CLI and the migration tool, with digests;
//! - the compatibility report and the parity manifest it was generated from;
//! - the performance contract, the scorecard, its raw history and the dashboard;
//! - the reference metadata and the supported-platform matrix;
//! - the exact commands that reproduce every one of them.
//!
//! Usage: `cargo run --release -p inillucent-compat --bin inillucent-release --
//! [--out <dir>] [--measurements <scorecard-dir>]`

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use inillucent_base::hash::Sha256;
use inillucent_compat::history::History;
use inillucent_compat::perf::Contract;
use inillucent_compat::{platform_name, workspace_root};

/// The platforms a release is claimed on.
const PLATFORMS: [&str; 2] = ["windows-x86_64", "linux-x86_64"];

/// The commands that reproduce every artifact, in the order they are run.
const COMMANDS: [(&str, &str); 9] = [
    (
        "the pinned reference",
        "tools/sqlite-reference.ps1   # or tools/sqlite-reference.sh on POSIX",
    ),
    (
        "the engine and its tools",
        "cargo build --release --workspace",
    ),
    (
        "the test evidence",
        "cargo run --release -p inillucent-compat --bin inillucent-evidence",
    ),
    (
        "the compatibility report",
        "cargo run --release -p inillucent-compat --bin inillucent-manifest",
    ),
    (
        "the performance scorecard",
        "cargo run --release -p inillucent-compat --bin inillucent-scorecard -- --scale all --rounds 30 \
         --label <name>",
    ),
    (
        "each optimization's A/B arm",
        "cargo run --release -p inillucent-compat --bin inillucent-scorecard -- --scale small --rounds 30 \
         --label <name> --disable covering-index   # then --disable indexed-write",
    ),
    (
        "the storage and write profiles",
        "cargo run --release -p inillucent-compat --bin inillucent-storageprofile && cargo run --release \
         -p inillucent-compat --bin inillucent-writeprofile",
    ),
    (
        "a legacy index migration",
        "cargo run --release -p inillucent-migrate -- <index-dir> <destination.db> --sqlite \
         .sqlite-ref/3.53.4/shell/sqlite3",
    ),
    (
        "this release candidate",
        "cargo run --release -p inillucent-compat --bin inillucent-release",
    ),
];

/// Builds the release candidate.
fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let out = flag(&arguments, "--out")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root().join("compat/release"));
    let measurements = flag(&arguments, "--measurements").map(PathBuf::from);
    match run(&out, measurements.as_deref()) {
        Ok(passed) => {
            println!("release candidate written to {}", out.display());
            if passed {
                ExitCode::SUCCESS
            } else {
                println!("the release gates are not all met; see release.md");
                ExitCode::FAILURE
            }
        }
        Err(reason) => {
            eprintln!("{reason}");
            ExitCode::FAILURE
        }
    }
}

/// Returns the value of a `--flag value` argument.
fn flag(arguments: &[String], name: &str) -> Option<String> {
    let position = arguments.iter().position(|argument| argument == name)?;
    arguments.get(position.saturating_add(1)).cloned()
}

/// One artifact the release ships.
struct Artifact {
    name: String,
    bytes: u64,
    sha256: String,
    description: String,
}

/// Digests one file, or reports that it is not there.
fn artifact(root: &Path, relative: &str, description: &str) -> Option<Artifact> {
    let path = root.join(relative);
    let bytes = std::fs::read(&path).ok()?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Some(Artifact {
        name: relative.to_string(),
        bytes: bytes.len() as u64,
        sha256: hasher.hex(),
        description: description.to_string(),
    })
}

/// Gathers every artifact, writes the report, and returns whether it passes.
fn run(out: &Path, measurements: Option<&Path>) -> Result<bool, String> {
    let root = workspace_root();
    std::fs::create_dir_all(out).map_err(|error| format!("cannot create {out:?}: {error}"))?;
    let suffix = std::env::consts::EXE_SUFFIX;
    // `inillucent-capi`, the drop-in `sqlite3_*` C ABI, was deleted with the old
    // engine it sat over. `inillucent-driver-capi` is what replaced it - a
    // bespoke API of its own rather than a `sqlite3.h` workalike - and it names
    // its artifact after the crate rather than the header for the reason its
    // own manifest gives: a `cdylib` named `inillucent_driver` would collide
    // with the `inillucent-driver` rlib it links.
    let library = if cfg!(windows) {
        "target/release/inillucent_driver_capi.dll"
    } else {
        "target/release/libinillucent_driver_capi.so"
    };
    let wanted: Vec<(&str, String, &str)> = vec![
        (
            "cli",
            format!("target/release/inillucent-shell{suffix}"),
            "the SQLite-like shell",
        ),
        (
            "migrate",
            format!("target/release/inillucent-migrate{suffix}"),
            "the resumable copy-and-verify migration tool",
        ),
        (
            "capi",
            library.to_string(),
            "the C ABI over inillucent-driver, for every language that is not Rust",
        ),
        (
            "report",
            "compat/compat-report.md".to_string(),
            "the compatibility report",
        ),
        (
            "report-json",
            "compat/compat-report.json".to_string(),
            "the same, machine readable",
        ),
        (
            "manifest",
            "compat/sqlite-3.53.4.toml".to_string(),
            "the parity manifest the report is generated from",
        ),
        (
            "contract",
            "compat/perf/contract.toml".to_string(),
            "the performance contract: weights, floors and the headline bound",
        ),
        (
            "references",
            "docs/reference-register.toml".to_string(),
            "every external project consulted, and in what capacity",
        ),
        (
            "layering",
            "docs/invariants/layering.toml".to_string(),
            "the dependency-direction contract",
        ),
        (
            "baseline",
            "compat/baseline/inillucent-core-baseline.json".to_string(),
            "the retrieval engine's frozen baseline",
        ),
        (
            "amendments",
            "compat/baseline/inillucent-core-amendments.toml".to_string(),
            "every declared change to it, with its reason",
        ),
    ];
    let mut artifacts: Vec<Artifact> = Vec::new();
    let mut missing: Vec<String> = Vec::new();
    for (_, relative, description) in &wanted {
        match artifact(&root, relative, description) {
            Some(found) => artifacts.push(found),
            None => missing.push(relative.clone()),
        }
    }
    // The scorecard's own outputs live under the agent-output area rather than
    // in the repository, because they are a measurement of one machine.
    //
    // Ticket-neutral, and overridable with `--measurements`. This used to name
    // one ticket's scratch directory, which meant a later ticket's release
    // candidate was assembled out of an earlier ticket's measurements: the
    // scorecard it had just published into `compat/release` was copied over
    // with the older one, mtime and all, and the report then quoted numbers
    // from a run that was not this build's.
    let scorecard_dir = measurements
        .map(Path::to_path_buf)
        .unwrap_or_else(|| root.join("_agent_output/measurements/scorecard"));
    let scorecard_name = scorecard_dir
        .strip_prefix(&root)
        .unwrap_or(&scorecard_dir)
        .to_string_lossy()
        .replace('\\', "/");
    for (relative, description) in [
        (
            "../migrate/release/corpus.db.migration-report.md",
            "the migration report for an index of the repository's own prose, at release size",
        ),
        (
            "../migrate/release/corpus.db.migration-manifest",
            "that migration's append-only manifest, which is what a resume reads",
        ),
        (
            "../migrate/full/corpus.db.migration-report.md",
            "the migration report for the small corpus that has one of everything",
        ),
        ("scorecard.md", "the performance scorecard"),
        (
            "arm-no-covering-index.md",
            "the same scorecard with the covering-index lever switched off",
        ),
        (
            "arm-no-indexed-write.md",
            "the same scorecard with the indexed-write lever switched off",
        ),
        (
            "arm-no-ordered-walk.md",
            "the same scorecard with the ordered-walk lever switched off",
        ),
        (
            "arm-no-streaming-group.md",
            "the same scorecard with the streaming-group lever switched off",
        ),
        (
            "arm-no-fused-bytecode.md",
            "the same scorecard with the fused-bytecode lever switched off",
        ),
        (
            "../checkpoint/checkpoint.md",
            "the checkpoint-scheduling lever, measured against its own arm and left off",
        ),
        ("scorecard.json", "the same, machine readable"),
        (
            "history.jsonl",
            "the raw performance history, one line per workload per run",
        ),
        (
            "dashboard.md",
            "every workload's ratio across every recorded run",
        ),
    ] {
        if let Some(found) = artifact(&scorecard_dir, relative, description) {
            // Copied into the candidate rather than referenced where they were
            // produced. A release is a directory somebody can hand over; a
            // report pointing at a gitignored scratch path on one machine is
            // not one, however correct its digests are.
            let landed = out.join(landing_name(relative));
            match std::fs::copy(scorecard_dir.join(relative), &landed) {
                Ok(_) => artifacts.push(Artifact {
                    name: format!("compat/release/{}", landing_name(relative)),
                    ..found
                }),
                Err(error) => {
                    missing.push(format!("{}: {error}", named(&scorecard_name, relative)))
                }
            }
        } else {
            missing.push(named(&scorecard_name, relative));
        }
    }

    let contract = Contract::parse(
        &std::fs::read_to_string(root.join("compat/perf/contract.toml"))
            .map_err(|error| format!("cannot read the performance contract: {error}"))?,
    )?;
    let history = History::load(&scorecard_dir.join("history.jsonl"));
    let report = render(&artifacts, &missing, &contract, &history);
    let passed = !report.contains("**FAIL**");
    std::fs::write(out.join("release.md"), &report)
        .map_err(|error| format!("cannot write the release report: {error}"))?;
    Ok(passed)
}

/// Returns the name one measurement artifact takes inside the candidate.
///
/// Flattened from its path rather than from its file name, because the two
/// migration reports are both called `corpus.db.migration-report.md` and one
/// would silently overwrite the other - which it did.
/// @param relative - the path as the gathering loop wrote it
fn landing_name(relative: &str) -> String {
    match relative.strip_prefix("../") {
        Some(rest) => rest.replace('/', "-"),
        None => relative.to_string(),
    }
}

/// Returns the repository-relative name of one measurement artifact.
///
/// They are gathered from the scorecard directory, and a couple of them sit
/// beside it rather than in it, so the path a reader is given is the one that
/// actually leads to the file rather than the one the loop happened to use.
/// @param relative - the path as the gathering loop wrote it
fn named(base: &str, relative: &str) -> String {
    match relative.strip_prefix("../") {
        Some(rest) => {
            let parent = base.rsplit_once('/').map_or("", |(head, _)| head);
            format!("{parent}/{rest}")
        }
        None => format!("{base}/{relative}"),
    }
}

/// Renders the release report.
fn render(
    artifacts: &[Artifact],
    missing: &[String],
    contract: &Contract,
    history: &History,
) -> String {
    let mut out = String::new();
    out.push_str("# inillucent release candidate\n\n");

    let latest = latest_label(history);
    let mut gates: Vec<(String, bool, String)> = Vec::new();
    gates.push(compatibility_gate());
    gates.extend(performance_gates(contract, history, latest.as_deref()));
    gates.push((
        "artifacts".to_string(),
        missing.is_empty(),
        if missing.is_empty() {
            format!("{} artifacts present and digested", artifacts.len())
        } else {
            format!("missing: {}", missing.join(", "))
        },
    ));

    let passed = gates.iter().all(|(_, ok, _)| *ok);
    if passed {
        out.push_str("**Every release gate is met.**\n\n");
    } else {
        out.push_str(
            "**This candidate does not pass.** The gates it fails are marked below, with the \
             numbers they were judged against. Nothing here argues that a number is acceptable: \
             the bars were written down before the runs.\n\n",
        );
    }

    out.push_str("## Gates\n\n| gate | verdict | detail |\n|---|---|---|\n");
    for (name, ok, detail) in &gates {
        let verdict = if *ok { "pass" } else { "**FAIL**" };
        out.push_str(&format!("| `{name}` | {verdict} | {detail} |\n"));
    }

    out.push_str("\n## Supported platforms\n\n| platform | evidence |\n|---|---|\n");
    for platform in PLATFORMS {
        let recorded = history
            .entries
            .iter()
            .any(|entry| entry.platform == platform);
        let evidence = if recorded {
            "compatibility evidence and a performance scorecard"
        } else {
            "compatibility evidence only"
        };
        let here = if platform == platform_name() {
            " (this machine)"
        } else {
            ""
        };
        out.push_str(&format!("| `{platform}`{here} | {evidence} |\n"));
    }

    out.push_str("\n## Artifacts\n\n| file | bytes | sha256 | what it is |\n|---|---:|---|---|\n");
    for found in artifacts {
        out.push_str(&format!(
            "| `{}` | {} | `{}` | {} |\n",
            found.name, found.bytes, found.sha256, found.description
        ));
    }
    if !missing.is_empty() {
        out.push_str("\nNot present in this candidate:\n\n");
        for name in missing {
            out.push_str(&format!("- `{name}`\n"));
        }
    }

    out.push_str("\n## Reproducing this\n\n");
    out.push_str(
        "A clean machine with a Rust toolchain and a C compiler reproduces every artifact above \
         with these commands, in this order. Nothing needs another database engine installed: \
         the reference is downloaded, checksum-verified against the sums SQLite publishes, and \
         built from source into `.sqlite-ref/`, which no inillucent crate links against.\n\n",
    );
    out.push_str("| artifact | command |\n|---|---|\n");
    for (what, command) in COMMANDS {
        out.push_str(&format!("| {what} | `{command}` |\n"));
    }

    out.push_str("\n## Upgrade and downgrade\n\n");
    out.push_str(
        "The default writer produces the SQLite file format and nothing else, so an upgrade is a \
         binary swap: the file a previous build wrote is the file this one opens, and the file \
         this one writes is one the pinned SQLite opens. That is what the interoperability suites \
         check in both directions, and what the migration tool's own probe checks on the database \
         it just built.\n\n\
         A downgrade is the same swap in reverse, with one condition: a database holding a \
         `inillucent_search` table is read by any build - the index lives in ordinary tables - but it \
         is *queried* only by a build that has the module. An older build opens the file, reads \
         every relational table, and reports `no such module: inillucent_search` for the search table \
         alone.\n\n\
         Legacy retrieval indexes migrate with `inillucent-migrate`, which never writes to the \
         source. Going back is not an undo; it is pointing the application at the directory that \
         never changed.\n",
    );

    out.push_str("\n## Known limitations\n\n");
    out.push_str(&limitations(history));
    out
}

/// Returns what each optimization lever was worth, read from the history.
///
/// Computed rather than written down, because a sentence carrying numbers is a
/// sentence that goes stale the next time anything is measured - and a stale
/// number in a release report is worse than no number.
/// @param history - the recorded runs
fn arms(history: &History) -> String {
    let platform = platform_name();
    let headline = |arm: &str| -> Option<f64> {
        history
            .entries
            .iter()
            .rfind(|entry| {
                entry.platform == platform
                    && entry.scale == "small"
                    && entry.workload == "*headline*"
                    && entry.arm == arm
            })
            .map(|entry| entry.ratio)
    };
    let Some(shipped) = headline("") else {
        return String::new();
    };
    let mut measured: Vec<String> = Vec::new();
    for (lever, name) in [
        ("covering-index", "the covering-index lever"),
        ("indexed-write", "the indexed-write lever"),
        ("ordered-walk", "the ordered-walk lever"),
        ("streaming-group", "the streaming-group lever"),
        ("fused-bytecode", "the fused-bytecode lever"),
    ] {
        if let Some(without) = headline(lever) {
            measured.push(format!("without {name} {without:.3}x"));
        }
    }
    if measured.is_empty() {
        return String::new();
    }
    format!(
        "- **What the levers that did land are worth**, measured rather than asserted, at the \
           small scale over thirty paired rounds: with everything on the weighted geometric mean \
           is {shipped:.3}x; {}. Every arm is in this candidate, and the correctness \
           shard that runs under each of them shows the plans change and the answers do \
           not.\n",
        measured.join("; ")
    )
}

/// Returns the compatibility gate, read from the report the manifest produced.
fn compatibility_gate() -> (String, bool, String) {
    let root = workspace_root();
    let Ok(text) = std::fs::read_to_string(root.join("compat/compat-report.json")) else {
        return (
            "compatibility".to_string(),
            false,
            "no compatibility report has been generated".to_string(),
        );
    };
    let count = |key: &str| -> usize {
        let needle = format!("\"{key}\":");
        text.find(&needle)
            .and_then(|at| text.get(at.saturating_add(needle.len())..))
            .and_then(|rest| {
                rest.trim_start()
                    .split([',', '}'])
                    .next()
                    .and_then(|value| value.trim().parse::<usize>().ok())
            })
            .unwrap_or(0)
    };
    let problems = text.matches("\"kind\":").count();
    let pass = count("pass");
    let missing = count("missing");
    // A missing row is not a problem: it is the denominator doing its job. What
    // fails this gate is a row that *claims* to pass without evidence, which is
    // the only kind of dishonesty a compatibility report can commit.
    let detail = format!(
        "{pass} capabilities pass, {missing} not implemented, {problems} unsupported claims"
    );
    ("compatibility".to_string(), problems == 0, detail)
}

/// Returns the label of the most recent scorecard run.
fn latest_label(history: &History) -> Option<String> {
    history.labels().last().cloned()
}

/// Returns the performance gates: the headline bound and every family floor.
fn performance_gates(
    contract: &Contract,
    history: &History,
    label: Option<&str>,
) -> Vec<(String, bool, String)> {
    let Some(label) = label else {
        return vec![(
            "performance".to_string(),
            false,
            "no scorecard has been run".to_string(),
        )];
    };
    let platform = platform_name();
    let mut gates = Vec::new();
    let mut scales: Vec<String> = Vec::new();
    for entry in &history.entries {
        if entry.label == label && entry.platform == platform && !scales.contains(&entry.scale) {
            scales.push(entry.scale.clone());
        }
    }
    if scales.is_empty() {
        return vec![(
            "performance".to_string(),
            false,
            format!("no scorecard recorded for {platform}"),
        )];
    }
    for scale in &scales {
        let headline = history
            .entries
            .iter()
            .find(|entry| {
                entry.label == label
                    && entry.platform == platform
                    && entry.scale == *scale
                    && entry.workload == "*headline*"
            })
            .map(|entry| (entry.ratio, entry.low));
        let (ratio, low) = headline.unwrap_or((0.0, 0.0));
        gates.push((
            format!("performance.headline.{scale}"),
            low >= contract.headline,
            format!(
                "weighted geometric mean {ratio:.3}x, lower bound {low:.3}x against a bound of \
                 {:.2}x",
                contract.headline
            ),
        ));
        let mut below: Vec<String> = Vec::new();
        for family in &contract.families {
            if !family.required {
                continue;
            }
            // The family's own aggregate, which is the number the contract's
            // floor is written about - not the worst workload inside it. The
            // scorecard records it as its own row so both artifacts read one
            // statistic rather than each computing their own.
            let row = format!("*family* {}", family.id);
            let bound = history
                .entries
                .iter()
                .find(|entry| {
                    entry.label == label
                        && entry.platform == platform
                        && entry.scale == *scale
                        && entry.workload == row
                })
                .map(|entry| entry.low);
            let Some(bound) = bound else {
                below.push(format!("{} not measured", family.id));
                continue;
            };
            if bound < contract.floor {
                below.push(format!("{} at {bound:.3}x", family.id));
            }
        }
        gates.push((
            format!("performance.floors.{scale}"),
            below.is_empty(),
            if below.is_empty() {
                format!(
                    "every required family is at or above {:.2}x",
                    contract.floor
                )
            } else {
                format!(
                    "below the {:.2}x floor: {}",
                    contract.floor,
                    below.join(", ")
                )
            },
        ));
    }
    let regressions = history.regressions(&platform);
    gates.push((
        "performance.regressions".to_string(),
        regressions.is_empty(),
        if regressions.is_empty() {
            "no workload has been slower than its best for two consecutive runs".to_string()
        } else {
            format!("{} open", regressions.len())
        },
    ));
    gates
}

/// Returns the known limitations, in the words the modules that own them use.
/// @param history - the recorded runs, which the arm comparison is read from
fn limitations(history: &History) -> String {
    let mut out = String::new();
    out.push_str(
        "- **Performance.** The engine is slower than the pinned reference on every family except \
           point reads by rowid, which it wins. The measured cause is the virtual machine rather \
           than the storage layer: one step of a table scan costs 3.7 ns, reading the row 21.7, \
           finding its fields 35.7 and decoding an integer 38.2 - while the machine on top of \
           that costs about two hundred nanoseconds per column read and comparison. Closing it is \
           opcode-level work: borrowed values through the whole register file, fused \
           superinstructions, and specialised scan loops.\n",
    );
    out.push_str(
        "- **Seven optional SQLite surfaces are not implemented**, and the manifest carries a \
           row for each so the denominator says so: the session extension, the pre-update hook, \
           the snapshot API, `unlock_notify`, RBU, geopoly, and the R-Tree geometry callbacks. \
           None of them is reachable from SQL or from the file format, so a database written by \
           this engine is not affected by their absence - an application that calls them is.\n",
    );
    out.push_str(&arms(history));
    out.push_str(
        "- **Two of the TDD's optimisation levers were implemented, measured, and left off.** \
           Checkpoint scheduling - bounding how many frames one automatic checkpoint copies, so \
           the cost is spread over the commits that caused it - makes no difference, and its own \
           counters say why: the checkpoint already runs after nearly every commit once the log \
           passes its threshold, about 5,700 times in 6,000, so there is no accumulated batch to \
           spread. The bound is a tunable rather than a default and the shipped behaviour is \
           unchanged. Group commit has nothing to group: writers are serialised, so transactions \
           do not overlap, and a commit already takes exactly the barriers the reference takes - \
           one sync in a write-ahead log at `synchronous=full`, none at `normal`, two in a \
           rollback journal. The evidence is that the two families where the barrier dominates \
           are at parity: `write.insert.autocommit` at 0.99x and `txn.autocommit` at 0.92x. \
           Sharing one barrier between two writers would need overlapping write transactions, \
           which is a change to the locking rather than a tuning lever.\n",
    );
    out.push_str(
        "- **Vectorisation and SIMD are not applicable to this execution model.** The lever's \
           name pairs them with bytecode fusion, which is implemented; the other two need a \
           columnar or batched interpreter, where one instruction works on many rows. This is a \
           row-at-a-time virtual machine, so there is no vector for an instruction to act on, and \
           saying so is more use than a benchmark of nothing.\n",
    );

    out.push_str(
        "- **FTS5's segment format inside `%_data` is first-party.** SQLite's is described only in \
           comments in `fts5_index.c` and is explicitly not a published format, unlike the \
           R-Tree's. What is matched is everything a reader outside the module sees: the five \
           table names, the layouts of `%_content`, `%_docsize` and `%_config`, the rows `MATCH` \
           finds, their order, and `bm25()` to the last digit.\n",
    );
    out.push_str(
        "- **A database opened through a caller-supplied C VFS journals rather than using a \
           write-ahead log.** `xShmMap` is the easiest part of the VFS contract to get subtly \
           wrong, and a log over a broken one corrupts silently.\n",
    );
    out.push_str(
        "- **A `inillucent_search` table has one row per document**, so the per-document cap in the \
           fusion never binds on it. An application that wants documents made of several chunks \
           models them in SQL - a document table and a join - which is what the migration tool \
           writes and what its own grouped check verifies.\n",
    );
    out.push_str(
        "- **The legacy engine's lexical ranking depends on the `k` it was asked for.** \
           Position-aware rescoring reaches `k * lexical_rescore_depth` hits and only ever scales \
           a score down, so a chunk just outside that window keeps its full BM25 score and \
           competes against rescored ones: ask for ten and ask for fifty, and the tail of the \
           ranking moves. Every path a search table sits behind goes through `search_branches`, \
           which retrieves at `candidates` depth, so the two agree when asked at that depth and \
           can differ when they are not. The migration compares them at one depth for exactly \
           that reason. A corpus of real prose is what exposed it: a small one has fewer chunks \
           than the window, so the window never binds.\n",
    );
    out.push_str(
        "- **A migrated vector index is not the same graph.** The legacy index's graph grew one \
           insert at a time; a migrated one is built in a single pass over every row, which is \
           better connected - that is what makes compaction worth its cost. Two different graphs \
           searched approximately give slightly different answers, sometimes one better and \
           sometimes the other, so the migration compares them with the approximation switched \
           off and reports separately what each finds of the exact answer at its default width. \
           An application that depends on a particular ranking of near-ties should expect it to \
           move.\n",
    );
    out.push_str(
        "- **A tombstone and a delete differ.** The legacy engine keeps a tombstoned chunk in the \
           inverted index and filters it at query time, so its corpus statistics do not move; a \
           search table deletes the row, so they do. Both make the document unreachable \
           immediately; deep orderings can differ until the legacy index is rebuilt.\n",
    );
    out.push_str(
        "- **A migration is refused when the source was built with a non-zero heading boost.** A \
           migrated search table indexes a chunk's text without its heading structure, which \
           contributes nothing at the measured default of zero and would contribute at anything \
           else - and the migrated index would then rank differently in a way nothing about it \
           looked wrong.\n",
    );
    out
}
