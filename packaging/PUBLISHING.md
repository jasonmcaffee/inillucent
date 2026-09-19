# Publishing: the six routes, what each one needs, and the order

Everything in this repository is built, tested and verified up to the upload.
This file is the last mile: which registries inillucent ships through, how an
archive is built and checked before it goes anywhere, and what a maintainer who
holds the accounts runs for each route.

Nothing here needs a credential to read. The commands that need one say so, and
the account itself belongs to a person rather than to this repository, so the
last step of every route is a person at a keyboard.

---

## Where it stands, at a glance

Updated 2026-09-14, at **0.1.2**. Both one-line installers work, verified by
running them as written against the live site: Windows, and Ubuntu 24.04. Three
archives are published - Windows, Linux x86-64 and Linux aarch64.

**Checked against the bytes the site is serving, on 2026-09-14 (task-1951).** All
four published files were downloaded off `inillucent.com` and hashed with
`sha256sum` rather than `Get-FileHash`, which is the one check a vanished cmdlet
cannot fake. All four match the published `SHA256SUMS`, and all four are
byte-identical to `dist/` on the Windows box. `0.1.1` is still served, `0.1.0`
answers 404 as intended, and both macOS names answer 404 because there is no
macOS archive.

**What 0.1.2 is for.** `inillucent setup-embeddings all` downloads 620 MB of ONNX
Runtime and weights, and every archive up to 0.1.1 was built without
`--features inillucent-cli/embed`, so the program that downloaded them answered
`no such function: embed`. task-1900 put the flag in the release scripts;
0.1.1 was cut before that, so cutting 0.1.2 was the whole of the fix. The
published Windows and Linux archives both answer
`SELECT length(embed('hello'))` with `3072`, the Linux one after
`setup-embeddings all` on a machine that had never run it.

