#!/usr/bin/env python3
"""Turn the committed article corpus into the CSV `inillucent import` reads.

Input is `../corpus/greek-philosophy.jsonl`, one record per Wikipedia article.
Output is `build/chunks.csv` with the columns `id,title,url,body`, which
`build-database.sh` loads into a staging table and then embeds.

## Why there are no section boundaries in here

The CirrusSearch dumps carry an article's plain text in one field and its
headings in another, and the second cannot be located inside the first. Measured
on `Seneca the Younger`: of its first ten headings, `Early life, family and
adulthood`, `Politics and exile`, `Imperial advisor` and `Retirement` appear
nowhere in the body, and `Life`, `Death` and `Philosophy` appear only where the
words happen to occur in a sentence. The text also has no newlines at all - it
is one continuous string of 42,486 characters.

So this splits on sentence boundaries and packs them to a length, which is what
the material actually supports. A chunker that claimed to split at headings
would be splitting at whichever paragraph happened to contain the word "Death".

## Why each chunk repeats its article title

`tests/synthetic-corpus.md` states the rule for the graded corpus: every chunk
carries its document title in its own text. It is what lets a keyword search for
"Seneca" reach a chunk whose sentences all say "he", and it gives the embedding
the subject of a passage that never names it. The same rule is applied here.
"""

import argparse
import csv
import json
import re
import sys
from pathlib import Path

# A sentence ends at . ? or ! followed by a space and something that starts a
# new sentence. The lookbehind keeps the common abbreviations and the initials
# in Greek and Roman names from ending one: `c. 4 BC`, `Lucius Annaeus`,
# `vol. ii`, `St. Paul`.
SENTENCE = re.compile(
    r"(?<![A-Z])(?<!\b[A-Z]\.)(?<!\bc\.)(?<!\bcf\.)(?<!\bvol\.)(?<!\bno\.)(?<!\bch\.)"
    r"(?<!\bSt\.)(?<!\bMr\.)(?<!\bDr\.)(?<!\bfl\.)(?<!\bed\.)(?<!\betc\.)"
    r"(?<=[.!?])\s+(?=[\"'(“‘]?[A-Z0-9])"
)

TARGET_CHARS = 1100
# A chunk shorter than this is folded into the one before it rather than stored.
# A forty character chunk retrieves badly and dilutes the average.
MIN_CHARS = 300


def sentences(text):
    """Splits an article's text into sentences.

    @param text - the article body, one continuous string with no newlines
    """
    text = " ".join(text.split())
    return [s for s in SENTENCE.split(text) if s]


def chunks_of(text, target=TARGET_CHARS):
    """Packs sentences into chunks of about `target` characters.

    One sentence of overlap between neighbours, so a passage whose answer
    straddles a boundary is complete in one of the two rather than cut in both.

    @param text - the article body
    @param target - the character count to pack up to
    """
    pieces = sentences(text)
    out = []
    current = []
    length = 0
    for piece in pieces:
        if current and length + len(piece) + 1 > target:
            out.append(" ".join(current))
            # The next chunk starts with the sentence that closed this one, so a
            # passage whose answer straddles the boundary is whole in one of
            # them. Unless that sentence and the next one cannot fit together at
            # all, in which case there is no overlap to carry and the long
            # sentence becomes a chunk of its own - a sentence is never split.
            last = current[-1]
            current = [last] if len(last) + len(piece) + 1 <= target else []
            length = len(last) + 1 if current else 0
        current.append(piece)
        length += len(piece) + 1
    if current:
        tail = " ".join(current)
        if out and len(tail) < MIN_CHARS:
            out[-1] = out[-1] + " " + tail
        else:
            out.append(tail)
    return out


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--corpus", required=True, help="the article JSONL to read")
    parser.add_argument("--out", required=True, help="where to write the chunk CSV")
    parser.add_argument(
        "--fts-out",
        help="where to write the narrower CSV the full-text table is loaded from",
    )
    parser.add_argument(
        "--target", type=int, default=TARGET_CHARS, help="characters per chunk"
    )
    arguments = parser.parse_args()

    rows = []
    articles = 0
    with open(arguments.corpus, "r", encoding="utf-8") as handle:
        for line in handle:
            article = json.loads(line)
            articles += 1
            for body in chunks_of(article["text"], arguments.target):
                rows.append(
                    {
                        "id": len(rows) + 1,
                        "title": article["title"],
                        "url": article["url"],
                        # The title breadcrumb, then the passage.
                        "body": f"{article['title']} > {body}",
                    }
                )

    out = Path(arguments.out)
    out.parent.mkdir(parents=True, exist_ok=True)
    with out.open("w", encoding="utf-8", newline="") as handle:
        writer = csv.DictWriter(
            handle, fieldnames=["id", "title", "url", "body"], lineterminator="\n"
        )
        writer.writeheader()
        writer.writerows(rows)

    # The full-text table's own file. It exists because an `INSERT ... SELECT`
    # into a virtual table is refused - exit code 3, status `unsupported` - so
    # the passages cannot be copied across from `passage` inside the database
    # and have to be loaded into the FTS5 table the same way they were loaded
    # into the staging one. Its columns are the FTS5 table's columns, in order,
    # because `.import` binds by position.
    if arguments.fts_out:
        fts = Path(arguments.fts_out)
        fts.parent.mkdir(parents=True, exist_ok=True)
        with fts.open("w", encoding="utf-8", newline="") as handle:
            writer = csv.DictWriter(
                handle, fieldnames=["id", "title", "body"], lineterminator="\n"
            )
            writer.writeheader()
            for row in rows:
                writer.writerow({k: row[k] for k in ("id", "title", "body")})

    lengths = sorted(len(r["body"]) for r in rows)
    print(
        f"{articles} articles -> {len(rows)} chunks, "
        f"shortest {lengths[0]}, median {lengths[len(lengths) // 2]}, longest {lengths[-1]} "
        f"-> {out}",
        file=sys.stderr,
    )


if __name__ == "__main__":
    main()
