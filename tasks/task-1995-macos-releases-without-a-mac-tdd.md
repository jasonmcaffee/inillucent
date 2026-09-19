# task-1995 — macOS releases built, signed and notarised on the Windows machine

## Introduction

`packaging/macos/README.md` opens with a sentence this task exists to delete:

> The macOS half of a release is built, signed and notarised on the MacBook and nowhere else.

That was true when it was written. `lipo`, `codesign`, `pkgbuild`, `productbuild`, `notarytool` and
`stapler` are all macOS programs, so the release was split across two machines: the Windows box built
Windows and both Linux architectures and published the site, and a second machine produced the four
macOS files and uploaded them to a GitHub release for the first machine to collect.

Every one of those six programs has a replacement that runs on Windows, and this repository already
carries one of them. `tools/cross/bin/rcodesign.exe` has been in the release toolchain since
task-1895, where it is used to read a signature back out of a Mach-O that arrived from the MacBook.
It also writes them. The same is true up and down the list: zig already links both Apple
architectures here, `rcodesign macho-universal-create` replaces `lipo`, and Apple's notary service is
an HTTPS API that answers a Windows client exactly as it answers a Mac one.

So the second machine is not needed to *produce* a macOS release. It is needed for one thing only,
and this design says so rather than pretending otherwise: **a Mach-O can only be executed on macOS.**
What replaces the MacBook's smoke test is set out in §6.

## The two links on the ticket, and why neither is the answer

The ticket offers two projects as starting points. Both were read. Neither can build this repository,
and the reason is the same in both cases: neither one gives a Windows machine a macOS userland.

**`jprx/darwin-vm`** runs iOS and macOS kernels under QEMU. Its own README says "This is not a full
iPhone/Mac emulator. Don't expect the screen, wifi, bluetooth, graphics, GUI apps, or full springboard
to work." It boots to a root shell for kernel debugging, and its image preparation step requires a Mac
to run `ipsw` against an Apple firmware file. A machine that cannot install Xcode cannot run
`pkgbuild`, and a kernel debugging harness is not a build host.

**`jamesstringer90/appsandbox`** creates desktop virtual machines, and it does run on Windows 11 — but
the macOS guest is offered only when the *host* is an Apple Silicon Mac, because that path is Apple's
own Virtualization.framework. On a Windows host it offers Windows 11 and Ubuntu. It solves a problem
this machine does not have.

There is also a rule underneath both of them. Apple's software licence agreement permits macOS to be
virtualised only on Apple-branded hardware, so a macOS virtual machine on this box would be a licence
violation whichever tool produced it — and building a signed commercial artifact on one would put the
Developer ID that signed it at risk. That is a poor foundation for a release pipeline, independent of
whether the tool works.

**The approach this design takes instead is to not need macOS at all.** Cross-compile the Mach-O,
sign it with a Rust implementation of Apple's code signing format, and notarise it over Apple's HTTPS
API. Nothing in that sequence runs Apple code, so nothing in it needs Apple hardware.

## What the release needs, and what already exists

A macOS release of inillucent is four files. `packaging/macos/release-macos.sh` produces them today on
a Mac, and the table below is the whole of what has to be replaced.

| Step | On a Mac | On Windows | Status before this task |
|---|---|---|---|
| Compile both architectures | `cargo build --target …` | `cargo-zigbuild` with zig as the linker | **already works** — `packaging/release-all.ps1` builds both Apple triples |
| Join them into one universal binary | `lipo -create` | `rcodesign macho-universal-create` | not wired up |
| Sign with the Developer ID, hardened runtime, secure timestamp | `codesign --options runtime --timestamp` | `rcodesign sign --code-signature-flags runtime` | not wired up |
| The `.tar.gz` and `.zip` | `tar`, `ditto` | `stage-layout.ps1`, which already writes both for Windows and Linux | **already works** for other targets |
| The `.pkg` | `pkgbuild` + `productbuild` | nothing existed — §4 | missing |
| Sign the `.pkg` | `productsign` | `rcodesign sign` accepts a XAR archive | not wired up |
| Notarise | `xcrun notarytool submit` | `rcodesign notary-submit` | not wired up |
| Staple the ticket | `xcrun stapler staple` | `rcodesign staple` | not wired up |
| Run the result | the Mac itself | **impossible** | §6 |

