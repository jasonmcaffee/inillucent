<#
.SYNOPSIS
    Reads and writes the nightly run's evidence, and decides what a release does with it.

.DESCRIPTION
    Dot sourced by `nightly.ps1`, which writes `_agent_output/nightly/latest.json` in the main
    checkout, and by `ship.ps1`, which reads it before it cuts a release.

    WHY A RELEASE READS THE NIGHTLY INSTEAD OF RUNNING A SUITE

    `ship.ps1` ran `inillucent-testrun --strict` over every target before a release. On the
    development machine that could never pass: there is no MySQL server, no live PostgreSQL, no Go
    and no openssl there. So every release was cut with `-SkipTests`, and its notes said "This release
    was published without running the test suite." The nightly runs every tier, strictly, with those
    absences declared, and records the commit it ran. A release of that commit needs nothing more.

    THE THREE ANSWERS

        green, same commit as the release    run no suite; the notes name the nightly run
        green, an older commit               run the merge cadence over what changed since then
        red, or no evidence at all           refuse, unless -SkipTests

    The match is on the exact commit, never on a branch name, so evidence for another branch cannot
    stand in for this one.
#>

function Get-MainCheckout {
    <#
    .SYNOPSIS
        The directory of the repository a worktree belongs to.

    .DESCRIPTION
        `git rev-parse --git-common-dir` names the original repository's `.git` from inside any
        worktree. The nightly writes its evidence there, and a release cut from its own worktree reads
        it from there, so both agree on one file.

    .PARAMETER Root
        Any checkout of the repository.
    #>
    param([string] $Root)
    $common = (& git -C $Root rev-parse --git-common-dir 2>$null)
    if (-not $common) { return $Root }
    $resolved = if ([System.IO.Path]::IsPathRooted($common)) { $common } else { Join-Path $Root $common }
    return Split-Path -Parent ([System.IO.Path]::GetFullPath($resolved))
}

function Get-NightlyEvidencePath {
    <#
    .SYNOPSIS
        Where the latest nightly verdict lives.

    .PARAMETER MainCheckout
        The main checkout, from Get-MainCheckout.
    #>
    param([string] $MainCheckout)
    return Join-Path $MainCheckout '_agent_output/nightly/latest.json'
}

function Read-NightlyEvidence {
    <#
    .SYNOPSIS
        The latest nightly verdict, or $null when there is none or it cannot be read.

    .PARAMETER Path
        The latest.json file.
    #>
    param([string] $Path)
    if (-not (Test-Path -LiteralPath $Path)) { return $null }
    try {
        return Get-Content -LiteralPath $Path -Raw | ConvertFrom-Json
    } catch {
        return $null
    }
}

function Resolve-ReleaseTestPlan {
    <#
    .SYNOPSIS
        Decides what a release does about tests, from the nightly evidence and the commit it releases.

    .DESCRIPTION
        Returns a hashtable:

            Action   'verified', 'changed' or 'refuse'
            Commit   the commit the nightly ran, when there is evidence
            Date     the date of that run
            Command  for 'changed', the runner arguments that test what moved since then
            Note     the sentence for the release notes
            Reason   for 'refuse', why

        A pure function of its two inputs, so the three cases can be tested without a nightly having
        run or a release being cut.

    .PARAMETER Evidence
        What Read-NightlyEvidence returned, or $null.

    .PARAMETER Head
        The full commit hash the release is cut from.
    #>
    param($Evidence, [string] $Head)
    if (-not $Evidence) {
        return @{
            Action = 'refuse'
            Reason = 'there is no nightly evidence (_agent_output/nightly/latest.json is missing or unreadable), so nothing says this commit was tested. Run pwsh packaging/nightly.ps1, or pass -SkipTests and accept an untested release.'
        }
    }
    $commit = "$($Evidence.commit)"
    $date = "$($Evidence.date)"
    $short = if ($commit.Length -ge 12) { $commit.Substring(0, 12) } else { $commit }
    if ("$($Evidence.result)" -ne 'green') {
        return @{
            Action = 'refuse'
            Commit = $commit
            Date = $date
            Reason = "the latest nightly run ($date, $short) was $($Evidence.result). A release is not cut over it. Fix what it found, let the nightly run again, or pass -SkipTests."
        }
    }
    $absent = @($Evidence.declared_absent | Where-Object { $_ })
    $caveat = if ($absent.Count -gt 0) {
        " That run declared these prerequisites absent on the machine, so the suites that need them were not evidenced: $($absent -join ', ')."
    } else { '' }
    if ($commit -eq $Head) {
        return @{
            Action = 'verified'
            Commit = $commit
            Date = $date
            Note = "Verified by the nightly run of $date at $short.$caveat"
        }
    }
    $headShort = if ($Head.Length -ge 12) { $Head.Substring(0, 12) } else { $Head }
    return @{
        Action = 'changed'
        Commit = $commit
        Date = $date
        Command = @('--changed', $commit, '--cadence', 'merge', '--strict')
        Note = "Verified by the nightly run of $date at $short, and by a merge cadence run of the changes from $short to $headShort.$caveat"
    }
}

function New-NightlyEvidence {
    <#
    .SYNOPSIS
        Builds the latest.json object the nightly writes.

    .PARAMETER Commit
        The commit the nightly ran.

    .PARAMETER Result
        'green' or 'red'.

    .PARAMETER Steps
        An ordered hashtable of step name to its outcome ('green', 'red', 'skipped: why').

    .PARAMETER Summary
        The runner's --summary JSON, as an object, or $null.
    #>
    param([string] $Commit, [string] $Result, $Steps, $Summary)
    return [ordered]@{
        commit = $Commit
        date = (Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ')
        result = $Result
        steps = $Steps
        declared_absent = if ($Summary) { @($Summary.declared_absent) } else { @() }
        not_evidenced_by_declaration = if ($Summary) { @($Summary.not_evidenced_by_declaration | ForEach-Object { $_.target }) } else { @() }
        failed = if ($Summary) { @($Summary.failed) + @($Summary.undetermined) + @($Summary.hollow | ForEach-Object { $_.target }) } else { @() }
    }
}
