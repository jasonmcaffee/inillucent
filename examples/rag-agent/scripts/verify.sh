#!/usr/bin/env bash
# Ask the committed database the questions it is supposed to be able to answer.
#
# Each case is a question and the Wikipedia article whose passages have to come
# back in the top five. A test that only asserted "some rows came back" would
# pass against an index full of the wrong vectors, which is the failure this is
# here to catch: rebuilding the corpus without re-embedding it pairs every
# vector with the wrong passage, and nothing about that looks broken.
#
#   INILLUCENT=/path/to/inillucent scripts/verify.sh
set -uo pipefail

here="$(cd "$(dirname "$0")/.." && pwd)"
cd "$here"
cli="${INILLUCENT:-inillucent}"
database="greek-philosophy.rdb"
failures=0

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

echo "== the model answers at all"
if ! "$cli" query "SELECT length(embed('x'))" > /dev/null 2>&1; then
  echo "embed(TEXT) does not answer. Run: inillucent setup-embeddings all" >&2
  exit 1
fi

echo "== every passage has a vector of the right width"
shape="$("$cli" --db "$database" query \
  "SELECT count(*) AS passages, count(v) AS vectors, min(vector_dims(v)) AS narrowest,
          max(vector_dims(v)) AS widest FROM passage")"
echo "$shape"
if ! printf '%s' "$shape" | grep -qE '(^|[^0-9])768([^0-9]|$)'; then
  echo "FAIL  the vectors are not 768 wide"
  failures=$((failures + 1))
fi

echo
echo "== ten questions, and the article each one has to find"
expect "who was Seneca"                                  "Seneca the Younger"
expect "what did the Stoics believe about death"         "Stoicism"
expect "the prisoners watching shadows on a cave wall"   "Allegory of the cave"
expect "which philosopher said everything is water"      "Thales of Miletus"
expect "pleasure as the absence of pain"                 "Epicurus"
expect "a Roman emperor who wrote a private notebook"    "Marcus Aurelius"
expect "you cannot step into the same river twice"       "Heraclitus"
expect "the woman mathematician murdered in Alexandria"  "Hypatia"
expect "man is the measure of all things"                "Protagoras"
expect "the school that met at the Lyceum"               "Aristotle"

echo
echo "== a rare name, which keyword search finds and meaning does not"
# Only the titles: the passages are 1,100 characters each and a terminal full of
# them hides the one thing being checked.
words="$("$cli" --db "$database" query \
  "SELECT id, title FROM passage_fts WHERE passage_fts MATCH 'Metrodorus' ORDER BY rank LIMIT 3")"
echo "$words"
if ! printf '%s' "$words" | grep -qiE "epicur"; then
  echo "FAIL  the keyword index did not find Metrodorus in the Epicurean articles"
  failures=$((failures + 1))
fi

echo
echo "== what the distance does and does not catch"
for question in "who was Seneca" \
                "what did Kant think about the categories of understanding" \
                "how do I configure a Kubernetes ingress controller"; do
  printf '%-58s ' "$question"
  "$cli" --db "$database" query \
    "SELECT round(min(vector_distance_cos(p.v, probe.q)), 4) AS nearest
     FROM passage p, (SELECT embed('search_query: ' || ?1) AS q) AS probe" \
    --params "[\"$question\"]" | tail -n 1
done
echo "The third question is from another subject and the distance says so. The"
echo "second is a philosophy question this corpus cannot answer - no Kant in it -"
echo "and it scores as close as the first. Only reading the passages catches that."

echo
if [ "$failures" -eq 0 ]; then
  echo "all checks passed"
else
  echo "$failures checks failed"
fi
exit "$failures"
