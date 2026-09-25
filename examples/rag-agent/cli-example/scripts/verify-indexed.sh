#!/usr/bin/env bash
# The same ten questions as verify.sh, asked through an HNSW index.
#
# `verify.sh` asks the committed database, which deliberately has no vector
# index, so it never exercises the probe at all. This builds one over a copy and
# asks the same ten through it. Every question still has to come back with the
# Wikipedia article its answer lives in.
#
# It exists because of the defect it would have caught. Until task-1911,
# `CREATE INDEX ... USING inillucent_hnsw` built an index that held every row in
# the session that built it and none the next time the file was opened, and an
# empty vector index answers zero rows rather than failing - so building the
# index the documentation tells you to build made the documented search return
# nothing, with nothing failing and nothing logged. `verify.sh` stayed green
# throughout, because the corpus it asks has no index on it.
#
#   INILLUCENT=/path/to/inillucent scripts/verify-indexed.sh
#
# The copy goes in the system temporary directory and is removed at the end. The
# committed database is never written to.
set -uo pipefail

here="$(cd "$(dirname "$0")/.." && pwd)"
cd "$here"
cli="${INILLUCENT:-inillucent}"
source_database="greek-philosophy.rdb"
database="${TMPDIR:-/tmp}/inillucent-rag-indexed-$$.rdb"
failures=0

cleanup() {
  rm -f "$database"
}
trap cleanup EXIT

cp "$source_database" "$database"

# Reads a one-row, one-column answer as plain text: the aligned output is a
# header, a rule and the value, so the value is the last line. Reading the JSON
# would want a JSON parser, and this script is not going to require one.
#
# $1 - the statement
number_of() {
  "$cli" --db "$database" query "$1" | tail -n 1 | tr -d '[:space:]'
}

echo "building the index over $(number_of "SELECT count(*) FROM passage") passages"
if ! "$cli" --db "$database" exec \
  "CREATE INDEX passage_v ON passage USING inillucent_hnsw (v)" >/dev/null; then
  echo "FAIL  the index could not be built"
  exit 1
fi

# **Read back through a new process, which is the whole point.** The store
# reported the rows it had just been given even when it had not kept them, so
# the question is what a later connection sees.
held="$(number_of "SELECT v FROM passage_v_state WHERE k = 'rows'")"
vectors="$(number_of "SELECT count(*) FROM passage WHERE v IS NOT NULL")"
if [ "$held" != "$vectors" ]; then
  echo "FAIL  the index holds $held after the reopen; the table has $vectors rows with vectors"
  exit 1
fi
echo "the index holds all $held of them after a reopen"
echo

# Runs the semantic search this example documents and prints the titles.
#
# $1 - the question
ask() {
  "$cli" --db "$database" query \
    "SELECT p.title FROM passage p, (SELECT embed('search_query: ' || ?1) AS q) AS probe
     ORDER BY vector_distance_cos(p.v, probe.q) LIMIT 5" \
    --params "[\"$1\"]"
}

# $1 - the question, $2 - the article that has to appear
expect() {
  local found
  found="$(ask "$1")"
  if printf '%s' "$found" | grep -qF "$2"; then
    printf 'ok    %-52s -> %s\n' "$1" "$2"
  else
    printf 'FAIL  %-52s -> expected %s, got:\n%s\n' "$1" "$2" "$found"
    failures=$((failures + 1))
  fi
}

expect "who was Seneca" "Seneca the Younger"
expect "what did the Stoics believe about death" "Stoicism"
expect "the prisoners watching shadows on a cave wall" "Allegory of the cave"
expect "which philosopher said everything is water" "Thales of Miletus"
expect "pleasure as the absence of pain" "Epicurus"
expect "a Roman emperor who wrote a private notebook" "Marcus Aurelius"
expect "you cannot step into the same river twice" "Heraclitus"
expect "the woman mathematician murdered in Alexandria" "Hypatia"
expect "man is the measure of all things" "Protagoras"
expect "the school that met at the Lyceum" "Aristotle"

echo
if [ "$failures" -eq 0 ]; then
  echo "all ten answered through the index"
else
  echo "$failures of ten failed through the index"
  exit 1
fi
