<#
.SYNOPSIS
    The nightly: every tier, the five target release build, the gates on that build, a rolling pre
    release, the timings committed, and a ticket when anything is red.

.DESCRIPTION
    A change is tested by what it touched (`inillucent-testrun --changed`), and a push is tested by
    the merge cadence in CI. Everything else happens here, once a night, on the development machine:

        1. worktree   a dedicated checkout of origin/main (J:/build/nightly by default), fetched fresh
        2. suite      inillucent-testrun --cadence nightly --strict --record --summary
        3. release    release-all.ps1 for all five targets, built in parallel with fat LTO, signed and
                      notarised, so a build or signing failure is found the night it happens
        4. gates      inillucent-fullgate, inillucent-writegate and inillucent-scorecard, built with the
                      release profile, against the medium fixture. A missed bar listed in
                      compat/perf/known-misses.txt does not make the night red
        5. publish    the archives and SHA256SUMS on a rolling `nightly` pre release on the public
                      mirror. Never a registry: a published version is permanent
        6. commit     tests/timings.toml, tests/nightly-history.tsv and compat/perf/nightly, pushed
        7. evidence   _agent_output/nightly/latest.json in the main checkout, which ship.ps1 reads
        8. ticket     when red, one on the board with the report, unless one for the same failures is
                      still open

    It exits 0 when the evidence is green and 1 when it is red. A step that could not run says why in
    latest.json and in the log.

    THE WORKTREE IS THE WORKING DIRECTORY

    cargo reads `.cargo/config.toml` from the working directory, not from the manifest it builds. A
    ticket's worktree carries one that points the target directory at that ticket's folder, and the
    0.1.8 release built into one for exactly that reason. The nightly has its own worktree and sets
    CARGO_TARGET_DIR to that worktree's `target` explicitly, so no config file anywhere changes where
    it builds.

    DECLARED ABSENCES

    The main checkout's gitignored `tests/prerequisites.local.toml` is copied into the worktree, so
    the suites whose prerequisites this machine lacks (MySQL, a live PostgreSQL, Go) are reported
    under their own heading and do not make the night red. latest.json names them, and a release
    note built from it says what the nightly did not evidence.

.PARAMETER Worktree
    The nightly's own checkout. Created on the first run.

.PARAMETER Branch
    The branch to test. `main` for the scheduled run; another branch for a deliberate trial.

.PARAMETER Force
    Run even when latest.json already records this commit as green.

.PARAMETER SkipRelease
    Leave out the release build, and so the gates and the pre release.

.PARAMETER SkipGates
    Leave out the gates and the scorecard.

.PARAMETER SkipPublish
    Build and gate, and upload nothing.

.PARAMETER NoCommit
    Record the timings in the worktree and commit nothing.

.PARAMETER NoTicket
    File no ticket when red.

.PARAMETER WhatIf
    Print the plan and change nothing.

.EXAMPLE
    pwsh packaging/nightly.ps1 -WhatIf
    pwsh packaging/nightly.ps1
    pwsh packaging/nightly.ps1 -Branch nightly-trial -NoCommit -SkipPublish
