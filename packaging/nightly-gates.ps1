<#
.SYNOPSIS
    Decides whether a performance gate's result makes the night red.

.DESCRIPTION
    Dot sourced by `nightly.ps1`. The functions are pure functions of a gate's exit code and printed
    lines, so `tests/nightly-gates.Tests.ps1` can test every case without running a gate.

    WHY A GATE THAT IS NOT MET IS NOT ALWAYS RED

    `inillucent-fullgate` exits 1 whenever any bar in the performance contract is missed. Three bars
    have been missed on every graded run since the contract was written, and docs/performance.md
    says so: `open.prepare` (1.69x against a 5.00x bar), `schema` (1.31x against 3.00x) and the peak
    resident set (9.5% more than SQLite against a bar of 5% less). The first nightly, on 2026-09-25,
    went red on exactly those three and filed a ticket. Every later night would have done the same,
    and `ship.ps1` refuses to release over a red night, so no release could have been cut until the
    contract was met.

    So a gate is graded against `compat/perf/known-misses.txt`, the committed list of bars the
    project already knows it misses:

        exit 0                              met
        exit 4                              not graded, because the machine was busy; not red
        exit 1, only known misses           not met on the known misses; not red
        exit 1, any other missed bar        red, naming the bar
        exit 1, a disagreement, a floor,    red: these are about correctness or a partial run, and
          or no verdict line                no list excuses them
        any other exit code                 red: the gate did not run

    A known miss that a run meets is named in the step, so the list can be shortened when the work
    that fixes it lands.
#>

function Read-KnownGateMisses {
    <#
    .SYNOPSIS
        The names in the known misses file, without comments and blank lines.

    .PARAMETER Path
        The file, normally compat/perf/known-misses.txt. A missing file means no known misses.
    #>
    param([string] $Path)
    if (-not (Test-Path -LiteralPath $Path)) { return @() }
    return @(Get-Content -LiteralPath $Path |
        ForEach-Object { ($_ -replace '#.*$', '').Trim() } |
        Where-Object { $_ })
}

function Get-GateFindings {
    <#
    .SYNOPSIS
        Reads a gate's printed report: its verdict line, the bars it met and missed, and the lines
        that no known miss can excuse.

    .DESCRIPTION
        A bar is a report row that starts with two spaces, has a name, two or more spaces, figures,
        and ends in MET or MISSED. The families table, the headline and the memory and processor
        time table are all printed that way by fullgate.rs and writegate.rs. The name may contain
        spaces and a comma ("peak resident set", "weighted, per the contract"), so it is everything
        before the first run of two spaces.

    .PARAMETER Lines
        Everything the gate printed.
    #>
    param([string[]] $Lines)
    $verdict = $null
    $met = @()
    $missed = @()
    $problems = @()
    foreach ($line in @($Lines)) {
        if ($line -match '^## gate: (?<verdict>MET|NOT MET|NOT GRADED)') { $verdict = $Matches.verdict; continue }
        if ($line -match '^  (?<name>\S.*?)\s{2,}.*\s(?<verdict>MET|MISSED)\s*$') {
            if ($Matches.verdict -eq 'MET') { $met += $Matches.name } else { $missed += $Matches.name }
            continue
        }
        if ($line -match 'UNDER THE FLOOR|disagreed|A FAMILY REPORTED NOTHING|PARTIAL RUN|no round has a value') {
            $problems += $line.Trim()
            continue
        }
        # A workload row in the result table ends in its `agreed` column.
        if ($line -match '^  \S+\s+\d+\s+\d+\s+.*\s(?<agreed>no)\s*$') { $problems += "disagreed: $($line.Trim())" }
    }
    return [pscustomobject]@{ Verdict = $verdict; Met = $met; Missed = $missed; Problems = $problems }
}

function Resolve-GateOutcome {
    <#
    .SYNOPSIS
        Turns one gate's exit code and report into a sentence for the step, and says whether it is red.

    .PARAMETER Name
        The gate, as the step names it: fullgate or writegate.

    .PARAMETER ExitCode
        What the gate exited with. 0 met, 1 not met, 2 an error, 4 not graded.

    .PARAMETER Lines
        Everything the gate printed.

    .PARAMETER Known
        The known misses, from Read-KnownGateMisses.
    #>
    param([string] $Name, [int] $ExitCode, [string[]] $Lines, [string[]] $Known)
    switch ($ExitCode) {
        0 { return [pscustomobject]@{ Red = $false; Text = "$Name met" } }
        4 { return [pscustomobject]@{ Red = $false; Text = "$Name not graded: the machine was not quiet" } }
        1 { }
        default { return [pscustomobject]@{ Red = $true; Text = "$Name exited $ExitCode" } }
    }
    $findings = Get-GateFindings -Lines $Lines
    if ($findings.Verdict -ne 'NOT MET') {
        return [pscustomobject]@{ Red = $true; Text = "$Name exited 1 and printed no NOT MET line" }
    }
    if ($findings.Problems.Count -gt 0) {
        return [pscustomobject]@{ Red = $true; Text = "$Name not met: $($findings.Problems -join '; ')" }
    }
    $known = @($Known)
    $new = @($findings.Missed | Where-Object { $known -notcontains $_ })
    if ($new.Count -gt 0) {
        return [pscustomobject]@{ Red = $true; Text = "$Name missed a bar that is not a known miss: $($new -join ', ')" }
    }
    if ($findings.Missed.Count -eq 0) {
        return [pscustomobject]@{ Red = $true; Text = "$Name exited 1 and printed no missed bar" }
    }
    $text = "$Name not met, on known misses only: $($findings.Missed -join ', ')"
    $fixed = @($findings.Met | Where-Object { $known -contains $_ })
    if ($fixed.Count -gt 0) { $text += "; met tonight, so no longer missed: $($fixed -join ', ')" }
    return [pscustomobject]@{ Red = $false; Text = $text }
}
