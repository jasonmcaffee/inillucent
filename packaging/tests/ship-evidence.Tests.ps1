<#
.SYNOPSIS
    The three things a release does with the nightly's evidence.

.DESCRIPTION
    `ship.ps1` no longer runs a suite of its own when the nightly has already tested the commit it
    releases. That decision is `Resolve-ReleaseTestPlan` in `packaging/nightly-evidence.ps1`, a pure
    function of the evidence and the release's commit, and these cases hold each of its answers:
    green for the same commit runs nothing and names the nightly, green for an older commit runs the
    merge cadence over what changed, and red or missing evidence refuses.

    Written for Pester 3.4, the version Windows ships, so it runs without installing anything:

        Invoke-Pester packaging/tests
#>

$here = Split-Path -Parent $MyInvocation.MyCommand.Path
. (Join-Path (Split-Path -Parent $here) 'nightly-evidence.ps1')

$head = 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'
$older = 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb'

Describe 'Resolve-ReleaseTestPlan' {
    It 'runs no suite for a green nightly of the same commit, and names it in the notes' {
        $evidence = [pscustomobject]@{ commit = $head; date = '2026-09-25T02:00:00Z'; result = 'green'; declared_absent = @() }
        $plan = Resolve-ReleaseTestPlan -Evidence $evidence -Head $head
        $plan.Action | Should Be 'verified'
        $plan.Note | Should Be 'Verified by the nightly run of 2026-09-25T02:00:00Z at aaaaaaaaaaaa.'
    }

    It 'says what the nightly did not evidence when it declared an absence' {
        $evidence = [pscustomobject]@{ commit = $head; date = 'd'; result = 'green'; declared_absent = @('go', 'mysql') }
        $plan = Resolve-ReleaseTestPlan -Evidence $evidence -Head $head
        $plan.Note | Should Match 'declared these prerequisites absent on the machine, so the suites that need them were not evidenced: go, mysql\.$'
    }

    It 'runs the merge cadence over the changes since a green nightly of an older commit' {
        $evidence = [pscustomobject]@{ commit = $older; date = 'd'; result = 'green'; declared_absent = @() }
        $plan = Resolve-ReleaseTestPlan -Evidence $evidence -Head $head
        $plan.Action | Should Be 'changed'
        ($plan.Command -join ' ') | Should Be "--changed $older --cadence merge --strict"
        $plan.Note | Should Match 'bbbbbbbbbbbb to aaaaaaaaaaaa'
    }

    It 'refuses a red nightly, even for the same commit' {
        $evidence = [pscustomobject]@{ commit = $head; date = 'd'; result = 'red'; declared_absent = @() }
        $plan = Resolve-ReleaseTestPlan -Evidence $evidence -Head $head
        $plan.Action | Should Be 'refuse'
        $plan.Reason | Should Match 'was red'
    }

    It 'refuses when there is no evidence at all' {
        $plan = Resolve-ReleaseTestPlan -Evidence $null -Head $head
        $plan.Action | Should Be 'refuse'
        $plan.Reason | Should Match 'no nightly evidence'
    }
}

Describe 'Read-NightlyEvidence' {
    It 'reads nothing from a file that is not there' {
        Read-NightlyEvidence -Path (Join-Path $TestDrive 'missing.json') | Should BeNullOrEmpty
    }

    It 'reads nothing from a file that is not JSON, rather than stopping the release on a parse error' {
        $path = Join-Path $TestDrive 'broken.json'
        Set-Content -LiteralPath $path -Value 'not json {'
        Read-NightlyEvidence -Path $path | Should BeNullOrEmpty
    }

    It 'round trips what the nightly writes' {
        $path = Join-Path $TestDrive 'latest.json'
        $written = New-NightlyEvidence -Commit $head -Result 'green' -Steps ([ordered]@{ suite = 'green' }) -Summary $null
        $written | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $path
        $read = Read-NightlyEvidence -Path $path
        $read.commit | Should Be $head
        $read.result | Should Be 'green'
        $read.steps.suite | Should Be 'green'
    }
}
