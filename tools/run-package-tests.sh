#!/bin/sh
# Runs every test the language packages own, as `packages/package-tests.toml`
# names them.
#
# `inillucent-testrun` builds and runs cargo targets; these are a Node test
# file, a Go test, a PHP script and two Python scripts, and none of them is one.
# So this is the one command that runs them, and
# `crates/inillucent-compat/tests/bindings.rs` is what fails when a file under
# `packages/` is not in the manifest this reads.
#
# Usage:
#
#     sh tools/run-package-tests.sh
#     INILLUCENT_STRICT=1 sh tools/run-package-tests.sh
#
# It needs a built command line and a built C ABI. When `INILLUCENT_BIN` and
# `INILLUCENT_DRIVER_LIB` are not set it looks in `target/debug` and then
# `target/release`, and says what to build when it finds neither.
#
# A row whose interpreter is absent prints a `; skipping` line and is counted.
# Under `INILLUCENT_STRICT=1` a skip makes the whole run exit non-zero, which is
# the same rule `inillucent-testrun --strict` applies to the cargo tiers: a run
# on a machine with nothing provisioned must not read as a pass.

set -e

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(dirname "$HERE")
MANIFEST="$ROOT/packages/package-tests.toml"

if [ ! -f "$MANIFEST" ]; then
  echo "$MANIFEST is not there" >&2
  exit 1
fi

# The built programs, if the caller did not name them.
if [ -z "$INILLUCENT_BIN" ]; then
  for profile in debug release; do
    for suffix in .exe ""; do
      candidate="$ROOT/target/$profile/inillucent$suffix"
      if [ -x "$candidate" ]; then INILLUCENT_BIN="$candidate"; break 2; fi
    done
  done
fi
if [ -z "$INILLUCENT_DRIVER_LIB" ]; then
  for profile in debug release; do
    for name in inillucent_driver_capi.dll libinillucent_driver_capi.so libinillucent_driver_capi.dylib; do
      candidate="$ROOT/target/$profile/$name"
      if [ -f "$candidate" ]; then INILLUCENT_DRIVER_LIB="$candidate"; break 2; fi
    done
  done
fi
export INILLUCENT_BIN INILLUCENT_DRIVER_LIB
[ -n "$INILLUCENT_BIN" ] && echo "command line: $INILLUCENT_BIN"
[ -n "$INILLUCENT_DRIVER_LIB" ] && echo "C ABI:        $INILLUCENT_DRIVER_LIB"

ran=0
skipped=0
failed=0
failures=""

# Each `command = "..."` line, in the order the manifest gives them. A row's
# path is printed beside it so a failure names the file rather than the verb.
paths=$(grep '^path = ' "$MANIFEST" | sed 's/^path = "//; s/"$//')
commands=$(grep '^command = ' "$MANIFEST" | sed 's/^command = "//; s/"$//')

count=$(printf '%s\n' "$paths" | wc -l | tr -d ' ')
at=1
while [ "$at" -le "$count" ]; do
  path=$(printf '%s\n' "$paths" | sed -n "${at}p")
  command=$(printf '%s\n' "$commands" | sed -n "${at}p")
  at=$((at + 1))
  [ -z "$command" ] && continue

  printf '\n=== %s\n    %s\n' "$path" "$command"
  output=$(cd "$ROOT" && sh -c "$command" 2>&1) && code=0 || code=$?
  printf '%s\n' "$output" | tail -5

  if printf '%s' "$output" | grep -q '; skipping'; then
    skipped=$((skipped + 1))
    continue
  fi
  if [ "$code" -eq 0 ]; then
    ran=$((ran + 1))
  else
    failed=$((failed + 1))
    failures="$failures
  $path (exit $code)"
  fi
done

printf '\n%d ran, %d skipped, %d failed\n' "$ran" "$skipped" "$failed"
if [ "$failed" -gt 0 ]; then
  printf 'these package tests failed:%s\n' "$failures" >&2
  exit 1
fi
if [ "$skipped" -gt 0 ] && [ "$INILLUCENT_STRICT" = "1" ]; then
  printf '%d package tests skipped, and --strict counts a skip\n' "$skipped" >&2
  exit 1
fi
exit 0