The compile half being finished already is worth saying plainly: task-1951 added
`aarch64-apple-darwin` and `x86_64-apple-darwin` to `rust-toolchain.toml`, and
`packaging/release-all.ps1` builds both with zig, including the two C dependencies that `tokenizers`
brings in — `onig` and `esaxx-rs` — which compile through `zig cc` rather than through a platform
toolchain. What that script does at the end of it is print

```
   built, unsigned: target/aarch64-apple-darwin/release
   sign them on a Mac with: packaging/macos/release-macos.sh
```

This task replaces those two lines with the rest of the release.

## Goals and non-goals

**Goals.** Each is checkable on this machine, and each has a script that checks it.

| # | Goal | How it is checked |
|---|---|---|
| G1 | One command on the Windows box produces the whole macOS release | `pwsh packaging/macos/release-macos.ps1 -Version <v>` |
| G2 | The four programs and the C ABI library are universal binaries carrying both architectures | `rcodesign extract macho-header` reports two Mach-O entries; the release script asserts it |
| G2b | The macOS version the binaries claim to need is the one the release states | `Assert-MinimumOs` reads `Minimum OS` out of both slices and fails on a mismatch |
| G3 | Every Mach-O is signed with the Developer ID, the hardened runtime flag and a trusted timestamp | `rcodesign verify`, and `rcodesign extract code-directory` showing the runtime flag |
| G4 | The `.pkg` is built on Windows and is a well formed flat package | `rcodesign` parses its table of contents, rewrites every offset and signs it; Apple's notary service unpacks it |
| G5 | Notarisation and stapling run from Windows | `rcodesign notary-submit --staple`, then `rcodesign staple --help`-style verification of the ticket |
| G6 | No secret is in the repository, on a command line, or in a process argument list | the certificate and the notary key live outside the checkout, sealed by DPAPI to Jason's account |
| G7 | A Developer ID certificate can be obtained without ever using a Mac | `packaging/macos/new-apple-csr.ps1` produces the key and the signing request; the rest is a browser |
| G8 | The archive layout, file names and `SHA256SUMS` are unchanged from the Mac built release | `stage-layout.ps1` is the single source of the layout and is untouched |
| G9 | Nothing about the Windows or Linux halves of the release changes | `packaging/release-all.ps1 -Targets windows` and `-Targets linux` behave exactly as before |

**Non-goals.**

- Running macOS anywhere, in any form, for any step.
- A `.dmg`. The `.pkg` is the double-click artifact and it is the one that can carry a stapled ticket.
- Replacing `packaging/macos/release-macos.sh`. It keeps working on a Mac and is left in place; this
  design adds a second, independent path rather than taking one away.
- App Store distribution, which needs a different certificate and Apple's own upload tooling.

## 1. The compile, and the two things about it that are already decided

`packaging/release-all.ps1` builds both Apple triples with `cargo-zigbuild`, and two details in it are
there because of earlier failures and must survive this change.

**`-Wl,-headerpad_max_install_names`** is passed per Apple target. A Mach-O linked by zig for x86-64
has no `LC_CODE_SIGNATURE` load command and no room to add one, so `rcodesign` refuses it with
"insufficient room to write code signature load command". The arm64 binary is unaffected, because
Apple Silicon requires a signature and zig writes an ad-hoc one. The padding costs nothing and makes
both architectures signable.

