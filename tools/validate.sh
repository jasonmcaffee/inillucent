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
#   tools/validate.sh --stage lint   # one stage by name

set -u

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
quick=0
only=''
failed=()

while [ $# -gt 0 ]; do
    case "$1" in
        --quick) quick=1 ;;
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
stage oracle 'the sixty-nine differential suites have nothing to compare against without it' \
    sh "$root/tools/sqlite-reference.sh"

# Two durability tests could not run without this, and nobody knew
# (task-1932, H10). `new_engine_log_lead.rs` builds an index through a 64-frame
# pool - the condition the defect it guards needed - and reads a 17 MB fixture
# that is not checked in. Until the skip marker landed its message matched none
# of the phrases --strict looked for, so the suite reported green having
# asserted nothing on every machine that had not built the file by hand. Eight
# seconds buys two durability tests that actually run.
stage fixtures 'the log-lead durability tests read a fixture that is not checked in' \
    sh "$root/tools/build-gate-fixtures.sh" "$root/_agent_output/fixtures"

stage contracts 'dependencies, layering, the command table and the test map' \
    cargo test --manifest-path "$root/Cargo.toml" -p inillucent-compat \
    --test policy --test selection --test command_parity --test harness

smoke() {
    cargo build --manifest-path "$root/Cargo.toml" -p inillucent-compat --bin inillucent-testrun --features testrun \
        && "$root/target/debug/inillucent-testrun" --tier smoke
}
stage smoke 'a real file opened, written, reopened, read' smoke

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
