#!/usr/bin/env bash
#
# Builds the release archive every installer and every package is made from.
# The POSIX half of packaging/release.ps1: same layout, same names, same
# SHA256SUMS, so a macOS or Linux archive is interchangeable with a Windows one
# everywhere downstream.
#
#   ./packaging/release.sh                       # host target
#   ./packaging/release.sh --target aarch64-apple-darwin
#   ./packaging/release.sh --skip-build          # stage what is already built
#   ./packaging/release.sh --smoke-only          # build, stage, install and run
#
# On macOS, packaging/macos/build-pkg.sh calls this twice - once per
# architecture - and lipo's the results into a universal binary. That is why
# --target is a parameter rather than being read from the host every time.
#
# WHAT IT REFUSES, AND WHY THAT IS THE POINT (task-1894)
#
# Before task-1894 this staged whatever was in the working tree and labelled it
# with whatever --version said. A release could be built from a dirty checkout,
# named for a version its binaries do not answer with, tagged at a different
# commit, and shipped without anybody opening the archive. None of that was
# detectable afterwards.
#
# So a release refuses unless the checkout is clean, the tag v<version> points
# at HEAD, the compiler is the pinned one, and the staged archive installs into
# an empty directory and answers. Each refusal has a named override, because a
# refusal nobody can get past is a refusal somebody deletes - and every override
# used is recorded in provenance.json, which SHA256SUMS covers.

set -uo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
version=""
target=""
skip_build=0
smoke_only=0
skip_smoke=0
allow_dirty=0
allow_untagged=0
allow_version_mismatch=0
waived=()

while [ $# -gt 0 ]; do
  case "$1" in
    --version) version="$2"; shift 2 ;;
    --target) target="$2"; shift 2 ;;
    --skip-build) skip_build=1; shift ;;
    --smoke-only) smoke_only=1; shift ;;
    --skip-smoke) skip_smoke=1; shift ;;
    --allow-dirty) allow_dirty=1; shift ;;
    --allow-untagged) allow_untagged=1; shift ;;
    --allow-version-mismatch) allow_version_mismatch=1; shift ;;
    -h|--help)
      sed -n '2,24p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
      exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

# Stops the release unless an override was passed, and records the waiver.
#
# $1 - 1 when the operator passed the override
# $2 - the waiver's name, recorded in the provenance
# $3 - what is wrong, and which flag gets past it
deny_unless() {
  if [ "$1" -ne 1 ]; then
    echo "release refused: $3" >&2
    exit 1
  fi
  echo "warning: $3 (waived with --$2)" >&2
  waived+=("$2")
}

# The version is the workspace's, so an archive can never claim a number the
# binaries inside it do not.
manifest_version="$(sed -n '/^\[workspace\.package\]/,/^\[/p' "$root/Cargo.toml" \
  | sed -n 's/^version *= *"\(.*\)"/\1/p' | head -1)"
[ -n "$manifest_version" ] || { echo "Cargo.toml declares no [workspace.package] version" >&2; exit 1; }
if [ -z "$version" ]; then
  version="$manifest_version"
fi

if [ -z "$target" ]; then
  target="$(rustc -vV | sed -n 's/^host: //p')"
fi

# **The version comes from one place, and an override has to agree with it.**
# `--version 2.0.0` on a tree whose binaries answer 0.1.0 produced an archive
# named for a release that did not exist, and nothing downstream could tell.
if [ "$version" != "$manifest_version" ]; then
  deny_unless "$allow_version_mismatch" allow-version-mismatch \
    "--version $version does not match the workspace manifest's $manifest_version, so the archive would be named for a version its binaries do not answer with."
fi

commit=""
if [ "$smoke_only" -eq 0 ]; then
  commit="$(git -C "$root" rev-parse HEAD 2>/dev/null || true)"
  [ -n "$commit" ] || { echo "a release has to be made from a git checkout, and this is not one" >&2; exit 1; }

  # A dirty checkout means the commit the archive claims is not the code inside
  # it, which makes every other check here a check on the wrong thing.
  if [ -n "$(git -C "$root" status --porcelain)" ]; then
    deny_unless "$allow_dirty" allow-dirty \
      "the working tree has uncommitted changes, so the commit this release records is not the code it contains. Commit or stash first."
  fi

  # The tag is what a person downloading the archive resolves back to source.
  tagged="$(git -C "$root" rev-list -n 1 "v$version" 2>/dev/null || true)"
  if [ -z "$tagged" ]; then
    deny_unless "$allow_untagged" allow-untagged \
      "there is no tag v$version, so nothing in the repository resolves this archive back to a commit."
  elif [ "$tagged" != "$commit" ]; then
    deny_unless "$allow_untagged" allow-untagged \
      "the tag v$version names $tagged and HEAD is $commit, so the archive would carry a version whose tag points at different code."
  fi

  # The pinned compiler is what every published number was produced with.
  pinned="$(sed -n 's/^channel *= *"\(.*\)"/\1/p' "$root/rust-toolchain.toml" 2>/dev/null | head -1)"
  running="$(rustc --version | cut -d' ' -f2)"
  if [ -n "$pinned" ] && [ -n "$running" ] && [ "$pinned" != "$running" ]; then
    deny_unless "$allow_dirty" allow-dirty \
      "rust-toolchain.toml pins $pinned and this is rustc $running, so the release would not be the build the repository grades itself against."
  fi

  [ "$skip_build" -eq 0 ] || waived+=("skip-build")
