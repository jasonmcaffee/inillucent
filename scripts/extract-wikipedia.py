#!/usr/bin/env python3
"""Turn a Wikimedia CirrusSearch dump into the compact JSONL the corpus builder reads.

The CirrusSearch dumps are used rather than the page dumps because they already
hold plain text and an array of section headings, so no wikitext has to be
parsed. Two files are produced:

  articles.jsonl  content namespace pages, the raw material for the page shaped,
                  design file shaped and board shaped sources
  talk.jsonl      Talk namespace pages, which are real threaded conversation and
                  so the raw material for the message shaped source

Records are emitted in dump order, which is stable for a given dump date, so the
whole pipeline is reproducible.
"""
import argparse
import gzip
import json
import sys
from pathlib import Path

MIN_ARTICLE_CHARS = 1200
MIN_TALK_CHARS = 400


def field(doc, name, default=None):
    v = doc.get(name)
    return v if v is not None else default


def stream(path):
    """Yield the document records, skipping the bulk index header lines.

    `path` may be "-", in which case the compressed dump is read from standard
    input. That is how the English Wikipedia dump is used: it is 43 GB and only
    the first part of it is needed, so it is streamed and the reader stops early
    rather than being downloaded in full.
    """
    if str(path) == "-":
        fh = gzip.open(sys.stdin.buffer, "rt", encoding="utf-8", errors="replace")
    else:
        fh = gzip.open(path, "rt", encoding="utf-8", errors="replace")
    with fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            try:
                doc = json.loads(line)
            except json.JSONDecodeError:
                continue
            if "index" in doc and "title" not in doc:
                continue
            yield doc


def write_articles(src, out, min_chars=MIN_ARTICLE_CHARS, limit=None):
    kept = dropped_short = 0
    with open(out, "w", encoding="utf-8") as fh:
        for doc in stream(src):
            if limit is not None and kept >= limit:
                break
            if doc.get("namespace") != 0:
                continue
            text = field(doc, "text", "")
            if len(text) < min_chars:
                dropped_short += 1
                continue
            title = (field(doc, "title", "") or "").strip()
            if not title:
                continue
            record = {
                "page_id": doc.get("page_id"),
                "title": title,
                "headings": [h for h in field(doc, "heading", []) if h and h.strip()],
                "categories": [c for c in field(doc, "category", []) if c and c.strip()],
                "timestamp": field(doc, "timestamp"),
                "text": text,
            }
            fh.write(json.dumps(record, ensure_ascii=False) + "\n")
            kept += 1
    return kept, dropped_short


def write_talk(src, out):
    """Talk and User talk pages, which carry the conversational register."""
    kept = 0
    with open(out, "w", encoding="utf-8") as fh:
        for doc in stream(src):
            ns = doc.get("namespace_text") or ""
            if ns not in ("Talk", "User talk", "Wikipedia talk", "Help talk"):
                continue
            text = field(doc, "text", "")
            if len(text) < MIN_TALK_CHARS:
                continue
            title = (field(doc, "title", "") or "").strip()
            if not title:
                continue
            record = {
                "page_id": doc.get("page_id"),
                "namespace": ns,
                "title": title,
                "headings": [h for h in field(doc, "heading", []) if h and h.strip()],
                "timestamp": field(doc, "timestamp"),
                "text": text,
            }
            fh.write(json.dumps(record, ensure_ascii=False) + "\n")
            kept += 1
    return kept


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("raw", help="directory holding the downloaded dumps, or - to read one from stdin")
    ap.add_argument("out", help="directory the JSONL files are written to")
    ap.add_argument("--articles-only", action="store_true", help="write only articles.jsonl")
    ap.add_argument("--talk-only", action="store_true", help="write only talk.jsonl")
    ap.add_argument("--name", default="articles.jsonl", help="output file name for articles")
    ap.add_argument("--min-chars", type=int, default=MIN_ARTICLE_CHARS)
    ap.add_argument("--limit", type=int, default=None, help="stop after this many articles")
    args = ap.parse_args()

    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)

    if args.raw == "-":
        kept, short = write_articles("-", out / args.name, args.min_chars, args.limit)
        print(f"{args.name}: kept {kept}, dropped {short} shorter than {args.min_chars} characters", flush=True)
        return

    raw = Path(args.raw)
    if not args.talk_only:
        kept, short = write_articles(raw / "simplewiki-content.json.gz", out / args.name, args.min_chars, args.limit)
        print(f"{args.name}: kept {kept}, dropped {short} shorter than {args.min_chars} characters", flush=True)
    if not args.articles_only:
        talk = write_talk(raw / "simplewiki-general.json.gz", out / "talk.jsonl")
        print(f"talk.jsonl: kept {talk} discussion pages", flush=True)


if __name__ == "__main__":
    main()
