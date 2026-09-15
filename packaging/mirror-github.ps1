<#
.SYNOPSIS
    Builds the public mirror commit for a release, and checks that the mirror
    holds the released code rather than something that looks like it.

.DESCRIPTION
    Releases are cut from `jasonmcaffee/inillucent`, which has 335 commits of
    development history and a `tasks/` directory full of ticket numbers. The
    public repository is `Black-Rainbow-Labs/Inillucent`, and it holds one
    commit per release instead. Every published URL names it: the Go module
    path, the npm `repository` and `bugs` fields, the PyPI project URLs, the
    PHP installer's error message and the Packagist submission.

    The two have no commit in common - `git merge-base` between them is empty -
    and until this script there was nothing that said how the second is made
    from the first, so nobody could tell whether a tag on the mirror named the
    code that was released under it.

    It turns out they always did. `v0.1.1` on the mirror and `v0.1.1` in the
    development repository have the same tree, 97e516e8, and the mirror's `main`
    carries the tree of development commit fe5a101 exactly. The mirror was built
    by copying a whole tree and committing it as one commit, which is the right
    thing; it was just done by hand and never written down.

    So this script does that, from the tag, with the equality checked rather
    than assumed:

        the mirror commit's tree IS the release tag's tree.

    `git commit-tree` takes the tree straight off the tag, so there is no copy
    step for anything to go wrong in. The result is a pure function of the tree,
    the parent, the message and the author, which means running this twice
    produces the same commit hash and anybody holding both repositories can
    recompute it.

    Nothing is pushed unless you pass -Push. The push is the act that publishes,
    and it is a decision rather than a step.

.PARAMETER Version
    The release to mirror, without the leading v. The tag v<Version> has to
    exist in this repository and is where the tree comes from.

.PARAMETER Remote
    The git remote for the public repository. Default brl.

.PARAMETER Message
    The mirror commit's subject. Defaults to the release commit's own subject
    with a leading `task-NNNN: ` removed, which is what the ten commits already
    on the mirror do.

.PARAMETER Push
    Push the branch and the tag to the remote. Without it the script builds the
    commit, checks it, and prints the two commands.

.PARAMETER Verify
    Check the mirror as it stands and exit. For every v<x.y.z> tag the remote
    has, it reports whether that tag's tree is the same tree as the tag of the
    same name here. Builds nothing and pushes nothing.

.EXAMPLE
    pwsh packaging/mirror-github.ps1 -Verify
    pwsh packaging/mirror-github.ps1 -Version 0.1.2
    pwsh packaging/mirror-github.ps1 -Version 0.1.2 -Push