fi

echo "inillucent $version for $target"

built="$root/target/release"
if [ "$skip_build" -eq 0 ]; then
  echo "building (release, locked)..."
  # `--features inillucent-cli/embed` is what makes `embed(TEXT)` answer in a
  # shipped binary. Without it `inillucent setup-embeddings all` downloads 620 MB
  # of ONNX Runtime and weights that the program which downloaded them cannot
  # use, and `docs/embeddings.md`'s own first example answers
  # `no such function: embed`. That was true of every release up to 0.1.1. It
  # costs 3.2 MB of binary and nothing at run time: `ort` links `load-dynamic`,
  # so a machine with no runtime installed still runs every command that does
  # not embed.
  cargo build --manifest-path "$root/Cargo.toml" --release --locked \
    --target "$target" \
    --features inillucent-cli/embed \
    -p inillucent-cli -p inillucent-migrate -p inillucent-driver-capi
  # `--target` moves the output under target/<triple>/release, and omitting it
  # does not. Both are handled rather than one being assumed, because the macOS
  # packaging always passes a triple and a developer running this by hand never
  # does.
  built="$root/target/$target/release"
fi

name="inillucent-$version-$target"
dist="$root/dist"
stage="$dist/$name"
rm -rf "$stage"
mkdir -p "$stage/bin" "$stage/lib" "$stage/include"

for program in inillucent inillucent-shell inillucent-mcp inillucent-migrate; do
  [ -f "$built/$program" ] || { echo "the build did not produce $built/$program" >&2; exit 1; }
  cp "$built/$program" "$stage/bin/"
done

# The C ABI, which is how every language that is not Rust reaches the engine.
copied_library=0
for library in libinillucent_driver_capi.dylib libinillucent_driver_capi.so inillucent_driver_capi.dll; do
  if [ -f "$built/$library" ]; then
    cp "$built/$library" "$stage/lib/"
    copied_library=1
  fi
done
[ "$copied_library" -eq 1 ] || { echo "the build produced no C ABI shared library" >&2; exit 1; }

cp "$root/drivers/inillucent-driver-capi/include/inillucent_driver.h" "$stage/include/"
cp "$root/README.md" "$stage/"
cp "$root/drivers/README.md" "$stage/DRIVER.md"

# README.md links into docs/ for every subject it does not cover itself, so the
# archive carries that directory or the front page it ships is full of dead
# links. AGENTS.md and agent-skills/ travel with it for the same reason: the
# readme sends an AI agent to both.
cp -R "$root/docs" "$stage/docs"
cp "$root/AGENTS.md" "$stage/"
cp -R "$root/agent-skills" "$stage/agent-skills"

# Two documents live under tests/ rather than under docs/, because they describe
# assets that sit beside them. README.md and three of the docs/ pages link to
# both, so they travel as well. Only the prose: the fixtures and the schedules
# are not part of a binary archive.
mkdir -p "$stage/tests"
cp "$root/tests/synthetic-corpus.md" "$stage/tests/"
cp "$root/tests/inillucent-testing-tdd.md" "$stage/tests/"

printf '%s' "$version" > "$stage/VERSION"
if [ -f "$root/LICENSE" ]; then
  cp "$root/LICENSE" "$stage/"
else
  echo "warning: LICENSE is missing; the archive will not carry one" >&2
fi

# --------------------------------------------------------------------------
# The smoke test: install what was staged, into an empty directory, and use it.
#
# **This is the check the review asked for and the one this script did not
# have.** Everything above it verifies that files were produced; this is the
# only part that finds out whether they work. It runs against the *staged copy*
# rather than against target/release, because what a person downloads is the
# staged copy - a library found by being beside the build would pass a test run
# in the build directory and fail on their machine.
# --------------------------------------------------------------------------