**The `embed` feature is on.** Without it the binaries cannot use the ONNX Runtime and weights that
the same program downloaded, and `docs/embeddings.md`'s first example answers `no such function:
embed`. It is what pulls in the two C dependencies, which is why the cross compile has to handle C at
all.

One measurement belongs here, because it turned into a limitation rather than a
decision. The arm64 binary this machine builds reports:

```
Platform: macOS
Minimum OS: 13.0.0
SDK: 15.5.0
```

The SDK version is what Apple's notary service checks, and 15.5.0 satisfies it.
The *minimum OS* is zig's default, and the intention here was to set it to 11.0
for arm64 and 10.13 for x86-64, which is what Rust's own target specifications
use. **That cannot be done through cargo-zigbuild, and this was measured rather
than assumed:**

- zig sets the minimum through its target string. `zig cc -target
  aarch64-macos.11.0-none` produces `Minimum OS: 11.0.0`, checked directly.
- cargo-zigbuild builds that string as `{arch}-macos-none{suffix}`, so a version
  suffix lands after the ABI and zig rejects it with `InvalidAbiVersion`.
- A second `-target` passed through `-C link-arg` does not get there either.
  `cargo-zigbuild zig cc` drops a duplicate `-target` deliberately, with a
  comment in its source saying so.
- `MACOSX_DEPLOYMENT_TARGET` appears nowhere in cargo-zigbuild, and
  `-mmacosx-version-min=11.0` is accepted and ignored by zig.

So the floor is macOS 13.0, which excludes macOS 11 and 12 — an Apple Silicon
Mac that never moved past Big Sur or Monterey will not run these binaries. That
is a first floor rather than a floor that moved: `dist/SHA256SUMS` has never
carried a darwin line, so no macOS release of inillucent has ever been published.

The release **asserts** it rather than assuming it. `Assert-MinimumOs` reads the
value back out of both slices of all five binaries and fails if it is not the
13.0.0 the release states, so a zig upgrade that moves the default stops the
release instead of quietly shipping a different floor. Lowering it is a
follow-up on cargo-zigbuild, not a flag anybody here forgot.

## 2. Universal binaries without `lipo`

`rcodesign macho-universal-create --output <path> <arm64> <x86_64>` writes the same fat Mach-O that
`lipo -create` writes: a fat header, then each architecture's slice aligned to its page size. There is
no Apple code involved on either side — the format is a documented header followed by concatenated
files.

The check that this worked is not "the command exited 0". The release script reads the result back
with `rcodesign extract macho-header --universal-index 0` and `--universal-index 1` and asserts that
one slice is `arm64` and the other is `x86_64`, for all five outputs: the four programs and
`libinillucent_driver_capi.dylib`.

## 3. Signing

```
rcodesign sign \
  --p12-file <developer-id.p12> --p12-password-file <R:\…\pw> \
  --code-signature-flags runtime \
  --timestamp-url http://timestamp.apple.com/ts01 \
  <file>
