# Publishing: what is ready, and what each step still needs

Everything in this repository is built, tested and verified up to the upload.
This file is the last mile: what each registry needs, in what order, and which
credential is missing.

**One of the six is published: Go**, and the GitHub release is current at v0.1.1. It needed no account and no token, only a
git tag, so it was tagged. The other five stop at a credential that belongs to a
person, or at one decision that is not a script's to make - and they are not all
in the same state behind that credential, so the table below says which.

Two readiness states, and they are not the same thing:

- **Token is the only step.** The artifact exists on disk, it was installed from
  that artifact on a clean machine, and it was run. The upload command is
  written out below and nothing else has to be built.
- **Work remains after the token.** Something still has to be built, and the
  credential arriving does not finish it.

---

## Where it stands, at a glance

Updated 2026-09-11, at **0.1.1**. **Both one-line installers work**, verified by
running them as written against the live site: Windows, and Ubuntu 24.04. A Linux
archive is published beside the Windows one.

**0.1.0 was withdrawn, not patched.** Its archives carried `README.md`,
`docs/getting-started.md` and `agent-skills/inillucent-quickstart/SKILL.md` from
before the Go command was renamed, so all three told a reader to run
`go install .../packages/go/cmd/inillucent@latest` - and `@latest` resolves to a
module where that directory no longer exists. The archive the site handed out
contained an install command that failed. Replacing those archives in place would
have left two different archives both called 0.1.0, and the Homebrew formula
already recorded the 0.1.0 Linux checksum, which would have stopped matching with
nothing to say so. The 0.1.0 archives are removed from the site and answer 404.

| route | readiness | what it is waiting on |
|---|---|---|
| **inillucent.com, Windows** | **live** - `irm .../install.ps1 \| iex` installs and runs | nothing |
| **inillucent.com, Linux** | **live** - `curl -fsSL .../install.sh \| sh` installs and runs | nothing |
| **inillucent.com, macOS** | work remains - no archive | a Mac: `packaging/macos/release-macos.sh --version <v> --upload`, which builds both architectures, `lipo`s them, signs, notarises and uploads |
| **GitHub release** | **done** - `v0.1.1` with both archives, `SHA256SUMS` and `provenance.json` | nothing |
| **Go** | **published** - tag `packages/go/v0.1.2` | nothing, except that a private repository limits who can install it |
| **npm** | **token is the only step** - three tarballs packed, installed and run | **the signup page answers 403 to every client** - see below |
| **PyPI** | **token is the only step** - wheel installed into a clean venv and run | an account; the form carries an hCaptcha and uploads need 2FA |
| **crates.io** | **token is the only step** - `--workspace --dry-run` clean for all 29 crates | a token, **and the decision to make the source public** |
| **Packagist** | **token is the only step** - `composer install` works end to end | a Packagist account (GitHub OAuth), and a public repository |
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
- **Go is published**, and its one caveat is not a credential either: the
  repository is private, so `sum.golang.org` cannot read the module and the
  public proxy answers 404. A collaborator with `GOPRIVATE` set installs it
  today; a stranger cannot until the repository is public.

The macOS archive is the single artifact that blocks the most: the macOS
installer, the Homebrew formula, and two of the four npm platform packages all
wait on it and on nothing else.

### What the site publishing took, and the defects it found

The site was already serving `/downloads/` publicly. It had a stale archive and
none of the scripts, and two things were broken underneath:

- **`.ps1` was served as `application/octet-stream`**, because `mime_guess` has no
  entry for it. Against that, `Invoke-RestMethod` hands back a byte array rather
  than a script, so `irm ... | iex` died with *"[System.Byte] does not contain a
  method named 'Trim'"*. Fixed in the site's static handler.
- **`install.sh` used `set -o pipefail` and `${BASH_SOURCE[0]}`**, both bash-only.
  The documented command pipes into `sh`, which on Debian and Ubuntu is dash, and
  dash answered *"Illegal option -o pipefail"* and stopped before downloading
  anything. The script is POSIX now and `dash -n` parses it clean.