# Compiles a C program against the shipped header and links the shipped library.
#
# $1 - the installed layout
# $2 - a scratch directory to build in
smoke_capi() {
  local installed="$1" scratch="$2"
  if ! command -v cc >/dev/null 2>&1; then
    # A missing compiler is a check that could not run, and it is said out loud
    # rather than skipped quietly: CI installs one, so this line appearing
    # there means the CI image changed.
    echo "warning: smoke: no C compiler, so the shipped header and library were not linked" >&2
    waived+=("no-c-compiler")
    return 0
  fi
  cat > "$scratch/smoke.c" <<'CSOURCE'
#include <stdio.h>
#include "inillucent_driver.h"

/* The smallest program that proves the shipped header and library agree: open,
 * connect, run, read one value back, and close in the documented order. */
int main(void)
{
    inillucent_db *db = NULL;
    inillucent_conn *conn = NULL;
    inillucent_rows *rows = NULL;
    inillucent_error *error = NULL;
    if (inillucent_open("smoke-capi.rdb", INILLUCENT_OPEN_CREATE, &db, &error) != INILLUCENT_OK) {
        printf("open failed\n");
        return 1;
    }
    if (inillucent_connect(db, &conn, &error) != INILLUCENT_OK) {
        printf("connect failed\n");
        return 1;
    }
    if (inillucent_execute(conn, "SELECT 41 + 1", 10, &rows, &error) != INILLUCENT_OK) {
        printf("query failed\n");
        return 1;
    }
    if (inillucent_value_int(rows, 0, 0) != 42) {
        printf("wrong answer\n");
        return 1;
    }
    inillucent_rows_free(rows);
    inillucent_conn_free(conn);
    inillucent_close(db, &error);
    printf("capi ok\n");
    return 0;
}
CSOURCE
  if ! cc -O0 -I "$installed/include" "$scratch/smoke.c" \
      -L "$installed/lib" -linillucent_driver_capi \
      -Wl,-rpath,"$installed/lib" -o "$scratch/smoke_capi"; then
    echo "the shipped header and library did not compile and link" >&2
    return 1
  fi
  local said
  said="$("$scratch/smoke_capi" 2>&1)" || true
  case "$said" in
    *"capi ok"*) : ;;
    *) echo "the C program built from the shipped archive did not run: $said" >&2; return 1 ;;
  esac
}

# Installs the staged layout into a scratch directory and exercises it.
#
# $1 - the staged layout
# $2 - the version the binaries should answer with
smoke() {
  local stage="$1" want="$2"
  local scratch
  scratch="$(mktemp -d)"
  # Copied rather than run in place: an installed tree is what a person gets,
  # and a binary that only works beside its build directory works for nobody.
  cp -R "$stage" "$scratch/inillucent"
  local installed="$scratch/inillucent"
  local cli="$installed/bin/inillucent"

  # 1. The command line answers, and answers with the version on the tin.
  local reported
  reported="$("$cli" --version 2>&1)" || { echo "the installed CLI did not run: $reported" >&2; return 1; }
  case "$reported" in
    *"$want"*) : ;;
    *) echo "the installed CLI reports '$reported', not $want - the archive is labelled for a build it does not contain" >&2; return 1 ;;
  esac

  # 2. A database is created, written, reopened and read. Reopened, because a
  #    write that never reached the file passes every check that does not close
  #    the database first.
  ( cd "$scratch" && ./inillucent/bin/inillucent --db smoke.rdb exec 'CREATE TABLE t (n INTEGER, s TEXT)' ) >/dev/null || return 1
  ( cd "$scratch" && ./inillucent/bin/inillucent --db smoke.rdb exec "INSERT INTO t (n, s) VALUES (1, 'one'), (2, 'two')" ) >/dev/null || return 1
  local counted
  counted="$(cd "$scratch" && ./inillucent/bin/inillucent --db smoke.rdb query 'SELECT count(*) FROM t' --output json)" || return 1
  case "$counted" in
    *2*) : ;;
    *) echo "the installed CLI read back '$counted' rather than 2 rows" >&2; return 1 ;;
  esac
  ( cd "$scratch" && ./inillucent/bin/inillucent --db smoke.rdb integrity-check ) >/dev/null \
    || { echo "the installed database failed its integrity check" >&2; return 1; }

  # 3. The MCP server initializes and answers a tool call, which is the surface
  #    an agent is handed and the one nothing downstream tests.
  #
  #    The database is named *relatively*, from the scratch directory, for two
  #    reasons. An absolute path goes into a JSON string, where a Windows
  #    separator is an escape sequence rather than a separator. And a relative
  #    name is what a person's first use looks like, so it is the case worth
  #    checking.
  local answered
  # The whole lifecycle, because the server enforces it: `initialize` needs
  # `protocolVersion`, `capabilities` and `clientInfo`, and every other method
  # is refused with -32002 until `notifications/initialized` has arrived.
  answered="$(cd "$scratch" && printf '%s\n%s\n%s\n%s\n' \
    '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"release-smoke","version":"1"}}}' \
    '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
    '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' \
    '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"inillucent_query","arguments":{"db":"smoke.rdb","sql":"SELECT count(*) FROM t"}}}' \
    | ./inillucent/bin/inillucent-mcp)"
  case "$answered" in
    *inillucent_query*) : ;;
    *) echo "the installed MCP server did not list its tools" >&2; return 1 ;;
  esac
  case "$answered" in
    *'"isError":false'*) : ;;
    *) echo "the installed MCP server refused a query it should have answered" >&2; return 1 ;;
  esac

  # 4. The shipped header and shared library compile and link, which is what
  #    every binding that is not Rust does with this archive.
  smoke_capi "$installed" "$scratch" || return 1
  rm -rf "$scratch"
}

