#!/usr/bin/env python3
"""Select the Greek philosophy articles this example's corpus is built from.

The two inputs are the extracts `scripts/fetch-public-corpus.sh` already
produces at the top of this repository: `enwiki-articles.jsonl`, 60,000 articles
streamed out of the English CirrusSearch content dump, and `articles.jsonl`, the
whole Simple English one. Neither is committed - they are 1.5 GB and 333 MB - so
this script is provenance rather than a step anybody has to run. Its output,
`corpus/greek-philosophy.jsonl`, is committed, and `chunk-corpus.py` reads that.

## Why this is a list of titles and not a rule

The first version of this script selected by Wikipedia category, on an allowlist
of forty-five exact category names that all sounded unambiguous: `Presocratic
philosophers`, `Neoplatonists`, `Stoic philosophers`, `4th-century BC Greek
philosophers`. It produced 117 articles, and among them were **Ayn Rand, Karl
Popper, Franz Brentano, John Henry Newman and Mortimer J. Adler** - all filed
under `Aristotelian philosophers` - along with **Druze** and **Augustinianism**
under `Neoplatonism`, and **Michel de Montaigne** under `Skeptic philosophers`.
Every one of those categories reaches across two thousand years, and no wording
of the rule separates the ancient members from the modern ones.

That matters more here than it would in a grading corpus. A reader of this
example has to be able to say whether the corpus contains the answer to their
question before they trust the answer. A list of titles can be read in ten
seconds. A category rule cannot, and the articles it admits by mistake are
exactly the ones a search will return when the corpus has nothing better.

Usage:
    python scripts/select-corpus.py --derived J:/inillucent-embeddings/derived \\
        --out corpus/greek-philosophy.jsonl
"""

import argparse
import json
import sys
from pathlib import Path

# Every article in this corpus, named. Ancient Greek philosophy and the
# Hellenistic and Roman schools that continued it, plus the handful of concept
# pages the demonstration questions land on.
#
# `Thales of Miletus` is here and `Thales` is not: they are the same person, and
# the Simple English page is a shorter duplicate of the English one.
TITLES = [
    # The field itself
    "Ancient Greek philosophy",
    "Pre-Socratic philosophy",
    "Hellenistic philosophy",
    "Classical Athens",
    # The Presocratics
    "Thales of Miletus",
    "Anaximander",
    "Anaximenes of Miletus",
    "Xenophanes",
    "Pythagoras",
    "Heraclitus",
    "Parmenides",
    "Zeno of Elea",
    "Eleaticism",
    "Empedocles",
    "Anaxagoras",
    "Leucippus",
    "Democritus",
    "Atomism",
    "Epimenides",
    "Anacharsis",
    # The Sophists
    "Sophist",
    "Protagoras",
    "Gorgias",
    "Prodicus",
    "Hippias",
    "Alcidamas",
    # Socrates and Plato
    "Socrates",
    "Socratic method",
    "Xenophon",
    "Plato",
    "Theory of forms",
    "Allegory of the cave",
    "Eudoxus of Cnidus",
    # Aristotle and the Peripatetics
    "Aristotle",
    "Nicomachean Ethics",
    "Theophrastus",
    "Aristoxenus",
    "Dicaearchus",
    "Alexander of Aphrodisias",
    "Eubulides",
    # The Cynics
    "Antisthenes",
    "Diogenes of Sinope",
    "Crates of Thebes",
    # The Stoics
    "Stoicism",
    "Zeno of Citium",
    "Chrysippus",
    "Posidonius",
    "Epictetus",
    "Seneca the Younger",
    "Marcus Aurelius",
    "Hierocles of Alexandria",
    # The Epicureans
    "Epicurus",
    "Epicureanism",
    "Lucretius",
    # The Sceptics
    "Skepticism",
    "Anaxarchus",
    "Favorinus",
    # The Neoplatonists
    "Plotinus",
    "Proclus",
    "Ammonius Saccas",
    "Ammonius Hermiae",
    "Simplicius of Cilicia",
    "Hypatia",
    "Demiurge",
    "Theurgy",
    # Romans who wrote about all of it, and the people who recorded it
    "Cicero",
    "Plutarch",
    "Diogenes Laertius",
    "Claudius Aelianus",
    "Anaximenes of Lampsacus",
    # The concepts the questions land on
    "Virtue",
    "Ethics",
    "Metaphysics",
    "Epistemology",
    "Logic",
    "Dialectic",
    "Substance theory",
    "Classical element",
    "Metempsychosis",
    "Genus–differentia definition",
]

