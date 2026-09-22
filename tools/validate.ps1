# The one validation entry point, and the only thing that runs the checks at all.
#
# **There is one command rather than a list in a README**, and that is the whole
# point of the file. Before task-1894 the checks existed - `cargo fmt`, the
# dependency policy, the selection map, the crash campaigns - and each of them
# lived in somebody's memory or in a paragraph of prose, so "did you run the
# checks" had no answer. A list of commands nobody runs is the same shape as a
# capability table nobody probes, which `drivers/README.md` already argues
# against.
#
# Every stage prints its own name and the elapsed time. **The script runs every
# stage and exits non-zero at the end naming all of them**, rather than stopping
# at the first failure - `Invoke-Stage` has recorded and carried on since it was
# written, and the header said otherwise (task-1969, 4.13). Nothing here needs a
# network, a server, or a corpus: what does is skipped by the suite that needs it
# and counted by `inillucent-testrun --strict`.
#
#   pwsh tools/validate.ps1              # everything
#   pwsh tools/validate.ps1 -Quick       # stops after `smoke`: toolchain,
#                                        # format, build, lint, defaults,
#                                        # dependencies, docs, oracle, fixtures,
#                                        # doctests, urls, contracts, compat,
#                                        # smoke - fourteen stages, not the four
#                                        # an earlier header claimed
#   pwsh tools/validate.ps1 -Stage lint  # one stage by name, wherever it sits
#
# The Unix equivalent is `tools/validate.sh`, and the two run the same stages.
# **Their order differs in one place and always did**: this script runs
# `defaults` before `dependencies` and that one runs `dependencies` first.
# Neither depends on the other. What was not the same, until task-1969, is where
# `coverage` sits: it was above the quick exit there and below `tests` here, so
# `--quick --coverage` measured on one platform and not the other while both
# headers said the order was the same.

