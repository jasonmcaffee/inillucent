# Retained corpora

Every file here is an input that once made inillucent and the pinned SQLite build
disagree. They are kept and replayed on every later run, so a fix stays fixed
whether or not the generator's seed happens to produce the same input again.

| directory | what it holds | who replays it |
|---|---|---|
| `btree/` | minimal operation sequences that once diverged from `ModelBTree`, shrunk to the smallest failing case | `inillucent-compat::durability::btree_model` |
| `syntax/` | statements the parser and the pinned release once disagreed about accepting | `inillucent-compat::syntax::differential_parser_fuzzing_finds_no_divergence` |
| `select/` | the foundational SELECT corpus: a schema and one query per line | `inillucent-slt`, which records SQLite's answers into `tests/conformance/` |

A retained file holds the **input**, never the annotation. That distinction is
not pedantic: a file of ``` `X` sqlite=false inillucent=true ``` lines replays as
*that string* rather than as `X`, so the corpus reads as though it is checking
something and is checking a different question every time. The divergences in
`syntax/` were written that way once, which is how the distinction was noticed.

`select/` is different in kind from the other two: it is a corpus of things that
should work rather than a corpus of things that once did not, and it is meant to
be added to freely. A statement earns its place there if it can be wrong in a
way that a simpler statement cannot.