# Below this an article is a stub whose chunks say nothing worth retrieving.
# 1,200 rather than a rounder number because the Simple English pages for
# `Stoicism` and `Theory of forms` are 1,446 and 1,333 characters, and a corpus
# of Greek philosophy that cannot answer a question about Stoicism is not one.
MIN_CHARS = 1200


def read_wanted(path, source, wiki_host, wanted):
    """Reads one extract and returns the articles this corpus wants from it.

    @param path - the derived .jsonl extract to read
    @param source - the name recorded on each article, 'enwiki' or 'simplewiki'
    @param wiki_host - the host the article's url is built against
    @param wanted - the set of titles to take
    """
    taken = {}
    if not path.exists():
        print(f"  {path} is not here, skipping", file=sys.stderr)
        return taken
    with path.open("r", encoding="utf-8") as handle:
        for line in handle:
            try:
                record = json.loads(line)
            except json.JSONDecodeError:
                continue
            title = record.get("title")
            text = record.get("text") or ""
            if title not in wanted or title in taken or len(text) < MIN_CHARS:
                continue
            taken[title] = {
                "title": title,
                "source": source,
                "url": f"https://{wiki_host}/wiki/" + title.replace(" ", "_"),
                "page_id": record.get("page_id"),
                "timestamp": record.get("timestamp"),
                "text": text,
            }
    return taken


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--derived",
        required=True,
        help="the directory holding enwiki-articles.jsonl and articles.jsonl",
    )
    parser.add_argument("--out", required=True, help="where to write the corpus JSONL")
    parser.add_argument(
        "--dump-date",
        default="20251222",
        help="the CirrusSearch dump date the extracts were built from",
    )
    arguments = parser.parse_args()

    wanted = set(TITLES)
    derived = Path(arguments.derived)
    print("reading the English extract", file=sys.stderr)
    english = read_wanted(derived / "enwiki-articles.jsonl", "enwiki", "en.wikipedia.org", wanted)
    print("reading the Simple English extract", file=sys.stderr)
    simple = read_wanted(derived / "articles.jsonl", "simplewiki", "simple.wikipedia.org", wanted)

    # English first. The English extract stops at 60,000 articles, so several
    # pages this corpus needs - Socrates and Stoicism among them - exist only in
    # the Simple English one, and are taken from there rather than dropped.
    articles = dict(simple)
    articles.update(english)

    out = Path(arguments.out)
    out.parent.mkdir(parents=True, exist_ok=True)
    with out.open("w", encoding="utf-8", newline="\n") as handle:
        for title in sorted(articles):
            article = articles[title]
            article["dump_date"] = arguments.dump_date
            handle.write(json.dumps(article, ensure_ascii=False) + "\n")

    characters = sum(len(a["text"]) for a in articles.values())
    from_english = sum(1 for a in articles.values() if a["source"] == "enwiki")
    print(
        f"{len(articles)} articles, {characters:,} characters "
        f"({from_english} English, {len(articles) - from_english} Simple English) -> {out}",
        file=sys.stderr,
    )
    missing = [t for t in TITLES if t not in articles]
    if missing:
        print(f"not in either extract: {', '.join(missing)}", file=sys.stderr)


if __name__ == "__main__":
    main()