```

Three parts of that line are load-bearing and each has a failure it prevents.

- **`--code-signature-flags runtime`** is the hardened runtime. Notarisation rejects a submission
  without it, and the rejection names the file rather than the flag.
- **`--timestamp-url`** attaches a trusted timestamp from Apple's timestamp server. Without one the
  signature stops verifying the day the certificate expires, rather than continuing to verify for
  everything signed while it was valid. `rcodesign` uses Apple's server by default; naming it makes
  the dependency visible.
- **Signing happens before packaging, never after.** This is carried over verbatim from the Mac
  script, where the comment reads: a package assembled from unsigned binaries and then signed passes
  `spctl` on the package and fails on first run.

`rcodesign` signs a universal binary by signing each slice inside it, which is what `codesign` does
too.

## 4. The `.pkg`, which is the only part with no existing tool

`pkgbuild` and `productbuild` have no Windows equivalent, and unlike the rest of this list there is no
Rust program that already does the job. A flat package is, though, an entirely documented format, and
two of its three hard parts already have a Rust implementation in the same crate family as
`rcodesign`.

A product archive is a XAR archive containing:

```
Distribution                          an XML description of the install
inillucent-<version>.pkg/PackageInfo  identifier, version, install location, file count
inillucent-<version>.pkg/Payload      a gzipped cpio archive of the files
inillucent-<version>.pkg/Bom          the bill of materials the installer records the receipt from
```

- **`Bom`** is the part with a format nobody would want to re-derive, and the plan was to call
  `apple-bom`'s `BomBuilder` for it. **That turned out not to work, and it cannot ever have worked.**
  `build_bom` writes the root record as `CString::new(b".\0")`, and `CString::new` rejects a string
  that already ends in a NUL, so the first call panics before a byte is produced. Fixing that reveals
  a second fault — it stores the whole path in each `BomBlockFile`, while the reader in the same
  crate rebuilds a path by walking `parent_path_id` and joining names, so every path would come back
  doubled — and a third, where each variable's block index is written one past the block it names, so
  `Paths` resolves to a paths block rather than to the tree the reader looks for. The crate's own
  documentation says "writing support is still a work in progress", and that is what it means.

  So `tools/macos-pkg/src/bom.rs` does the block assembly, adapted from that builder with the three
  faults fixed and attributed in its header. What it keeps is `apple_bom::format`, the binary layout
  of each block type, which is the crate's *read* path and is the half that has been run against BOMs
  Apple produced. The test reads every BOM the writer produces back with `apple_bom::ParsedBom` and
  compares the paths, the modes, the sizes and the CRC-32s to what went in.
- **`Payload`** is a cpio archive in the `odc` format, gzipped. A writer for it is about sixty lines
  and the format is ten octal fields and a name per entry.
- **`PackageInfo` and `Distribution`** are small XML documents whose shape is fixed by what the
  existing `packaging/macos/Distribution.xml` already says.
- **The XAR container** is a 28 byte header, a zlib compressed XML table of contents, and a heap of
  zlib compressed file data. `apple-xar` reads that format and does not write it, so the writer is new
  code.

**What reads the result back.** Not `apple-xar` as a test dependency: it reaches `ring` through its
signing support, and `ring` compiles C that needs the Windows SDK headers on the include path, so
`cargo test` would pass or fail depending on whether the shell had been through `vcvars64.bat`. The
independent reader is `rcodesign`, run as a program by the release script. It parses the table of
contents, rewrites every entry's offset and signs the archive, which exercises more of the format than
reading it would, and a package the writer produces that `rcodesign` cannot read fails the release on
the machine that built it.

**The modes are decided, not read.** NTFS has no executable bit, so a payload built from what the file
system says would install four programs that cannot be run. The caller names the directories whose
contents are executable — the same rule `stage-layout.ps1` applies when it writes the `.tar.gz`.

Signing the result needs no new work: `rcodesign sign` states that it signs "A XAR archive (commonly
a .pkg installer file)", and the Developer ID Installer certificate is the identity it is given.

## 5. Notarisation, from Windows

Apple's notary service is an HTTPS API in front of S3. `rcodesign notary-submit` uploads the asset,
polls for the result, and with `--staple` writes the returned ticket into the container.

It authenticates with an **App Store Connect API key**, not with an Apple ID and an app-specific
password. The key is three values — an issuer ID, a key ID, and an ECDSA private key downloaded once
as a `.p8` file — created at appstoreconnect.apple.com under Users and Access → Integrations. This is
better than the keychain profile the Mac script uses, for reasons that have nothing to do with the
operating system: it is revocable on its own, it is scoped to one role, and it does not carry the
password to Jason's Apple ID.

`rcodesign encode-app-store-connect-api-key` folds the three into one JSON file, which is what the
release script passes with `--api-key-file`.

**What gets submitted, and what gets stapled.** The `.pkg` is submitted and stapled, so it installs on
a machine with no network. The `.zip` is submitted so that the notary registers the cdhash of every
binary inside it, and it is not stapled, because no container outside `.pkg`, `.dmg` and `.app` can
carry a ticket. That is the same split the Mac script makes.

## 6. The one thing Windows cannot do, and what stands in for it

`packaging/macos/verify-macos.sh` is the current release gate. It quarantines the published archive
the way a browser would, asks `spctl` whether Gatekeeper accepts it, verifies every signature, runs a
database round trip, asks the MCP server for its tool list, and runs the x86-64 slice under Rosetta.

Four of those seven checks need to execute a Mach-O, and **no amount of tooling makes that possible on
Windows.** Saying so is the design rather than a gap in it. What replaces them is three things that
are genuinely checkable here, and one that is checkable by somebody else.

1. **Everything structural, locally.** Both architectures present; the hardened runtime flag set; the
   signature verifying against the certificate that made it; the certificate chaining to an Apple root
   and carrying the Developer ID Application profile — `fetch-macos-artifacts.ps1` already reads
   exactly these out of a Mach-O with `rcodesign`, and it is reused rather than rewritten.
2. **Apple's notary service, which is a real external check.** It unpacks the submission, walks every
   Mach-O inside it, and rejects the submission for an unsigned binary, a missing hardened runtime, a
   missing secure timestamp, an SDK that is too old, or a package it cannot parse. An `Accepted`
   result is a statement by Apple about the exact bytes that were submitted. It is the closest thing
   to a Gatekeeper run that exists off a Mac, and the release refuses to publish without it.
3. **The Linux and Windows binaries are built from the same commit and are smoke tested here.** A
   fault in the program rather than in the packaging shows up there.
4. **A person with any Mac can run the published gate**, unchanged, against the published bytes:
   `curl -fsSL https://inillucent.com/downloads/verify-macos.sh | sh -s -- --version <v>`. It is a
   one-line check on borrowed hardware and it stays in the repository for exactly that.

The release script prints, at the end, which of the seven checks it ran and which four it could not,
so that a release cut on Windows never reads as though it had been through the Mac gate.

## 7. Credentials, and getting them without a Mac

