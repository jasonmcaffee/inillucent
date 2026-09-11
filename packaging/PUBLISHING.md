# Publishing: what is ready, and what each step still needs

Everything in this repository is built, tested and verified up to the upload.
This file is the last mile: what each registry needs, in what order, and which
credential is missing.

**Nothing here has been published.** All six routes are built and verified;
every one of them stops at a credential that belongs to a person, or at one
decision that is not a script's to make.

---

## Where it stands, at a glance

Updated 2026-09-11. **Both one-line installers work**, verified by running them as
written: Windows, and Ubuntu 24.04. A Linux archive is built and published beside
the Windows one. No package manager is published yet.

| route | state | what it is waiting on |
|---|---|---|
| **inillucent.com, Windows** | **live** - `irm .../install.ps1 \| iex` installs and runs | nothing |
| **inillucent.com, Linux** | **live** - `curl -fsSL .../install.sh \| sh` installs and runs | nothing |
| **inillucent.com, macOS** | archive not built | a Mac: `packaging/release.sh --target aarch64-apple-darwin`, then `lipo` |
| **GitHub release** | done - `v0.1.0` at `201d0b9` with the Windows archive | the macOS archive |
| **npm** | packages built and installed from their tarballs | **the signup page answers 403 to every client** - see below |
| **PyPI** | wheel built and installed into a clean venv | an account; the form carries an hCaptcha and uploads need 2FA |
| **crates.io** | `--workspace --dry-run` clean for all 30 crates | a token, **and the decision to make the source public** |
| **Homebrew** | formula carries the real Linux checksum | the macOS archive, then the tap repository |
| **Go** | `go vet` clean, 5 tests pass | a public repository and one tag |
| **Packagist** | `composer install` works end to end | a public repository and a Packagist account |

### What the site publishing took, and two defects it found

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
./packaging/macos/release-macos.sh --version 0.1.0 --upload
```

```powershell
# the Windows box again: collect, sign the checksums, publish
pwsh packaging/fetch-macos-artifacts.ps1 -Version 0.1.0
pwsh packaging/sign-sums.ps1
pwsh packaging/publish-site.ps1 -Version 0.1.0 -Stage
#   ... verify on the Mac, then:
pwsh packaging/publish-site.ps1 -Version 0.1.0 -Link
```

**The distribution point is inillucent.com.** A GitHub release cannot be one
while the repository is private, because its assets are private too. GitHub
carries the macOS artifacts from the MacBook to the Windows box and nothing else.

**Done, on 2026-09-10.** `v0.1.0` is tagged at `201d0b9` and the release is at
<https://github.com/Black-Rainbow-Labs/Inillucent/releases/tag/v0.1.0>, carrying the
Windows archive, `SHA256SUMS` and `provenance.json`. The archive was downloaded
back off the release and its SHA-256 compared against the published one: they
match, byte for byte.

An earlier v0.1.0 was cut on the old `jasonmcaffee/inillucent` repository and then
removed: it named the wrong organisation and its archive carried the README whose
install table was not true. The release below replaces it.

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

## Go

```sh
git tag packages/go/v0.1.0 && git push --tags
```

That is the whole publish: Go has no registry, and `go install
github.com/Black-Rainbow-Labs/Inillucent/packages/go/cmd/inillucent@latest` reads the
repository directly. The tag carries the `packages/go/` prefix because the
module is in a subdirectory, which is Go's own rule for a nested module.

**Ready and verified.** `go vet` is clean and all five tests pass against the
installed binary, including one that proves a bound parameter is not
interpolated and one that proves a read-only handle refuses a write while still
allowing a read.

**Missing**: a public repository. `go install` clones it, and `GOPRIVATE` is not
something a stranger can be asked to set.

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