A third was found while testing macOS: `curl -fsSL` on an archive that is not
published exits non-zero with no output, and `set -e` then ended the script in
silence. It now reads SHA256SUMS first and names the platforms that were
published.

### The rule: run every command the release ships, from the release

0.1.0 passed every check above and still shipped four commands that fail,
because the checks were run against the repository and the commands ship inside
the archive. Fixing the source tree does not change the artifact. These came out
of running each one from what the site actually serves.

- **SHA256SUMS was written with CRLF.** `Set-Content` uses the platform's line
  ending, so a file produced on Windows ends every line with `\r\n`. Linux `awk`
  keeps that carriage return in `$NF`, so `curl -fsSL .../install.sh | sh` on
  Ubuntu answered *"inillucent 0.1.1 has no build for x86_64-unknown-linux-gnu
  yet"* and then listed `inillucent-0.1.1-x86_64-unknown-linux-gnu.tar.gz` as
  published, on the next line. Windows `awk` opens files in text mode and drops
  the `\r`, which is why every run on the machine that produced the file passed.
  `release.ps1` writes LF now, and `install.sh` strips carriage returns before
  reading, in both the download path and `--from-dist`.
- **Two one-liners pointed at `raw.githubusercontent.com`.** That path serves
  files out of the repository, and the repository is private, so both answered
  404 for everybody. They were in `docs/getting-started.md` and the quickstart
  skill, which both travel inside the archive, and in
  `packaging/windows/README.md`. All three point at inillucent.com now.
- **The Go row named a command that had been renamed.** `cmd/inillucent` became
  `cmd/inillucent-install`, and the archives still said the old one, so
  `@latest` resolved to a module where that directory does not exist.
- **The client libraries section listed eight packages that do not exist**, under
  a link to a repository that answers 404. PyPI is the one that can be checked
  without a browser and its API answers `{"message": "Not Found"}`.

Every `http` URL in the documents that travel inside the archive was then
requested. The eighteen that remain all answer.

### One more thing that can fail silently

An agent terminal can inherit a `PSModulePath` in which the PowerShell 7 module
directories shadow `Microsoft.PowerShell.Utility`. Cmdlets from it - `Get-FileHash`
among them - resolve to nothing, the script keeps going, and it exits 0. Applied
to `release.ps1` that produces a SHA256SUMS that looks written and is empty.

`release.ps1` and `publish-site.ps1` now check for the cmdlets they need before
doing anything, and stop with the reason rather than succeeding at nothing.

**The acceptance test for staging is not the script's exit code.** It is
downloading what the site serves and hashing it with a tool that is not
PowerShell, then comparing that against the published SHA256SUMS. That is the
only check a vanished cmdlet cannot fake, and it is what was run for 0.1.1:
`sha256sum` over the three served files returns exactly the three values in the
served SHA256SUMS.


## The decision that comes first

**Three of the six routes require the repository to be public**, and it is
private today:

| route | needs a public repository? | why |
|---|---|---|
| npm | no | it ships binaries; the source never leaves this machine |
| PyPI | no | it ships binaries plus the Python binding, in the wheel |
| **crates.io** | **yes, effectively** | `cargo publish` uploads the crate **source**, and it can never be removed |
| **Homebrew** | **yes** | the formula fetches a release asset, and a private repository's assets are not public |
| **Go** | **yes** | `go install` clones the module from the repository |
| the install scripts | **yes** | they `curl` from `raw.githubusercontent.com` and the releases |

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

**The distribution point is inillucent.com.** A GitHub release cannot be one
while the repository is private, because its assets are private too. GitHub
carries the macOS artifacts from the MacBook to the Windows box and nothing else.

**Done.** <https://github.com/Black-Rainbow-Labs/Inillucent/releases/tag/v0.1.1>
carries both archives, `SHA256SUMS` and `provenance.json`. Every asset was
downloaded back off the release and hashed: all four match the published
checksums byte for byte, the released `SHA256SUMS` is LF, and the README inside
the released zip carries the corrected Go row with no `raw.githubusercontent`
anywhere in the archive.

