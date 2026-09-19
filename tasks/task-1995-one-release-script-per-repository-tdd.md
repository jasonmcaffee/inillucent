# task-1995 — one build and deploy script per repository

## Introduction

Both products can now be built for every platform on the Windows machine. What neither has is **one
command that ships them.** inillucent's release is ten routes spread across eight scripts and a
2,000 line prose document that says which to run in what order. unluminous has a single script
already, but the macOS half it gained today is behind a flag, and nothing at the end of either says
what actually reached a user.

This is not a tidiness argument. Two faults measured in this repository today are what the design
below exists to prevent, and both are on disk right now.

**A release that stopped half way and stayed there.** `CHANGELOG.md` says of 0.1.3:

> **Tagged and not published.** The GitHub release is a draft waiting on the Linux archives.

The tag was cut on 2026-09-15. Four days later inillucent.com still serves 0.1.2, the GitHub release
is still a draft, and the entry that explains the hole had to be written after the fact "because a
reader assumes it is a mistake in their checkout". Nothing failed loudly. The person running the
release simply reached the end of what they remembered.

**A version that drifted across the manifests that carry it.** The version is written in six places
and they no longer agree:

| file | says | but names |
|---|---|---|
| `Cargo.toml` `[workspace.package]` | 0.1.4 | — |
| `packages/npm/inillucent/package.json` | `"version": "0.1.4"` | its five platform packages at **0.1.2** |
| `packages/python/pyproject.toml` | 0.1.4 | — |
| `packaging/homebrew/inillucent.rb` | `version "0.1.4"` | archives at **0.1.2** in every `url` |
| `inillucent.com/downloads/VERSION` | **0.1.2** | — |

The npm one is a live defect: installing `inillucent@0.1.4` resolves platform packages at 0.1.2, so
the wrapper and the binary it loads are different releases. Nothing catches this, because no single
thing writes all six.

## Goals and non-goals

**Goals.** Each is checkable, and the script checks it.

| # | Goal | How it is checked |
|---|---|---|
| G1 | One command releases inillucent, every route | `pwsh packaging/ship.ps1` |
| G2 | One command releases unluminous, every route | `pwsh tools/release.ps1` |
| G3 | A route with no credential is skipped and named, never a silent omission and never a hard failure | the final report lists every route with `published`, `skipped` or `failed` and a reason |
| G4 | Every file that carries the version is written by the same step | `-WhatIf` prints the six paths and their before and after |
| G5 | A missing credential stops the run **before** anything is tagged or pushed | the preflight runs first and mutates nothing |
| G6 | A late failure is resumable without rebuilding | `-Only site,npm` and `-Skip tests,notarise` |
| G7 | An agent working in either repository finds the script without being told | a section at the top of `AGENTS.md` in both, and in unluminous's `CLAUDE.md` |
| G8 | Nothing about the existing scripts' behaviour changes | they keep their parameters; the new script calls them |

**Non-goals.**

- Obtaining the credentials that ten routes are waiting on. That is `task-1996`, and this design's
  answer to a missing one is to skip the route and say so.
- Replacing the scripts that do the work. `release-all.ps1`, `package-linux.ps1`, `publish-site.ps1`,
  `cargo-publish.ps1`, `build.mjs` and the rest stay exactly as they are and keep working on their
  own. The new script is an orchestrator, not a rewrite.
- CI. `task-1968` removed the GitHub workflows and there is none; this runs on the machine.

## 1. The shape both scripts share

Five phases, in this order, and the order is the design:

```
1. preflight   read every credential and tool, decide which routes can run, print the plan.
               Mutates nothing. A route that cannot run is decided HERE, not half way through.
2. version     write the new version into every file that carries it, in one step.
3. build       compile, sign, package, notarise. Local only; nothing has left the machine yet.
4. publish     tag, push, GitHub, the site, the registries. The first phase that is visible.
5. report      a table: route, outcome, and for a skip, the reason and what would fix it.
```

**Why preflight is separate and first.** The 0.1.3 failure is a release that pushed a tag and then
ran out of things it could do. A tag is the one step that cannot be taken back quietly, so nothing
reaches it until the script knows which routes will run. A run that can only publish three of ten
routes still runs — it says so at the start and again at the end.

**Why a skip is not a failure.** Eight of inillucent's ten routes are waiting on a token today. A
script that refuses to run without all of them is a script that can never run, so it would be
bypassed by hand, and running it by hand is the fault this replaces. A skip is loud, named, and
carries the one sentence that would fix it.

**`-WhatIf` on both**, printing the same plan the run would follow, with every path it would write.

## 2. inillucent: `packaging/ship.ps1`

