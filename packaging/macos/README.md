# macOS packaging — the whole release, on the Windows machine

Until task-1995 this file opened by saying the macOS half of a release was built,
signed and notarised on the MacBook and nowhere else. That is no longer true.
Every macOS program the release used has a replacement that runs on Windows:

| Apple's program | What runs here instead |
|---|---|
| `lipo` | `rcodesign macho-universal-create` |
| `codesign` | `rcodesign sign` |
| `pkgbuild` | `tools/macos-pkg` |
| `productbuild` | `tools/macos-pkg`, which writes the product archive directly |
| `productsign` | `rcodesign sign`, which signs a XAR archive |
| `notarytool` | `rcodesign notary-submit` |
| `stapler` | `rcodesign staple` |

**One command does all of it:**

```powershell
pwsh packaging/macos/release-macos.ps1
```

It builds both architectures with zig, joins them into universal binaries, signs
them with the hardened runtime and a trusted timestamp, writes the `.tar.gz` that
`install.sh` and Homebrew fetch and the `.zip` that Apple's notary accepts,
builds the `.pkg` from the binaries it just signed, notarises both containers and
staples the ticket to the `.pkg`.

`packaging/release-all.ps1` calls it, so the ordinary release is still one
command for every target at once.

## The one thing that cannot happen here

**A Mach-O only executes on macOS.** `verify-macos.sh` has seven checks and four
of them run the binary: `spctl` on a quarantined copy, a database round trip, the
MCP server's tool list, and the x86-64 slice under Rosetta. No tooling changes
that, and the release script prints which checks it ran and which four it could
not, every time, so a release cut here is never mistaken for one that went
through the Mac gate.

Two things stand in for them.

**Apple's notary service is a real external check.** It unpacks the submission,
walks every Mach-O inside it, and rejects an unsigned binary, a missing hardened
runtime, a missing secure timestamp, an SDK that is too old, or a package it
cannot parse. An `Accepted` is a statement by Apple about the exact bytes that
were submitted, and the release does not publish without one.

**`verify-macos.sh` still exists and still runs against the published bytes.** On
any Mac that can be borrowed:

```sh
curl -fsSL https://inillucent.com/downloads/verify-macos.sh | sh -s -- --version 0.1.4
```

## The minimum macOS version, which is 13.0 and cannot currently be lowered

The binaries declare `Minimum OS: 13.0.0`. That is zig's default rather than a
choice, and it was measured rather than assumed:

- zig sets the minimum through its own target string. `zig cc -target
  aarch64-macos.11.0-none` produces `Minimum OS: 11.0.0`.
- cargo-zigbuild builds that string as `{arch}-macos-none{suffix}`, which puts a
  version after the ABI, and zig rejects it with `InvalidAbiVersion`.
- A second `-target` passed as a link argument does not reach zig either:
  `cargo-zigbuild zig cc` drops a duplicate `-target` deliberately.
- `MACOSX_DEPLOYMENT_TARGET` appears nowhere in cargo-zigbuild, and
  `-mmacosx-version-min` is accepted and ignored by zig.

So macOS 11 and 12 are excluded, which matters for an Apple Silicon Mac that
never moved past Big Sur or Monterey. This is a first floor rather than a floor
that moved — no macOS release of inillucent has been published before. The
release asserts the value rather than trusting it, so a zig upgrade that changes
the default stops the release instead of shipping a different floor quietly.

---

## The one-time setup

Three things, none of which is in this repository and none of which ever should
be. `release-macos.ps1` refuses to run until they exist and says which is
missing.

### 1. The Rust targets

`rust-toolchain.toml` names them, so `rustup` installs them on a fresh machine.
Nothing to do by hand.

### 2. The two certificates — $99/year, and no Mac

Enrol at <https://developer.apple.com/programs/> ($99/year, needs an Apple ID
with two-factor on). There is no way to sign for macOS without a membership.

The certificates come from a browser, not from Xcode. A certificate is issued
from a signing request, and a signing request is a file — what produced it does
not matter to Apple:

```powershell
pwsh packaging/macos/new-apple-csr.ps1 -Kind application
pwsh packaging/macos/new-apple-csr.ps1 -Kind installer
```

Each run writes a `.csr` into `%LOCALAPPDATA%\inillucent\apple\` and seals the
private key it belongs to with DPAPI. Then, at developer.apple.com →
Certificates, Identifiers & Profiles → Certificates → **+**:

- the flavour is **Developer ID Application** for the programs and the library,
  and **Developer ID Installer** for the `.pkg`. Both are needed; a `.pkg` signed
  with the Application certificate fails notarisation with a message that does
  not say which one is wrong;
- the profile type is **G2 Sub-CA (Xcode 11.4.1 or later)**;
- upload the `.csr`, download the `.cer`, and install it:

```powershell
pwsh packaging/macos/new-apple-csr.ps1 -Kind application -Certificate <the .cer>
```

which checks it is the right flavour before accepting it.

A `.p12` exported from a Mac's keychain works too: put it in the same directory
as `developer-id-<kind>.p12` and seal its password once with

```powershell
pwsh -c ". packaging/macos/apple-credentials.ps1; Set-AppleP12Password -Kind application"
```

### 3. An App Store Connect API key, for notarisation

At <https://appstoreconnect.apple.com> → Users and Access → Integrations, create
a key with the **Developer** role. That gives an issuer ID, a key ID, and a `.p8`
file downloadable once. Fold them into one file, seal it, and delete both plain
copies — the `.p8` is an ECDSA private key and there is no reason for it to sit
on a disk in the clear:

```powershell
. packaging\macos\apple-credentials.ps1
$scratch = Get-AppleScratchDir           # the RAM disk
$json = Join-Path $scratch 'notary-key.json'
tools\cross\bin\rcodesign.exe encode-app-store-connect-api-key -o $json `
  <issuer-id> <key-id> <path to AuthKey_<key-id>.p8>
Protect-AppleSecret -Value (Get-Content $json -Raw) `
  -Path (Join-Path (Get-AppleCredentialDir) 'notary-key.json.sealed')
