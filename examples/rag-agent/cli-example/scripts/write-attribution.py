#!/usr/bin/env python3
"""Write ../corpus/ATTRIBUTION.md from the committed corpus.

Wikipedia text is CC BY-SA 4.0, which permits redistribution and requires
attribution and the same licence on the result. The rest of this repository
avoids the question by not committing any Wikipedia text at all - `.gitignore`
says so - and this example cannot, because a database that has to be built
before it can be searched is the thing the example exists to remove.

So the licence is satisfied the way it is meant to be. This file lists every
article, links to the page it came from, and names the dump it was taken out of,
so a reader can find the page history and the authors behind any passage an
answer cited.

Usage:
    python scripts/write-attribution.py --corpus ../corpus/greek-philosophy.jsonl \\
        --out ../corpus/ATTRIBUTION.md
"""

import argparse
import json
from pathlib import Path

HEADER = """# Where this corpus comes from

Both examples in `examples/rag-agent/` search the Wikipedia articles below, taken from the
{dump_date} CirrusSearch dumps of the English and Simple English Wikipedias. The text is
unchanged. The command line example splits it into passages of about 1,100 characters with the
article title in front, and the Rust example into overlapping chunks of about 1,000 characters.

**Licence: [CC BY-SA 4.0](https://creativecommons.org/licenses/by-sa/4.0/).** The text is used and
redistributed here under that licence, and anything derived from it carries the same one. Each
article's authors are named in its page history, which the link beside it reaches.

{count} articles, {characters:,} characters. `../cli-example/scripts/select-corpus.py` holds the list of titles and
the reason it is a list rather than a rule.

| article | wiki | characters |
|---|---|---:|
"""


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--corpus", required=True, help="the article JSONL to read")
    parser.add_argument("--out", required=True, help="where to write the attribution page")
    arguments = parser.parse_args()

    articles = []
    with open(arguments.corpus, "r", encoding="utf-8") as handle:
        for line in handle:
            articles.append(json.loads(line))
    articles.sort(key=lambda article: article["title"])

    wiki_names = {"enwiki": "English", "simplewiki": "Simple English"}
    lines = [
        HEADER.format(
            dump_date=articles[0].get("dump_date", "20251222"),
            count=len(articles),
            characters=sum(len(a["text"]) for a in articles),
        )
    ]
    for article in articles:
        lines.append(
            f"| [{article['title']}]({article['url']}) "
            f"| {wiki_names.get(article['source'], article['source'])} "
            f"| {len(article['text']):,} |\n"
        )

    out = Path(arguments.out)
    out.write_text("".join(lines), encoding="utf-8", newline="\n")
    print(f"{len(articles)} articles -> {out}")


if __name__ == "__main__":
    main()