| route | readiness | what it is waiting on |
|---|---|---|
| **inillucent.com, Windows** | **live at 0.1.2** - `irm .../install.ps1 \| iex` installs and runs | nothing |
| **inillucent.com, Linux x86-64** | **live at 0.1.2** - `curl -fsSL .../install.sh \| sh` installs and runs | nothing |
| **inillucent.com, Linux aarch64** | **published at 0.1.2**, and **never run** - there is no ARM machine here. `tools/release-verify-linux.sh` reads its glibc floor, its shared libraries and its modes out of the archive and passes; L4 and L5 skip themselves | a machine that can run it |
| **inillucent.com, macOS** | **no archive, at 0.1.1 or 0.1.2** - and it cannot be produced on this machine, nor by anything else: `task-1968` removed the GitHub workflows and there is no CI at all now. See [the macOS archive](#the-macos-archive-the-one-thing-that-needs-a-different-machine) | **a Mac, an Apple Developer Program membership, two Developer ID certificates and a stored `notarytool` profile.** Then one command on it: `./packaging/macos/release-macos.sh --version 0.1.2 --upload` |
| **SHA256SUMS signature** | not signed, at 0.1.1 or 0.1.2. The signing path itself is verified: run with a throwaway key it signs, the signature verifies, and a different public key is rejected on the key id | **a minisign key pair**, created once. `packaging/sign-sums.ps1` reads the secret key from `INILLUCENT_MINISIGN_KEY`, and `packaging/inillucent.pub` has to exist before it will sign at all |
| **the .deb and the .rpm** | **not published, and the packaging is verified** - both were built from the published 0.1.2 Linux archive, the `.deb` was extracted in WSL and the program it carries wrote, reopened and read a database, and the `.rpm` header lists the same nine paths with the same modes | an OpenPGP key. `packaging/linux/package-linux.ps1` signs by default because `apt` and `dnf` will not install an unsigned package from outside a distribution's own repository. `gpg` is on the box with an empty keyring |
| **GitHub release** | **cut for 0.1.2** on 2026-09-14, on the `v0.1.2` tag whose tree is the released tree, with all five assets. Every one was downloaded back off the release and hashed: all five match the published `SHA256SUMS` and `dist/` byte for byte. **Visible to everybody** since task-1961 made both repositories public: `tools/check-public-urls.mjs` fetches every URL a shipped package names with no credential and all nine answer 200 | nothing |
| **Go** | **published**. `packages/go/v0.1.2` is pushed and `proxy.golang.org` serves the module to a caller with no credential: `/@latest` and `/@v/list` both answer 200, checked by `tools/check-public-urls.mjs` in `tools/validate`. See [the Go route](#the-go-route-verified-through-the-module-proxy) for what was and was not run | nothing |
| **npm** | **token is the only step** - three tarballs packed, installed and run | an account token |
| **PyPI** | **token is the only step** - wheel installed into a clean venv and run | an account with 2FA, and a token minted from it |
| **crates.io** | **token is the only step** - `--workspace --dry-run` clean for every publishable crate. The source is public as of task-1961 | a token |
| **Packagist** | **one step** - `composer install` works end to end, the package itself is ready, and the repository the submit form has to read is public now | a Packagist account (GitHub OAuth) |
| **Homebrew** | **waiting on the macOS archive, and on nothing else.** `update.sh` fills both Linux checksums from `dist/SHA256SUMS` correctly and reports the macOS one as missing, which was run to check | the macOS archive; then the tap repository. `brew install --HEAD` additionally needs the repository to be public, because the formula's `head` spec clones it |

Read across that table one more way, because it is the part that decides what to
chase:

- **npm, PyPI and crates.io are finished except for the upload.** The artifact is
  on disk, a clean machine installed from that exact artifact, and the installed
  thing ran. `npm publish`, `twine upload` and `cargo publish` are the only
  commands left. Each is written out below.
- **Packagist needs an account and nothing else.** The submit form reads the
  repository, which is public now.
- **Homebrew is not waiting on a credential at all** - a formula is a pull
  request and needs no account. It is waiting on the macOS archive, which needs
  a Mac. A token would change nothing.
- **Go is published.** `go install` resolves through `proxy.golang.org`, which
  clones the repository with no credential; it answers 200 for both `@latest`
  and `@v/list` now that the repository is public.

The macOS archive is the single artifact that blocks the most: the macOS
installer, the Homebrew formula, and two of the four npm platform packages all
wait on it and on nothing else.

---

## The rule the release scripts are built around

**Run every command the release ships, from the release.** 0.1.0 passed every
check that was run against the repository and still shipped four commands that
fail, because the checks were run against the repository and the commands ship
inside the archive. Fixing the source tree does not change the artifact.

That rule is why `packaging/release.ps1` extracts what it just built and drives
it, and it is what found each of these.

### What running the artifact found

- **SHA256SUMS was written with CRLF.** `Set-Content` uses the platform's line
  ending, so a file produced on Windows ends every line with `\r\n`. Linux `awk`
  keeps that carriage return in `$NF`, so `curl -fsSL .../install.sh | sh` on
  Ubuntu answered *"inillucent 0.1.1 has no build for x86_64-unknown-linux-gnu
  yet"* and then listed `inillucent-0.1.1-x86_64-unknown-linux-gnu.tar.gz` as
  published, on the next line. Windows `awk` opens files in text mode and drops
  the `\r`, which is why every run on the machine that produced the file passed.
  `release.ps1` writes LF now, and `install.sh` strips carriage returns before
  reading, in both the download path and `--from-dist`.
- **A clean checkout on Windows turns every shell script into CRLF**, when
  `core.autocrlf` is true and the repository carries no `.gitattributes`.
  `packaging/publish-site.ps1` publishes `packaging/install.sh` and
  `packaging/macos/verify-macos.sh` out of the checkout, and `sh` on a CRLF
  script dies on its first line. `packaging/install.sh` escaped by accident - it
  carries a literal carriage return inside a `tr -d` argument, and git leaves
  such a file alone - so the one script whose CRLF would have been noticed at
  once is the one that was never converted. `.gitattributes` pins `*.sh` to LF.
- **`install.sh` used `set -o pipefail` and `${BASH_SOURCE[0]}`**, both bash-only.
  The documented command pipes into `sh`, which on Debian and Ubuntu is dash, and
  dash answered *"Illegal option -o pipefail"* and stopped before downloading
  anything. The script is POSIX now and `dash -n` parses it clean.
- **`.ps1` was served as `application/octet-stream`**, because `mime_guess` has
  no entry for it. Against that, `Invoke-RestMethod` hands back a byte array
  rather than a script, so `irm ... | iex` died with *"[System.Byte] does not
  contain a method named 'Trim'"*. Fixed in the site's static handler.
- **`curl -fsSL` on an archive that is not published exits non-zero with no
  output**, and `set -e` then ended the installer in silence. It reads
  SHA256SUMS first now and names the platforms that were published.
- **The Go row named a command that had been renamed.** `cmd/inillucent` became
  `cmd/inillucent-install`, and the archives still said the old one, so
  `@latest` resolved to a module where that directory does not exist.

### What cutting 0.1.2 found in the gate itself

None of these is reachable by building the workspace (task-1934):

- **Five checks spoke a handshake `inillucent-mcp` no longer accepts.** It
  enforces the MCP lifecycle now - `initialize` needs `protocolVersion`,
  `capabilities` and `clientInfo`, and every other method answers -32002 until
  `notifications/initialized` arrives. `packaging/release.ps1`'s smoke test
  **refused to build the archive at all**; `packaging/release.sh`,
  `tools/release-verify-linux.sh` L5, `packaging/macos/verify-macos.sh` A5 and
  the Homebrew formula's `test do` block all failed the same way.
  `tools/doc-facts/check.mjs` already sent the whole handshake, which is how the
  five were spotted.
- **Two of those read the answer through `grep -q`**, which stops at its first
  match and closes the pipe; the server's next write then fails, it exits
  non-zero, and `set -o pipefail` reported a pipeline that had answered
  correctly as failed. Both read into a variable now.
- **No release had ever carried an ARM Linux archive**, because
  `rust-toolchain.toml` named only the two x86-64 targets and cargo answered
  `can't find crate for 'core'`. zig supplies the linker and the glibc floor;
  the standard library for the target still has to be installed.
  `rust-toolchain.toml` names it now.

### What running the rest of the packaging found (task-1951)

None of these is reachable by reading the scripts. Each came from running one.

- **`Update-Sha256Sums` dropped the `provenance.json` line.** `release.ps1` and `release.sh` both
  write the archives into `SHA256SUMS` and then append the provenance's hash; the shared helper in
  `packaging/stage-layout.ps1` rewrote the file over the archives alone. So `release-all.ps1` and
  `linux/package-linux.ps1` each turn a four line `SHA256SUMS` into a three line one, and
  `publish-site.ps1` copies that file to the site verbatim. Its own comment says `provenance.json`
  is published *because* `SHA256SUMS` names it, so the result is a provenance served with nothing
  stating its hash. Measured: `release-all.ps1 -Targets macos` rewrote a file that matched the
  published one into one that did not. The helper writes the provenance line now, last, which is
  where the release scripts put it.
- **`sign-sums.ps1` signed with any key and reported success.** Its check that the signature verifies
  against the key readers would use sat inside `if (Test-Path packaging/inillucent.pub)`, and that
  file has never existed, so the check never ran. It is a refusal with a named override now,
  `-AllowUnverifiedKey`, like every other refusal in this directory. The signing path itself works:
  run with a throwaway key it signs, the signature verifies, and a different public key is rejected
  on the key id.
- **`homebrew/update.sh` printed the push command above the reason not to push.** With an archive
  missing it wrote the formula, printed `Then, in the tap: git add ... && git push`, and then warned
  that the file still carried `REPLACE_WITH_THE_UNIVERSAL_DARWIN_SHA256`. The push line is printed
  only when the formula is complete now.
- **`rust-toolchain.toml` named three targets out of five**, so `release-all.ps1` stopped at its
  macOS step with `can't find crate for std`. The macOS section above has it.
- **`fetch-macos-artifacts.ps1` reported success having checked nothing, and published what it
  refused.** It is the gate between the macOS artifacts and the site, and on a machine without
  `rcodesign` the committed version printed a warning, said *"every check passed"*, exited 0, and
  wrote the artifacts into `dist/SHA256SUMS`. Run against three text files reading
  `this is not a Mach-O`, that is exactly what it did. Four faults in one script:
  a missing `rcodesign` was a warning rather than a refusal, so the only check standing between an
  unsigned Mach-O and the site could be absent; `Update-Sha256Sums` ran **before** the failure gate,
  so a run ending with *"these artifacts must not be published"* had already listed them as
  published; an empty `SHA256SUMS-macos` verified nothing and reported no failure, which is what a
  `shasum` missing from the Mac's PATH produces; and `tar --force-local` is GNU tar only, so the
  step died on Windows' own bsdtar with a usage dump. All four are fixed, the refusal has
  `-AllowUnverifiedSignatures` as its named override, and the committed and fixed scripts were run
  side by side in isolated trees to show the difference.
- **The `.deb` and the `.rpm` were recorded as not built and are one command away.** Both were built
  from the published 0.1.2 Linux archive with the shipped template and the `nfpm` already in
  `tools/cross/bin`. The `.deb` was extracted in WSL and the program it carries created a database,
  wrote to it, reopened it and read the rows back; the `.rpm`'s header lists the same nine paths with
  the same modes. Signing is the only step left, and the modes the template sets survive being built
  on a filesystem with no execute bit, which is what its own comment says went wrong the first time.

### One more thing that can fail silently

A terminal can inherit a `PSModulePath` in which the PowerShell 7 module
directories shadow `Microsoft.PowerShell.Utility`. Cmdlets from it -
`Get-FileHash` among them - resolve to nothing, the script keeps going, and it
exits 0. Applied to `release.ps1` that produces a SHA256SUMS that looks written
and is empty.

`release.ps1` and `publish-site.ps1` now check for the cmdlets they need before
doing anything, and stop with the reason rather than succeeding at nothing.

**The acceptance test for staging is not the script's exit code.** It is
downloading what the site serves and hashing it with a tool that is not
PowerShell, then comparing that against the published SHA256SUMS. That is the
only check a vanished cmdlet cannot fake, and it is what was run for 0.1.1:
`sha256sum` over the three served files returns exactly the three values in the
served SHA256SUMS.

### 0.1.0 was withdrawn, not patched

Its archives carried `README.md`, `docs/getting-started.md` and
`agent-skills/inillucent-quickstart/SKILL.md` from before the Go command was
renamed, so all three told a reader to run
`go install .../packages/go/cmd/inillucent@latest` - and `@latest` resolves to a
module where that directory no longer exists. The archive the site handed out
contained an install command that failed.

Replacing those archives in place would have left two different archives both
called 0.1.0, and the Homebrew formula already recorded the 0.1.0 Linux
checksum, which would have stopped matching with nothing to say so. The 0.1.0
archives are removed from the site and answer 404, and the GitHub release is
marked *"inillucent 0.1.0 (withdrawn - use 0.1.1)"* with the assets left
attached, because deleting them would remove the record of what was published.

---

## The macOS archive: the one thing that needs a different machine

Added 2026-09-14 (task-1951).

There is no macOS archive at 0.1.1 or at 0.1.2, and it is the single artifact that blocks the most:
the macOS download on inillucent.com, the Homebrew formula, and two of the four npm platform
packages all wait on it and on nothing else.

### What is missing is an Apple identity, not a compiler

The Mach-O binaries cross compile on the Windows box. `packaging/release-all.ps1 -Targets macos`
builds `aarch64-apple-darwin` and `x86_64-apple-darwin` through cargo-zigbuild, and deliberately
archives neither, because an archive of unsigned Mach-O is not something anybody should be able to
pick up by accident.

What cannot be done here, and could never have been done by a runner either, is everything after
the compiler:

| step | what it needs | where it can run |
|---|---|---|
| `lipo` the two slices into universal binaries | macOS, or `rcodesign` | the Mac; `rcodesign` on Windows can do this part |
| `codesign` with the hardened runtime and a trusted timestamp | **a Developer ID Application certificate** | wherever the certificate is |
| sign the `.pkg` | **a Developer ID Installer certificate** | the same |
| `notarytool submit` and `stapler staple` | **an Apple notarisation credential** | the same |
| `spctl --assess` on a quarantined copy | **macOS** | a Mac, and nowhere else |
| run the universal binary, and the x86-64 slice under Rosetta | **macOS** | a Mac, and nowhere else |

The two certificates come with an Apple Developer Program membership, which is $99 a year and needs
an Apple ID with two factor authentication. The notarisation credential is an app specific password
stored in the Mac's keychain as the profile `inillucent-notary`.
`packaging/macos/README.md` has the one time setup, and `release-macos.sh` refuses to start until all
three exist and says which is missing.

### There is no CI, and what went with it

**`task-1968` removed `.github/workflows/` outright.** `task-1922` had already measured the reason:
56 runs on this repository, none of them green, and every run after 9 September produced no jobs at
all. Nothing on a runner builds, tests or packages this repository now. `tools/validate.ps1` and
`tools/validate.sh`, on the machine making the change, are the whole gate.

**Two things stopped existing with the workflows.** The nightly fuzz run, which `SECURITY.md` says.
And `macos-latest` in the `validate` matrix, which `task-1932` had added a fortnight earlier -- the
only place the workspace was ever built and tested on a real Mac. The Mach-O binaries this box cross
compiles are now compiled and never run, by anybody, before they are published. Read the rest of
this page on that basis: there is no second machine that will notice.

**It would not have produced the archive either**, and that reasoning is unchanged. A runner is a
real Mac and could `lipo`, build the `.pkg` and run the binaries, but with no Developer ID
certificate it can only produce an ad hoc signature, and `verify-macos.sh` asks `spctl` for
`source=Notarized Developer ID` and would reject it. The archive needed a Mac with the signing
identity on it before, and it needs one now.

### The defect this ticket found in the build half

`rust-toolchain.toml` named three targets and not the two Apple ones, so on a machine holding only
the pinned toolchain, `pwsh packaging/release-all.ps1` stopped at its macOS step with

```
error[E0463]: can't find crate for `std`
note: the `aarch64-apple-darwin` target may not be installed
```

This is the same fault task-1934 fixed for `aarch64-unknown-linux-gnu`, which is why no release
before 0.1.2 carried an ARM Linux archive. It was invisible for the Apple pair because
`target/aarch64-apple-darwin/release` already held binaries from 2026-09-12, built before the pin,
and nothing rebuilt them. `rust-toolchain.toml` names all five targets now.

### What Jason has to run, and where

**Since task-1995 there is no second machine.** `packaging/release-all.ps1` on
the Windows box builds, signs, packages and notarises macOS as well, because
`rcodesign` and `tools/macos-pkg` replace every Apple program the release used
and Apple's notary service is an HTTPS API. `packaging/macos/README.md` is the
detail, including how the two Developer ID certificates are obtained in a
browser rather than in Xcode.

The paragraph below describes the route that was in place when this section was
written, and it still works on a Mac.

On the MacBook, with the repository checked out at the `v0.1.2` tag:

```sh
./packaging/macos/release-macos.sh --version 0.1.2 --upload
```

It builds both architectures, joins them, signs them, writes the `.tar.gz` and the `.zip`, builds and
signs the `.pkg`, notarises both, staples the ticket, runs the result, and uploads the four files.
`--upload` puts them on the `v0.1.2` release, **which exists now** with the tag naming the released
tree; before 2026-09-14 it did not, and `gh release create` would have made one from the mirror's
default branch, which held 0.1.1's tree. Without `--upload` the script prints the four paths and
`packaging/fetch-macos-artifacts.ps1 -FromDirectory` takes them from a folder instead.

Then, back on the Windows box:

```powershell
pwsh packaging/fetch-macos-artifacts.ps1 -Version 0.1.2     # or -FromDirectory <path>
pwsh packaging/publish-site.ps1 -Version 0.1.2 -Stage
```

`fetch-macos-artifacts.ps1` refuses rather than warns when it cannot read the signatures, and it
writes nothing into `dist/SHA256SUMS` unless every check passed. That was not true until task-1951;
what it did instead is in [what running the rest of the packaging found](#what-running-the-rest-of-the-packaging-found-task-1951).

and on any Mac, against the bytes the site is then serving:

```sh
curl -fsSL https://inillucent.com/downloads/verify-macos.sh | sh -s -- --version 0.1.2
```

and only once that passes:

```powershell
pwsh packaging/publish-site.ps1 -Version 0.1.2 -Link
```

task-1934 fixed `verify-macos.sh`'s A5 check before anybody had run it on a Mac: it spoke an MCP
handshake `inillucent-mcp` refuses, and it read the answer through `grep -q`, which closes the pipe
and made `set -o pipefail` report a correct answer as a failure. The script is the macOS release
gate, and it was failing for two reasons before it had ever been run.

---

## `dist/` and `packages/npm/staged/`: generated, ignored, and not a source of truth

Recorded here because a review asked which they were (task-1961, D4), and reading
`packaging/release.ps1` is what answers it.

Both are **build output**, both are in `.gitignore`, and neither is tracked:

- **`dist/`** is what `packaging/release.ps1` writes: the archives for each target, `SHA256SUMS`
  and `provenance.json`. It is the directory every publish step reads and every installer's bytes
  come from. A copy of a file that also lives in the tree is a copy the release put there.
- **`packages/npm/staged/`** is what `packages/npm/build.mjs` assembles out of `dist/`: the
  manifests and the shims, which are source, copied beside the built binaries, which are not. It is
  rebuilt from `dist/` on every release.

So neither needs a "frozen at release N" note and neither can go one version behind in a way that
matters: a stale copy on one machine is overwritten by the next release run, and nothing reads
either directory out of a clone. What a reader needs from a release is the archive on
`inillucent.com` or the GitHub release, and what a contributor needs is the source.

---

## The GitHub mirror: what it holds, and what it is for

Added 2026-09-14 (task-1951).

### Both repositories are public, as of task-1961

There are two. `jasonmcaffee/inillucent` is where releases are cut: 335 commits, `main` at
`40a8e3b`, the tags `v0.1.1` and `v0.1.2`, and no GitHub releases at all.
`Black-Rainbow-Labs/Inillucent` is the one every published package names: ten commits, `main` at
`135c5cc`, and the `v0.1.0` and `v0.1.1` releases with their assets attached.

Both answered 404 to everybody who was not signed in as the owner until task-1961 made them public.
Re-checked on 2026-09-14 with no credential of any kind, no token and no cookie:

| URL | was | is |
|---|---|---|
| `github.com/Black-Rainbow-Labs/Inillucent` | 404 | **200** |
| `github.com/Black-Rainbow-Labs/Inillucent/releases` | 404 | **200** |
| `github.com/Black-Rainbow-Labs/Inillucent/issues` | 404 | **200** |
| `github.com/Black-Rainbow-Labs/Inillucent/blob/main/drivers/README.md` | 404 | **200** |
| `proxy.golang.org/.../packages/go/@latest` | 404 | **200** |

`tools/check-public-urls.mjs` is that check, kept so it does not have to be done by hand again. It
reads every `github.com` URL out of the tracked files a package ships, fetches each one with no
`Authorization` header and no cookie, and prints what an anonymous reader gets. It is a
`tools/validate` stage as of task-1961: nine links, all of them reachable without signing in. It
was left out of the script on purpose while it was red, because a check that is red for a reason
nobody intends to fix teaches people to ignore the script it is in.

This is the same fault the Unluminous release had, found separately: a release whose download link
answers 404 for every visitor while looking correct to the person who published it.

### What being private cost, until task-1961

**Nothing that a user installs.** Every archive download in every installer is `inillucent.com`:
`install.sh`, `install.ps1`, the Homebrew formula's three release URLs, the Go
`cmd/inillucent-install` and the PHP `bin/inillucent-install` all read `inillucent.com/downloads`,
check the SHA-256 against the published `SHA256SUMS`, and never touch GitHub. The 0.1.2 archives
were downloaded off the site and hashed on 2026-09-14: all four match, byte for byte.

**One route does not work at all, and it is recorded below as published.** `go install` resolves a
module through `proxy.golang.org`, and the proxy clones the repository:

```
GET https://proxy.golang.org/github.com/!black-!rainbow-!labs/!inillucent/packages/go/@latest
404  not found: module github.com/Black-Rainbow-Labs/Inillucent/packages/go:
     git ls-remote ... exit status 128: fatal: could not read Username
```

`pkg.go.dev` for that module is 404 as well. The Go route was verified on this machine, where git
holds a credential, so it resolved. That is the same shape as an archive nobody opened: checked as
the author rather than as a stranger.

**Four more places hand a reader a link that 404s**: the npm package's `homepage`, `repository` and
`bugs`; the PyPI project's `Homepage` and `Issues`; `composer.json`'s issue URL; and the
`install.sh` fallback that tells somebody with no prebuilt archive to clone the repository. The
Homebrew formula's `head` spec is a fifth, so `brew install --HEAD` fails while
`brew install` would work.

### The histories share nothing, and the trees have always matched

`git merge-base` between the two `main` branches is empty. The mirror's history is ten squashed
commits beginning at `Inillucent: an embedded database for agents, written in Rust`; the
development history is 335.

The content is a different matter, and it lines up exactly:

| | tree |
|---|---|
| `v0.1.1` on the mirror (`b52e297`) | `97e516e8` |
| `v0.1.1` here (`458992d`) | `97e516e8` |

`git diff b52e297 458992d` is empty. `brl/main`'s tree is the tree of development commit `fe5a101`
exactly, found by scanning this repository's trees for it. So the mirror was made by taking a whole
tree and committing it as one commit, with the `task-NNNN:` prefix removed from the subject. That is
the right way to run a public mirror. It was done by hand, written down nowhere, and never checked,
so nobody could tell a correct tag from a plausible one.

### The recommendation

**Make `Black-Rainbow-Labs/Inillucent` public, holding one commit per release and nothing else.**
Keep cutting releases from `jasonmcaffee/inillucent`.

| what the mirror holds | |
|---|---|
| one commit per release | its tree **is** the release tag's tree, checked rather than assumed |
| the tag `v<x.y.z>` on that commit | the same name the release has here |
| a GitHub release on that tag | carrying the files `SHA256SUMS` names |
| nothing else | no development branches, no ticket numbers in subjects |

The three alternatives, and why each is worse:

- **Make `jasonmcaffee/inillucent` public and retire the mirror.** That publishes 335 commit
  subjects carrying ticket numbers and a `tasks/` directory of internal design documents. It also
  changes the Go module path, and a Go module path that changes is a different module: every
  `go install` command in every shipped document, and both published `packages/go/*` tags, stop
  meaning anything. Every other published URL already names `Black-Rainbow-Labs/Inillucent` too.
- **Retire GitHub and publish only from inillucent.com.** A Go module path is a repository URL, so
  this drops the Go route or moves the module to another public host, which renames it in the same
  way.
- **Leave both private.** Not what was done; kept here because the argument for
  the choice that was made is the argument against this one. Then the record
  would have to say so: the Go row below stops saying published,
  `packages/go/go.mod` moves or the Go route is withdrawn, and the npm and PyPI metadata point at
  `inillucent.com` instead of a repository nobody can open.

### How to carry it out

```powershell
pwsh packaging/mirror-github.ps1 -Verify                # what the mirror holds now
pwsh packaging/mirror-github.ps1 -Version 0.1.2         # build the commit; pushes nothing
pwsh packaging/mirror-github.ps1 -Version 0.1.2 -Push   # publish the commit and the tag
```

`mirror-github.ps1` takes the tree straight off the `v0.1.2` tag with `git commit-tree`, so there is
no copy step for anything to go wrong in, and it refuses if the commit it built does not carry that
tree. The result is a pure function of the tree, the parent, the message and the author, which means
running it twice gives the same commit hash and anybody holding both repositories can recompute it.
For 0.1.2 that commit is `703de98`, tree `4419e10`, and `git diff 703de98 v0.1.2` is empty.

The tag it pushes is **annotated**, because the two already on the mirror are and because an
annotated tag is the only kind that records who made it and when. `git tag` can only write
`refs/tags/<name>`, and this repository already has a local `v<version>` naming a commit in the
development history, so the tag object is written with `git mktag` and put under a ref this script
owns.

`-Verify` reads the mirror as it stands and reports one row per release tag:

```
  v0.1.0     same tree
  v0.1.1     same tree
  v0.1.2     same tree

every release tag on the mirror holds the released tree
```

### What was done on 2026-09-14, and what was not

The 0.1.2 commit and tag are **pushed**, and the v0.1.2 release is **cut**, with all five assets.
None of that was a visibility decision: the repository was private before that day's work and
private after it, so it published nothing to anybody at the time - the flip came later, in
task-1961. What it did was make the mirror's record true and give the macOS upload something to
attach to - `release-macos.sh --upload` runs `gh release create "v$version"`
when the release is absent, and with no `v0.1.2` tag on the remote that would have created one from
the default branch, which held 0.1.1's tree.

Every asset was downloaded back off the release and hashed: all five match the published
`SHA256SUMS`, and all five are byte-identical to `dist/` and to what inillucent.com serves. The
release page, the release API and an asset download all still answer **404** to a signed-out reader,
which was checked with no credential.

**What was not done, because it is the decision and not a command:** making the repository public.
That is **Settings → General → Change repository visibility → Public** on
`Black-Rainbow-Labs/Inillucent`. After it, `node tools/check-public-urls.mjs` goes green and belongs
in `tools/validate.*` as a network-gated check. It is deliberately not there now, because a check
that is red from the day it lands is a check somebody deletes.

### One thing to settle before making it public

**`tasks/` is in the mirror's tree**, and at 0.1.2 that is 32 design documents named by ticket
number. They are readable and they are internal, and they become public along with everything else.

If they should not be public, the place to remove them is **this repository**, not the mirror. The
mirror's whole claim is that its tree is the release tree; dropping a directory only on the mirror
breaks the one property that makes a tag checkable, and it would mean the archive on inillucent.com
and the repository on GitHub no longer hold the same code. Removing `tasks/` here, or moving it
outside the released tree, keeps both true.

---

## The decision that comes first

**Three of the six routes require the source to be public**:

| route | needs a public repository? | why |
|---|---|---|
| npm | no | it ships binaries; the source never leaves the build machine |
| PyPI | no | it ships binaries plus the Python binding, in the wheel |
| **crates.io** | **yes, effectively** | `cargo publish` uploads the crate **source**, and it can never be removed |
| Homebrew | **no**, for `brew install` | the formula's three release URLs are `inillucent.com`. Only the `head` spec clones GitHub, so `brew install --HEAD` is the one command that needs it |
| **Go** | **yes**, and measured | `go install` resolves through `proxy.golang.org`, which clones the repository. It answers `404 ... fatal: could not read Username` today |
| the install scripts | **no** | `install.sh` and `install.ps1` download from `inillucent.com`. They only name GitHub in the "build it yourself" message, which points at a 404 |

So the first question is not "which registry" but **"does inillucent become
public source?"** It is a one-way door: crates.io versions cannot be deleted
(`cargo yank` hides a version from new resolution and leaves it downloadable
forever), and a public git history cannot be un-published.

If the answer is no for now, **npm and PyPI still work**, and they are the two
that reach the most people fastest.

---

## Step 0 - the release, which everything else reads

Two machines. `packaging/README.md` has the full sequence; the short form is:

```powershell
# the Windows box, all of it: every target, signed, notarised and packaged
pwsh tools/cross/fetch-toolchain.ps1
pwsh packaging/release-all.ps1
pwsh packaging/linux/package-linux.ps1
pwsh packaging/sign-sums.ps1
pwsh packaging/publish-site.ps1 -Version 0.1.4 -Stage
#   ... verify on any Mac that can be borrowed, then:
pwsh packaging/publish-site.ps1 -Version 0.1.4 -Link
```

**The distribution point is inillucent.com**, and since task-1995 GitHub carries
nothing at all: there are no macOS artifacts to move between machines, because
the machine that builds them is the machine that publishes them.

`release.ps1` refuses to build an untagged archive, so the tag comes first.
`provenance.json` records the commit, the tag, the toolchain and the six checks
the release script made - clean checkout, tag matches HEAD, version agrees,
built from source, installed and run, C ABI linked - and the release is cut with
**no waivers**.

Every asset of `v0.1.1` was downloaded back off the release and hashed: all four
match the published checksums byte for byte, the released `SHA256SUMS` is LF,
and the README inside the released zip carries the corrected Go row.

---

## crates.io

```powershell
cargo login                                # opens a browser; GitHub OAuth
pwsh packaging/cargo-publish.ps1           # dry run
pwsh packaging/cargo-publish.ps1 -Execute  # asks you to type PUBLISH
```

**Ready.** `cargo publish --workspace --dry-run` completes for every publishable
crate, in dependency order, with no errors. The crates excluded by their own
manifests are the ones nobody should depend on: the assurance harness, the
benchmark (which links PostgreSQL and pgvector to measure against them), the
test oracle, the simulator and the two crates kept only for reading SQLite
files.

Getting there needed two fixes that are now in:

- every internal edge in `[workspace.dependencies]` carries a `version` beside
  its `path`, because a published crate cannot resolve a path;
- `inillucent-base`'s build script read `../../compat/errors.toml`, which is
  **above the crate directory and therefore not in the tarball**, so the crate
  could not be published and nothing above it could either. The manifests are
  now vendored into `crates/inillucent-base/manifests/` as well, the workspace
  copy still wins, and the build **fails if the two have drifted** - a vendored
  copy nothing checks is a copy that goes stale, and this one generates the
  engine's error table.

**Missing**: a crates.io API token. `cargo login` needs a browser and a GitHub
account; there is no way to mint one from a terminal.

**Consequence**: this publishes the source of every published crate under MIT,
permanently.

---

## npm

```sh
npm login                                  # browser, plus a one-time code
node packages/npm/build.mjs --publish
```

**Ready.** `npm publish --dry-run` is clean, and the packages were built,
packed, installed from their tarballs into a scratch project and driven end to
end - the shim resolved the platform binary, ran it, passed the exit code back,
and the Node API returned real rows.

The shape is esbuild's: `inillucent` is a shim with four
`@inillucent/cli-<platform>` packages as `optionalDependencies`, so npm installs
only the one that runs on the machine. No postinstall, no download at install
time, so `npm ci` works offline and behind a proxy.

**Missing: a token.** A **granular access token**, made from the account's
Access Tokens page on npmjs.com, is what a machine should use: put it in
`~/.npmrc` as `//registry.npmjs.org/:_authToken=...` and the publish needs no
login at all. An interactive `npm login` works too and asks for a password and a
second factor.

**The signup page cannot be driven programmatically.** `/signup` answers HTTP
403 to every client - to `curl` with its default user agent and to `curl` with a
Chrome user agent alike, while `registry.npmjs.org` answers normally in the same
second - and serves a DataDome challenge instead of a form. Creating the account
is a browser and a person; there is no automation route past it and none is
worth looking for.

**Note**: only `@inillucent/cli-win32-x64` can be built on a Windows box. The
other three platform packages need their archives from step 0. Publishing the
wrapper without them is fine - npm treats a missing `optionalDependency` as a
platform the package does not serve - but the wrapper should go **last**,
because it pins them at an exact version.

---

## PyPI

```sh
python -m pip install build twine
python packages/python/build.py --publish            # or --publish --test for TestPyPI
```

**Ready.** The wheel builds, and it was installed into a clean virtual
environment and exercised both ways: the console scripts run, and
`inillucent.Database(...)` opened a file through the bundled C ABI library and
returned rows. It carries the four binaries, the shared library, the header and
the reference driver binding, so `pip install inillucent` gets a command *and*
an in-process driver with no compiler.

**Missing: an account, and it is deliberately not created here.** Creating one
means choosing a password *and* binding a second factor - PyPI has required 2FA
for uploading since January 2024, and an API token cannot be minted without it.
Both of those credentials would then be held by whoever created the account
rather than by the maintainer, which is worse than not having the package
published: it is an account in somebody's name that they cannot get into.

The registration form carries a required `h-captcha-response` field and an
hCaptcha challenge, so it is a browser and a person as well.

So this one stops here on purpose. Register, enable 2FA on your own
authenticator, mint an API token, and then run the two commands above.

**Note**: PyPI refuses a plain `linux_x86_64` wheel; the Linux one has to be
built in a `manylinux` container. `build.py` says so rather than uploading
something that will be rejected after the fact.

---

## Homebrew

```sh
# once: create the tap repository Black-Rainbow-Labs/homebrew-inillucent on GitHub
./packaging/homebrew/update.sh --tap ../homebrew-inillucent
# then, in the tap:
git add Formula/inillucent.rb && git commit -m "inillucent 0.1.2" && git push
```

`brew install black-rainbow-labs/inillucent/inillucent` after that.

**Ready**, except that `update.sh` fills the formula's checksums from
`dist/SHA256SUMS` and **refuses to finish while an archive is missing** - so it
needs step 0's macOS and Linux archives first. It says which are missing rather
than leaving a placeholder in a file somebody would push.

**Missing**: the tap repository, the two macOS archives and the Linux one.

**Not verified here**: Homebrew does not run on Windows, so the formula has been
written and reviewed but not executed. `brew audit --strict --new inillucent`
and `brew test inillucent` are the two commands to run on the Mac before pushing
the tap; the formula's `test do` block is an end-to-end one that creates a
database, writes to it, reads it back and asks the MCP server for its tool list.

---

## Go - tagged, and not installable

```sh
git tag packages/go/v0.1.2 && git push origin packages/go/v0.1.2
```

That is the whole publish, and all three tags are pushed: `packages/go/v0.1.0`,
`packages/go/v0.1.1` and `packages/go/v0.1.2`. Go has no registry and no account:
`go install` reads the repository directly, and a tag is the release. The tag
carries the `packages/go/` prefix because the module is in a subdirectory, which
is Go's own rule for a nested module.

### The Go route, verified through the module proxy

**It was not installable by anybody until task-1961 made the repository
public.** `go install` does not clone the repository itself; it asks
`proxy.golang.org`, and the proxy clones on its behalf with no credential. While
the repository was private that read:

```
GET https://proxy.golang.org/github.com/!black-!rainbow-!labs/!inillucent/packages/go/@latest
404  not found: module github.com/Black-Rainbow-Labs/Inillucent/packages/go:
     git ls-remote ... exit status 128: fatal: could not read Username
```

Re-checked on 2026-09-14 with no credential of any kind, by
`node tools/check-public-urls.mjs`, which `tools/validate` runs:

| URL | anonymous |
|---|---|
| `proxy.golang.org/github.com/!black-!rainbow-!labs/!inillucent/packages/go/@latest` | **200** |
| `proxy.golang.org/github.com/!black-!rainbow-!labs/!inillucent/packages/go/@v/list` | **200** |

The proxy is the thing `go install` asks, and it answers as a signed-out caller,
which is the whole of what being private broke. **`go install` itself was not
re-run**, and the reason is worth writing down rather than leaving as an
implication: the Go toolchain is not installed on this machine, and the earlier
run that made the route look published was taken here, where git holds a
credential - which is exactly the reading that was wrong the first time. The
proxy answering a caller with no credential is the stronger evidence of the two,
because it is the caller a user actually is.

What is still unverified, and what would verify it: one `go install` from a
machine or container with the Go toolchain and no git credential. It needs no
account and no token.

```sh
go install github.com/Black-Rainbow-Labs/Inillucent/packages/go/cmd/inillucent-install@v0.1.2
inillucent-install
```

**The prefix belongs to the tag and not to the version argument.** Passing the
tag name there is rejected outright:

```
go: ...cmd/inillucent-install@packages/go/v0.1.0: invalid version:
    version "packages/go/v0.1.0" invalid: disallowed version string
```

Every document in this repository said `@packages/go/v0.1.0` until it was run.
`@v0.1.1` is what works, and `@latest` resolves to the newest prefixed tag.

**Verified on this machine**, with Go 1.25.1: `go build ./...` and `go vet ./...`
clean, `go test ./...` passing, `go install` resolving the published tag, and the
installed `inillucent-install` downloading the release from inillucent.com,
checking its SHA-256, writing all four programs, and those programs then
creating a database, running a statement, returning a row and answering an MCP
`initialize` handshake.

### The two defects publishing it found

- **The command installed over itself.** The directory was `cmd/inillucent`, so
  `go install` put a program called `inillucent` in GOBIN - and that program's
  job is to put a program called `inillucent` in GOBIN. On Windows it failed at
  the last of the four files with *"The process cannot access the file because
  it is being used by another process"*, leaving three installed and the CLI
  missing. The command's own help text already called it `inillucent-install`;
  only the directory disagreed, and the directory is what names the binary. It
  is `cmd/inillucent-install` now, which is also what the Composer package calls
  the same program.
- **The documented version argument was invalid**, as above.

Neither was reachable without installing it the way a stranger would. `go build`
and `go test` were both clean the whole time.

---

## Packagist (Composer)

Submit `https://github.com/Black-Rainbow-Labs/Inillucent` at
<https://packagist.org/packages/submit>, then add the GitHub webhook Packagist
offers so a tag updates the package.

**This needs the repository to be public**, which it is as of task-1961.
Packagist reads the repository to find `composer.json` and to list the tags, and
a private one answered 404 to it exactly as it did to everybody else.

`composer.json` is at the **repository root**, because Packagist reads a
repository rather than a subdirectory; it autoloads `Inillucent\` from
`packages/php/src/` and declares the two `bin` scripts.

**Ready and verified.** `composer validate` passes, every file lints under PHP
8.3, and the package was exercised end to end against a real database: a batch,
a bound query, a hostile value round-tripping through binding unchanged,
`describe`, a `not_found` status raised as a typed exception, and a read-only
handle refusing a write while still answering a read.

**Missing**: a Packagist account, through GitHub OAuth, and a public
repository.

---

## The order, and why it is the order

1. **The archives on inillucent.com**, because the npm platform packages, the
   Homebrew formula, the Go installer and the PHP installer all fetch from
   there. Publishing any of them first publishes a package that cannot install.
   This used to say "the GitHub release", and it was true when the installers
   downloaded from GitHub; they download from `inillucent.com` now.
2. **The public mirror**, because Go and Packagist need the repository to be
   readable and because the links every other package prints point at it.
3. **crates.io**, which is self-contained and is also the fallback every other
   route names when a platform has no prebuilt archive.
4. **npm and PyPI**, in either order.
5. **Homebrew and Packagist**, which are one commit and one form.

---

## The one thing that cannot be automated

Every registry above requires an interactive login: a browser, an OAuth
redirect, and in PyPI's case a mandatory second factor. That is deliberate on
their part - it is what stops somebody else publishing under your name - and it
means the last step of each route belongs to a person at a keyboard, not to a
script. Everything up to it is a command in this directory.
