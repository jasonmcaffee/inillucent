# Both sites are part of the release, and a release is not done until they prove it

inillucent.com and unluminous.com are where a person actually gets the software. A release that
reaches GitHub and the registries and leaves a site behind is a release most people cannot get, and
neither product notices today. This says what "the site is correct for this release" means, makes it
the same idea for both products, and makes publishing fail when it is not true.

## What is wrong now

**unluminous.com's update manifest describes Windows only.** `/releases/latest.json` carries
`installer`, `installerBytes` and `installerSha256`, and nothing about macOS. The in-app update check
reads that manifest before it reads anything else, and blackrainbowlabs.com prints the version from
it. So a Mac running Unluminous is told about a Windows installer, and the only reason the macOS
archive is reachable at all is that the download page happens to link it.

**Neither site is checked platform by platform.** inillucent's `Test-SiteVersion` reads
`downloads/VERSION` and confirms every name in the published `SHA256SUMS` resolves. That catches a
missing file; it does not catch a **download page that stopped offering a platform**, because the
page is not consulted. unluminous checks the manifest's version and HEADs the macOS zip - so a
Windows installer that failed to copy would pass.

**Nothing compares what is served against what was built.** A HEAD returning 200 says a file is
there. It does not say it is this release's file. A stale artifact of the right name passes every
check both products currently make.

**The version lives on more than one surface per site and only one is read.** inillucent has
`downloads/VERSION` and the `version` field in the page's data. unluminous has the manifest and the
page. A publish that writes one and not the other is invisible.

## What a correct site means

One definition, both products:

> A release declares the platforms it ships. For each declared platform the site must **link** an
> artifact, **serve** it, and serve **this release's bytes**; and every place the site states a
> version must state the released one.

"This release's bytes" is decided by the checksum the release already publishes - `SHA256SUMS` for
inillucent, the manifest's own `installerSha256` and the new `macosSha256` for unluminous. Both are
written from the built artifact, so comparing the served file against them closes the loop between
what was built and what a stranger downloads.

## The platforms each product declares

| | inillucent | unluminous |
|---|---|---|
| Windows x86-64 | `.zip` | `UnluminousSetup-<v>-x64.exe` |
| macOS universal | `.pkg` **and** `.tar.gz` | `Unluminous-<v>-macos.zip` |
| Linux x86-64 | `.tar.gz`, `.deb`, `.rpm` | not built |
| Linux aarch64 | `.tar.gz`, `.deb`, `.rpm` | not built |

unluminous has no Linux target and this does not invent one: the table is what the product builds,
and a platform that is not built is absent from the table rather than reported missing on every
release. If unluminous gains a Linux build the row is added here and the check follows.

## The changes

### 1. unluminous's manifest gains macOS

`latest.json` gains `macos`, `macosBytes` and `macosSha256` beside the Windows three, written from
the archive the release just built and notarised. Absent when a release has no macOS archive, rather
than stale - a manifest naming the previous release's zip is worse than one naming none, because the
update check would offer it.

### 2. One check per product, run in the publish phase

A function that takes the release version and the platform table and returns the first thing that is
wrong, or nothing:

- the page links an artifact for each declared platform
- each artifact answers 200
- each artifact's SHA-256 matches the published checksum
- every version the site states equals the release

For inillucent this replaces the body of `Test-SiteVersion`, so the `site` route already calls it.
For unluminous it replaces the two `Test-Destination` blocks with one that covers both platforms.

Hashing means downloading. inillucent's nine artifacts are about 240 MB, which is not worth pulling
on every release, so the check compares **`Content-Length` against the built artifact's size** for
every platform and hashes the **smallest artifact per platform** in full. A truncated or stale file
changes its length; a file of exactly the right length and wrong contents is not a failure mode any
of this has produced, and the full `SHA256SUMS` is published for anyone who wants certainty.

### 3. Failing the route, not warning

`Test-SiteVersion` returning a reason already fails the `site` route and shows in the report.
unluminous's `Test-Destination` only prints `NOT THERE` in red and carries on, so a release that
missed the site still ends with "Unluminous 0.54.0 is released". It gets an exit code: any
destination that is not there makes the script exit non-zero after printing the whole report, so the
report is still complete and the failure is not silent.

## How each change is proven

| Change | Proof |
|---|---|
| macOS in the manifest | fetch `/releases/latest.json` after a release and read `macosSha256`; compare against the local archive's hash |
| inillucent's platform check | remove one artifact from the staged site, run the check, confirm it names that platform; put it back |
| unluminous's platform check | same, against a staged copy |
| the exit code | run unluminous's release with `-SkipSite` on a version the site does not have and confirm a non-zero exit |
| no false failure | run both checks against the current live sites, which are correct, and get no complaint |

## What this deliberately does not do

**It does not publish the sites differently.** inillucent already stages, links and deploys;
unluminous already publishes its page and manifest. The gap is that neither proves the result, and
proving it is the whole of this.

**It does not add a Linux build to unluminous.** That is a product decision and a new packaging
target, not a site check.

**It does not verify the macOS `.pkg` installs.** No Mac is involved in this release and none is
involved here; the signature and the notarisation are already checked at build time by `rcodesign`,
and `packaging/verify-installs.sh` reads the signature off the published file.
