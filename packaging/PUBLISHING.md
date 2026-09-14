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
| **inillucent.com, macOS** | work remains - no archive | a Mac: `packaging/macos/release-macos.sh --version <v> --upload`, which builds both architectures, `lipo`s them, signs, notarises and uploads |
| **SHA256SUMS signature** | not signed, at 0.1.1 or 0.1.2 | the minisign secret key. `packaging/sign-sums.ps1` reads it from `INILLUCENT_MINISIGN_KEY`, and `packaging/inillucent.pub` does not exist either |
| **the .deb and the .rpm** | not built, at 0.1.1 or 0.1.2 | an OpenPGP key. `packaging/linux/package-linux.ps1` signs by default because `apt` and `dnf` will not install an unsigned package from outside a distribution's own repository |
| **GitHub release** | `v0.1.1`, and **not cut for 0.1.2** | the histories have to be reconciled first: `Black-Rainbow-Labs/Inillucent` on GitHub holds a `main` that shares no commits with the one releases are cut from, so a `v0.1.2` tag there would name code that is not in it |
| **Go** | **published** - tag `packages/go/v0.1.2` | nothing |
| **npm** | **token is the only step** - three tarballs packed, installed and run | an account token |
| **PyPI** | **token is the only step** - wheel installed into a clean venv and run | an account with 2FA, and a token minted from it |
| **crates.io** | **token is the only step** - `--workspace --dry-run` clean for every publishable crate | a token, **and the decision to make the source public** |
| **Packagist** | **token is the only step** - `composer install` works end to end | a Packagist account (GitHub OAuth) |
| **Homebrew** | **work remains after the token** - and there is no token | the macOS archive, which does not exist; then the tap repository |

Read across that table one more way, because it is the part that decides what to
chase:

- **npm, PyPI, crates.io and Packagist are finished except for the upload.** The
  artifact is on disk, a clean machine installed from that exact artifact, and
  the installed thing ran. `npm publish`, `twine upload`, `cargo publish` and the
  Packagist submit form are the only commands left. Each is written out below.
- **Homebrew is not waiting on a credential at all** - a formula is a pull
  request and needs no account. It is waiting on the macOS archive, which needs
  a Mac. A token would change nothing.
- **Go is published.** `go install` reads the repository directly and a tag is
  the release, so there is no registry and no account in that route at all.

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

## The decision that comes first

**Three of the six routes require the source to be public**:

| route | needs a public repository? | why |
|---|---|---|
| npm | no | it ships binaries; the source never leaves the build machine |
| PyPI | no | it ships binaries plus the Python binding, in the wheel |
| **crates.io** | **yes, effectively** | `cargo publish` uploads the crate **source**, and it can never be removed |
| **Homebrew** | **yes** | the formula fetches a release asset, and a private repository's assets are not public |
| **Go** | **yes** | `go install` clones the module from the repository, and `sum.golang.org` cannot read a private one |
| the install scripts | **yes** | they fetch from the releases |

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
# the Windows box: Windows and both Linux architectures, then the packages
pwsh tools/cross/fetch-toolchain.ps1
pwsh packaging/release-all.ps1
pwsh packaging/linux/package-linux.ps1
```

```sh
# the MacBook: build, sign, notarise, verify, hand over
./packaging/macos/release-macos.sh --version 0.1.2 --upload
```

```powershell
# the Windows box again: collect, sign the checksums, publish
pwsh packaging/fetch-macos-artifacts.ps1 -Version 0.1.2
pwsh packaging/sign-sums.ps1
pwsh packaging/publish-site.ps1 -Version 0.1.2 -Stage
#   ... verify on the Mac, then:
pwsh packaging/publish-site.ps1 -Version 0.1.2 -Link
```

**The distribution point is inillucent.com.** GitHub carries the macOS artifacts
from the MacBook to the Windows box; the site is what a user downloads from.

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

## Go - published

```sh
git tag packages/go/v0.1.2 && git push origin packages/go/v0.1.2
```

That is the whole publish. Three tags are pushed: `packages/go/v0.1.0`,
`packages/go/v0.1.1` and `packages/go/v0.1.2`, and `@latest` resolves to the last
of them. Go has no registry and no account: `go install` reads the repository
directly, and a tag is the release. The tag carries the `packages/go/` prefix
because the module is in a subdirectory, which is Go's own rule for a nested
module.

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

**Verified**, with Go 1.25.1: `go build ./...` and `go vet ./...` clean,
`go test ./...` passing, `go install` resolving the published tag, and the
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

`composer.json` is at the **repository root**, because Packagist reads a
repository rather than a subdirectory; it autoloads `Inillucent\` from
`packages/php/src/` and declares the two `bin` scripts.

**Ready and verified.** `composer validate` passes, every file lints under PHP
8.3, and the package was exercised end to end against a real database: a batch,
a bound query, a hostile value round-tripping through binding unchanged,
`describe`, a `not_found` status raised as a typed exception, and a read-only
handle refusing a write while still answering a read.

**Missing**: a Packagist account, through GitHub OAuth.

---

## The order, and why it is the order

1. **The GitHub release**, because the npm platform packages, the Homebrew
   formula, the Go installer and the PHP installer all fetch from it. Publishing
   any of them first publishes a package that cannot install.
2. **crates.io**, which is self-contained and is also the fallback every other
   route names when a platform has no prebuilt archive.
3. **npm and PyPI**, in either order.
4. **Homebrew and Packagist**, which are one commit and one form.

---

## The one thing that cannot be automated

Every registry above requires an interactive login: a browser, an OAuth
redirect, and in PyPI's case a mandatory second factor. That is deliberate on
their part - it is what stops somebody else publishing under your name - and it
means the last step of each route belongs to a person at a keyboard, not to a
script. Everything up to it is a command in this directory.