Remove-Item $json, <the .p8> -Force
```

`New-AppleNotarySession` unseals it onto the RAM disk for the length of a
submission and the release deletes it in a `finally`. A plain `notary-key.json`
is still accepted, with a warning saying to seal it.

An App Store Connect key is used rather than an Apple ID and an app-specific
password because it is revocable on its own and does not carry the password to
the Apple ID itself.

**To check the credentials without submitting anything**, ask Apple to list what
this key has sent before. It is a read-only call and it is the cheapest proof
that the issuer id, the key id and the `.p8` are a working set:

```powershell
. packaging\macos\apple-credentials.ps1
$session = New-AppleNotarySession
tools\cross\bin\rcodesign.exe notary-list --api-key-file $session.Path
Remove-AppleNotarySession -Session $session
```

### Where the secrets live, and why that is enough

`%LOCALAPPDATA%\inillucent\apple\` holds them, and `$env:INILLUCENT_APPLE_DIR`
overrides the location. The private key and any `.p12` password are sealed with
`ConvertFrom-SecureString`, which encrypts under the current Windows account, so
what is on disk is worthless on another machine or to another user and there is
no key of its own to keep somewhere else. `rcodesign` reads a key from a file, so
for the seconds a signature takes the key is a file — written to the RAM disk at
`R:\`, which is memory, and deleted in a `finally`.

Nothing secret is in this repository, on a command line, or in an environment
variable a child process inherits.

---

## Proving the pipeline before the certificates exist

```powershell
pwsh packaging/macos/release-macos.ps1 -SelfSigned -SkipNotarize -Unpublishable
```

signs with a certificate generated on the spot. That exercises every step except
the two that are about Apple's opinion of the certificate: notarisation refuses
it and Gatekeeper would refuse it. The artifacts go to `dist/unpublishable/`
rather than `dist/`, because they are bit for bit what a release looks like apart
from who signed them, and that is how an unsignable build gets published by
accident.

---

## The Mac path, which still works

`release-macos.sh` produces the same artifacts on a Mac and is unchanged.
`build-pkg.sh` and `notarize.sh` predate it and build and notarise the `.pkg`
alone. `packaging/fetch-macos-artifacts.ps1` still collects and verifies what a
Mac produced. A machine with a Mac available loses nothing; a machine without one
is no longer stopped.

The Mac route needs what it always needed: the two certificates in the login
keychain, and a `notarytool` credential profile called `inillucent-notary`.

---

## What the `.pkg` is, and where it comes from

A product archive is a XAR archive holding four things:

```
Distribution                          an XML description of the install
Resources/                            the welcome, conclusion and licence text
inillucent-component-<version>.pkg/PackageInfo
inillucent-component-<version>.pkg/Payload   a gzipped cpio archive of the files
inillucent-component-<version>.pkg/Bom       the list the receipt is recorded from
```

`tools/macos-pkg` writes all of it. Its `README` in the source header says why
each part is written by hand rather than called, including the three faults in
`apple-bom`'s own builder that make it unusable as published.

`rcodesign` is the independent reader: it parses the table of contents, rewrites
every entry's offset and signs the archive, so a package the writer got wrong
fails on the machine that built it rather than on somebody's Mac.

**Neither `rcodesign` nor Apple's notary opens the `Bom`.** The 0.1.8 package
was signed, notarised and published with a `Bom` that stopped at the end of its
block table, with no free list after it. Installer.app opens the `Bom` before
it installs anything, and on every Mac it aborted in `_ReadFreeList` with
`EXC_CRASH (SIGABRT)`. The tests in `tools/macos-pkg/src/bom.rs` now check the
layout macOS reads, compared against a `Bom` Apple's tools wrote:
`cargo test --manifest-path tools/macos-pkg/Cargo.toml`. A change to the writer
ships through `packaging/ship.ps1` like any other fix, since this machine builds,
signs and notarises the `.pkg`. The one thing it cannot do is run Installer.app.

## A note on where it installs

`/usr/local`, which is the conventional place for a package that is not from
Apple and is what Homebrew uses on Intel. It needs the administrator password
once, at install time, which a `.pkg` asks for anyway. `install.sh` does not
touch `/usr/local` at all — it stays inside `$HOME` — and the two can coexist,
though `PATH` order then decides which `inillucent` a shell finds. `which -a
inillucent` is how somebody finds that out.