The name is new because `packaging/release.ps1` already means "stage one archive" and
`release-all.ps1` means "build every target". Neither is the top, and reusing either name would make
two things called the same. Every other script's header gains a line pointing at this one.

| route | what runs | needs | today |
|---|---|---|---|
| build | `release-all.ps1 -Targets all` | zig, cargo-zigbuild, MSVC for the Windows target | works |
| macOS sign + notarise | `macos/release-macos.ps1` | the Developer ID certificates and the notary key | works, as of today |
| `.deb` / `.rpm` | `linux/package-linux.ps1` | `INILLUCENT_GPG_KEY` | skipped, no key |
| checksums | `stage-layout.ps1`'s `Update-Sha256Sums` | — | works |
| signature | `sign-sums.ps1` | `INILLUCENT_MINISIGN_KEY` | skipped, no key |
| tag and push | `git` | a writable remote | works |
| GitHub release | `gh release create` | `gh`, signed in | works |
| public mirror | `mirror-github.ps1 -Push -Verify` | the `brl` remote | works |
| inillucent.com | `publish-site.ps1 -Stage` then `-Link` | the site checkout | works |
| crates.io | `cargo-publish.ps1 -Execute` | a token | skipped, no token |
| npm | `packages/npm/build.mjs --publish` | a token | skipped, no token |
| PyPI | `packages/python/build.py --publish` | a token | skipped, no token |
| Go | a `packages/go/vX.Y.Z` tag | the remote | works |
| Homebrew | `homebrew/update.sh --tap` | the tap checkout | skipped, no tap |

**The site is staged and linked in two calls with the artifacts verified in between**, which is the
existing design and is kept: a download link pointing at an artifact nobody has checked is worse than
no link.

**The version step writes all six paths named in the introduction**, and refuses to continue if it
finds a seventh it does not know about — a `grep` for the old version across the tracked tree that
reports anything it did not write itself. That is what stops this drifting again.

## 3. unluminous: `tools/release.ps1`, extended

It is already the single script, and what it needs is three changes rather than a rewrite.

1. **macOS becomes part of a release rather than a flag.** `-Macos` was added today and defaults off,
   because the SDK and the certificate may be absent. Defaulting off means the next release forgets
   it. So it runs **whenever its prerequisites are present** and is skipped, named, with the reason,
   when they are not — the same rule as every inillucent route. `-SkipMacos` forces it off.
2. **A preflight**, for the same reason: the credentials are read and the plan printed before the
   version is bumped, so a release cannot tag and then discover it cannot publish.
3. **A final report.** Today it prints progress and stops. Two sites, two GitHub repositories and
   two platforms is six destinations, and after a release nobody can say from the output which of
   the six actually changed.

`installer/macos/build-on-windows.ps1` already reports what it checked and what it could not, and
that text becomes part of the release's report rather than scrolling past mid-run.

## 4. What "verified" means here

Not "the script exits 0". Each route is verified by asking the destination, which is what
`publish.ps1` on the unluminous site already does and what the rest will do:

| route | the check |
|---|---|
| site | `GET` the published artifact and the page; compare the served size and hash to `dist/` |
| GitHub | `gh release view` lists the expected assets |
| npm / PyPI / crates.io | the registry's own metadata endpoint answers with the new version |
| Go | `proxy.golang.org/.../@latest` answers the new version |
| macOS | Apple's notary answered `Accepted`, and the ticket is in the artifact |

A route whose check fails is `failed` in the report, not `published`, even when the command that ran
it exited 0. `tools/check-public-urls.mjs` already does exactly this for nine URLs with no
credential, and it is reused rather than rewritten.

## 5. Making an agent find it

A script nobody knows about is the same as no script. Both repositories get a short section at the
**top** of `AGENTS.md`, above everything else, because that file is what an agent reads first:

> **Releasing: one command.** `pwsh packaging/ship.ps1` builds every target, signs and notarises
> macOS, writes the version into all six manifests, tags, pushes, publishes the GitHub release, the
> mirror, inillucent.com and every registry a token exists for, and prints what reached each one.
> Do not run the individual scripts in `packaging/` by hand; they are what it calls.

unluminous's `CLAUDE.md` already says "when the work is done and verified, run the release"; that
paragraph gains the macOS sentence and the report.

## 6. Verification plan

1. `-WhatIf` on both, from a clean checkout: the plan names every route and every path, and nothing
   is written.
2. The preflight with a credential deliberately absent: the route is listed as skipped with its
   reason, and the run still proceeds.
3. A real inillucent release end to end, and the checks in §4 run against the live site, the live
   GitHub release and the live module proxy.
4. A real unluminous release end to end, with macOS included by prerequisite rather than by flag.
5. The version step's own check: after a bump, a `grep` for the previous version across the tracked
   tree finds only the changelog and the history.