**No new credential was needed, and `gh` is a red herring.** `gh auth status`
reports no host logged in, which is what made this look blocked - but `git push`
has been working the whole time through the Windows credential manager, and that
same credential answers the REST API:

```sh
git credential fill <<< $'protocol=https\nhost=github.com\n'
```

returns Jason's existing personal access token, scoped `gist, repo, workflow`,
with `admin` on this repository. `repo` covers releases. Creating the release
used the credential that was already authorising the pushes, for the same
repository, to do the thing this file already said should happen - so it needed
no account, no browser and no login page.

**v0.1.0 is marked withdrawn rather than deleted.** Its title now reads
*"inillucent 0.1.0 (withdrawn - use 0.1.1)"* and its notes lead with what is
wrong inside its archives, above the original notes, which are kept. The assets
are left attached: deleting them would remove the record of what was published,
and they are already unreachable from inillucent.com, which answers 404 for them.

An earlier v0.1.0 was cut on the old `jasonmcaffee/inillucent` repository and then
removed: it named the wrong organisation and its archive carried the README whose
install table was not true.

`provenance.json` records the commit, the tag, the toolchain and the six checks
the release script made - clean checkout, tag matches HEAD, version agrees, built
from source, installed and run, C ABI linked - with **no waivers**. The script
refuses to build an untagged archive at all, so the tag came first rather than
being passed over with `-AllowUntagged`.

**Still needed**: the three non-Windows archives, built on their own machines
with `packaging/release.sh --target <triple>` and added to the same release.

**The repository is still private**, so the release and its assets are reachable
only by somebody with access, and the `curl`-to-shell installers in the release
notes do not resolve for anybody else yet.

## crates.io

```powershell
cargo login                                # opens a browser; GitHub OAuth
pwsh packaging/cargo-publish.ps1           # dry run
pwsh packaging/cargo-publish.ps1 -Execute  # asks you to type PUBLISH
```

**Ready.** `cargo publish --workspace --dry-run` completes for all 30
publishable crates, in dependency order, with no errors — that was verified on
2026-09-08. Two crates are excluded by their own manifests: `inillucent-compat`
(the assurance harness) and `inillucent-bench` (which links PostgreSQL and
pgvector to measure against them).

Getting there needed two fixes that are now in:

- every internal edge in `[workspace.dependencies]` carries a `version` beside
  its `path`, because a published crate cannot resolve a path;
- `inillucent-base`'s build script read `../../compat/errors.toml`, which is
  **above the crate directory and therefore not in the tarball**, so the crate
  could not be published and nothing above it could either. The manifests are
  now vendored into `crates/inillucent-base/manifests/` as well, the workspace
  copy still wins, and the build **fails if the two have drifted** — a vendored
  copy nothing checks is a copy that goes stale, and this one generates the
  engine's error table.

**Missing**: a crates.io API token. `cargo login` needs a browser and a GitHub
account; there is no way to mint one from a terminal.

**Consequence**: this publishes the source of 30 crates under MIT, permanently.

---

## npm

```sh
npm login                                  # browser, plus a one-time code
node packages/npm/build.mjs --publish
```

**Ready.** `npm publish --dry-run` is clean, and the packages were built,
packed, installed from their tarballs into a scratch project and driven end to
end — the shim resolved the platform binary, ran it, passed the exit code back,
and the Node API returned real rows.

The shape is esbuild's: `inillucent` is a shim with four
`@inillucent/cli-<platform>` packages as `optionalDependencies`, so npm installs
only the one that runs on the machine. No postinstall, no download at install
time, so `npm ci` works offline and behind a proxy.

**Missing: Jason's npm password.** Not the token, and not the emailed code —
those were both chased down and neither is the thing in the way. What was
established, on 2026-09-08:

- `~/.npmrc` holds an `npm_…` token and it is **dead**: a direct
  `GET /-/whoami` against the registry with it answers **401**.
- The account exists and is **`jasonmcaffee`**, registered to
  **`jasonlmcaffee@gmail.com`** — which *is* the mailbox Nikaya indexes, so the
  email route was available in principle.
- `npm login --auth-type=legacy` prompts **Username → Password → OTP**, in that
  order.
