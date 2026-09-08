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
$answer = Read-Host 'Type PUBLISH to continue'
if ($answer -ne 'PUBLISH') {
    Write-Host 'Nothing was published.'
    exit 1
}

& cargo @arguments
if ($LASTEXITCODE -ne 0) {
    Write-Host ''
    Write-Host 'The publish stopped part way. Crates that went up are up: fix what failed,'
    Write-Host 'then run this again - cargo skips the versions that already exist.'
    exit $LASTEXITCODE
}

Write-Host ''
Write-Host 'Published. Check https://crates.io/crates/inillucent-cli'
Write-Host 'Then:  cargo install inillucent-cli'
