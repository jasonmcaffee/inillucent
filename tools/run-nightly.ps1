<#
.SYNOPSIS
    Runs the nightly tier and appends the verdict per target to
    tests/nightly-history.tsv.

.DESCRIPTION
    The `nightly` tier holds the long forms: a hundred thousand ledger
    transactions checked against a model and against SQLite, and every published
    release's database read by this build and written for it to read. None of
    them fits in a change's test run, and all of them are the shape of thing
    that stops being true quietly.

    So they run on a schedule, and every run appends a row per target to
    `tests/nightly-history.tsv` - the date, the commit, the machine, the target,
    the verdict and how long it took. The file is the answer to "when did this
    last actually pass", which a green run that nobody recorded cannot give.

    `--strict` is always passed. Several nightly suites need something the
    workspace cannot build - the pinned SQLite oracle, a downloaded release
    binary - and they report success when it is absent. Without `--strict` a
    machine with none of them installed would append a month of passes having
    run nothing.

.PARAMETER Register
    Register the scheduled task that runs this script every night, and exit.

.PARAMETER Unregister
    Remove that scheduled task and exit.

.PARAMETER At
    What time the scheduled task runs. Default 03:00.

.PARAMETER SkipBuild
    Do not build the runner first. For a second run in the same session.

.EXAMPLE
    pwsh tools/run-nightly.ps1

.EXAMPLE
    pwsh tools/run-nightly.ps1 -Register
