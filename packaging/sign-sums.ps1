<#
.SYNOPSIS
    Signs dist/SHA256SUMS with the project's minisign key.

.DESCRIPTION
    Every installer in packaging/ verifies what it downloaded against
    SHA256SUMS. That protects against a truncated or corrupted transfer, and
    against nothing else: whoever could replace an archive could replace the
    checksum file beside it. A detached signature over SHA256SUMS is what closes
    that, and it is the whole of the Linux trust story, because Linux has no
    Gatekeeper and nothing to notarise.

    minisign rather than GnuPG for this one, because a reader can check it with
    one command and no keyring:

        minisign -Vm SHA256SUMS -P RWQ...    (the public key is on the site)

    The .deb and .rpm are signed separately, with an OpenPGP key, because apt
    and dnf will not read anything else. Two keys, two audiences.

.PARAMETER PublicKey
    The minisign public key, so the script can refuse to sign with the wrong
    secret key. Defaults to packaging/inillucent.pub.

.PARAMETER SecretKey
    The minisign secret key file. Defaults to $env:INILLUCENT_MINISIGN_KEY.

.PARAMETER PassphraseEnv
    The environment variable holding the secret key's passphrase. Default
    INILLUCENT_MINISIGN_PASSPHRASE.

.PARAMETER AllowUnverifiedKey
    Sign without checking the result against a public key. Only for creating the
    project's key pair for the first time, when there is no published key to
    check against yet.

.EXAMPLE
    pwsh packaging/sign-sums.ps1
#>
[CmdletBinding()]
param(
    [string] $PublicKey,
    [string] $SecretKey = $env:INILLUCENT_MINISIGN_KEY,
    [string] $PassphraseEnv = 'INILLUCENT_MINISIGN_PASSPHRASE',
    [switch] $AllowUnverifiedKey
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot

. (Join-Path $PSScriptRoot 'stage-layout.ps1')
$minisign = Join-Path (Get-CrossBin -Root $root) 'minisign.exe'
if (-not (Test-Path -LiteralPath $minisign)) {
    throw 'minisign is missing. Run: pwsh tools/cross/fetch-toolchain.ps1'
}
if (-not $PublicKey) { $PublicKey = Join-Path $PSScriptRoot 'inillucent.pub' }

# The public key is required, not optional.
#
# The check below - does this signature verify against the key readers will
# check it with - used to sit inside `if (Test-Path $PublicKey)`, and
# packaging/inillucent.pub has never existed. So the script signed with whatever
# secret key it was handed, printed `signed`, exited 0, and had verified
# nothing. Every guard in this directory is a refusal with a named override
# rather than a silent skip, and this one is now the same.
if (-not (Test-Path -LiteralPath $PublicKey) -and -not $AllowUnverifiedKey) {
    throw "$PublicKey does not exist, so a signature made here cannot be checked " +
          "against the key a reader would use. Create the pair once with:`n" +
          "  tools/cross/bin/minisign.exe -G -p packaging/inillucent.pub -s <somewhere outside this repository>/inillucent.key`n" +
          'then commit inillucent.pub and keep the secret key off this machine. ' +
          'Pass -AllowUnverifiedKey to sign without that check.'
}

if (-not $SecretKey) {
    throw 'no secret key. Set INILLUCENT_MINISIGN_KEY to the minisign key file, or pass -SecretKey.'
}
if (-not (Test-Path -LiteralPath $SecretKey)) {
    # **The value is described, never printed (task-1995).** This said "$SecretKey does not exist",
    # and INILLUCENT_MINISIGN_KEY is a path that sits one mistake away from being the key itself -
    # set it to the key material and the error writes the project's signing key to the terminal, to
    # the transcript and to any log the release was piped into. It happened. The key was rotated.
    $shape = if ($SecretKey -match 'minisign') { 'the key material itself' } else { "$($SecretKey.Length) characters" }
    throw "INILLUCENT_MINISIGN_KEY does not name a file that exists; it holds $shape. It must be the path to a minisign key file."
}

$sums = Join-Path $root 'dist/SHA256SUMS'
if (-not (Test-Path -LiteralPath $sums)) {
    throw "$sums does not exist. Build a release first."
}
$signature = "$sums.minisig"
if (Test-Path -LiteralPath $signature) { Remove-Item -LiteralPath $signature -Force -Confirm:$false }

$passphrase = [System.Environment]::GetEnvironmentVariable($PassphraseEnv)
if ($null -eq $passphrase) {
    throw "$PassphraseEnv is not set. An empty string is allowed for a key created with -W."
}

# The comment travels inside the signature, so a reader who has the file but not
# the page it came from can still tell what it is.
$comment = "inillucent release, signed on $(Get-Date -Format 'yyyy-MM-dd')"
$passphrase | & $minisign -S -s $SecretKey -m $sums -c $comment -t $comment
if ($LASTEXITCODE -ne 0) { throw "minisign failed with $LASTEXITCODE" }

if (Test-Path -LiteralPath $PublicKey) {
    & $minisign -V -p $PublicKey -m $sums
    if ($LASTEXITCODE -ne 0) { throw "the signature does not verify against $PublicKey" }
    $verified = "verified against $PublicKey"
} else {
    $verified = 'NOT verified - there is no public key, and -AllowUnverifiedKey was passed'
}

Write-Host ''
Write-Host "signed  $signature"
Write-Host "        $verified"