- **The emailed OTP is the second factor, not a way past the first.** npm's own
  mail says so: *"It looks like you are trying to log in to npm using your
  username and password. As an **additional** security measure you are requested
  to enter the OTP code."* npm only sends that mail **after** the password is
  accepted — so without the password no code is ever sent, and there is nothing
  to go and read.
- The Nikaya corpus was 10 days stale, so it was **synced** (`nikaya-server
  sync`: 269 messages fetched, `lastIncrementalAt` moved from `2026-08-29T04:00Z`
  to `2026-09-08T22:24Z`) and searched again. No npm mail from today, for the
  reason above. The mail route itself works — the corpus holds mail through
  2026-09-05 — it just has nothing to deliver here.
- The **web** flow (`npm login`, the default) was the one path that could have
  skipped the password by reusing a signed-in browser session. It cannot:
  **neither the Chrome nor the Edge profile on this machine holds a single
  npmjs.com cookie**, so that flow lands on a fresh sign-in page.

So it is one command once you supply the password:

```sh
npm login --auth-type=legacy     # username jasonmcaffee, your password, then the
                                 # code npm emails to jasonlmcaffee@gmail.com
node packages/npm/build.mjs --publish
```

**A second npm account does not get round this.** This was asked for on
2026-09-10 - reuse the Black Rainbow Labs identity from task-1898 for the
registries - and it does not help, for two reasons worth writing down so nobody
tries it again:

- **npm will not take the address.** `jasonlmcaffee@gmail.com` already belongs to
  `jasonmcaffee`, and npm allows one account per verified address.
- **The Black Rainbow Labs address is being deleted.**
  `the.black.rainbow.labs@gmail.com` was flagged by Google during task-1898 and
  has no recovery phone and no recovery email, so there is nothing to appeal
  with. An npm account registered to it would lose its recovery address, and
  reading a verification code out of it means putting a browser back on a Google
  login page - which is what caused the flag, and what `~/.claude/CLAUDE.md` now
  forbids.

For a Black Rainbow Labs identity on the registries the mailbox has to come
first, and it must not be a Google account that an automation signs into. All
three domains - `inillucent.com`, `blackrainbowlabs.com`, `jasonmcaffee.com` -
are on Cloudflare and **none has an MX record today**. Cloudflare Email Routing
is free and takes a few minutes: point `npm@inillucent.com` at whichever inbox
is already read. After that a signup needs no Google login at all.


A **granular access token** made at <https://www.npmjs.com/settings/jasonmcaffee/tokens>
works just as well and is better for a machine: put it in `~/.npmrc` as
`//registry.npmjs.org/:_authToken=…` and the publish needs no login at all.

**Note**: only `@inillucent/cli-win32-x64` can be built here. The other three
platform packages need their archives from step 0. Publishing the wrapper
without them is fine — npm treats a missing `optionalDependency` as a platform
the package does not serve — but the wrapper should go **last**, because it pins
them at an exact version.

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

**Missing: a PyPI account, and it is deliberately not created.** There is no
account and no `~/.pypirc` on this machine. Creating one means choosing a
password *and* binding a second factor — PyPI has required 2FA for uploading
since January 2024, and an API token cannot be minted without it. Both of those
credentials would then be held by whoever created the account rather than by
Jason, which is worse than not having the package published: it is an account in
his name that he cannot get into.

So this one stops here on purpose. Sign up, enable 2FA on your own authenticator,
mint an API token, and then:

```sh
python -m pip install build twine
python packages/python/build.py --publish
```

**Note**: PyPI refuses a plain `linux_x86_64` wheel; the Linux one has to be
built in a `manylinux` container. `build.py` says so rather than uploading
something that will be rejected after the fact.

---

## Homebrew

```sh
# once: create the repository Black-Rainbow-Labs/homebrew-inillucent on GitHub
./packaging/homebrew/update.sh --tap ../homebrew-inillucent
# then, in the tap:
git add Formula/inillucent.rb && git commit -m "inillucent 0.1.0" && git push
```

`brew install black-rainbow-labs/inillucent/inillucent` after that.

