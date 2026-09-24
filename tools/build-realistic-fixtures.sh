#!/bin/sh
# Builds `compat/fixtures/realistic/*.db` from the SQL beside them, with the
# pinned SQLite 3.53.4 shell.
#
# Usage:
#
#     sh tools/build-realistic-fixtures.sh
#     sh tools/build-realistic-fixtures.sh --check    # say whether they are stale
#
# The fixtures are **checked in**, because `migrate_realistic.rs` needs the same
# bytes on every machine and building them needs a shell not every machine has.
# What is also checked in is the SQL, so the fixture is reproducible from
# something a reviewer can read - which a `.db` in a diff is not.
#
# **The pinned shell, not whatever `sqlite3` is on the path.** A fixture built
# by a different SQLite is a fixture whose page layout, whose FTS5 shadow
# tables and whose `sqlite_stat1` rows are a different version's, and the
# migration test would then be graded against a database this repository has
# never pinned. `tools/sqlite-reference.{ps1,sh}` is what puts it there.

set -e

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(dirname "$HERE")
FIXTURES="$ROOT/compat/fixtures/realistic"
SHELL_DIR="$ROOT/.sqlite-ref/3.53.4/shell"

SQLITE=""
for candidate in "$SHELL_DIR/sqlite3.exe" "$SHELL_DIR/sqlite3"; do
  if [ -x "$candidate" ]; then SQLITE="$candidate"; break; fi
done
if [ -z "$SQLITE" ]; then
  echo "the pinned SQLite shell is not built; run tools/sqlite-reference.ps1 or .sh; skipping" >&2
  exit 0
fi

check=0
[ "$1" = "--check" ] && check=1

stale=0
for source in "$FIXTURES"/*.sql; do
  name=$(basename "$source" .sql)
  target="$FIXTURES/$name.db"
  built="$FIXTURES/$name.db.building"
  rm -f "$built"
  # `-bail`, or a statement that fails leaves a half-built fixture and the
  # script goes on to check its size. The first version did exactly that and
  # left a `.building` file behind after a foreign key refused a row.
  if ! "$SQLITE" -bail "$built" < "$source" > /dev/null; then
    echo "$name.sql did not build; see the error above" >&2
    rm -f "$built"
    exit 1
  fi
  # `VACUUM` so the file is the smallest it can be, and so two builds of the
  # same SQL are the same size: a fixture whose size depended on the order
  # pages happened to be freed in would make every diff of it unreadable.
  "$SQLITE" -bail "$built" "VACUUM;" > /dev/null
  size=$(wc -c < "$built" | tr -d ' ')

  if [ "$check" -eq 1 ]; then
    if [ ! -f "$target" ]; then
      echo "$name.db is not built at all"
      stale=1
    elif ! cmp -s "$built" "$target"; then
      echo "$name.db is not what $name.sql builds"
      stale=1
    else
      echo "$name.db is what $name.sql builds ($size bytes)"
    fi
    rm -f "$built"
    continue
  fi

  if [ "$size" -gt 2097152 ]; then
    echo "$name.db is $size bytes, past the two megabyte ceiling a checked-in fixture has" >&2
    rm -f "$built"
    exit 1
  fi
  mv "$built" "$target"
  echo "wrote $name.db ($size bytes)"
done

exit "$stale"
