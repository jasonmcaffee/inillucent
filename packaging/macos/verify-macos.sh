#!/usr/bin/env bash
#
# Runs a macOS release the way a reader will, and refuses it if anything is off.
#
#   ./packaging/macos/verify-macos.sh --archive dist/inillucent-0.1.1-universal-apple-darwin.tar.gz
#   ./packaging/macos/verify-macos.sh --version 0.1.1        # fetch the published one
#
# This is the release gate. Everything else about a macOS build can be checked
# from Windows - the architectures, the signature, the hardened runtime, the
# timestamp - but whether the program runs cannot be, and neither can whether
# Gatekeeper accepts it. Both are checked here.
#
# It needs nothing installed: curl, shasum, xattr, spctl and codesign are all
# part of macOS. Run it against the published download before the site links to
# it, or against a local archive during a release.

set -euo pipefail

if [ "$(uname -s)" != "Darwin" ]; then
  echo "spctl and codesign are macOS-only; this has to run on a Mac." >&2
  exit 1
fi

archive=""
version=""
base="https://inillucent.com/downloads"
failures=0

while [ $# -gt 0 ]; do
  case "$1" in
    --archive) archive="$2"; shift 2 ;;
    --version) version="$2"; shift 2 ;;
    --base-url) base="$2"; shift 2 ;;
    -h|--help) sed -n '2,17p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

check() {
  # Reports one assertion and remembers a failure without stopping, so one run
  # says everything that is wrong rather than only the first thing.
  local label="$1"; shift
  if "$@" >"$work/out" 2>&1; then
    echo "  ok    $label"
  else
    failures=$((failures + 1))
    echo "  FAIL  $label"
    sed 's/^/          /' "$work/out"
  fi
}

if [ -z "$archive" ]; then
  [ -n "$version" ] || { echo "pass --archive or --version" >&2; exit 2; }
  archive="$work/inillucent-$version-universal-apple-darwin.tar.gz"
  echo "downloading $base/$(basename "$archive")"
  curl -fsSL "$base/$(basename "$archive")" -o "$archive"
  curl -fsSL "$base/SHA256SUMS" -o "$work/SHA256SUMS"

  # A1: the bytes are the published bytes.
  expected="$(grep " $(basename "$archive")\$" "$work/SHA256SUMS" | awk '{print $1}')"
  actual="$(shasum -a 256 "$archive" | awk '{print $1}')"
  if [ "$expected" = "$actual" ]; then
    echo "  ok    A1 sha256 matches SHA256SUMS"
  else
    echo "  FAIL  A1 sha256 $actual, SHA256SUMS says $expected"
    failures=$((failures + 1))
  fi
fi

[ -f "$archive" ] || { echo "$archive does not exist" >&2; exit 1; }

# The quarantine attribute is what a browser download carries, and it is what
# makes Gatekeeper look at the file at all. Setting it by hand is the only way
# to test the path a reader takes.
xattr -w com.apple.quarantine "0081;00000000;verify-macos;" "$archive" 2>/dev/null || true

mkdir -p "$work/unpacked"
tar --directory "$work/unpacked" -xzf "$archive"
inner="$(find "$work/unpacked" -mindepth 1 -maxdepth 1 -type d | head -1)"
[ -n "$inner" ] || { echo "the archive has no directory inside it" >&2; exit 1; }
bin="$inner/bin"

echo
echo "verifying $(basename "$archive")"

# A2: Gatekeeper. This is the assertion that fails when notarisation was missed,
# and the reason the release ends on a Mac.
if spctl -a -vvv -t exec "$bin/inillucent" 2>&1 | tee "$work/spctl" | grep -q 'source=Notarized Developer ID'; then
  echo "  ok    A2 spctl: notarised Developer ID"
else
  echo "  FAIL  A2 spctl did not report a notarised Developer ID"
  sed 's/^/          /' "$work/spctl"
  failures=$((failures + 1))
fi

# A3: every signature is valid and meets its own designated requirement.
for file in "$bin/inillucent" "$bin/inillucent-shell" "$bin/inillucent-mcp" \
            "$bin/inillucent-migrate" "$inner/lib/libinillucent_driver_capi.dylib"; do
  check "A3 codesign $(basename "$file")" codesign --verify --strict --verbose=2 "$file"
done

# A4: it is a database, so the test is a database.
db="$work/verify.rdb"
check "A4 create" "$bin/inillucent" create "$db"
check "A4 CREATE TABLE" "$bin/inillucent" --db "$db" exec "CREATE TABLE note (id INTEGER PRIMARY KEY, body TEXT)"
check "A4 INSERT" "$bin/inillucent" --db "$db" exec "INSERT INTO note VALUES (1,'from the published archive')"
if "$bin/inillucent" --db "$db" query "SELECT body FROM note" 2>&1 | grep -q 'from the published archive'; then
  echo "  ok    A4 SELECT returned the row"
else
  echo "  FAIL  A4 SELECT did not return the row"
  failures=$((failures + 1))
fi

# A5: the MCP server answers, because half of what this ships is for an agent.
# The whole lifecycle: the server refuses `tools/list` with -32002 until
# `initialize` and `notifications/initialized` have both arrived, and refuses an
# `initialize` that carries no `protocolVersion`.
#
# Read into a variable rather than piped into `grep -q`. `grep -q` stops at its
# first match and closes the pipe, the server's next write fails and it exits
# non-zero, and `set -o pipefail` then reports the pipeline as failed however
# well the server answered.
listed="$(printf '%s\n%s\n%s\n' \
     '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"verify-macos","version":"1"}}}' \
     '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
     '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' \
   | "$bin/inillucent-mcp" --db "$db" 2>/dev/null || true)"
case "$listed" in
  *'"tools"'*)
    echo "  ok    A5 inillucent-mcp listed its tools"
    ;;
  *)
    echo "  FAIL  A5 inillucent-mcp did not list its tools"
    failures=$((failures + 1))
    ;;
esac

# A6: the x86-64 half, under Rosetta. On an Apple silicon Mac this is the only
# test of that slice, and it is the one that catches a bad Intel build.
if [ "$(uname -m)" = "arm64" ]; then
  if arch -x86_64 "$bin/inillucent" --version >/dev/null 2>&1; then
    echo "  ok    A6 the x86-64 slice runs under Rosetta"
  else
    echo "  FAIL  A6 the x86-64 slice did not run (is Rosetta installed?)"
    failures=$((failures + 1))
  fi
else
  check "A6 the arm64 slice runs" arch -arm64 "$bin/inillucent" --version
fi

# A7: the C ABI, which is how every binding that is not Rust reaches the engine.
check "A7 the dylib loads" /usr/bin/python3 -c "
import ctypes, sys
ctypes.CDLL('$inner/lib/libinillucent_driver_capi.dylib')
"

echo
if [ "$failures" -eq 0 ]; then
  echo "all checks passed"
else
  echo "$failures check(s) failed - do not publish this build" >&2
  exit 1
fi
