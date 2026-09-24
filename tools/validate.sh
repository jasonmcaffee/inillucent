#!/usr/bin/env bash
# The one validation entry point, on Unix. See `tools/validate.ps1` for the
# argument this file exists for: the checks all existed before task-1894 and
# each one lived in somebody's memory, so "did you run the checks" had no
# answer.
#
# Every stage prints its name and its elapsed time. **The script runs every
# stage and exits non-zero at the end naming all of them**, rather than stopping
# at the first failure - the header said it stopped for two tickets while
# `stage()` recorded the name and carried on, which is the more useful behaviour
# and is what a reader should be told (task-1969, 4.13).
#
#   tools/validate.sh                # everything
#   tools/validate.sh --quick        # stops after `smoke`: toolchain, format,
#                                    # build, lint, dependencies, defaults,
#                                    # docs, oracle, fixtures, doctests, urls,
#                                    # contracts, compat, smoke - fourteen
#                                    # stages, not the four an earlier header
#                                    # claimed
#   tools/validate.sh --coverage     # everything, plus the coverage measurement
#   tools/validate.sh --stage lint   # one stage by name, wherever it sits -
#                                    # `--quick --stage tests` runs `tests`

set -u

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
quick=0
only=''
coverage=0
failed=()

while [ $# -gt 0 ]; do
    case "$1" in
        --quick) quick=1 ;;
        --coverage) coverage=1 ;;
        --stage) only="${2:-}"; shift ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
    shift
done

# Runs one named stage, timing it and recording a failure.
#
# $1 - the stage's name, which --stage selects on
# $2 - one sentence saying what it is for
# rest - the command
stage() {
    local name="$1"; shift
    local because="$1"; shift
    if [ -n "$only" ] && [ "$only" != "$name" ]; then return 0; fi
    echo
    echo "=== $name === $because"
    local started
    started=$(date +%s)
    if "$@"; then
        echo "--- $name ok in $(( $(date +%s) - started ))s"
    else
        echo "--- $name FAILED after $(( $(date +%s) - started ))s"
        failed+=("$name")
    fi
}

versions() { rustc --version && cargo --version; }
stage toolchain 'the pinned compiler is the one in use' versions

# **`inillucent-bench` is not formatted, and that is not an oversight to correct
# here.** It is not in `policy.rs`'s governed list and has been unformatted since
# before the rearchitecture (636 diffs at the commit task-1894 started from). The
# lint stage below still covers it.
#
# **`inillucent-core` came off this list in task-1932 (H9).** It is governed now,
# so `policy.rs`'s `the_governed_crates_are_formatted` checks it and leaving it
# out here would be a check that disagrees with the one that gates a merge. Its
# files are pinned by SHA3-256 in
# `compat/baseline/inillucent-core-baseline.json` and every one of them is a
# declared amendment on that ticket.
formatting() {
    local packages=()
    local member
    # `name` is declared here on purpose: bash scopes dynamically, so an
    # undeclared assignment inside a stage would overwrite the caller's `name`
    # and the report would end up labelled with the last package.
    local name
    # The workspace members, read from the manifest's own list so a new
    # crate is covered the day it is added.
    for member in $(sed -n 's@^ *"\(crates/[a-z-]*\|drivers/[a-z-]*\)",@\1@p' "$root/Cargo.toml"); do
        name="${member##*/}"
        case "$name" in
            inillucent-bench) continue ;;
        esac
        packages+=(-p "$name")
    done
    cargo fmt --manifest-path "$root/Cargo.toml" "${packages[@]}" --check
}
stage format 'a governed crate that is unformatted fails policy.rs anyway' formatting

# --locked, because a check allowed to update Cargo.lock is a check of a
# dependency set nobody reviewed.
stage build 'every target compiles from the lock file as it stands' \
    cargo check --manifest-path "$root/Cargo.toml" --workspace --all-targets --all-features --locked

stage lint 'the strict lint set, which the pinned compiler fixes' \
    cargo clippy --manifest-path "$root/Cargo.toml" --workspace --all-targets --all-features --locked -- -D warnings

# **What a declared dependency drags in behind it, and under what licence.**
# `policy.rs` checks the edges a manifest names; nothing looked at the resolved
# graph, so a crate with a published advisory or a licence this repository cannot
# ship passed every check the workspace had (task-1946, M8). `deny.toml` at the
# root says what is allowed and why.
#
# `cargo-deny` is installed when it is absent rather than skipped: a stage that
# quietly does nothing on the machine that has not got the tool is a stage that
# reports green having checked nothing.
dependencies() {
    if ! cargo deny --version >/dev/null 2>&1; then
        echo '    installing cargo-deny'
        cargo install cargo-deny --locked
    fi
    cargo deny --manifest-path "$root/Cargo.toml" check
}
stage dependencies 'advisories, licences and the resolved graph, which the manifest checks cannot see' dependencies