[CmdletBinding()]
param(
    # Skips the long test tiers, for a check before pushing.
    [switch] $Quick,
    # Also measures coverage, which rebuilds the workspace instrumented.
    [switch] $Coverage,
    # Runs one stage by name and stops.
    [string] $Stage = ''
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$failed = @()

function Invoke-Stage {
    <#
    .SYNOPSIS
    Runs one named stage, timing it and recording a failure.

    .PARAMETER Name
    The stage's name, which `-Stage` selects on.

    .PARAMETER Because
    One sentence saying what the stage is for, printed with it.

    .PARAMETER Body
    The commands to run.
    #>
    param(
        [Parameter(Mandatory)] [string] $Name,
        [Parameter(Mandatory)] [string] $Because,
        [Parameter(Mandatory)] [scriptblock] $Body
    )
    if ($Stage -and $Stage -ne $Name) { return }
    Write-Host ''
    Write-Host "=== $Name === $Because" -ForegroundColor Cyan
    $started = Get-Date
    & $Body
    $code = $LASTEXITCODE
    $elapsed = ((Get-Date) - $started).TotalSeconds
    if ($code -ne 0) {
        Write-Host ("--- $Name FAILED after {0:N1}s (exit {1})" -f $elapsed, $code) -ForegroundColor Red
        $script:failed += $Name
    } else {
        Write-Host ("--- $Name ok in {0:N1}s" -f $elapsed) -ForegroundColor Green
    }
}

# The compiler the repository is graded with. A run on any other one is
# measuring a different build, so it is named rather than assumed.
Invoke-Stage -Name 'toolchain' -Because 'the pinned compiler is the one in use' -Body {
    rustc --version
    cargo --version
}

# **`inillucent-bench` is not formatted, and that is not an oversight to correct
# here.** It is not in `policy.rs`'s governed list and has been unformatted
# since before the rearchitecture (636 diffs at the commit task-1894 started
# from). The lint stage below still covers it.
#
# **`inillucent-core` came off this list in task-1932 (H9).** It is governed
# now, so `policy.rs`'s `the_governed_crates_are_formatted` checks it and
# leaving it out here would be a check that disagrees with the one that gates a
# merge. Its files are pinned by SHA3-256 in
# `compat/baseline/inillucent-core-baseline.json` and every one of them is a
# declared amendment on that ticket.
Invoke-Stage -Name 'format' -Because 'a governed crate that is unformatted fails policy.rs anyway' -Body {
    $skip = @('inillucent-bench')
    # The workspace members, read from the manifest's own list so a new
    # crate is covered the day it is added.
    $members = Select-String -Path "$root/Cargo.toml" -Pattern '^\s*"(crates|drivers)/([a-z-]+)",' |
        ForEach-Object { $_.Matches[0].Groups[2].Value } |
        Where-Object { $skip -notcontains $_ } |
        ForEach-Object { '-p'; $_ }
    cargo fmt --manifest-path "$root/Cargo.toml" @members --check
}

# `--locked`, because a check that was allowed to update `Cargo.lock` is a check
# of a dependency set nobody reviewed.
Invoke-Stage -Name 'build' -Because 'every target compiles from the lock file as it stands' -Body {
    cargo check --manifest-path "$root/Cargo.toml" --workspace --all-targets --all-features --locked
}

Invoke-Stage -Name 'lint' -Because 'the strict lint set, which the pinned compiler fixes' -Body {
    cargo clippy --manifest-path "$root/Cargo.toml" --workspace --all-targets --all-features --locked -- -D warnings
}

# **The configuration a `cargo install` produces, which nothing built until
# task-1961 (A14).** Every stage above passes `--all-features`, so the feature
# set a user gets by default was never compiled anywhere, and
# `inillucent-storage`'s two independent features were never built crossed.
Invoke-Stage -Name 'defaults' -Because 'the feature set a cargo install produces, which --all-features never builds' -Body {
    cargo check --manifest-path "$root/Cargo.toml" --workspace --all-targets --locked
    if ($LASTEXITCODE -ne 0) { return }
    cargo check --manifest-path "$root/Cargo.toml" -p inillucent-storage --features check --locked
    if ($LASTEXITCODE -ne 0) { return }
    cargo check --manifest-path "$root/Cargo.toml" -p inillucent-storage --features opcode-probe --locked
    if ($LASTEXITCODE -ne 0) { return }
    cargo check --manifest-path "$root/Cargo.toml" -p inillucent-storage --features check,opcode-probe --locked
}

# **What a declared dependency drags in behind it, and under what licence.**
# `policy.rs` checks the edges a manifest names; nothing looked at the resolved
# graph, so a crate with a published advisory or a licence this repository cannot
# ship passed every check the workspace had (task-1946, M8). `deny.toml` at the
# root says what is allowed and why.
#
# `cargo-deny` is installed when it is absent rather than skipped: a stage that
# quietly does nothing on the machine that has not got the tool is a stage that
# reports green having checked nothing, which is the failure
# `tests/inillucent-testing-tdd.md` rule 1.5 names.
Invoke-Stage -Name 'dependencies' -Because 'advisories, licences and the resolved graph, which the manifest checks cannot see' -Body {
    & cargo deny --version *> $null
    if ($LASTEXITCODE -ne 0) {
        Write-Host '    installing cargo-deny'
        cargo install cargo-deny --locked
    }
    cargo deny --manifest-path "$root/Cargo.toml" check
}

# **A broken intra-doc link is a build failure here rather than a hole in the
# published documentation.** `cargo doc` warns about a `[`Type`]` that resolves
# to nothing; nothing turned that warning into an error, so the first place it
# would have been noticed is a docs.rs page nobody was watching.
Invoke-Stage -Name 'docs' -Because 'a link that resolves to nothing is a defect, not a warning' -Body {
    $env:RUSTDOCFLAGS = '-D warnings'
    try {
        cargo doc --manifest-path "$root/Cargo.toml" --workspace --no-deps --all-features
    } finally {
        Remove-Item Env:RUSTDOCFLAGS -ErrorAction SilentlyContinue
    }
}

# **Built before anything grades against it (task-1932, H10).** Sixty-nine
# differential tests across ten files compare this engine with SQLite 3.53.4,
# and each of them skips when the oracle is absent. `--strict` at the end of
# this script counts those skips and fails - so on a machine without the oracle
# this script was either failing every run, or `--strict` was not what gated a
# merge. Nothing in the repository could say which, because nothing in the
# repository built the oracle. This stage does, and the run's log now carries
# the evidence that it did.
#
# The script is idempotent and cheap on a second run: every artifact is checked
# against the SHA3-256 sum SQLite publishes and re-downloaded only when it is
# absent or `-Force` is given.
Invoke-Stage -Name 'oracle' -Because 'the sixty-nine differential suites have nothing to compare against without it' -Body {
    & pwsh -NoProfile -File "$root/tools/sqlite-reference.ps1"
}

# **Two durability tests could not run without this, and nobody knew
# (task-1932, H10).** `new_engine_log_lead.rs` builds an index through a
# 64-frame pool, which is the condition the defect it guards needed, and it
# reads a 17 MB fixture that is not checked in. Until the skip marker landed,
# its message matched none of the phrases `--strict` looked for, so the suite
# reported green having asserted nothing on every machine that had not built
# the file by hand. Eight seconds buys two durability tests that actually run.
Invoke-Stage -Name 'fixtures' -Because 'the log-lead durability tests read a fixture that is not checked in' -Body {
    # **Git Bash, and forward slashes (task-1962).** This stage failed on every
    # Windows run, for two reasons at once, and the two durability tests the
    # fixture exists for skipped every time - which is the failure the stage was
    # added to stop.
    #
    # `$root` holds a Windows path with backslashes in it, and bash reads a
    # backslash as an escape, so every separator was eaten and the argument
    # arrived as one run-together word. And `bash` on PATH is the WSL launcher
    # under System32: it runs a Linux filesystem where a drive-lettered path is
    # not a path at all, and it answers "No such file or directory" for a script
    # that is right there. Git for Windows ships the bash every script in this
    # repository is written for.
    $posix = $root -replace '\\', '/'
    $shell = @(
        "$env:ProgramFiles/Git/bin/bash.exe",
        "${env:ProgramFiles(x86)}/Git/bin/bash.exe",
        "$env:LOCALAPPDATA/Programs/Git/bin/bash.exe"
    ) | Where-Object { Test-Path $_ } | Select-Object -First 1
    if (-not $shell) { $shell = 'bash' }
    & $shell "$posix/tools/build-gate-fixtures.sh" "$posix/_agent_output/fixtures"
}

# The four contracts `AGENTS.md` names, plus the selection map. Each of them
# fails a build rather than producing a review comment, which is the point of
# having them.
# **The examples on the crate a user depends on (task-1961, T6).** A doctest is
# the one kind of example that cannot rot: it is compiled and run.
Invoke-Stage -Name 'doctests' -Because 'the examples a cargo add reader depends on, compiled and run' -Body {
    cargo test --manifest-path "$root/Cargo.toml" --doc -p inillucent-driver
    if ($LASTEXITCODE -ne 0) { return }
    cargo test --manifest-path "$root/Cargo.toml" --doc -p inillucent
}

# **The public URLs every shipped package names, fetched with no credential.**
# Wired in now that both repositories are public (task-1961, S4).
Invoke-Stage -Name 'urls' -Because 'a URL a shipped package names has to resolve for somebody with no credential' -Body {
    node "$root/tools/check-public-urls.mjs"
}

# `gates_fail_closed` is here rather than only in the full run (task-1961,
# criterion 10; task-1969, 4.13). It is the only test any gate program has, and a
# quick run that skipped it was a quick run with nothing holding the programs
# that decide pass or fail.
Invoke-Stage -Name 'contracts' -Because 'dependencies, layering, the command table and the test map' -Body {
    cargo test --manifest-path "$root/Cargo.toml" -p inillucent-compat --test policy --test selection --test command_parity --test harness --test gates_fail_closed
}

# **A published compatibility report may not carry its own unresolved Problems
# table (task-1946, M5).** `compat/compat-report.md` shipped fourteen rows saying
# a capability the manifest calls `pass` has no passing result recorded on
# linux-x86_64. `inillucent-manifest report` has always exited non-zero when it
# finds one; nothing ever ran it, so the table grew instead.
#
# It regenerates the report from `compat/results` as it goes, so a run whose
# recorded results have moved leaves the checked-in report agreeing with them.
# That is also what makes the stage fail a checkout whose report is stale: the
# `harness.report.reproducible` suite compares the two.
Invoke-Stage -Name 'compat' -Because 'the published compatibility report states no unresolved problem' -Body {
    cargo run --manifest-path "$root/Cargo.toml" -p inillucent-compat --bin inillucent-manifest -- report
}

# **The exit code is checked between the two statements (task-1932, H12).**
# Without the guard, a failed build left the previous `inillucent-testrun` on
# disk and the runner then passed smoke against a stale binary - a green stage
# for a build that did not happen, which is the exact shape of "a test that
# cannot fail" the testing standard forbids. `Invoke-Stage` reads
# `$LASTEXITCODE` after the body, so only the *last* statement decided the
# stage.
Invoke-Stage -Name 'smoke' -Because 'a real file opened, written, reopened, read' -Body {
    cargo build --manifest-path "$root/Cargo.toml" -p inillucent-compat --bin inillucent-testrun --features testrun
    if ($LASTEXITCODE -ne 0) { return }
    & "$root/target/debug/inillucent-testrun" --tier smoke
}

# **`-Stage` reaches past the quick exit (task-1969, 4.13).** `-Quick` means
# "stop after smoke"; it does not mean "refuse to run the stage I named", and
# `-Quick -Coverage -Stage coverage` has to measure on both platforms or the two
# scripts disagree again.
if ($Quick -and -not $Stage) {
    if ($failed.Count -gt 0) {
        Write-Host ''
        Write-Host "FAILED: $($failed -join ', ')" -ForegroundColor Red
        exit 1
    }
    Write-Host ''
    Write-Host 'quick validation passed' -ForegroundColor Green
    exit 0
}

# The security suites, run before the engine tiers because they are the fastest
# way to find out that a change reopened a hole.
Invoke-Stage -Name 'security' -Because 'root confinement, the C ABI lifetimes, and migration transport' -Body {
    cargo test --manifest-path "$root/Cargo.toml" -p inillucent-compat --test confinement
    if ($LASTEXITCODE -ne 0) { return }
    cargo test --manifest-path "$root/Cargo.toml" -p inillucent-driver-capi --test abi --test conformance
    if ($LASTEXITCODE -ne 0) { return }
    cargo test --manifest-path "$root/Cargo.toml" -p inillucent-remote --test transport
}

# `--strict` is the flag that matters: several suites report success when a
# prerequisite is absent, and without it a green on a machine with nothing
# installed reads the same as a green on one with everything.
Invoke-Stage -Name 'tests' -Because 'every selected suite, with missing prerequisites named' -Body {
    & "$root/target/debug/inillucent-testrun" --strict
}

# **Every published count, against the engine that produced it (task-1969,
# 4.4).** `tools/doc-facts/check.mjs` checks the 30 verbs, the 63 dot commands,
# the 416 probe cases, the test and target counts and the private-reference
# patterns, and it is checked by nothing else. Its only caller was
# `packaging/release.ps1`, without `--run-tests`, so the two test facts it holds
# were never in scope in any gate.
#
# After `build` and `tests`, because it reads the built binaries and the runner's
# own summary, and both are fresh by the time it runs.
Invoke-Stage -Name 'doc-facts' -Because 'every count a document states, against the engine that produced it' -Body {
    node "$root/tools/doc-facts/check.mjs" --run-tests
}

# **The four language wrappers, against the binary this run built (task-1969,
# 4.5).** See the note in `tools/validate.sh`; the reasoning is the same and is
# not repeated. `INILLUCENT_BIN` points all four at this build, and setting it
# is what makes the Go suite fail rather than skip when the binary is absent.
#
# A toolchain that is not installed is announced with the `; skipping` marker
# and passed over, which is the same answer `--strict` gives for a suite whose
# prerequisite is absent.
Invoke-Stage -Name 'wrappers' -Because 'the Go, Node, PHP and Python wrappers, against the binary this run built' -Body {
    $binary = Join-Path $root 'target/release/inillucent.exe'
    if (-not (Test-Path $binary)) { $binary = Join-Path $root 'target/debug/inillucent.exe' }
    if (-not (Test-Path $binary)) {
        Write-Host 'no built inillucent to point the wrappers at; run `cargo build --release -p inillucent-cli`; skipping'
        $global:LASTEXITCODE = 1
        return
    }
    $env:INILLUCENT_BIN = $binary
    $wrong = 0

    if (Get-Command go -ErrorAction SilentlyContinue) {
        $said = & go -C (Join-Path $root 'packages/go') test ./... -v 2>&1 | Out-String
        Write-Host $said
        if ($said -match '--- SKIP') {
            Write-Host 'the Go suite skipped a test with INILLUCENT_BIN set, which it may not'
            $wrong = 1
        }
    } else {
        Write-Host 'no go on PATH; install one from https://go.dev/dl/; skipping'
    }

    if (Get-Command node -ErrorAction SilentlyContinue) {
        & node --test (Join-Path $root 'packages/npm/inillucent/*.test.mjs')
        if ($LASTEXITCODE -ne 0) { $wrong = 1 }
    } else {
        Write-Host 'no node on PATH; install one from https://nodejs.org/; skipping'
    }

    if (Get-Command php -ErrorAction SilentlyContinue) {
        & php (Join-Path $root 'packages/php/tests/target.php')
        if ($LASTEXITCODE -ne 0) { $wrong = 1 }
        & php (Join-Path $root 'packages/php/tests/roundtrip.php')
        if ($LASTEXITCODE -ne 0) { $wrong = 1 }
        # **The conformance runner, which nothing invoked** (task-2066 §4.4.3).
        # The other four languages' runners were driven from somewhere; this one
        # was written, checked in, and started by no script - so PHP's record of
        # the shared suite was whatever a person had last produced by hand.
        & php (Join-Path $root 'packages/php/tests/conformance.php')
        if ($LASTEXITCODE -ne 0) { $wrong = 1 }
    } else {
        Write-Host 'no php on PATH; install one from https://www.php.net/downloads; skipping'
    }

    # **A `python3` that `Get-Command` finds and that will not start
    # (task-1970).** Windows puts an app execution alias for the Store's Python
    # on PATH at `WindowsApps\python3.exe`. It is a reparse point into a package
    # this account cannot execute, so `Get-Command` answers with it and starting
    # it fails with "The system cannot find the path specified" - which ended
    # this stage with an unhandled error rather than running the conformance
    # suite, on a machine that has a working `python` further down PATH. Being
    # on PATH is not the question; answering is. So each candidate is asked its
    # version and the first that answers is the one used.
    $python = $null
    foreach ($name in 'python3', 'python') {
        $found = Get-Command $name -ErrorAction SilentlyContinue
        if (-not $found) { continue }
        try { & $found.Source --version *> $null } catch { continue }
        if ($LASTEXITCODE -eq 0) { $python = $found.Source; break }
    }
    if ($python) {
        & $python (Join-Path $root 'drivers/bindings/python/run_conformance.py')
        if ($LASTEXITCODE -ne 0) { $wrong = 1 }
    } else {
        Write-Host 'no python on PATH; install one from https://www.python.org/downloads/; skipping'
    }

    $global:LASTEXITCODE = $wrong
}


# **Coverage, behind a switch, so the published number can be re-measured
# (task-1961, T2).** Not `--branch`: that needs a nightly option and
# `rust-toolchain.toml` pins the compiler to stable.
if ($Coverage) {
    # The shell the `schema_forms` cases need is built by `our_shell`, into this
    # run's own target directory. See the note in tools/validate.sh.
    # `--write` puts the table into `docs/repository.md` between two marker
    # comments instead of printing it for somebody to paste, which is why the
    # page's prose said 40.9% where its own table said 40.6% (task-1969, 4.12).
    Invoke-Stage -Name 'coverage' -Because 'the coverage number docs/repository.md publishes, re-measured' -Body {
        node "$root/tools/coverage.mjs" --per-crate --write
    }
}

Write-Host ''
if ($failed.Count -gt 0) {
    Write-Host "FAILED: $($failed -join ', ')" -ForegroundColor Red
    exit 1
}
Write-Host 'validation passed' -ForegroundColor Green
exit 0