if [ "$skip_smoke" -eq 0 ]; then
  echo "smoking the staged archive..."
  smoke "$stage" "$version" || { echo "the staged archive did not pass its smoke test" >&2; exit 1; }
  echo "smoke ok"
else
  waived+=("skip-smoke")
fi

if [ "$smoke_only" -eq 1 ]; then
  echo
  echo "staged  $stage"
  echo "smoke only: no archive, no checksums, no provenance"
  exit 0
fi

archive="$dist/$name.tar.gz"
rm -f "$archive"
tar --directory "$dist" -czf "$archive" "$name"

# One SHA256SUMS over everything in dist, rewritten each time, so an installer
# verifies what it downloaded against one file.
sums="$dist/SHA256SUMS"
: > "$sums"
for file in "$dist"/*.tar.gz "$dist"/*.zip; do
  [ -f "$file" ] || continue
  if command -v sha256sum >/dev/null 2>&1; then
    (cd "$dist" && sha256sum "$(basename "$file")") >> "$sums"
  else
    # macOS ships shasum rather than sha256sum, and its output has the two
    # fields the other way round.
    (cd "$dist" && shasum -a 256 "$(basename "$file")") >> "$sums"
  fi
done

# --------------------------------------------------------------------------
# Provenance: what this archive is, and what was checked before it existed.
#
# **It records the waivers.** A release made with --allow-dirty is a legitimate
# thing to want on a bad afternoon and an illegitimate thing to forget, so the
# file says so and SHA256SUMS covers the file. A provenance that recorded only
# the happy path would be true of every release and say nothing about any.
# --------------------------------------------------------------------------
digest() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  else
    shasum -a 256 "$1" | cut -d' ' -f1
  fi
}

waived_json=""
for name_of in ${waived[@]+"${waived[@]}"}; do
  waived_json="$waived_json\"$name_of\","
done
waived_json="${waived_json%,}"

held() {
  # Reports whether a waiver was used, as a JSON boolean.
  #
  # $1 - the waiver's name
  local wanted="$1" one
  for one in ${waived[@]+"${waived[@]}"}; do
    [ "$one" = "$wanted" ] && { printf 'false'; return; }
  done
  printf 'true'
}

provenance="$dist/provenance.json"
cat > "$provenance" <<PROV
{
  "version": "$version",
  "target": "$target",
  "commit": "$commit",
  "tag": "v$version",
  "toolchain": "$(rustc --version)",
  "built_at": "$(date -u +%Y-%m-%dT%H:%M:%SZ)",
  "archive": {
    "name": "$name.tar.gz",
    "sha256": "$(digest "$archive")"
  },
  "checks": {
    "clean_checkout": $(held allow-dirty),
    "tag_matches_head": $(held allow-untagged),
    "version_agrees": $(held allow-version-mismatch),
    "built_from_source": $(held skip-build),
    "installed_and_run": $(held skip-smoke),
    "c_abi_linked": $(held no-c-compiler)
  },
  "waived": [$waived_json]
}
PROV

# The provenance goes into SHA256SUMS as well, so a downloader who verified the
# archive has also verified the claims made about it.
if command -v sha256sum >/dev/null 2>&1; then
  (cd "$dist" && sha256sum provenance.json) >> "$sums"
else
  (cd "$dist" && shasum -a 256 provenance.json) >> "$sums"
fi

echo
echo "staged     $stage"
echo "archive    $archive"
echo "provenance $provenance"
echo "sums       $sums"
if [ "${#waived[@]}" -gt 0 ] 2>/dev/null; then
  echo "warning: this release waived: ${waived[*]}" >&2
fi
cat "$sums"