# **The configuration a `cargo install` produces, which nothing built until
# task-1961 (A14).** Every stage above passes `--all-features`, and
# `grep -rn 'no-default-features'` over every script and manifest returned
# nothing, so the feature set a user gets by default was never compiled
# anywhere. `inillucent-storage` has two independent features, `opcode-probe`
# and `check`, with fifteen `cfg(feature)` sites between them, and no job built
# them crossed.
defaults() {
    cargo check --manifest-path "$root/Cargo.toml" --workspace --all-targets --locked || return 1
    cargo check --manifest-path "$root/Cargo.toml" -p inillucent-storage --features check --locked || return 1
    cargo check --manifest-path "$root/Cargo.toml" -p inillucent-storage --features opcode-probe --locked || return 1
    cargo check --manifest-path "$root/Cargo.toml" -p inillucent-storage --features check,opcode-probe --locked
}
stage defaults 'the feature set a cargo install produces, which --all-features never builds' defaults

# **A broken intra-doc link is a build failure here rather than a hole in the
# published documentation.**
documentation() {
    RUSTDOCFLAGS='-D warnings' cargo doc --manifest-path "$root/Cargo.toml" \
        --workspace --no-deps --all-features
}
stage docs 'a link that resolves to nothing is a defect, not a warning' documentation

# Built before anything grades against it (task-1932, H10). Sixty-nine
# differential tests across ten files compare this engine with SQLite 3.53.4 and
# each of them skips when the oracle is absent; --strict at the end of this
# script counts those skips and fails. So on a machine without the oracle this
# script was either failing every run or --strict was not what gated a merge,
# and nothing in the repository could say which, because nothing in it built the
# oracle. This stage does, and the run's log now carries the evidence.
#
# Idempotent and cheap on a second run: every artifact is checked against the
# SHA3-256 sum SQLite publishes and re-downloaded only when it is absent.
#
# **`bash`, not `sh`, and this is why no Linux run ever had an oracle
# (task-1946, M5).** `tools/sqlite-reference.sh` is `#!/usr/bin/env bash` and
# uses `set -o pipefail`; `/bin/sh` on Debian and Ubuntu is dash, which answers
# `Illegal option -o pipefail` and stops at line 15. So this stage failed at
# once on every Linux run, every oracle-graded suite then skipped, and
# `compat/results/linux-x86_64.jsonl` recorded no passing result for any of
# them - which is the fourteen rows the Problems table in
# `compat/compat-report.md` carried. The same defect, in the same shape, as the
# one `packaging/PUBLISHING.md` records for `install.sh`.
stage oracle 'the sixty-nine differential suites have nothing to compare against without it' \
    bash "$root/tools/sqlite-reference.sh"

# Two durability tests could not run without this, and nobody knew
# (task-1932, H10). `new_engine_log_lead.rs` builds an index through a 64-frame
# pool - the condition the defect it guards needed - and reads a 17 MB fixture
# that is not checked in. Until the skip marker landed its message matched none
# of the phrases --strict looked for, so the suite reported green having
# asserted nothing on every machine that had not built the file by hand. Eight
# seconds buys two durability tests that actually run.
# `bash` for the same reason: it is a bash script too.
stage fixtures 'the log-lead durability tests read a fixture that is not checked in' \
    bash "$root/tools/build-gate-fixtures.sh" "$root/_agent_output/fixtures"

# **The examples on the crate a user depends on (task-1961, T6).** There were
# four executable doctests in about 270,000 lines and none of them on the public
# API. Every public type on the driver's front page now carries one that opens a
# real file, and a doctest is the one kind of example that cannot rot: it is
# compiled and run.
doctests() {
    cargo test --manifest-path "$root/Cargo.toml" --doc -p inillucent-driver || return 1
    cargo test --manifest-path "$root/Cargo.toml" --doc -p inillucent
}
stage doctests 'the examples a cargo add reader depends on, compiled and run' doctests

# **The public URLs every shipped package names, fetched with no credential.**
# Wired in now that both repositories are public (task-1961, S4). It was written
# in task-1946 and deliberately left out of this script while it was red -
# commit 12964ab's message says so - and a check that is red for a reason nobody
# intends to fix teaches people to ignore the script it is in.
stage urls 'a URL a shipped package names has to resolve for somebody with no credential' \
    node "$root/tools/check-public-urls.mjs"

# `gates_fail_closed` is here rather than only in the full run (task-1961,
# criterion 10; task-1969, 4.13). It is the only test any gate program has, and a
# quick run that skipped it was a quick run with nothing holding the programs
# that decide pass or fail.
stage contracts 'dependencies, layering, the command table and the test map' \
    cargo test --manifest-path "$root/Cargo.toml" -p inillucent-compat \
    --test policy --test selection --test command_parity --test harness \
    --test gates_fail_closed

