#!/usr/bin/env bash
# The one validation entry point, on Unix. See `tools/validate.ps1` for the
# argument this file exists for: the checks all existed before task-1894 and
# each one lived in somebody's memory, so "did you run the checks" had no
# answer.
#
# Every stage prints its name and its elapsed time, and the script stops at the
# first failure with a non-zero exit code.
#
#   tools/validate.sh                # everything
#   tools/validate.sh --quick        # fmt, lint, contracts and smoke only
#   tools/validate.sh --coverage     # everything, plus the coverage measurement
#   tools/validate.sh --stage lint   # one stage by name

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
# task-1961 (A14).** Every stage above and every CI job passes `--all-features`,
# and `grep -rn 'no-default-features'` over every workflow, script and manifest
# returned nothing, so the feature set a user gets by default was never compiled
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

stage contracts 'dependencies, layering, the command table and the test map' \
    cargo test --manifest-path "$root/Cargo.toml" -p inillucent-compat \
    --test policy --test selection --test command_parity --test harness

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
coverage_run() {
    node "$root/tools/coverage.mjs" --per-crate
}
if [ "$coverage" -eq 1 ]; then
    stage coverage 'the coverage number docs/repository.md publishes, re-measured' coverage_run
fi

if [ "$quick" -eq 1 ]; then
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

echo
if [ "${#failed[@]}" -gt 0 ]; then
    echo "FAILED: ${failed[*]}"
    exit 1
fi
echo 'validation passed'
