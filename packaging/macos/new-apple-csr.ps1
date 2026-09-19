<#
.SYNOPSIS
    Produces the private key and the certificate signing request Apple exchanges
    for a Developer ID certificate, on Windows, with no Mac involved.

.DESCRIPTION
    packaging/macos/README.md used to say to create the certificates in Xcode,
    under Settings, Accounts, Manage Certificates. That needs a Mac, and it is
    not the only way: a certificate is issued from a signing request, and a
    signing request is a file. What produced it does not matter to Apple.

    Run it twice, once per certificate. Each run writes the request and seals the
    private key it belongs to:

        pwsh packaging/macos/new-apple-csr.ps1 -Kind application
        pwsh packaging/macos/new-apple-csr.ps1 -Kind installer

    THE BROWSER STEPS, WHICH ARE THE ONLY PART THAT IS NOT HERE

      1. developer.apple.com, Certificates, Identifiers and Profiles, then the
         + beside Certificates.
      2. Choose the flavour:
           Developer ID Application  - signs the four programs and the library
           Developer ID Installer    - signs the .pkg
         Both are needed. A .pkg signed with the Application certificate fails
         notarisation with a message that does not say which one is wrong.
      3. Profile Type: G2 Sub-CA (Xcode 11.4.1 or later).
      4. Upload the .csr this script wrote, download the .cer Apple returns, and
         put it beside the key as developer-id-<kind>.cer. -Certificate does
         that and checks it is the right kind.

    This needs an Apple Developer Program membership, which is $99 a year. There
    is no way to sign for macOS without one, and that was already true when the
    release ran on a Mac.

    THE PRIVATE KEY IS SEALED AND THE PLAIN TEXT COPY IS DELETED

    The key exists in the clear for as long as it takes to write the signing
    request from it, on the RAM disk at R:\, and is then sealed with DPAPI into
    the credential directory. apple-credentials.ps1 unseals it for the seconds a
    signature takes. rcodesign's own documentation warns that a PEM key on a
    file system "is not very secure", and this is the answer to that.

.PARAMETER Kind
    `application` or `installer`.

.PARAMETER Certificate
    The .cer downloaded from Apple. Given this, the script installs and checks
    the certificate instead of writing a new signing request.

.EXAMPLE
    pwsh packaging/macos/new-apple-csr.ps1 -Kind application
    pwsh packaging/macos/new-apple-csr.ps1 -Kind application -Certificate ~\Downloads\developerID_application.cer
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateSet('application', 'installer')]
    [string] $Kind,
    [string] $Certificate
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
. (Join-Path $PSScriptRoot 'apple-credentials.ps1')

$rcodesign = Join-Path $root 'tools/cross/bin/rcodesign.exe'
if (-not (Test-Path -LiteralPath $rcodesign)) {
    throw 'rcodesign is missing. Run: pwsh tools/cross/fetch-toolchain.ps1'
}

$dir = Get-AppleCredentialDir
New-Item -ItemType Directory -Force -Path $dir | Out-Null
$sealedKey = Join-Path $dir "developer-id-$Kind.key.sealed"
$csr = Join-Path $dir "developer-id-$Kind.csr"
$installed = Join-Path $dir "developer-id-$Kind.cer"