# **A published compatibility report may not carry its own unresolved Problems
# table (task-1946, M5).** compat/compat-report.md shipped fourteen rows saying
# a capability the manifest calls `pass` has no passing result recorded on
# linux-x86_64. `inillucent-manifest report` has always exited non-zero when it
# finds one; nothing ever ran it, so the table grew instead.
#
# It regenerates the report from `compat/results` as it goes, so a run whose
# recorded results have moved leaves the checked-in report agreeing with them.
# That is also what makes the stage fail a checkout whose report is stale: the
# `harness.report.reproducible` suite compares the two.
compat_report() {
    cargo run --manifest-path "$root/Cargo.toml" -p inillucent-compat \
        --bin inillucent-manifest -- report
}
stage compat 'the published compatibility report states no unresolved problem' compat_report

smoke() {
    cargo build --manifest-path "$root/Cargo.toml" -p inillucent-compat --bin inillucent-testrun --features testrun \
        && "$root/target/debug/inillucent-testrun" --tier smoke
}
stage smoke 'a real file opened, written, reopened, read' smoke

# **`--stage` reaches past the quick exit (task-1969, 4.13).** `--quick` means
# "stop after smoke"; it does not mean "refuse to run the stage I named". The two
# read the same until `coverage` moved below this line, after which
# `--quick --coverage --stage coverage` would have measured nothing on Unix and
# everything on Windows - which is the platform difference this whole section is
# about.
if [ "$quick" -eq 1 ] && [ -z "$only" ]; then
    if [ "${#failed[@]}" -gt 0 ]; then
        echo; echo "FAILED: ${failed[*]}"; exit 1
    fi
    echo; echo 'quick validation passed'; exit 0
fi

security() {
    cargo test --manifest-path "$root/Cargo.toml" -p inillucent-compat --test confinement \
        && cargo test --manifest-path "$root/Cargo.toml" -p inillucent-driver-capi --test abi --test conformance \
        && cargo test --manifest-path "$root/Cargo.toml" -p inillucent-remote --test transport
}
stage security 'root confinement, the C ABI lifetimes, and migration transport' security

# --strict is the flag that matters: several suites report success when a
# prerequisite is absent, and without it a green on a machine with nothing
# installed reads the same as a green on one with everything.
stage tests 'every selected suite, with missing prerequisites named' \
    "$root/target/debug/inillucent-testrun" --strict

# **Every published count, against the engine that produced it (task-1969,
# 4.4).** `tools/doc-facts/check.mjs` checks the 30 verbs, the 63 dot commands,
# the 416 probe cases, the test and target counts and the private-reference
# patterns, and it is checked by nothing else. Its only caller was
# `packaging/release.ps1`, on Windows, without `--run-tests` - so on Unix it ran
# at no point in any gate.
#
# After `build` and `tests`, because it reads the built binaries and the runner's
# own summary, and both are fresh by the time it runs. `--run-tests` is what puts
# the two test facts in scope; without it they report as out of scope rather than
# as measured.
stage doc-facts 'every count a document states, against the engine that produced it' \
    node "$root/tools/doc-facts/check.mjs" --run-tests

