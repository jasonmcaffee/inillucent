# The one validation entry point. CI runs this; so should you.
#
# **There is one command rather than a list in a README**, and that is the whole
# point of the file. Before task-1894 the checks existed - `cargo fmt`, the
# dependency policy, the selection map, the crash campaigns - and each of them
# lived in somebody's memory or in a paragraph of prose, so "did you run the
# checks" had no answer. A list of commands nobody runs is the same shape as a
# capability table nobody probes, which `drivers/README.md` already argues
# against.
#
# Every stage prints its own name and the elapsed time, and the script stops at
# the first failure with a non-zero exit code. Nothing here needs a network, a
# server, or a corpus: what does is skipped by the suite that needs it and
# counted by `inillucent-testrun --strict`.
#
#   pwsh tools/validate.ps1              # everything
#   pwsh tools/validate.ps1 -Quick       # fmt, lint, contracts and smoke only
#   pwsh tools/validate.ps1 -Stage lint  # one stage by name
#
# The Unix equivalent is `tools/validate.sh`, which runs the same stages in the
# same order.

[CmdletBinding()]
param(
    # Skips the long test tiers, for a check before pushing.
    [switch] $Quick,
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

# **`inillucent-core` and `inillucent-bench` are not formatted, and that is not
# an oversight to correct here.** Neither is in `policy.rs`'s governed list, both
# have been unformatted since before the rearchitecture (636 diffs at the commit
# task-1894 started from), and every file of them is pinned by SHA3-256 in
# `compat/baseline/inillucent-core-baseline.json`. Formatting them would move 35
# pinned files for a reason unconnected to anything being changed. The lint stage
# below still covers them.
Invoke-Stage -Name 'format' -Because 'a governed crate that is unformatted fails policy.rs anyway' -Body {
    $skip = @('inillucent-core', 'inillucent-bench')
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

# The four contracts `AGENTS.md` names, plus the selection map. Each of them
# fails a build rather than producing a review comment, which is the point of
# having them.
Invoke-Stage -Name 'contracts' -Because 'dependencies, layering, the command table and the test map' -Body {
    cargo test --manifest-path "$root/Cargo.toml" -p inillucent-compat --test policy --test selection --test command_parity --test harness
}

Invoke-Stage -Name 'smoke' -Because 'a real file opened, written, reopened, read' -Body {
    cargo build --manifest-path "$root/Cargo.toml" -p inillucent-compat --bin inillucent-testrun --features testrun
    & "$root/target/debug/inillucent-testrun" --tier smoke
}

if ($Quick) {
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

Write-Host ''
if ($failed.Count -gt 0) {
    Write-Host "FAILED: $($failed -join ', ')" -ForegroundColor Red
    exit 1
}
Write-Host 'validation passed' -ForegroundColor Green
exit 0
