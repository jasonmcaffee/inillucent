<#
.SYNOPSIS
    Finds a token `gh` can use on a machine where `gh auth login` was never run.

.DESCRIPTION
    Dot sourced by `ship.ps1`, whose github route publishes the release, and by `nightly.ps1`, whose
    publish step replaces the rolling `nightly` pre release. Both call `gh`, and `gh` has never been
    logged in on the development machine. The nightly of 2026-09-25 built every archive and then
    stopped at `gh release create` with `To get started with GitHub CLI, please run: gh auth login`,
    because only `ship.ps1` knew how to find the token.
#>

function Resolve-GitHubToken {
    <#
    .SYNOPSIS
        A token gh can use, from GH_TOKEN, from a gh login, or from the credential git already has.

    .DESCRIPTION
        **`gh` has never been logged in on this machine, and the release needed it anyway.** The
        github route checked only that the program was installed, so preflight said `[run ]` and the
        publish would have answered `To get started with GitHub CLI, please run: gh auth login` -
        the same false positive the npm route had twice. It is not a small one: inillucent 0.1.3
        reached the site and PyPI on 2026-09-19 with no GitHub release at all, and nothing said so.

        Asking for `gh auth login` would be asking for a second credential for a host this machine
        is already authenticated to. `git push` works here because Git Credential Manager holds an
        OAuth token for github.com, and `git credential fill` is the supported way to read it - the
        same interface git itself uses. So the order is: an explicit GH_TOKEN, then a real gh login,
        then git's stored credential.

        Returns $null when GH_TOKEN is not needed (gh has its own login) or when nothing was found.
        The token is returned rather than printed; the caller puts it in GH_TOKEN and clears it.
    #>
    if ($env:GH_TOKEN) { return $env:GH_TOKEN }
    & gh auth status *> $null
    if ($LASTEXITCODE -eq 0) { return $null }   # gh has its own login; leave it alone.
    $answer = ("protocol=https`nhost=github.com`n`n" | & git credential fill 2>$null)
    $line = $answer | Where-Object { $_ -like 'password=*' } | Select-Object -First 1
    if (-not $line) { return $null }
    return $line.Substring('password='.Length)
}