#>
[CmdletBinding()]
param(
    [string] $Version,
    [string] $Remote = 'brl',
    [string] $Message,
    [switch] $Push,
    [switch] $Verify
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot

# --------------------------------------------------------------------------
# git, asked for one thing at a time.
#
# Every call goes through here with -C $root so the script never depends on the
# directory it was started from, and so a failure names the command that failed
# instead of leaving a caller to guess from an empty string.
#
# All three read $args rather than declaring a parameter, and they have to. A
# function with a param block is an advanced function, PowerShell gives it the
# common parameters, and it then refuses `git commit-tree -p <parent>` with
# "the parameter name 'p' is ambiguous. Possible matches include:
# -ProgressAction, -PipelineVariable". A simple function binds nothing and hands
# every argument through as written, which is what passing a command line to
# another program needs.
# --------------------------------------------------------------------------

<#
.SYNOPSIS
    Runs a git command in this repository and returns its output as one string
    with LF line endings.

.DESCRIPTION
    Out-String joins with CRLF whatever git produced, so a caller that splits the
    result on LF gets a carriage return on the end of every line but the last.
    A regex anchored with $ then matches only that last line, which is a check
    that reads one row of its input and reports the other rows as absent. The
    carriage returns come off here, once, rather than at each call site.
.PARAMETER Arguments
    The git arguments, without the leading git.
#>
function Invoke-Git {
    $arguments = $args
    $output = & git -C $root @arguments 2>&1
    if ($LASTEXITCODE -ne 0) {
        throw "git $($arguments -join ' ') failed with $LASTEXITCODE`n$output"
    }
    return (($output | Out-String) -replace "`r", '').TrimEnd("`n")
}

<#
.SYNOPSIS
    Runs a git command and returns its output as an array of non-empty lines.
.PARAMETER Arguments
    The git arguments, without the leading git.
#>
function Invoke-GitLines {
    $arguments = $args
    $text = Invoke-Git @arguments
    if (-not $text) { return @() }
    return @($text -split "`n" | Where-Object { $_.Trim().Length -gt 0 })
}

<#
.SYNOPSIS
    Runs a git command and returns its output, or $null when it fails, for the
    questions whose answer is allowed to be "there is no such thing".
.PARAMETER Arguments
    The git arguments, without the leading git.
#>
function Invoke-GitOrNull {
    $arguments = $args
    $output = & git -C $root @arguments 2>&1
    if ($LASTEXITCODE -ne 0) { return $null }
    return (($output | Out-String) -replace "`r", '').TrimEnd("`n")
}

<#
.SYNOPSIS
    The tree object a ref points at, or $null when the ref does not resolve.
.PARAMETER Ref
    Any ref, tag or commit.
#>
function Get-TreeOf {
    param([string] $Ref)
    return Invoke-GitOrNull rev-parse --verify --quiet "$Ref^{tree}"
}

<#
.SYNOPSIS
    The release tags the remote holds, newest version first.

.DESCRIPTION
    Read off the remote rather than out of the local tag list, because the local
    list carries tags from both repositories and cannot say which remote has
    which. Only v<x.y.z> is a release tag here; the packages/go/ ones name a Go
    module version and are handled by the Go route.

    **Both kinds of tag count.** `ls-remote` prints an annotated tag twice, as
    `refs/tags/X` and again as `refs/tags/X^{}` for the commit inside it, and a
    lightweight tag only once. Matching the `^{}` line alone therefore finds
    annotated tags and reports every lightweight one as absent - which is what
    this did to v0.1.2 in the minute after pushing it.
.PARAMETER RemoteName
    The git remote to ask.
#>
function Get-RemoteReleaseTags {
    param([string] $RemoteName)
    $tags = @()
    foreach ($line in (Invoke-GitLines ls-remote --tags $RemoteName)) {
        if ($line -match 'refs/tags/(v\d+\.\d+\.\d+)(\^\{\})?$') { $tags += $Matches[1] }
    }
    return @($tags | Sort-Object -Unique)
}

<#
.SYNOPSIS
    Reports, for one release tag, whether the remote's copy names the same tree
    as this repository's copy.

.DESCRIPTION
    This is the whole claim the mirror makes. A tag whose tree differs is a tag
    that says it is a release and is not one, and it is invisible from the
    outside because the files look right and the commit hash was never expected
    to match.
.PARAMETER Tag
    The tag name, for instance v0.1.2.
.PARAMETER RemoteName
    The git remote holding the mirror.
#>
function Test-MirrorTag {
    param([string] $Tag, [string] $RemoteName)

    $here = Get-TreeOf -Ref $Tag
    $thereRef = Invoke-GitOrNull rev-parse --verify --quiet "refs/mirror-check/$RemoteName/$Tag^{tree}"

    if (-not $here) {
        return [pscustomobject]@{ Tag = $Tag; State = 'not released here'; Here = ''; There = $thereRef }
    }
    if (-not $thereRef) {
        return [pscustomobject]@{ Tag = $Tag; State = 'not on the mirror'; Here = $here; There = '' }
    }
    $state = if ($here -eq $thereRef) { 'same tree' } else { 'DIFFERENT TREE' }
    return [pscustomobject]@{ Tag = $Tag; State = $state; Here = $here; There = $thereRef }
}

<#
.SYNOPSIS
    Fetches the remote's branches and tags into refs this script owns, so
    comparing against the mirror never touches the local tags.

.DESCRIPTION
    The local tag v0.1.1 already names a commit in the development history.
    Fetching the mirror's tag of the same name into refs/tags would either be
    refused or would overwrite it, and overwriting it would silently change what
    a later release verifies against. Everything from the mirror lands under
    refs/mirror-check/<remote>/ instead.
.PARAMETER RemoteName
    The git remote to fetch.
#>
function Sync-MirrorRefs {
    param([string] $RemoteName)
    Invoke-Git fetch --quiet --force $RemoteName "+refs/heads/*:refs/mirror-check/$RemoteName/heads/*" | Out-Null
    Invoke-Git fetch --quiet --force $RemoteName "+refs/tags/*:refs/mirror-check/$RemoteName/*" | Out-Null
}

<#
.SYNOPSIS
    The subject line to put on the mirror commit.

.DESCRIPTION
    The mirror's existing ten commits are the development subjects with the
    ticket number taken off the front - "task-1836: the GitHub release is
    v0.1.1" became "The GitHub release is v0.1.1". A ticket number means nothing
    to a reader who cannot open the board, so the same rule applies here, and
    the first letter is put back into upper case after the prefix is removed.
.PARAMETER Ref
    The release commit whose subject to take.
#>
function Get-MirrorSubject {
    param([string] $Ref)
    $subject = Invoke-Git log -1 --format=%s $Ref
    $subject = $subject -replace '^task-\d+:\s*', ''
    if ($subject.Length -gt 0) {
        $subject = $subject.Substring(0, 1).ToUpperInvariant() + $subject.Substring(1)
    }
    return $subject
}

# --------------------------------------------------------------------------

if (-not (Get-Command git -ErrorAction SilentlyContinue)) { throw 'git is not on PATH' }

$remoteUrl = Invoke-GitOrNull remote get-url $Remote
if (-not $remoteUrl) {
    throw "there is no remote called $Remote. git remote -v lists the ones there are."
}

Write-Host "mirror remote  $Remote  $remoteUrl"
Sync-MirrorRefs -RemoteName $Remote

if ($Verify) {
    $tags = Get-RemoteReleaseTags -RemoteName $Remote
    if (-not $tags) { Write-Host 'the mirror has no release tags'; exit 0 }

    Write-Host ''
    $bad = 0
    foreach ($tag in $tags) {
        $result = Test-MirrorTag -Tag $tag -RemoteName $Remote
        Write-Host ("  {0,-10} {1}" -f $result.Tag, $result.State)
        if ($result.State -ne 'same tree') { $bad++ }
    }

    # The tags this repository has released and the mirror does not carry. A
    # release the mirror never got is the failure this file exists to make
    # visible, so it is listed rather than left to be noticed.
    $localReleases = Invoke-GitLines tag --list 'v*' | Where-Object { $_ -match '^v\d+\.\d+\.\d+$' }
    $missing = @($localReleases | Where-Object { $tags -notcontains $_ })
    if ($missing) {
        Write-Host ''
        foreach ($tag in $missing) { Write-Host ("  {0,-10} released here, not on the mirror" -f $tag) }
        $bad += @($missing).Count
    }

    Write-Host ''
    if ($bad -gt 0) { Write-Host "$bad tag(s) to reconcile"; exit 1 }
    Write-Host 'every release tag on the mirror holds the released tree'
    exit 0
}

if (-not $Version) { throw 'pass -Version <x.y.z>, or -Verify to check the mirror as it stands.' }

$tag = "v$Version"
$tree = Get-TreeOf -Ref $tag
if (-not $tree) { throw "$tag does not exist here. The release tag is where the tree comes from." }

$releaseCommit = Invoke-Git rev-parse "$tag^{commit}"
Write-Host "release        $tag  $releaseCommit"
Write-Host "tree           $tree"

# Already mirrored, and mirrored correctly, is a success rather than an error:
# this script is run again whenever anybody is unsure, and it should say so.
$existing = Invoke-GitOrNull rev-parse --verify --quiet "refs/mirror-check/$Remote/$tag^{tree}"
if ($existing -eq $tree) {
    Write-Host ''
    Write-Host "$tag is already on $Remote and holds this exact tree. Nothing to do."
    exit 0
}
if ($existing) {
    throw "$Remote already has $tag, and it holds tree $existing rather than $tree. " +
          'A published tag is not moved. Cut a new version instead.'
}

$parent = Invoke-GitOrNull rev-parse --verify --quiet "refs/mirror-check/$Remote/heads/main"
if (-not $parent) { throw "$Remote has no main branch to build on." }
Write-Host "mirror main    $parent"

if (-not $Message) { $Message = Get-MirrorSubject -Ref $releaseCommit }
Write-Host "message        $Message"

# The author is the release commit's, date and all, so the mirror's history
# lines up with the release history and so this commit is reproducible: the same
# tree, parent, message and author always hash to the same commit.
$authorName  = Invoke-Git log -1 --format=%an $releaseCommit
$authorEmail = Invoke-Git log -1 --format=%ae $releaseCommit
$authorDate  = Invoke-Git log -1 --format=%aI $releaseCommit

$env:GIT_AUTHOR_NAME     = $authorName
$env:GIT_AUTHOR_EMAIL    = $authorEmail
$env:GIT_AUTHOR_DATE     = $authorDate
$env:GIT_COMMITTER_NAME  = $authorName
$env:GIT_COMMITTER_EMAIL = $authorEmail
$env:GIT_COMMITTER_DATE  = $authorDate

$commit = Invoke-Git commit-tree $tree -p $parent -m $Message

# The check this file exists for. commit-tree cannot in fact produce a commit
# with a different tree than the one it was handed, which is exactly why the
# tree is taken from the tag rather than copied into a working directory - but
# a guard that can only pass is still what makes the claim checkable by someone
# reading the output, and it costs one command.
$built = Invoke-Git rev-parse "$commit^{tree}"
if ($built -ne $tree) { throw "the commit that was built holds tree $built rather than $tree" }

# An annotated tag, not a lightweight one.
#
# v0.1.0 and v0.1.1 on the mirror are annotated, so a lightweight v0.1.2 would
# be the odd one out, and an annotated tag is the only kind that records who
# tagged it and when. `git tag` can only write refs/tags/<name>, and this
# repository already has a local v<version> naming a commit in the development
# history, so the object is written with `git mktag` and put under a ref this
# script owns instead.
$tagObject = @"
object $commit
type commit
tag $tag
tagger $authorName <$authorEmail> $(Invoke-Git log -1 --format=%at $releaseCommit) +0000

inillucent $Version
"@ -replace "`r", ''
$tagSha = ($tagObject | & git -C $root mktag)
if ($LASTEXITCODE -ne 0) { throw 'git mktag refused the tag object' }
$tagSha = $tagSha.Trim()

Invoke-Git update-ref "refs/mirror/$Remote/main" $commit | Out-Null
Invoke-Git update-ref "refs/mirror/$Remote/$tag" $tagSha | Out-Null

# The tag has to name the commit that was just built, and a tag object that
# names something else is a tag nobody would look inside.
$tagged = Invoke-Git rev-parse "$tagSha^{commit}"
if ($tagged -ne $commit) { throw "the tag object names commit $tagged rather than $commit" }

Write-Host ''
Write-Host "built          $commit"
Write-Host "               tree $built, which is $tag's tree"
Write-Host "tag object     $tagSha, annotated, naming $commit"
Write-Host ''

if (-not $Push) {
    Write-Host 'Nothing has been pushed. These two commands publish it:'
    Write-Host ''
    Write-Host "  git push $Remote refs/mirror/$Remote/main:refs/heads/main"
    Write-Host "  git push $Remote refs/mirror/$Remote/${tag}:refs/tags/$tag"
    Write-Host ''
    Write-Host 'Then cut the GitHub release against that tag, and attach the four'
    Write-Host 'files from dist/ that SHA256SUMS names.'
    exit 0
}

Invoke-Git push $Remote "refs/mirror/$Remote/main:refs/heads/main" | Out-Null
Write-Host "pushed         $Remote main -> $commit"
Invoke-Git push $Remote "refs/mirror/$Remote/${tag}:refs/tags/$tag" | Out-Null
Write-Host "pushed         $Remote $tag"

Sync-MirrorRefs -RemoteName $Remote
$after = Invoke-Git rev-parse "refs/mirror-check/$Remote/$tag^{tree}"
if ($after -ne $tree) { throw "after pushing, $Remote's $tag holds tree $after rather than $tree" }
Write-Host ''
Write-Host "$Remote $tag holds tree $tree, read back off the remote"
