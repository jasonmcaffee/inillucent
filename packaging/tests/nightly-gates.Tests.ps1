<#
.SYNOPSIS
    How the nightly grades a performance gate.

.DESCRIPTION
    `Resolve-GateOutcome` in `packaging/nightly-gates.ps1` decides whether a gate's exit code and
    report make the night red. The report lines below are copied from the gates log of the nightly
    of 2026-09-25, which went red on the three bars docs/performance.md already lists as missed.

    Written for Pester 3.4, the version Windows ships, so it runs without installing anything:

        Invoke-Pester packaging/tests
#>

$here = Split-Path -Parent $MyInvocation.MyCommand.Path
. (Join-Path (Split-Path -Parent $here) 'nightly-gates.ps1')

$known = @('open.prepare', 'schema', 'peak resident set', 'processor time')

# The fullgate report of 2026-09-25, cut to the rows the grading reads.
$fullgate = @(
    '## result',
    '  workload                  inillucent ns      sqlite ns     ratio       low      high  agreed',
    '  prepare.trivial                 2838750        1630500     0.58x     0.57x     0.59x  yes',
    '  correlated.exists.selective          18500          22400     1.22x     1.18x     1.25x  yes',
    '## families',
    '  family               ratio       low      high      bar     worst  verdict',
    '  open.prepare         1.75x     1.72x     1.77x    5.00x     0.58x  MISSED',
    '  read.point          31.98x    31.49x    32.50x    2.00x    17.70x  MET',
    '  schema               1.36x     1.33x     1.38x    3.00x     1.38x  MISSED',
    '## headline',
    '  geometric mean                 ratio       low      high    bound  verdict',
    '  weighted, per the contract     5.35x     4.86x     5.38x    3.00x  MET',
    '  unweighted, family-equal       4.63x     4.37x     4.65x        -  reported',
    '## floor: no required family below 1.00x',
    '  every required family is above it',
    '## memory and processor time, against the contract',
    '  peak resident set                43.36 MiB     37.24 MiB     1.16x   0.95x  MISSED',
    '  processor time                  421.88 ms   1078.12 ms     0.39x   0.40x  MET',
    '## gate: NOT MET'
)

Describe 'Get-GateFindings' {
    It 'reads the verdict, the missed bars and the met bars, names with spaces and commas included' {
        $findings = Get-GateFindings -Lines $fullgate
        $findings.Verdict | Should Be 'NOT MET'
        ($findings.Missed -join '|') | Should Be 'open.prepare|schema|peak resident set'
        ($findings.Met -join '|') | Should Be 'read.point|weighted, per the contract|processor time'
        $findings.Problems.Count | Should Be 0
    }

    It 'finds a workload whose answer did not agree with SQLite' {
        $lines = $fullgate + '  point.rowid                     1451600       49999050    35.23x    32.96x    35.65x  no'
        (Get-GateFindings -Lines $lines).Problems[0] | Should Match '^disagreed: point\.rowid'
    }
}

Describe 'Resolve-GateOutcome' {
    It 'is not red when every missed bar is a known miss, and names a known miss that was met' {
        $outcome = Resolve-GateOutcome -Name 'fullgate' -ExitCode 1 -Lines $fullgate -Known $known
        $outcome.Red | Should Be $false
        $outcome.Text | Should Be 'fullgate not met, on known misses only: open.prepare, schema, peak resident set; met tonight, so no longer missed: processor time'
    }

    It 'is red when a bar that is not a known miss is missed' {
        $lines = $fullgate -replace 'read\.point          31\.98x    31\.49x    32\.50x    2\.00x    17\.70x  MET', 'read.point           1.98x     1.49x     2.50x    2.00x     1.70x  MISSED'
        $outcome = Resolve-GateOutcome -Name 'fullgate' -ExitCode 1 -Lines $lines -Known $known
        $outcome.Red | Should Be $true
        $outcome.Text | Should Be 'fullgate missed a bar that is not a known miss: read.point'
    }

    It 'is red when the list of known misses is empty' {
        (Resolve-GateOutcome -Name 'fullgate' -ExitCode 1 -Lines $fullgate -Known @()).Red | Should Be $true
    }

    It 'is red on a disagreement, whatever the list says' {
        $lines = $fullgate + '  a workload disagreed; the two databases are kept in "C:\\scratch"'
        $outcome = Resolve-GateOutcome -Name 'fullgate' -ExitCode 1 -Lines $lines -Known $known
        $outcome.Red | Should Be $true
        $outcome.Text | Should Match 'disagreed'
    }

    It 'is red on a family under the floor' {
        $lines = $fullgate + '  schema               0.85x  UNDER THE FLOOR'
        (Resolve-GateOutcome -Name 'fullgate' -ExitCode 1 -Lines $lines -Known $known).Red | Should Be $true
    }

    It 'is red when exit code 1 comes with no NOT MET line, because the gate stopped part way' {
        $lines = $fullgate | Where-Object { $_ -ne '## gate: NOT MET' }
        (Resolve-GateOutcome -Name 'fullgate' -ExitCode 1 -Lines $lines -Known $known).Red | Should Be $true
    }

    It 'is not red when the machine was not quiet, and says so' {
        $outcome = Resolve-GateOutcome -Name 'writegate' -ExitCode 4 -Lines @() -Known $known
        $outcome.Red | Should Be $false
        $outcome.Text | Should Be 'writegate not graded: the machine was not quiet'
    }

    It 'is red on an error exit' {
        $outcome = Resolve-GateOutcome -Name 'fullgate' -ExitCode 2 -Lines @() -Known $known
        $outcome.Red | Should Be $true
        $outcome.Text | Should Be 'fullgate exited 2'
    }

    It 'is not red when the gate is met' {
        (Resolve-GateOutcome -Name 'writegate' -ExitCode 0 -Lines @() -Known $known).Red | Should Be $false
    }
}

Describe 'Read-KnownGateMisses' {
    It 'reads the checked in list without its comments' {
        $path = Join-Path (Split-Path -Parent (Split-Path -Parent $here)) 'compat/perf/known-misses.txt'
        (Read-KnownGateMisses -Path $path) -join '|' | Should Be 'open.prepare|schema|peak resident set|processor time'
    }

    It 'reads no known misses from a file that is not there' {
        @(Read-KnownGateMisses -Path (Join-Path $TestDrive 'missing.txt')).Count | Should Be 0
    }
}
