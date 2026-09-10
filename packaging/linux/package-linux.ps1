<#
.SYNOPSIS
    Builds the .deb and .rpm from the staged Linux archives, on Windows.

.DESCRIPTION
    nfpm is one Go executable that writes both formats and signs both with an
    OpenPGP key. No dpkg, no rpmbuild, no container and no Linux machine: the
    packages are written from the same staged directories the .tar.gz archives
    are made from, so a package and a tarball of one version hold the same bytes.

    Signing is what makes `apt` and `dnf` willing to install a package that did
    not come from a distribution's own repository, so it is on by default and has
    to be turned off deliberately with -Unsigned.

    nfpm reads a signing key from a file rather than from an agent, so the key is
    exported from GnuPG into the user's temp directory for the length of the run
    and deleted in a finally. It is never written inside the repository, and the
    passphrase only ever arrives through an environment variable.

.PARAMETER Version
    Overrides the version taken from the workspace manifest.

.PARAMETER Unsigned
    Skip the OpenPGP signature. For testing the packaging itself.

.PARAMETER GpgKey
    The key id or user id to sign with. Defaults to $env:INILLUCENT_GPG_KEY.

.PARAMETER GpgPassphraseEnv
    The environment variable holding that key's passphrase. Default
    INILLUCENT_GPG_PASSPHRASE. Never a parameter, so it cannot reach a shell
    history or a process listing.

.EXAMPLE
    pwsh packaging/linux/package-linux.ps1
    pwsh packaging/linux/package-linux.ps1 -Unsigned
#>
[CmdletBinding()]
param(
    [string] $Version,
    [switch] $Unsigned,
    [string] $GpgKey = $env:INILLUCENT_GPG_KEY,
    [string] $GpgPassphraseEnv = 'INILLUCENT_GPG_PASSPHRASE'
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
. (Join-Path $root 'packaging/stage-layout.ps1')

$nfpm = Join-Path $root 'tools/cross/bin/nfpm.exe'
if (-not (Test-Path -LiteralPath $nfpm)) {
    throw 'nfpm is missing. Run: pwsh tools/cross/fetch-toolchain.ps1'
}
$gpg = 'C:\Program Files\Git\usr\bin\gpg.exe'

if (-not $Version) { $Version = Get-WorkspaceVersion -Root $root }
$dist = Join-Path $root 'dist'
$work = Join-Path $dist '_nfpm'
New-Item -ItemType Directory -Force -Path $work | Out-Null

# The Rust triple on the left, the names Debian and RPM use on the right.
$architectures = @(
    @{ Triple = 'x86_64-unknown-linux-gnu'; Nfpm = 'amd64'; Deb = 'amd64'; Rpm = 'x86_64' },
    @{ Triple = 'aarch64-unknown-linux-gnu'; Nfpm = 'arm64'; Deb = 'arm64'; Rpm = 'aarch64' }
)

function Export-SigningKey {
    <#
    .SYNOPSIS
        Writes the OpenPGP secret key to a temporary file, because nfpm signs
        from a file rather than through an agent.

    .DESCRIPTION
        The file goes in the user's temp directory, never in the repository, and
        the caller deletes it in a finally. The passphrase is read from the
        environment and passed to GnuPG on stdin rather than on the command line.

    .PARAMETER Key
        The key id or user id to export.

    .PARAMETER Passphrase
        Its passphrase.
    #>
    param([string] $Key, [string] $Passphrase)

    if (-not (Test-Path -LiteralPath $gpg)) {
        throw "gpg is not at $gpg. Install Git for Windows, which ships it, or pass -Unsigned."
    }
    $keyFile = Join-Path ([System.IO.Path]::GetTempPath()) ("inillucent-signing-" + [guid]::NewGuid().ToString('N') + '.asc')
    $Passphrase | & $gpg --batch --yes --pinentry-mode loopback --passphrase-fd 0 `
        --armor --output $keyFile --export-secret-keys $Key
    if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath $keyFile)) {
        throw "gpg could not export $Key"
    }
    return $keyFile
}

Write-Host "inillucent $Version, Linux packages"

$keyFile = $null
try {
    $signatureBlock = ''
    if (-not $Unsigned) {
        if (-not $GpgKey) {
            throw 'no signing key. Set INILLUCENT_GPG_KEY, pass -GpgKey, or pass -Unsigned.'
        }
        $passphrase = [System.Environment]::GetEnvironmentVariable($GpgPassphraseEnv)
        if (-not $passphrase) { throw "$GpgPassphraseEnv is not set, so the key cannot be used" }
        $keyFile = Export-SigningKey -Key $GpgKey -Passphrase $passphrase
        $env:NFPM_PASSPHRASE = $passphrase
        $escaped = ($keyFile -replace '\\', '/')
        $signatureBlock = @"

deb:
  signature:
    key_file: $escaped
    type: origin
rpm:
  signature:
    key_file: $escaped
"@
    }

    $template = Get-Content -Path (Join-Path $PSScriptRoot 'nfpm.template.yaml') -Raw

    foreach ($architecture in $architectures) {
        $stage = Join-Path $dist "inillucent-$Version-$($architecture.Triple)"
        if (-not (Test-Path -LiteralPath $stage)) {
            throw "$stage does not exist. Run: pwsh packaging/release-all.ps1 -Targets linux"
        }

        # nfpm resolves `src` against the working directory, so the staged path
        # goes in absolutely, with forward slashes, which it accepts on Windows.
        $config = Join-Path $work "$($architecture.Nfpm).yaml"
        $filled = $template.
            Replace('@VERSION@', $Version).
            Replace('@ARCH@', $architecture.Nfpm).
            Replace('@STAGE@', ($stage -replace '\\', '/')) + $signatureBlock
        Set-Content -Path $config -Value $filled -Encoding utf8

        foreach ($format in @('deb', 'rpm')) {
            $target = if ($format -eq 'deb') {
                Join-Path $dist "inillucent_${Version}_$($architecture.Deb).deb"
            } else {
                Join-Path $dist "inillucent-$Version.$($architecture.Rpm).rpm"
            }
            & $nfpm package --config $config --packager $format --target $target | Out-Null
            if ($LASTEXITCODE -ne 0) { throw "nfpm failed for $format/$($architecture.Nfpm) with $LASTEXITCODE" }
            Write-Host "  $target"
        }
    }
} finally {
    if ($keyFile -and (Test-Path -LiteralPath $keyFile)) {
        Remove-Item -LiteralPath $keyFile -Force -Confirm:$false
    }
    if ($env:NFPM_PASSPHRASE) { Remove-Item Env:\NFPM_PASSPHRASE }
}

$sums = Update-Sha256Sums -Dist $dist
Write-Host ''
Write-Host "sums $sums"