This is the only part that needs Jason, and it needs him once.

**The Apple Developer Program membership** is $99 a year and there is no way to sign for macOS
without it. That was already true in task-1895; nothing here changes it.

**The certificates come from a browser, not from Xcode.** The README currently says to create them in
Xcode → Settings → Accounts → Manage Certificates, which needs a Mac. The certificate is issued from a
certificate signing request, and a signing request is a file — it does not matter what made it:

```powershell
pwsh packaging/macos/new-apple-csr.ps1 -Name developer-id-application
```

writes an RSA private key and a `.csr` beside it, using `rcodesign
generate-certificate-signing-request`. At developer.apple.com → Certificates → **+**, the flavour is
**Developer ID Application**, the profile type is **G2 Sub-CA (Xcode 11.4.1 or later)**, the `.csr` is
uploaded, and a `.cer` comes back. The script then combines the `.cer` with the private key it kept
into a `.p12`. The same sequence, run a second time with **Developer ID Installer**, produces the
identity that signs the `.pkg`.

**Where they live.** Not in the repository, not in an environment variable that a child process
inherits, and not on a command line. `%LOCALAPPDATA%\inillucent\apple\` holds the two `.p12` files and
the notary key JSON, and the `.p12` passwords are stored with PowerShell's `ConvertFrom-SecureString`,
which seals them with DPAPI to Jason's Windows account — the same mechanism ai-service's secret store
uses, and worthless on any other machine or account. At signing time the password is written to the
RAM disk `R:`, passed to `rcodesign` as `--p12-password-file`, and deleted in a `finally`.

**Until the certificates exist**, `-SelfSigned` signs with a certificate `rcodesign
generate-self-signed-certificate` makes on the spot. That proves every step of the pipeline except the
two that are about Apple's opinion of the certificate: notarisation refuses it, and Gatekeeper would
refuse it. It is how this task is verified before the membership exists, and it is not a release mode
— the script refuses to write `dist/` artifacts under `-SelfSigned` without `-Unpublishable`, which
names them so they cannot be mistaken for a release.

## 8. The files

```
packaging/macos/release-macos.ps1     the whole macOS release on Windows, one command
packaging/macos/new-apple-csr.ps1     private key + signing request + .p12 assembly, no Mac
packaging/macos/apple-credentials.ps1 resolving the certificate and the notary key, sealed
tools/macos-pkg/                      the flat package writer: cpio, BOM, XAR
packaging/release-all.ps1             calls the above instead of printing "sign them on a Mac"
packaging/macos/README.md             rewritten: the Windows path first, the Mac path kept
tools/cross/fetch-toolchain.ps1       unchanged — rcodesign 0.29.0 is already pinned in it
```

## 9. Verification plan

Run in this order, because each step's failure mode is cheapest to read on its own.

1. `pwsh packaging/release-all.ps1 -Targets macos` — both architectures compile.
2. `rcodesign extract macho-target` on each — platform macOS, and the minimum OS §1 measured.
3. `macho-universal-create`, then `extract macho-header --universal-index 0|1` — both slices present.
4. `rcodesign sign` with a self-signed certificate, then `rcodesign verify` — a signature that reads
   back, with the hardened runtime flag set.
5. `tools/macos-pkg` builds the `.pkg`; `apple-flat-package`'s reader parses it; `rcodesign sign`
   signs it and `rcodesign print-signature-info` reads the signature back.
6. The archives, and `SHA256SUMS-macos`, byte-for-byte in the layout `stage-layout.ps1` defines.
7. With real credentials: `rcodesign notary-submit --staple` on the `.pkg`, and on the `.zip` without
   `--staple`. An `Accepted` from Apple is the gate.

Steps 1 to 6 need nothing from Apple and are what this task is verified by. Step 7 needs the
membership and the certificates, and is the one part that waits on §7.

## 10. unluminous

The same toolchain applies to `C:\jason\dev\unluminous` and the ticket asks for both. Its shape is
different in one way that matters: it is a desktop application rather than four command line programs,
so the artifact is an `.app` bundle rather than loose binaries, and `rcodesign sign` signs a bundle
directly — it recurses into nested Mach-O files and writes the `_CodeSignature/CodeResources` that a
bundle needs. That is a capability `rcodesign` has and this repository does not exercise. The pieces
it shares with inillucent — the cross compile, the universal binary, the notary key, the credential
storage — are the same ones, which is why inillucent is done first.
