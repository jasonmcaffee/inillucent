<#
.SYNOPSIS
    Publishes the workspace to crates.io.

.DESCRIPTION
    `cargo publish --workspace` does the whole job since Cargo 1.90: it works
    out the order from the dependency graph, publishes into a temporary local
    registry as it goes so each crate can be verified against the one below it,
    and waits for the index between uploads. This script exists to say two
    things that a bare cargo command does not.

    **The first is that publishing is irreversible.** A crate version on
    crates.io cannot be removed. `cargo yank` hides it from new resolution and
    leaves it downloadable forever, because everything that already depends on
    it must keep building. So there is no undo for this command, and it asks
    before it runs.

    **The second is that this makes the source public.** crates.io publishes the
    crate tarball, which is the source. The repository is private today. Running
    this with -Execute publishes 30 crates' source under MIT, permanently. That
    is what the ticket asked for and it should still be a decision somebody
    makes on purpose rather than a script's side effect.

    Two crates are excluded by their own manifests: `inillucent-compat`, the
    assurance harness, and `inillucent-bench`, which links PostgreSQL and
    pgvector to measure against them. Both carry `publish = false`.

.PARAMETER Execute
    Actually publish. Without it this is a dry run, which is what you want to
    run first and what CI should run on every change.

.PARAMETER Token
    A crates.io API token. Without it, cargo uses whatever `cargo login` stored
    in ~/.cargo/credentials.toml. Prefer `cargo login`: a token on a command
    line is a token in a shell history.

.EXAMPLE
    pwsh packaging/cargo-publish.ps1              # dry run, changes nothing
    pwsh packaging/cargo-publish.ps1 -Execute     # publishes, after confirming
#>
[CmdletBinding()]
param(
    [switch] $Execute,
    [switch] $Confirmed,
    [string] $Token
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot

$arguments = @('publish', '--manifest-path', (Join-Path $root 'Cargo.toml'), '--workspace', '--locked')
if ($Token) { $arguments += @('--token', $Token) }

if (-not $Execute) {
    Write-Host 'Dry run. Nothing will be uploaded.'
    Write-Host 'Run again with -Execute to publish for real.'
    Write-Host ''
    & cargo @arguments --dry-run
    exit $LASTEXITCODE
}

Write-Host ''
Write-Host 'This publishes the whole workspace to crates.io.' -ForegroundColor Yellow
Write-Host ''
Write-Host '  * A published version can never be removed. `cargo yank` hides it from new'
Write-Host '    resolution and leaves it downloadable forever.'
Write-Host '  * crates.io publishes the crate tarball, which is the source. This repository'
Write-Host '    is private; publishing makes it public, under MIT, permanently.'
Write-Host '  * The version being published is the one in [workspace.package], and the same'
Write-Host '    number can never be published twice.'
Write-Host ''
# **-Confirmed exists because ship.ps1 cannot answer a prompt (task-1995).** Without it this route
# printed the warning, read an empty line from a non-interactive host, said "Nothing was published"
# and exited 1 - so crates.io was a route the one-command release could never run, and the report
# called it a failure rather than a question nobody was there to answer. The confirmation is still
# given: ship.ps1's preflight names this route in the plan it prints before anything is written.
if (-not $Confirmed) {
    $answer = Read-Host 'Type PUBLISH to continue'
    if ($answer -ne 'PUBLISH') {
        Write-Host 'Nothing was published.'
        exit 1
    }
}

# **crates.io rate limits NEW crates, and this workspace is 25 of them (task-1995).** A new account
# gets a small burst and then one new crate every ten minutes, so `cargo publish --workspace` stops
# with `429 Too Many Requests ... Please try again after <date>` having published a handful. Crates
# that went up stay up and cargo skips them on the next run, so the whole job is "run it again when
# the clock says you may" - which is a person sitting with a timer for several hours, and the kind
# of thing that gets abandoned half done. The refusal carries the exact time to come back, so this
# reads it and waits.
$attempt = 0
while ($true) {
    $attempt++
    $output = & cargo @arguments 2>&1 | Tee-Object -Variable captured
    $output | ForEach-Object { Write-Host $_ }
    if ($LASTEXITCODE -eq 0) { break }

    $text = ($captured | Out-String)
    if ($text -notmatch 'Please try again after ([^)]+?) and see') {
        Write-Host ''
        Write-Host 'The publish stopped part way. Crates that went up are up: fix what failed,'
        Write-Host 'then run this again - cargo skips the versions that already exist.'
        exit $LASTEXITCODE
    }

    $until = $null
    if (-not [datetime]::TryParse($Matches[1], [ref] $until)) {
        Write-Host "crates.io asked us back at $($Matches[1]), which could not be parsed. Run this again then."
        exit $LASTEXITCODE
    }
    # A minute of slack, because the server's clock is the one that counts.
    $wait = [math]::Max(30, ($until.ToUniversalTime() - [datetime]::UtcNow).TotalSeconds + 60)
    if ($wait -gt 3600) {
        Write-Host "crates.io asked us back at $until, which is more than an hour away. Run this again then."
        exit $LASTEXITCODE
    }
    Write-Host ''
    Write-Host "rate limited: $([int] $wait)s until $($until.ToLocalTime().ToString('HH:mm:ss')), attempt $attempt. Crates already up are skipped on the next pass." -ForegroundColor Yellow
    Start-Sleep -Seconds $wait
}

Write-Host ''
Write-Host 'Published. Check https://crates.io/crates/inillucent-cli'
Write-Host 'Then:  cargo install inillucent-cli'