#>
[CmdletBinding()]
param(
    [switch] $Register,
    [switch] $Unregister,
    [string] $At = '03:00',
    [switch] $SkipBuild
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$taskName = 'inillucent nightly tier'

function Get-MachineLabel {
    <#
    .SYNOPSIS
        A stable label for this machine that is not its name.

    .DESCRIPTION
        The same rule `tests/performance-history.tsv` uses: the first eight hex
        digits of a digest of whatever the operating system calls the machine,
        so two machines get two labels and one machine keeps its own across
        runs, without the name itself being committed. `INILLUCENT_MACHINE`
        overrides it when a fleet would rather read `ci-linux-x64`.
    #>
    if ($env:INILLUCENT_MACHINE) { return $env:INILLUCENT_MACHINE }
    $name = [System.Environment]::MachineName
    $bytes = [System.Text.Encoding]::UTF8.GetBytes($name)
    $digest = [System.Security.Cryptography.SHA256]::Create().ComputeHash($bytes)
    $hex = ($digest | ForEach-Object { $_.ToString('x2') }) -join ''
    return "machine-$($hex.Substring(0, 8))"
}

function Get-SelectedTargets {
    <#
    .SYNOPSIS
        The targets the nightly tier selects, as `package::name`.

    .PARAMETER Runner
        The built `inillucent-testrun`.
    #>
    param([string] $Runner)
    $listing = & $Runner --tier nightly --list 2>&1
    $targets = @()
    foreach ($line in $listing) {
        $fields = ($line -split '\s+') | Where-Object { $_ }
        if ($fields.Count -ge 2 -and $fields[1] -match '::') { $targets += $fields[1] }
    }
    return $targets
}

function Get-RecordedSeconds {
    <#
    .SYNOPSIS
        How long each target took, out of the ledger `--record` just wrote.

    .DESCRIPTION
        `tests/timings.toml` records a time only for a target that passed - a
        failure's duration is how long it took to hit its first assertion, which
        is not a measurement of the suite. So a target with no row here is one
        that did not pass, and the history writes `-` for its seconds rather
        than a number that would mean something else.
    #>
    $path = Join-Path $root 'tests/timings.toml'
    $times = @{}
    if (-not (Test-Path -LiteralPath $path)) { return $times }
    $target = $null
    foreach ($line in (Get-Content -LiteralPath $path)) {
        $text = $line.Trim()
        if ($text -match '^target\s*=\s*"(.+)"$') { $target = $Matches[1] }
        elseif ($text -match '^milliseconds\s*=\s*(\d+)$' -and $target) {
            $times[$target] = [math]::Round([double]$Matches[1] / 1000.0, 1)
            $target = $null
        }
    }
    return $times
}

function Get-Verdicts {
    <#
    .SYNOPSIS
        Reads each target's verdict out of what the runner printed.

    .DESCRIPTION
        The runner prints the targets that failed under `FAILED:`, the ones it
        could not settle under `UNDETERMINED`, and the ones whose prerequisite
        was missing under the line about suites that evidenced nothing. A target
        named in none of those passed.

    .PARAMETER Output
        The runner's output, line by line.

    .PARAMETER Targets
        Every target the tier selected.
    #>
    param([string[]] $Output, [string[]] $Targets)
    $verdicts = @{}
    foreach ($target in $Targets) { $verdicts[$target] = 'pass' }
    $section = ''
    foreach ($line in $Output) {
        $text = "$line"
        if ($text -match '^FAILED:') { $section = 'fail'; continue }
        if ($text -match '^UNDETERMINED') { $section = 'fail'; continue }
        if ($text -match 'evidenced nothing') { $section = 'skipped'; continue }
        if ($text -match '^---' -or $text -match '^\s*$') { $section = ''; continue }
        if (-not $section) { continue }
        $fields = ($text -split '\s+') | Where-Object { $_ }
        if ($fields.Count -ge 1 -and $verdicts.ContainsKey($fields[0])) {
            $verdicts[$fields[0]] = $section
        }
    }
    return $verdicts
}

function Write-History {
    <#
    .SYNOPSIS
        Appends one row per target, creating the file with its header.

    .PARAMETER Verdicts
        Each target's verdict.

    .PARAMETER Seconds
        Each passing target's measured time.
    #>
    param([hashtable] $Verdicts, [hashtable] $Seconds)
    $path = Join-Path $root 'tests/nightly-history.tsv'
    if (-not (Test-Path -LiteralPath $path)) {
        $header = @(
            '# The nightly history: which long-form suite passed, and when.',
            '#',
            '# Appended by `pwsh tools/run-nightly.ps1`, one row per target per run, never edited.',
            '#',
            '# `verdict` is pass, fail, or skipped - and skipped is the one worth reading. A',
            '# nightly suite needs something the workspace cannot build (the pinned SQLite',
            '# oracle, a downloaded release binary), and it reports success when that is',
            '# absent. The run passes --strict so those are counted and named rather than',
            '# read as passes, and they are recorded here as skipped so a month of them is',
            '# visible as a month of nothing having been checked.',
            '#',
            '# `seconds` is written only for a target that passed: a failure''s duration is how',
            '# long it took to reach its first assertion, which is not a measurement of the',
            '# suite.',
            '#',
            '# `machine` is a label, not a hostname - the same rule performance-history.tsv uses.',
            '#',
            "stamp`tcommit`tmachine`ttarget`tverdict`tseconds"
        )
        [System.IO.File]::WriteAllText($path, ($header -join "`n") + "`n", (New-Object System.Text.UTF8Encoding $false))
    }
    $stamp = (Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ')
    $commit = (& git -C $root rev-parse --short HEAD 2>$null)
    # **This run's own two outputs do not make the tree dirty.** `--record`
    # writes `tests/timings.toml` and the lines below write
    # `tests/nightly-history.tsv`, both before this reads the tree - so every
    # row this script has ever written says `-dirty` about the files the script
    # is itself writing. A row that says dirty when nothing but its own output
    # moved sends a reader looking for a code change that is not there, which is
    # the same class of misleading the marker exists to prevent. task-2066 fixed
    # the identical shape in `inillucent-perfhistory`.
    $ours = @('tests/nightly-history.tsv', 'tests/timings.toml')
    $changed = @(& git -C $root status --porcelain | Where-Object { $_.Trim() } | Where-Object {
        $ours -notcontains $_.Substring(3).Trim()
    })
    if ($changed.Count -gt 0) { $commit = "$commit-dirty" }
    $machine = Get-MachineLabel
    $rows = foreach ($target in ($Verdicts.Keys | Sort-Object)) {
        $said = if ($Seconds.ContainsKey($target)) { $Seconds[$target] } else { '-' }
        "$stamp`t$commit`t$machine`t$target`t$($Verdicts[$target])`t$said"
    }
    [System.IO.File]::AppendAllText($path, ($rows -join "`n") + "`n", (New-Object System.Text.UTF8Encoding $false))
    Write-Host "appended $($rows.Count) row(s) to tests/nightly-history.tsv"
}

# ---------------------------------------------------------------------------

if ($Register -or $Unregister) {
    # **A task in the user's own account, not the system's.** The run needs the
    # cargo toolchain, the MSVC environment and the downloaded release binaries,
    # all of which are the user's; a SYSTEM task would find none of them and
    # would append a month of failures nobody could explain.
    if (Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue) {
        Unregister-ScheduledTask -TaskName $taskName -Confirm:$false
        Write-Host "removed the scheduled task '$taskName'"
    }
    if ($Unregister) { return }
    $action = New-ScheduledTaskAction -Execute 'pwsh.exe' `
        -Argument "-NoProfile -File `"$(Join-Path $root 'tools/run-nightly.ps1')`""
    $trigger = New-ScheduledTaskTrigger -Daily -At $At
    $settings = New-ScheduledTaskSettingsSet -StartWhenAvailable `
        -DontStopIfGoingOnBatteries -AllowStartIfOnBatteries `
        -ExecutionTimeLimit (New-TimeSpan -Hours 6)
    Register-ScheduledTask -TaskName $taskName -Action $action -Trigger $trigger `
        -Settings $settings -Description 'Runs inillucent-testrun --tier nightly --strict and appends tests/nightly-history.tsv' | Out-Null
    Write-Host "registered '$taskName' to run every day at $At"
    return
}

. (Join-Path $root 'packaging/stage-layout.ps1')
# `onig_sys` compiles oniguruma with cl.exe, and a scheduled task has no INCLUDE.
Import-MsvcEnvironment | Out-Null

if (-not $SkipBuild) {
    & cargo build --manifest-path (Join-Path $root 'Cargo.toml') -p inillucent-compat `
        --bin inillucent-testrun --features testrun
    if ($LASTEXITCODE -ne 0) { throw 'the runner would not build' }
}

# **Cargo is asked where it put the binary rather than told.** `target/` beside
# the manifest is only the default: a `.cargo/config.toml` can move it to
# another drive, which is what an agent's worktree does to keep its builds off
# the repository, and this script then looked for a runner that was never going
# to be there. `cargo metadata` answers with the directory in use.
$metadata = & cargo metadata --manifest-path (Join-Path $root 'Cargo.toml') --no-deps --format-version 1 |
    ConvertFrom-Json
$target = if ($metadata.target_directory) { $metadata.target_directory } else { Join-Path $root 'target' }
$runner = Join-Path $target 'debug/inillucent-testrun.exe'
if (-not (Test-Path -LiteralPath $runner)) {
    throw "inillucent-testrun is not at $runner. Build it: cargo build -p inillucent-compat --bin inillucent-testrun --features testrun"
}

$targets = Get-SelectedTargets -Runner $runner
if (-not $targets) { throw 'the nightly tier selected no targets' }
Write-Host "nightly tier: $($targets -join ', ')"

$logs = Join-Path $root 'target/nightly'
New-Item -ItemType Directory -Force -Path $logs | Out-Null
$log = Join-Path $logs ((Get-Date).ToUniversalTime().ToString('yyyyMMdd-HHmmss') + '.log')

$output = & $runner --tier nightly --strict --record 2>&1 | ForEach-Object {
    $text = "$_"
    Write-Host $text
    $text
}
$status = $LASTEXITCODE
[System.IO.File]::WriteAllText($log, ($output -join "`n") + "`n", (New-Object System.Text.UTF8Encoding $false))

Write-History -Verdicts (Get-Verdicts -Output $output -Targets $targets) -Seconds (Get-RecordedSeconds)
Write-Host "the run's output is in $log"
exit $status