# **The four language wrappers, against the binary this run built (task-1969,
# 4.5).** Each of them is somebody's entry point to this engine and each was
# tested by reading its own source as text:
#
# - the Go suite's five engine tests skipped wherever the gate ran them,
#   because the gate built `target/release/inillucent` and set neither
#   `INILLUCENT_BIN` nor `PATH`, so `go test` exited 0 having run five cases
#   that check a platform string table;
# - `packages/npm/inillucent/resolve.test.mjs` and
#   `packages/php/tests/target.php` read the platform table and never called
#   `query` or `exec`;
# - `drivers/bindings/python/run_conformance.py` is presented by
#   `drivers/README.md` as the proof that a second language can implement the
#   driver from the documents, and was run by nothing.
#
# `INILLUCENT_BIN` is what points all four at this build. Setting it is also
# what makes the Go suite fail rather than skip when the binary is absent,
# which is the shape `drivers/inillucent-driver-capi/tests/conformance.rs`
# already uses for `INILLUCENT_CAPI_ASAN`.
#
# **A toolchain that is not installed is announced and skipped, not failed.**
# The marker is the one `tests/inillucent-testing-tdd.md` §9 asks for, so a
# reader sees which language did not run and what installs it. That is the same
# answer `--strict` gives for a suite whose prerequisite is absent, and it is
# why this stage can be in every validate run rather than behind a flag.
wrappers() {
    local binary="$root/target/release/inillucent"
    if [ ! -x "$binary" ]; then
        binary="$root/target/debug/inillucent"
    fi
    if [ ! -x "$binary" ]; then
        echo "no built inillucent to point the wrappers at; run \`cargo build --release -p inillucent-cli\`; skipping"
        return 1
    fi
    export INILLUCENT_BIN="$binary"
    local failed=0

    if command -v go >/dev/null 2>&1; then
        # `-v` so a `--- SKIP` line is printed rather than folded away, and the
        # grep below is what turns one into a failure: a wrapper suite that
        # skipped every engine test is the state this stage exists to end.
        local said
        said="$(go -C "$root/packages/go" test ./... -v 2>&1)"
        echo "$said"
        if [ -n "$(printf '%s' "$said" | grep -F -- '--- SKIP')" ]; then
            echo "the Go suite skipped a test with INILLUCENT_BIN set, which it may not"
            failed=1
        fi
    else
        echo "no go on PATH; install one from https://go.dev/dl/; skipping"
    fi

    if command -v node >/dev/null 2>&1; then
        node --test "$root/packages/npm/inillucent/"*.test.mjs || failed=1
    else
        echo "no node on PATH; install one from https://nodejs.org/; skipping"
    fi

    if command -v php >/dev/null 2>&1; then
        php "$root/packages/php/tests/target.php" || failed=1
        php "$root/packages/php/tests/roundtrip.php" || failed=1
        # **The conformance runner, which nothing invoked** (task-2066 §4.4.3).
        # The other four languages' runners were driven from somewhere; this one
        # was written, checked in, and started by no script - so PHP's record of
        # the shared suite was whatever a person had last produced by hand, and
        # on most machines that is nothing.
        php "$root/packages/php/tests/conformance.php" || failed=1
    else
        echo "no php on PATH; install one from https://www.php.net/downloads; skipping"
    fi

    # **Being on PATH is not the question; answering is (task-1970).** See the
    # note in tools/validate.ps1: Windows puts a Python app execution alias on
    # PATH that resolves and then will not start, and Git Bash sees the same
    # entry. A candidate that cannot print its own version is not a Python.
    local python=''
    local candidate
    for candidate in python3 python; do
        if command -v "$candidate" >/dev/null 2>&1 && "$candidate" --version >/dev/null 2>&1; then
            python="$candidate"
            break
        fi
    done
    if [ -n "$python" ]; then
        "$python" "$root/drivers/bindings/python/run_conformance.py" || failed=1
    else
        echo "no python on PATH; install one from https://www.python.org/downloads/; skipping"
    fi

    return "$failed"
}
stage wrappers 'the Go, Node, PHP and Python wrappers, against the binary this run built' wrappers


# **Coverage, behind a flag, so the published number can be re-measured
# (task-1961, T2).** The only numbers on record before this named `rustdb-vm`, a
# crate that no longer exists. Behind a flag because it rebuilds the whole
# workspace instrumented and runs every suite again, which is tens of minutes.
#
# **Not `--branch`.** That needs `-Z coverage-options=branch`, a nightly option,
# and `rust-toolchain.toml` pins the compiler to stable 1.95.0 for the reason
# written beside the pin. Region and line coverage are what a pinned toolchain
# can measure and are what `docs/repository.md` publishes.
#
# The three excluded crates are the retrieval tier, which `tests/selection.toml`
# already excludes from the fast run: they need ONNX and a corpus, and on a
# machine without either they contribute uninstrumented zeros rather than a
# number.
#
# The twelve `schema_forms` cases that round-trip a database through the shell
# need that shell in this run's own target directory, and
# `inillucent_compat::interchange::our_shell` builds it there. Building it here
# instead does not work: `cargo llvm-cov` cleans the target directory before it
# starts, so anything built ahead of it is gone by the time a test looks.
#
# **Through `tools/coverage.mjs` rather than `cargo llvm-cov` directly.** The
# report names one object file per test binary and there are 176 of them, which
# is a command line of about forty thousand characters - past what Windows
# accepts, so the run ended with `os error 206` and no number after twenty
# minutes of work. The wrapper re-runs that same command through a response
# file, and prints the per-crate table `docs/repository.md` publishes.
#
# `--write` puts that table into the page between two marker comments instead of
# leaving it for somebody to paste, which is why the page's prose used to say
# 40.9% where its table said 40.6% (task-1969, 4.12).
coverage_run() {
    node "$root/tools/coverage.mjs" --per-crate --write
}
if [ "$coverage" -eq 1 ]; then
    stage coverage 'the coverage number docs/repository.md publishes, re-measured' coverage_run
fi

echo
if [ "${#failed[@]}" -gt 0 ]; then
    echo "FAILED: ${failed[*]}"
    exit 1
fi
echo 'validation passed'