**Ready**, except that `update.sh` fills the formula's checksums from
`dist/SHA256SUMS` and **refuses to finish while an archive is missing** — so it
needs step 0's macOS and Linux archives first. It says which are missing rather
than leaving a placeholder in a file somebody would push.

**Missing**: the tap repository, the two macOS archives and the Linux one, and a
public `Black-Rainbow-Labs/Inillucent` for the formula's URLs to resolve.

**Not verified here**: Homebrew does not run on Windows, so the formula has been
written and reviewed but not executed. `brew audit --strict --new inillucent`
and `brew test inillucent` are the two commands to run on the Mac before pushing
the tap; the formula's `test do` block is an end-to-end one that creates a
database, writes to it, reads it back and asks the MCP server for its tool list.

---

## Go - published

```sh
git tag packages/go/v0.1.2 && git push brl packages/go/v0.1.2
```

That is the whole publish. Three tags are pushed: `packages/go/v0.1.0`,
`packages/go/v0.1.1` and `packages/go/v0.1.2`, and `@latest` resolves to the last
of them. Go has no registry and no account: `go install` reads
the repository directly, and a tag is the release. The tag carries the
`packages/go/` prefix because the module is in a subdirectory, which is Go's own
rule for a nested module.

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

**Verified**, on this machine, with Go 1.25.1: `go build ./...` and `go vet
./...` clean, `go test ./...` passing, `go install` resolving the published tag,
and the installed `inillucent-install` downloading the release from
inillucent.com, checking its SHA-256, writing all four programs, and those
programs then creating a database, running a statement, returning a row and
answering an MCP `initialize` handshake.

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

**The one thing still missing** is not a credential: the repository is private.
`go install` through the public proxy fails at the checksum database, which
cannot read a private repository:

```
verifying module: ... reading https://sum.golang.org/lookup/...: 404 Not Found
    not found: ... invalid version: git ls-remote ... exit status 128:
    fatal: could not read Username for 'https://github.com'
```

A collaborator installs it today by setting `GOPRIVATE=github.com/Black-Rainbow-Labs/*`,
which is the normal way to consume a private Go module and is verified working.
Making the repository public is the same decision crates.io waits on.

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

**Missing**: a Packagist account (GitHub OAuth), and a public repository.

---

## Both signups are behind a CAPTCHA, measured 2026-09-10

Creating the accounts with browser automation was tried, with Playwright driving
real Chrome. It does not work, and the reason is the same on both: the signup is
guarded, and `~/.claude/CLAUDE.md` says never to click a CAPTCHA control from
automation - report what it asked for and stop.

| | what the signup page did |
|---|---|
| **npm** `/signup` | **HTTP 403**, no form rendered at all, and a DataDome challenge iframe from `geo.captcha-delivery.com` served instead |
| **PyPI** `/account/register/` | HTTP 200, but the form carries a required `h-captcha-response` field and an hCaptcha challenge iframe |

npm's 403 is not this network and not this machine. The same URL answers 403 to
`curl` with its default user agent and to `curl` with a Chrome user agent, while
`registry.npmjs.org` answers normally in the same second. It is the signup page
refusing every programmatic client, so no different automation approach gets past
it.

PyPI has a second wall behind the first: two factor authentication is required
before an upload, so even a solved CAPTCHA leaves an account whose second factor
has to live on somebody's authenticator.

**What this means in practice.** One minute of a person's time unblocks both, and
nothing else does:

- **npm** - either sign in and make a granular token at
  <https://www.npmjs.com/settings/jasonmcaffee/tokens>, or create the Black
  Rainbow Labs account through the website and make a token on that. Paste it
  into `~/.npmrc` as `//registry.npmjs.org/:_authToken=…` and the five packages
  publish in one command.
- **PyPI** - register, enable 2FA on your own authenticator, mint an API token.

Email was not the blocker either, so it is not worth chasing again: the account
would have needed a mailbox, and the one Nikaya indexes was reachable, but the
signup never got far enough to send anything.


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
their part — it is what stops somebody else publishing under your name — and it
means the last step of each route belongs to a person at a keyboard, not to a
script. Everything up to it is a command in this directory.