if ($Certificate) {
    if (-not (Test-Path -LiteralPath $Certificate)) { throw "$Certificate does not exist" }
    if (-not (Test-Path -LiteralPath $sealedKey)) {
        throw "$sealedKey is missing. A certificate is only usable with the key whose request produced it."
    }
    Copy-Item -LiteralPath $Certificate -Destination $installed -Force

    # Reading the certificate back is what catches the three mistakes that are
    # otherwise found by a failed notarisation hours later: the wrong flavour,
    # a certificate Apple did not issue, and one that pairs with a different key.
    #
    # **The profile name is matched as `analyze-certificate` spells it**, which is
    # CamelCase: `Guessed Certificate Profile: DeveloperIdApplication`. The first
    # version of this checked for `developer-id-application`, which is how
    # `print-signature-info` spells the same thing, so it refused the first real
    # certificate Apple issued and said it was not a Developer ID one.
    $report = & $rcodesign analyze-certificate --certificate-der-file $installed 2>&1
    $expected = if ($Kind -eq 'application') { 'DeveloperIdApplication' } else { 'DeveloperIdInstaller' }
    $profile = ($report | Select-String -Pattern 'Guessed Certificate Profile:\s*(\S+)').Matches.Groups[1].Value
    $fromApple = [bool]($report | Select-String -Pattern 'Signed by Apple\?:\s*true')
    if ($profile -ne $expected -or -not $fromApple) {
        Remove-Item -LiteralPath $installed -Force -Confirm:$false
        throw "that certificate reads as '$profile' (signed by Apple: $fromApple); this is the -Kind $Kind slot, which needs $expected."
    }
    Write-Host "installed $installed"
    $report | Select-String -Pattern 'Subject CN|Team ID|Guessed Certificate Profile|Signed by Apple|Not Valid After' |
        ForEach-Object { "  $($_.ToString().Trim())" }
    Write-Host ''
    Write-Host 'The release now finds it by itself: pwsh packaging/macos/release-macos.ps1'
    return
}

if (Test-Path -LiteralPath $sealedKey) {
    throw "$sealedKey already exists. Apple issues a certificate against the key that made the request, so replace it only when starting over."
}

# The key is generated by rcodesign rather than by openssl so that this needs
# nothing installed beyond the release toolchain. The self-signed certificate it
# writes alongside the key is discarded: only the key half is used, and the
# certificate that matters is the one Apple issues from the request below.
#
# **Discarded means removed before sealing, and that took a bad signature to
# notice.** The first version sealed the whole unified PEM. `rcodesign sign
# --pem-file` then read both halves, and its documented rule is that "all
# remaining certificates are assumed to constitute the CA issuing chain and will
# be added to the signature data" - so every signature carried the throwaway
# certificate next to Apple's, reading back as a second `Developer ID
# Application` with `chains_to_apple_root_ca: false`.
$scratch = Get-AppleScratchDir
$stamp = [guid]::NewGuid().ToString('N')
$keyFile = Join-Path $scratch "new-$Kind-$stamp.pem"
try {
    & $rcodesign generate-self-signed-certificate --algorithm rsa --profile "developer-id-$Kind" `
        --person-name 'inillucent release key' --validity-days 1 --pem-unified-file $keyFile
    if ($LASTEXITCODE -ne 0) { throw "rcodesign could not generate a key ($LASTEXITCODE)" }

    & $rcodesign generate-certificate-signing-request --pem-file $keyFile --csr-pem-file $csr
    if ($LASTEXITCODE -ne 0) { throw "rcodesign could not write the signing request ($LASTEXITCODE)" }

    $unified = Get-Content -Path $keyFile -Raw
    $key = [regex]::Match($unified, '(?s)-----BEGIN (RSA )?PRIVATE KEY-----.*?-----END (RSA )?PRIVATE KEY-----').Value
    if (-not $key) { throw 'rcodesign wrote a PEM with no private key block in it' }
    Protect-AppleSecret -Value ($key + "`n") -Path $sealedKey
} finally {
    if (Test-Path -LiteralPath $keyFile) { Remove-Item -LiteralPath $keyFile -Force -Confirm:$false }
}

$flavour = if ($Kind -eq 'application') { 'Developer ID Application' } else { 'Developer ID Installer' }
Write-Host ''
Write-Host "signing request: $csr"
Write-Host "private key:     $sealedKey   (sealed with DPAPI to this Windows account)"
Write-Host ''
Write-Host 'Next, in a browser:'
Write-Host '  developer.apple.com -> Certificates, Identifiers & Profiles -> Certificates -> +'
Write-Host "  flavour: $flavour"
Write-Host '  profile type: G2 Sub-CA (Xcode 11.4.1 or later)'
Write-Host "  upload $csr, download the .cer, then run:"
Write-Host "    pwsh packaging/macos/new-apple-csr.ps1 -Kind $Kind -Certificate <the .cer>"
