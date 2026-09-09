#!/usr/bin/env bash
# Builds the three SQLite fixtures the performance gates read.
#
#   ./tools/build-gate-fixtures.sh [output directory]
#
# The fixtures are not checked in: they are 17 MB, 94 MB and 600 MB, and they are
# built by the pinned SQLite 3.53.4 shell so that both arms of a comparison start
# from a file SQLite itself wrote. Run tools/sqlite-reference.sh (or .ps1 on
# Windows) first to put that shell in .sqlite-ref/.
#
# Each gate run needs its OWN COPY of a fixture. The schema.index workload leaves
# an index behind on the SQLite arm, so a second run against the same file stops
# on `index main_label already exists`. Copy the file per run:
#
#   ./tools/build-gate-fixtures.sh /tmp/fixtures
#   cp /tmp/fixtures/medium.db /tmp/fixtures/medium-run1.db
#   target/release/inillucent-fullgate /tmp/fixtures/medium-run1.db --scale medium \
#       --rounds 30 --page-size 32768 --frames 4096
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${1:-$ROOT/_agent_output/gate-fixtures}"

# The pinned shell, wherever this platform put it. INILLUCENT_SQLITE_SHELL wins,
# which is how a machine with the reference somewhere else runs this unchanged.
SQ="${INILLUCENT_SQLITE_SHELL:-}"
if [ -z "$SQ" ]; then
  for candidate in \
    "$ROOT/.sqlite-ref/3.53.4/shell/sqlite3.exe" \
    "$ROOT/.sqlite-ref/3.53.4/shell/sqlite3" \
    "$ROOT/.sqlite-ref/3.53.4/sqlite3.exe" \
    "$ROOT/.sqlite-ref/3.53.4/sqlite3"; do
    if [ -x "$candidate" ]; then SQ="$candidate"; break; fi
  done
fi
if [ -z "$SQ" ] || [ ! -x "$SQ" ]; then
  echo "the pinned SQLite 3.53.4 shell is not built." >&2
  echo "run tools/sqlite-reference.sh (Linux, macOS) or tools/sqlite-reference.ps1 (Windows)," >&2
  echo "or set INILLUCENT_SQLITE_SHELL to a 3.53.4 shell." >&2
  exit 1
fi

mkdir -p "$OUT"

# name : main rows : side rows : wide rows
for scale in small:5000:1250:100 medium:100000:25000:400 large:600000:150000:2000; do
  IFS=: read -r name rows side wide <<<"$scale"
  echo "--- $name: $rows main rows, $side side rows, $wide wide rows ---"
  cat > "$OUT/build-$name.sql" <<SQL
PRAGMA page_size=4096;
PRAGMA journal_mode=delete;
PRAGMA synchronous=full;
CREATE TABLE main_table(id INTEGER PRIMARY KEY, key INTEGER NOT NULL, category INTEGER NOT NULL, label TEXT NOT NULL, payload BLOB);
CREATE INDEX main_key ON main_table(key);
CREATE INDEX main_category ON main_table(category, key);
CREATE TABLE side_table(id INTEGER PRIMARY KEY, owner INTEGER NOT NULL, note TEXT);
CREATE INDEX side_owner ON side_table(owner);
CREATE TABLE wide(id INTEGER PRIMARY KEY, body TEXT);
CREATE TABLE digits(n INTEGER PRIMARY KEY);
INSERT INTO digits(n) VALUES (0),(1),(2),(3),(4),(5),(6),(7),(8),(9);
INSERT INTO main_table(id, key, category, label, payload)
  SELECT seq, (seq * 2654435761) % $rows, seq % 64,
         'row ' || seq || ' lorem ipsum dolor sit amet consectetur', zeroblob(48)
  FROM (SELECT (((((d0.n * 10 + d1.n) * 10 + d2.n) * 10 + d3.n) * 10 + d4.n) * 10 + d5.n) + 1 AS seq
        FROM digits d0, digits d1, digits d2, digits d3, digits d4, digits d5)
  WHERE seq <= $rows;
INSERT INTO side_table(id, owner, note)
  SELECT seq, ((seq * 7) % $rows) + 1, 'note ' || seq
  FROM (SELECT ((((d0.n * 10 + d1.n) * 10 + d2.n) * 10 + d3.n) * 10 + d4.n) + 1 AS seq
        FROM digits d0, digits d1, digits d2, digits d3, digits d4)
  WHERE seq <= $side;
INSERT INTO wide(id, body)
  SELECT seq, replace(hex(zeroblob(2048)), '0', 'x')
  FROM (SELECT ((d0.n * 10 + d1.n) * 10 + d2.n) + 1 AS seq FROM digits d0, digits d1, digits d2)
  WHERE seq <= $wide;
ANALYZE;
SQL
  rm -f "$OUT/$name.db"
  "$SQ" "$OUT/$name.db" < "$OUT/build-$name.sql"
done

echo
echo "built in $OUT:"
ls -la "$OUT"/*.db
