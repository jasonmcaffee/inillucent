#!/usr/bin/env bash
#
# Checks a Linux release the way the machines that will run it do.
#
#   ./tools/release-verify-linux.sh --version 0.1.0
#   ./tools/release-verify-linux.sh --version 0.1.0 --no-containers
#
# Runs from WSL or from any Linux machine, against the archives in dist/. Two
# kinds of check:
#
#   * ones that read the binary - the glibc floor and the shared libraries it
#     needs - which are what decide whether it starts on a distribution nobody
#     here has;
#   * ones that run it, in this shell and, when Docker is available, in a
#     container of each distribution the floor claims to cover.
#
# The glibc floor is the assertion that matters most and the one that is easiest
# to break by accident: building on this machine's WSL rather than through
# cargo-zigbuild raises it from 2.28 to 2.39, which refuses to start on Debian
# 12, Ubuntu 22.04 and RHEL 9, and nothing about the build says so.

set -uo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
version=""
floor="2.28"
containers=1
failures=0

while [ $# -gt 0 ]; do
  case "$1" in
    --version) version="$2"; shift 2 ;;
    --glibc-floor) floor="$2"; shift 2 ;;
    --no-containers) containers=0; shift ;;
    -h|--help) sed -n '2,21p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

if [ -z "$version" ]; then
  version="$(sed -n '/^\[workspace\.package\]/,/^\[/p' "$root/Cargo.toml" \
    | sed -n 's/^version *= *"\(.*\)"/\1/p' | head -1)"
fi

pass() { echo "  ok    $1"; }
fail() { echo "  FAIL  $1"; failures=$((failures + 1)); }

# Compares two dotted versions and answers whether the first is at most the
# second, which is the question "does this binary run on that glibc".
at_most() {
  [ "$(printf '%s\n%s\n' "$1" "$2" | sort -V | head -1)" = "$1" ]
}

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

for triple in x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu; do
  archive="$root/dist/inillucent-$version-$triple.tar.gz"
  echo
  echo "== $triple"
  if [ ! -f "$archive" ]; then
    fail "L0 $archive does not exist"
    continue
  fi

  unpacked="$work/$triple"
  mkdir -p "$unpacked"
  tar --directory "$unpacked" -xzf "$archive"
  stage="$unpacked/inillucent-$version-$triple"
  bin="$stage/bin"

  # L1: the glibc floor. objdump reads the versioned symbol references the
  # linker recorded, and the highest of them is the oldest glibc that can load
  # the binary.
  highest="$(objdump -T "$bin/inillucent" 2>/dev/null \
    | grep -o 'GLIBC_[0-9.]*' | sed 's/GLIBC_//' | sort -V | tail -1)"
  if [ -z "$highest" ]; then
    fail "L1 could not read the glibc symbol versions"
  elif at_most "$highest" "$floor"; then
    pass "L1 needs at most glibc $floor (highest reference is $highest)"
  else
    fail "L1 needs glibc $highest, which is newer than the $floor floor"
  fi

  # L2: nothing outside the C library. A release that picked up a dependency
  # from the build machine is a release that will not start elsewhere.
  unexpected="$(objdump -p "$bin/inillucent" 2>/dev/null | awk '/NEEDED/ { print $2 }' \
    | grep -vE '^(libc|libm|libdl|libpthread|librt|ld-linux.*)\.so' || true)"
  if [ -z "$unexpected" ]; then
    pass "L2 links only the C library"
  else
    fail "L2 also needs: $(echo "$unexpected" | tr '\n' ' ')"
  fi

  # L3: the modes survived the archive. Built on Windows, where there is no
  # execute bit, so this is set by tar rather than read from disk.
  if [ -x "$bin/inillucent" ]; then
    pass "L3 the programs are executable"
  else
    fail "L3 $bin/inillucent is not executable ($(stat -c '%A' "$bin/inillucent"))"
  fi

  # L4: it runs, here, if this machine can run this architecture.
  if [ "$(uname -m)" = "$(echo "$triple" | cut -d- -f1)" ]; then
    db="$work/$triple.rdb"
    if "$bin/inillucent" create "$db" >/dev/null 2>&1 \
      && "$bin/inillucent" --db "$db" exec "CREATE TABLE note (id INTEGER PRIMARY KEY, body TEXT)" >/dev/null 2>&1 \
      && "$bin/inillucent" --db "$db" exec "INSERT INTO note VALUES (1,'verified')" >/dev/null 2>&1 \
      && "$bin/inillucent" --db "$db" query "SELECT body FROM note" 2>/dev/null | grep -q verified; then
      pass "L4 create, insert and select returned the row"
    else
      fail "L4 the database round trip failed"
    fi

    # The whole lifecycle: the server refuses `tools/list` with -32002 until
    # `initialize` and `notifications/initialized` have both arrived, and
    # refuses an `initialize` that carries no `protocolVersion`.
    #
    # Read into a variable rather than piped into `grep -q`. `grep -q` stops at
    # its first match and closes the pipe, the server's next write fails and it
    # exits non-zero, and `set -o pipefail` then reports the pipeline as failed
    # however well the server answered.
    listed="$(printf '%s\n%s\n%s\n' \
        '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"release-verify","version":"1"}}}' \
        '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
        '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' \
      | "$bin/inillucent-mcp" --db "$db" 2>/dev/null)"
    case "$listed" in
      *'"tools"'*) pass "L5 inillucent-mcp listed its tools" ;;
      *) fail "L5 inillucent-mcp did not list its tools" ;;
    esac
  else
    echo "  skip  L4/L5 this machine is $(uname -m), not $(echo "$triple" | cut -d- -f1)"
  fi

  # L6: the distributions the floor claims. A container each, because the claim
  # is about machines nobody here has.
  if [ "$containers" -eq 1 ] && command -v docker >/dev/null 2>&1 \
     && docker info >/dev/null 2>&1 && [ "$triple" = "x86_64-unknown-linux-gnu" ]; then
    for image in debian:10 rockylinux:8 ubuntu:22.04 ubuntu:24.04; do
      if docker run --rm -v "$stage:/inillucent:ro" "$image" \
           /inillucent/bin/inillucent --version >/dev/null 2>&1; then
        pass "L6 starts on $image"
      else
        fail "L6 does not start on $image"
      fi
    done
  elif [ "$containers" -eq 1 ] && [ "$triple" = "x86_64-unknown-linux-gnu" ]; then
    echo "  skip  L6 Docker is not running, so the older distributions were not tried"
  fi
done

echo
if [ "$failures" -eq 0 ]; then
  echo "all checks passed"
else
  echo "$failures check(s) failed - do not publish this build" >&2
  exit 1
fi
