#!/usr/bin/env bash
# Build greek-philosophy.rdb from the committed corpus.
#
# Nobody needs to run this. The database it produces is committed, which is the
# point of the example - a reader asks a question in their first minute instead
# of embedding a corpus first. It is here so the database can be rebuilt after
# an engine change, and so the commands that made it are readable.
#
# It uses nothing but the inillucent command line and, for the chunking, Python.
#
#   INILLUCENT=/path/to/inillucent scripts/build-database.sh
#
# The binary has to be 1.0.30 or later and carry the `embed` feature. Released
# binaries carry it; to build one:
#
#   cargo build --release -p inillucent-cli --features inillucent-cli/embed
set -euo pipefail

here="$(cd "$(dirname "$0")/.." && pwd)"
cd "$here"
cli="${INILLUCENT:-inillucent}"
python_bin="${PYTHON:-}"
if [ -z "$python_bin" ]; then
  if command -v python3 > /dev/null; then python_bin=python3; else python_bin=python; fi
fi

if ! "$cli" --version > /dev/null 2>&1; then
  echo "cannot run '$cli'. Set INILLUCENT to the binary." >&2
  exit 1
fi

# The build embeds every chunk, so it needs the model. A binary built without
# the `embed` feature answers "no such function: embed" instead, which is a
# different problem from a missing model and is worth telling apart here rather
# than half way through the insert.
if ! "$cli" query "SELECT length(embed('x'))" > /dev/null 2>&1; then
  echo "embed(TEXT) does not answer. Either this binary was built without" >&2
  echo "--features embed, or the model is not installed:" >&2
  echo "    inillucent setup-embeddings all" >&2
  exit 1
fi

echo "== chunking the corpus"
"$python_bin" scripts/chunk-corpus.py \
  --corpus ../corpus/greek-philosophy.jsonl \
  --out build/chunks.csv

rm -f greek-philosophy.rdb greek-philosophy.rdb-wal.0000000001
rm -f chunks.rdb chunks.rdb-wal.0000000001

# The chunks are loaded into a database of their own rather than into a staging
# table beside the passages. A dropped table leaves its pages behind as free
# space - measured here, the 2.9 MB of CSV left 3.4 MB of nothing in the file
# that gets committed - and there is no VACUUM to reclaim them. A second file
# that is deleted afterwards costs nothing and leaves the committed database
# holding only what it is for.
#
# `import` takes its column names from the header row and every column is text,
# which is why `id` is cast on the way across.
echo "== loading the chunks"
"$cli" create chunks.rdb
"$cli" --db chunks.rdb import build/chunks.csv --table chunk

echo "== creating the schema"
"$cli" create greek-philosophy.rdb
"$cli" --db greek-philosophy.rdb batch "
CREATE TABLE passage (
  id    INTEGER PRIMARY KEY,
  title TEXT NOT NULL,
  url   TEXT NOT NULL,
  body  TEXT NOT NULL,
  v     VECTOR(768)
);
CREATE VIRTUAL TABLE passage_fts USING fts5(id, title, body);
"

# Two things decide whether this statement works well.
#
# It carries the `search_document: ` prefix. embed(TEXT) embeds the text it is
# given and adds nothing of its own. nomic-embed-text-v1.5 is trained with
# `search_document: ` on stored text and `search_query: ` on questions, and a
# corpus embedded without the prefix does not fail - it answers slightly worse,
# for ever. Every query in this example uses the other prefix.
#
# It has no DETACH. `batch` runs its statements in one transaction, and a
# database cannot be detached inside one. The attachment ends with the process.
#
# It runs with the model resident. Opening a session on the weights costs 650 to
# 800 ms and an embedding through an open one costs 12 to 36 ms, and this is one
# process embedding a few thousand chunks.
echo "== embedding (a couple of minutes)"
INILLUCENT_EMBED_RESIDENCY=resident "$cli" --db greek-philosophy.rdb batch "
ATTACH 'chunks.rdb' AS staging;
INSERT INTO passage (id, title, url, body, v)
SELECT CAST(id AS INTEGER), title, url, body, embed('search_document: ' || body)
FROM staging.chunk;
"

echo "== indexing"
# **There is no HNSW index here, on purpose - and the purpose changed.** It used
# to be that `CREATE INDEX ... USING inillucent_hnsw (v)` made the search return
# nothing at all: the store kept what it was given only until the file was
# reopened, and an empty vector index answers zero rows rather than failing. That
# was fixed in task-1911, and `scripts/verify-indexed.sh` asks this corpus's own
# ten questions through an index to keep it fixed.
#
# The index is still left off because nothing here needs it. 2,661 passages is an
# exhaustive cosine over 8 MB of vectors, which is milliseconds, and
# `docs/vector-search.md` says to build the index when the search is slow rather
# than before it is.

# The full-text table is filled from `passage` in one statement. inillucent
# 1.0.29 refused an `INSERT ... SELECT` into a virtual table, and this script
# loaded the same rows from a second CSV file then. 1.0.30 runs it.
"$cli" --db greek-philosophy.rdb exec "
INSERT INTO passage_fts (id, title, body) SELECT id, title, body FROM passage
"

# Folds the log back into the file, so what is committed is one self-contained
# database rather than a file plus a log segment nobody would think to commit.
"$cli" --db greek-philosophy.rdb checkpoint

rm -f chunks.rdb chunks.rdb-wal.0000000001
rm -f greek-philosophy.rdb-wal.0000000001

"$cli" --db greek-philosophy.rdb query \
  "SELECT count(*) AS passages, count(v) AS vectors, count(DISTINCT title) AS articles FROM passage"
ls -l greek-philosophy.rdb
