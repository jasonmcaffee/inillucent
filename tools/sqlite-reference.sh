#!/usr/bin/env bash
# Downloads, verifies, and builds the pinned SQLite 3.53.4 oracle on POSIX.
#
# The oracle is a separate child process compiled from the official amalgamation.
# It is the only form in which SQLite appears in this workspace, and nothing here
# is a production dependency: the artifacts land in the gitignored .sqlite-ref/
# directory and no inillucent crate links against them.
#
# Every download is checked against the SHA3-256 sum SQLite publishes, using
# inillucent's own implementation, so a corrupted or substituted archive cannot
# become the thing every parity claim is measured against.
#
# Usage: tools/sqlite-reference.sh [--force]

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
version="3.53.4"
release="3530400"
ref_dir="$root/.sqlite-ref/$version"
src_dir="$ref_dir/src"
force="${1:-}"

mkdir -p "$ref_dir"

fetch() {
  local name="$1" url="$2" target="$ref_dir/$1"
  if [ -f "$target" ] && [ "$force" != "--force" ]; then
    echo "have $name"
  else
    echo "downloading $name"
    curl -fsSL "$url" -o "$target"
  fi
  ( cd "$root" && cargo run --quiet -p inillucent-compat --bin inillucent-manifest -- verify-artifact "$name" "$target" )
}

fetch "sqlite-amalgamation-$release.zip" "https://sqlite.org/2026/sqlite-amalgamation-$release.zip"
fetch "sqlite-tools-linux-x64-$release.zip" "https://sqlite.org/2026/sqlite-tools-linux-x64-$release.zip"

# unzip is not installed everywhere, and this has to work on a machine where
# the agent cannot install packages, so python3's zipfile stands in for it.
extract() {
  local archive="$1" destination="$2"
  mkdir -p "$destination"
  if command -v unzip >/dev/null 2>&1; then
    ( cd "$destination" && unzip -q -o "$archive" )
  else
    python3 -c "import sys, zipfile; zipfile.ZipFile(sys.argv[1]).extractall(sys.argv[2])" "$archive" "$destination"
  fi
}

if [ ! -d "$src_dir" ] || [ "$force" = "--force" ]; then
  rm -rf "$src_dir" "$ref_dir/sqlite-amalgamation-$release"
  extract "$ref_dir/sqlite-amalgamation-$release.zip" "$ref_dir"
  mv "$ref_dir/sqlite-amalgamation-$release" "$src_dir"
fi
# Keyed on this platform's own binary rather than on the directory. The two
# scripts share `shell/`, so testing the directory made the pair
# order-dependent: whichever ran first created it and the other skipped its
# extraction, leaving a checkout with the Windows tools and no Linux ones -
# which is a differential suite whose oracle cannot open a database, reported
# as fifteen failing attach tests rather than as a missing file.
if [ ! -x "$ref_dir/shell/sqlite3" ] || [ "$force" = "--force" ]; then
  extract "$ref_dir/sqlite-tools-linux-x64-$release.zip" "$ref_dir/shell"
  chmod +x "$ref_dir/shell/sqlite3" 2>/dev/null || true
fi

echo "building the oracle driver"
cc -O2 -o "$ref_dir/sqlite-oracle" \
  -DSQLITE_ENABLE_FTS5 \
  -DSQLITE_ENABLE_RTREE \
  -DSQLITE_ENABLE_MATH_FUNCTIONS \
  -DSQLITE_ENABLE_COLUMN_METADATA \
  -DSQLITE_ENABLE_PREUPDATE_HOOK \
  -DSQLITE_ENABLE_SESSION \
  -DSQLITE_ENABLE_DBSTAT_VTAB \
  -DSQLITE_THREADSAFE=1 \
  -I "$src_dir" \
  "$root/compat/oracle/sqlite_driver.c" \
  "$src_dir/sqlite3.c" \
  -lm -lpthread

echo "building the amalgamation object the ABI probes link against"
cc -O2 -c -o "$ref_dir/sqlite3.o"   -DSQLITE_ENABLE_FTS5   -DSQLITE_ENABLE_RTREE   -DSQLITE_ENABLE_MATH_FUNCTIONS   -DSQLITE_ENABLE_COLUMN_METADATA   -DSQLITE_ENABLE_PREUPDATE_HOOK   -DSQLITE_ENABLE_SESSION   -DSQLITE_ENABLE_DBSTAT_VTAB   -DSQLITE_THREADSAFE=1   -I "$src_dir"   "$src_dir/sqlite3.c"

echo "building the benchmark driver"
cc -O2 -o "$ref_dir/sqlite-bench" \
  -DSQLITE_ENABLE_FTS5 \
  -DSQLITE_ENABLE_RTREE \
  -DSQLITE_ENABLE_MATH_FUNCTIONS \
  -DSQLITE_ENABLE_COLUMN_METADATA \
  -DSQLITE_ENABLE_PREUPDATE_HOOK \
  -DSQLITE_ENABLE_SESSION \
  -DSQLITE_ENABLE_DBSTAT_VTAB \
  -DSQLITE_THREADSAFE=1 \
  -I "$src_dir" \
  "$root/compat/oracle/sqlite_bench.c" \
  "$src_dir/sqlite3.c" \
  -lm -lpthread

echo "oracle: $ref_dir/sqlite-oracle"
echo "bench: $ref_dir/sqlite-bench"
echo "set INILLUCENT_SQLITE_ORACLE=$ref_dir/sqlite-oracle to run the differential tests"