#>
[CmdletBinding()]
param(
    [string] $Worktree = 'J:/build/nightly',
    [string] $Branch = 'main',
    [switch] $Force,
    [switch] $SkipRelease,
    [switch] $SkipGates,
    [switch] $SkipPublish,
    [switch] $NoCommit,
    [switch] $NoTicket,
    [switch] $WhatIf
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
. (Join-Path $PSScriptRoot 'stage-layout.ps1')
. (Join-Path $PSScriptRoot 'nightly-evidence.ps1')
. (Join-Path $PSScriptRoot 'nightly-gates.ps1')
. (Join-Path $PSScriptRoot 'github-token.ps1')

$main = Get-MainCheckout -Root $repo
$evidencePath = Get-NightlyEvidencePath -MainCheckout $main
$stamp = (Get-Date).ToUniversalTime().ToString('yyyyMMdd-HHmmss')
$runDir = Join-Path $main "_agent_output/nightly/$stamp"
$steps = [ordered]@{}

function Write-Step {
    <#
    .SYNOPSIS
        Prints a step heading and records it in the run log.

    .PARAMETER Name
        The step.
    #>
    param([string] $Name)
    Write-Host ''
    Write-Host "== $Name" -ForegroundColor Cyan
}

function Invoke-Native {
    <#
    .SYNOPSIS
        Runs a program, copying what it prints to the console and to a log, and returns its exit code.

    .PARAMETER Log
        The file its output is appended to.

    .PARAMETER Program
        The program.

    .PARAMETER Arguments
        Its arguments.
    #>
    param([string] $Log, [string] $Program, [string[]] $Arguments)
    & $Program @Arguments 2>&1 | ForEach-Object {
        $text = "$_"
        Write-Host $text
        Add-Content -LiteralPath $Log -Value $text
    }
    return $LASTEXITCODE
}

function Initialize-Worktree {
    <#
    .SYNOPSIS
        Creates or refreshes the nightly worktree at origin/<Branch> and returns its commit.

    .DESCRIPTION
        The worktree is detached at the remote branch and forced to it, because nothing in it is
        anybody's work: it is rebuilt from the remote every night. The gitignored prerequisites the
        suites need, the pinned SQLite build and the gate fixtures, are copied from the main checkout
        the first time, and the machine's declaration of absent prerequisites every time.

        Every git command but the last sends what it prints to the console with Out-Host. A
        PowerShell function returns everything its commands write to standard output, so on
        2026-09-25 `git worktree add` printed `HEAD is now at 8f389cc ...` into the return value.
        latest.json recorded that sentence as the commit, which no release commit can ever equal,
        and the records commit and three rows of tests/nightly-history.tsv said `HEAD is`.
    #>
    & git -C $main fetch origin --quiet | Out-Host
    if ($LASTEXITCODE -ne 0) { throw 'git fetch origin failed in the main checkout' }
    if (-not (Test-Path -LiteralPath (Join-Path $Worktree '.git'))) {
        & git -C $main worktree add --detach $Worktree "origin/$Branch" | Out-Host
        if ($LASTEXITCODE -ne 0) { throw "could not create the worktree $Worktree" }
    } else {
        & git -C $Worktree fetch origin --quiet | Out-Host
        & git -C $Worktree checkout --detach --force "origin/$Branch" | Out-Host
        if ($LASTEXITCODE -ne 0) { throw "could not move $Worktree to origin/$Branch" }
    }
    foreach ($folder in @('.sqlite-ref', '_agent_output/fixtures')) {
        $from = Join-Path $main $folder
        $to = Join-Path $Worktree $folder
        if ((Test-Path -LiteralPath $from) -and -not (Test-Path -LiteralPath $to)) {
            New-Item -ItemType Directory -Force -Path (Split-Path -Parent $to) | Out-Null
            Copy-Item -LiteralPath $from -Destination $to -Recurse
        }
    }
    $declaration = Join-Path $main 'tests/prerequisites.local.toml'
    if (Test-Path -LiteralPath $declaration) {
        Copy-Item -LiteralPath $declaration -Destination (Join-Path $Worktree 'tests/prerequisites.local.toml') -Force
    }
    $head = "$(& git -C $Worktree rev-parse HEAD)".Trim()
    if ($head -notmatch '^[0-9a-f]{40}$') { throw "git rev-parse HEAD in $Worktree answered '$head', not a commit" }
    return $head
}

function Invoke-Suite {
    <#
    .SYNOPSIS
        Builds the runner and runs every tier strictly, recording timings and a JSON summary.
    #>
    $log = Join-Path $runDir 'suite.log'
    $code = Invoke-Native -Log $log -Program 'cargo' -Arguments @('build', '--manifest-path', (Join-Path $Worktree 'Cargo.toml'),
        '-p', 'inillucent-compat', '--bin', 'inillucent-testrun', '--features', 'testrun')
    if ($code -ne 0) { return 'red: the runner would not build' }
    $runner = Join-Path $env:CARGO_TARGET_DIR 'debug/inillucent-testrun.exe'
    $code = Invoke-Native -Log $log -Program $runner -Arguments @('--cadence', 'nightly', '--strict', '--record',
        '--summary', (Join-Path $runDir 'summary.json'))
    switch ($code) {
        0 { return 'green' }
        1 { return 'red: the suite ran and was red' }
        2 { return 'red: the suite did not run; nothing was graded' }
        default { return "red: the runner answered $code" }
    }
}

function Invoke-Release {
    <#
    .SYNOPSIS
        Runs the release build for all five targets, as a release would.
    #>
    $log = Join-Path $runDir 'release.log'
    $code = Invoke-Native -Log $log -Program 'pwsh' -Arguments @('-NoProfile', '-File',
        (Join-Path $Worktree 'packaging/release-all.ps1'), '-Targets', 'all')
    if ($code -ne 0) { return "red: release-all.ps1 exited $code" }
    return 'green'
}

function Invoke-Gates {
    <#
    .SYNOPSIS
        Builds the three gate programs with the release profile and runs them on the medium fixture.

    .DESCRIPTION
        The gates and the scorecard are where a performance claim is measured, so they run on the
        release profile (fat LTO, one codegen unit) and not on a test build. Each gate gets its own
        copy of the fixture: a gate leaves an index behind on the SQLite side, and a second run against
        the same file stops on it. The scorecard appends to compat/perf/nightly/history.jsonl, which
        is committed, so a regression shows as a diff.

        A gate that exits 1 is graded against compat/perf/known-misses.txt by Resolve-GateOutcome
        in nightly-gates.ps1, which explains why a known miss does not make the night red.
    #>
    $log = Join-Path $runDir 'gates.log'
    $known = Read-KnownGateMisses -Path (Join-Path $Worktree 'compat/perf/known-misses.txt')
    $code = Invoke-Native -Log $log -Program 'cargo' -Arguments @('build', '--manifest-path', (Join-Path $Worktree 'Cargo.toml'),
        '--release', '-p', 'inillucent-compat', '--bin', 'inillucent-fullgate', '--bin', 'inillucent-writegate',
        '--bin', 'inillucent-scorecard')
    if ($code -ne 0) { return 'red: the gate programs would not build' }
    $fixture = Join-Path $Worktree '_agent_output/fixtures/medium.db'
    if (-not (Test-Path -LiteralPath $fixture)) { return "skipped: $fixture is not there; run tools/build-gate-fixtures.sh" }
    $release = Join-Path $env:CARGO_TARGET_DIR 'release'
    $scratch = Join-Path $runDir 'gates'
    New-Item -ItemType Directory -Force -Path $scratch | Out-Null
    $outcomes = @()
    Copy-Item -LiteralPath $fixture -Destination (Join-Path $scratch 'medium-full.db')
    $outcomes += Invoke-Gate -Log $log -Name 'fullgate' -Known $known -Program (Join-Path $release 'inillucent-fullgate.exe') -Arguments @(
        (Join-Path $scratch 'medium-full.db'), '--scale', 'medium', '--rounds', '30', '--page-size', '32768', '--frames', '4096')
    Copy-Item -LiteralPath $fixture -Destination (Join-Path $scratch 'medium-write.db')
    $outcomes += Invoke-Gate -Log $log -Name 'writegate' -Known $known -Program (Join-Path $release 'inillucent-writegate.exe') -Arguments @(
        (Join-Path $scratch 'medium-write.db'), '--scale', 'medium')
    $code = Invoke-Native -Log $log -Program (Join-Path $release 'inillucent-scorecard.exe') -Arguments @(
        '--scale', 'medium', '--out', (Join-Path $Worktree 'compat/perf/nightly'), '--label', "nightly-$stamp")
    $outcomes += [pscustomobject]@{ Red = ($code -ne 0); Text = $(if ($code -eq 0) { 'scorecard ran' } else { "scorecard exited $code" }) }
    $text = ($outcomes | ForEach-Object { $_.Text }) -join '; '
    if (@($outcomes | Where-Object { $_.Red }).Count -gt 0) { return "red: $text" }
    return "green: $text"
}

function Invoke-Gate {
    <#
    .SYNOPSIS
        Runs one gate, appending its report to the gates log, and grades what it printed.

    .PARAMETER Log
        The gates log.

    .PARAMETER Name
        The gate's name in the step: fullgate or writegate.

    .PARAMETER Known
        The known misses.

    .PARAMETER Program
        The gate program.

    .PARAMETER Arguments
        Its arguments.
    #>
    param([string] $Log, [string] $Name, [string[]] $Known, [string] $Program, [string[]] $Arguments)
    $before = if (Test-Path -LiteralPath $Log) { @(Get-Content -LiteralPath $Log).Count } else { 0 }
    $code = Invoke-Native -Log $Log -Program $Program -Arguments $Arguments
    $lines = @(Get-Content -LiteralPath $Log | Select-Object -Skip $before)
    return Resolve-GateOutcome -Name $Name -ExitCode $code -Lines $lines -Known $Known
}

function Publish-NightlyPrerelease {
    <#
    .SYNOPSIS
        Replaces the rolling `nightly` pre release on the public mirror with tonight's archives.

    .DESCRIPTION
        The tag is `nightly`, which no registry reads as a version: the Go proxy and Packagist take
        only tags shaped like versions, and a registry that took this one would keep it for ever. The
        release is marked pre release and its notes say which commit and date it is. install.sh and
        the site never point at it.

    .PARAMETER Commit
        The commit the archives were built from.
    #>
    param([string] $Commit)
    $url = (& git -C $main remote get-url brl 2>$null)
    if (-not $url -or $url -notmatch 'github\.com[:/](?<owner>[^/]+)/(?<name>[^/.]+)') {
        return 'skipped: this checkout has no brl remote naming the public mirror'
    }
    $mirror = "$($Matches.owner)/$($Matches.name)"
    $dist = Join-Path $Worktree 'dist'
    $version = Get-WorkspaceVersion -Root $Worktree
    $assets = @(Get-ChildItem -LiteralPath $dist -File |
        Where-Object { ($_.Name -like "*$version*" -or $_.Name -like 'SHA256SUMS*') -and $_.Name -notlike '*-apple-darwin.zip' } |
        ForEach-Object { $_.FullName })
    if ($assets.Count -eq 0) { return "red: dist holds no archive for $version" }
    $date = (Get-Date).ToUniversalTime().ToString('yyyy-MM-dd')
    $notes = @(
        "A nightly build of commit $Commit, made on $date. It is not a release.",
        '',
        'It is replaced every night, is marked as a pre release, and is not published to any registry.',
        "Its archives carry the workspace version $version, which is the last release's number, so a",
        'nightly archive and a release archive can have the same name. Take a release unless you need',
        'what changed since it.',
        '',
        'The mirror holds only release commits, so the `nightly` tag points at the newest release commit',
        'there. The commit named above is the one the archives were built from.'
    ) -join "`n"
    # gh is not logged in on this machine; the token is the one `git push` already uses.
    $previousToken = $env:GH_TOKEN
    $resolved = Resolve-GitHubToken
    if ($resolved) { $env:GH_TOKEN = $resolved }
    try {
        & gh release view nightly --repo $mirror --json tagName 2>$null | Out-Null
        if ($LASTEXITCODE -eq 0) {
            & gh release delete nightly --repo $mirror --cleanup-tag --yes | Out-Host
            if ($LASTEXITCODE -ne 0) { return 'red: the previous nightly pre release could not be removed' }
        }
        & gh release create nightly @assets --repo $mirror --prerelease --title "nightly $date" --notes $notes | Out-Host
        if ($LASTEXITCODE -ne 0) { return 'red: gh release create failed' }
        return 'green'
    } finally {
        $env:GH_TOKEN = $previousToken
    }
}

function Add-NightlyHistory {
    <#
    .SYNOPSIS
        Appends one row per nightly tier target to tests/nightly-history.tsv.

    .DESCRIPTION
        The same file and the same columns tools/run-nightly.ps1 writes: when each long form suite
        last actually passed. The verdict comes from the runner's summary rather than from its text.

    .PARAMETER Summary
        The runner's summary.

    .PARAMETER Commit
        The commit tested.
    #>
    param($Summary, [string] $Commit)
    $path = Join-Path $Worktree 'tests/nightly-history.tsv'
    if (-not (Test-Path -LiteralPath $path)) { return }
    $runner = Join-Path $env:CARGO_TARGET_DIR 'debug/inillucent-testrun.exe'
    $targets = @(& $runner --tier nightly --list 2>$null | ForEach-Object {
        $fields = ("$_" -split '\s+') | Where-Object { $_ }
        if ($fields.Count -ge 2 -and $fields[1] -match '::') { $fields[1] }
    })
    $failed = @($Summary.failed) + @($Summary.undetermined)
    $skipped = @($Summary.hollow | ForEach-Object { $_.target }) + @($Summary.not_evidenced_by_declaration | ForEach-Object { $_.target })
    $times = @{}
    $current = $null
    foreach ($line in (Get-Content -LiteralPath (Join-Path $Worktree 'tests/timings.toml'))) {
        if ($line -match '^target\s*=\s*"(.+)"$') { $current = $Matches[1] }
        elseif ($line -match '^milliseconds\s*=\s*(\d+)$' -and $current) {
            $times[$current] = [math]::Round([double]$Matches[1] / 1000.0, 1)
            $current = $null
        }
    }
    $when = (Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ')
    $short = $Commit.Substring(0, 7)
    $machine = "machine-" + (([System.Security.Cryptography.SHA256]::Create().ComputeHash(
        [System.Text.Encoding]::UTF8.GetBytes([System.Environment]::MachineName)) |
        ForEach-Object { $_.ToString('x2') }) -join '').Substring(0, 8)
    $rows = foreach ($target in ($targets | Sort-Object)) {
        $verdict = if ($failed -contains $target) { 'fail' } elseif ($skipped -contains $target) { 'skipped' } else { 'pass' }
        $seconds = if ($verdict -eq 'pass' -and $times.ContainsKey($target)) { $times[$target] } else { '-' }
        "$when`t$short`t$machine`t$target`t$verdict`t$seconds"
    }
    if ($rows) {
        [System.IO.File]::AppendAllText($path, (($rows -join "`n") + "`n"), (New-Object System.Text.UTF8Encoding $false))
    }
}

function Save-NightlyRecords {
    <#
    .SYNOPSIS
        Commits the timings, the nightly history and the scorecard history, and pushes them.

    .DESCRIPTION
        One push attempt, and one more after a rebase if the branch moved during the night. A push
        that still fails is recorded as red, because the next night would otherwise schedule from
        stale timings without anybody knowing.

    .PARAMETER Commit
        The commit tested.
    #>
    param([string] $Commit)
    $paths = @('tests/timings.toml', 'tests/nightly-history.tsv', 'compat/perf/nightly') |
        Where-Object { Test-Path -LiteralPath (Join-Path $Worktree $_) }
    & git -C $Worktree add -- @paths
    & git -C $Worktree diff --cached --quiet
    if ($LASTEXITCODE -eq 0) { return 'green: nothing changed' }
    & git -C $Worktree commit --quiet -m "nightly: timings and scorecard history for $($Commit.Substring(0, 12))"
    if ($LASTEXITCODE -ne 0) { return 'red: the commit failed' }
    & git -C $Worktree push --quiet origin "HEAD:$Branch"
    if ($LASTEXITCODE -eq 0) { return 'green' }
    & git -C $Worktree fetch origin --quiet
    & git -C $Worktree rebase "origin/$Branch"
    if ($LASTEXITCODE -ne 0) {
        & git -C $Worktree rebase --abort
        return "red: $Branch moved and the records would not rebase onto it"
    }
    & git -C $Worktree push --quiet origin "HEAD:$Branch"
    if ($LASTEXITCODE -ne 0) { return 'red: the push failed twice' }
    return 'green'
}

function Send-NightlyTicket {
    <#
    .SYNOPSIS
        Files a ticket for a red night, unless one for the same failures is still open.

    .DESCRIPTION
        The board token comes from the sealed store through ai-service's skipToken.cjs, the same way
        an agent terminal gets it, and lives only in this process's memory: it is never written to a
        file, a log or a command line argument that another process could read.

    .PARAMETER Commit
        The commit tested.

    .PARAMETER Evidence
        What latest.json now says.
    #>
    param([string] $Commit, $Evidence)
    $api = if ($env:TASKS_API) { $env:TASKS_API } else { 'http://localhost:8091/tasks' }
    $resolver = 'C:/jason/dev/ai-service/backend/skipToken.cjs'
    if (-not (Test-Path -LiteralPath $resolver)) { return "skipped: $resolver is not there, so the board cannot be reached" }
    $token = & node -e "process.stdout.write(require('$resolver').resolveSkipToken(['CLAUDE_SKIP_TOKEN']))"
    if (-not $token) { return 'skipped: no board token could be resolved' }
    $headers = @{ 'x-skip-token' = $token }
    $red = @($Evidence.steps.GetEnumerator() | Where-Object { "$($_.Value)" -like 'red*' } | ForEach-Object { $_.Key })
    $signature = (@($red) + @($Evidence.failed | Sort-Object)) -join ', '
    $open = @()
    foreach ($status in @('new', 'in_progress', 'qa_failed')) {
        $found = Invoke-RestMethod -Headers $headers -Uri "$api/search?q=$([uri]::EscapeDataString('Nightly red'))&status=$status"
        $open += @($found | Where-Object { "$($_.description)" -like "*signature: $signature*" })
    }
    if ($open.Count -gt 0) { return "skipped: $($open[0].taskKey) is open for the same failures" }
    $board = Invoke-RestMethod -Headers $headers -Uri "$api/board"
    $report = if (Test-Path -LiteralPath (Join-Path $runDir 'suite.log')) {
        (Get-Content -LiteralPath (Join-Path $runDir 'suite.log') -Tail 80) -join "`n"
    } else { '' }
    $lines = @(
        "The nightly run of $((Get-Date).ToUniversalTime().ToString('yyyy-MM-dd')) at $Commit was red.",
        '',
        'Steps:'
    ) + @($Evidence.steps.GetEnumerator() | ForEach-Object { "- $($_.Key): $($_.Value)" }) + @(
        '',
        "Failing targets: $(if ($Evidence.failed) { $Evidence.failed -join ', ' } else { 'none' })",
        '',
        "Everything the night wrote is in $runDir.",
        '',
        "signature: $signature",
        '',
        'The end of the suite log:',
        '```',
        $report,
        '```'
    )
    $body = @{
        title = "Nightly red $((Get-Date).ToUniversalTime().ToString('yyyy-MM-dd')): $signature"
        description = ($lines -join "`n")
        sprintId = $board.activeSprint.id
        agentProjectId = 'inillucent'
        assignee = 'claude'
        model = 'opus[1m]'
        priority = 'high'
    } | ConvertTo-Json -Depth 4
    $created = Invoke-RestMethod -Method Post -Headers $headers -Uri $api -ContentType 'application/json; charset=utf-8' `
        -Body ([System.Text.Encoding]::UTF8.GetBytes($body))
    return "filed $($created.taskKey)"
}

# ---------------------------------------------------------------------------

Write-Host "inillucent nightly - $Worktree at origin/$Branch"
Write-Host "   evidence: $evidencePath"
if ($WhatIf) {
    $evidence = Read-NightlyEvidence -Path $evidencePath
    $last = if ($evidence) { "$($evidence.result) at $($evidence.commit) on $($evidence.date)" } else { 'none' }
    Write-Host "   the last night: $last"
    Write-Host '   would fetch origin, move the worktree to the branch, and exit 0 if that commit is already green'
    Write-Host '   would run: inillucent-testrun --cadence nightly --strict --record --summary'
    if (-not $SkipRelease) { Write-Host '   would run: release-all.ps1 -Targets all (five builds in parallel, fat LTO, signed and notarised)' }
    if (-not $SkipRelease -and -not $SkipGates) { Write-Host '   would run: inillucent-fullgate, inillucent-writegate, inillucent-scorecard on the medium fixture' }
    if (-not $SkipRelease -and -not $SkipPublish) { Write-Host '   would replace the rolling nightly pre release on the public mirror' }
    if (-not $NoCommit) { Write-Host "   would commit tests/timings.toml, tests/nightly-history.tsv and compat/perf/nightly, and push to $Branch" }
    Write-Host '   would write latest.json, and file a ticket if the night is red'
    Write-Host ''
    Write-Host 'nothing was written.' -ForegroundColor Yellow
    return
}

New-Item -ItemType Directory -Force -Path $runDir | Out-Null
Write-Step 'worktree'
$commit = Initialize-Worktree
Write-Host "   $commit"
$previous = Read-NightlyEvidence -Path $evidencePath
if (-not $Force -and $previous -and "$($previous.commit)" -eq $commit -and "$($previous.result)" -eq 'green') {
    Write-Host "   $commit was already green on $($previous.date); nothing to do."
    exit 0
}

$env:CARGO_TARGET_DIR = Join-Path $Worktree 'target'
# This machine has a network, so the HTTP client's tests fetch the real files the installer fetches
# instead of skipping, as they do in CI.
$env:INILLUCENT_NETWORK_TESTS = '1'
# `onig_sys` compiles oniguruma with cl.exe, and a scheduled task has no INCLUDE.
Import-MsvcEnvironment | Out-Null

Write-Step 'suite'
$steps['suite'] = Invoke-Suite
$summaryPath = Join-Path $runDir 'summary.json'
$summary = if (Test-Path -LiteralPath $summaryPath) { Get-Content -LiteralPath $summaryPath -Raw | ConvertFrom-Json } else { $null }

if ($SkipRelease) {
    $steps['release'] = 'skipped: -SkipRelease'
} else {
    Write-Step 'release'
    $steps['release'] = Invoke-Release
}
if ($SkipRelease -or $SkipGates) {
    $steps['gates'] = 'skipped: not asked for'
} else {
    Write-Step 'gates'
    $steps['gates'] = Invoke-Gates
}
if ($SkipRelease -or $SkipPublish) {
    $steps['publish'] = 'skipped: not asked for'
} elseif ("$($steps['release'])" -ne 'green') {
    $steps['publish'] = 'skipped: the release build was not green'
} else {
    Write-Step 'publish'
    $steps['publish'] = Publish-NightlyPrerelease -Commit $commit
}
if ($summary) { Add-NightlyHistory -Summary $summary -Commit $commit }
if ($NoCommit) {
    $steps['commit'] = 'skipped: -NoCommit'
} else {
    Write-Step 'commit'
    $steps['commit'] = Save-NightlyRecords -Commit $commit
}

$red = @($steps.Values | Where-Object { "$_" -like 'red*' }).Count -gt 0
$result = if ($red) { 'red' } else { 'green' }
$evidence = New-NightlyEvidence -Commit $commit -Result $result -Steps $steps -Summary $summary
if ($red -and -not $NoTicket) {
    Write-Step 'ticket'
    try {
        $evidence['ticket'] = Send-NightlyTicket -Commit $commit -Evidence ([pscustomobject]$evidence)
    } catch {
        $evidence['ticket'] = "red: the ticket could not be filed: $($_.Exception.Message)"
    }
    Write-Host "   $($evidence['ticket'])"
}
$json = $evidence | ConvertTo-Json -Depth 6
[System.IO.File]::WriteAllText((Join-Path $runDir 'latest.json'), $json, (New-Object System.Text.UTF8Encoding $false))
# The evidence a release reads is written only for the branch releases are cut from.
if ($Branch -eq 'main') {
    [System.IO.File]::WriteAllText($evidencePath, $json, (New-Object System.Text.UTF8Encoding $false))
}

Write-Host ''
foreach ($name in $steps.Keys) { Write-Host ("   {0,-8} {1}" -f $name, $steps[$name]) }
Write-Host ''
Write-Host "the night was $result. Everything it wrote is in $runDir" -ForegroundColor $(if ($red) { 'Red' } else { 'Green' })
exit $(if ($red) { 1 } else { 0 })
