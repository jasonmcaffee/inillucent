# The feature probe

The instrument behind [`docs/feature-comparison.md`](../../docs/feature-comparison.md).

It asks one question, 416 times: **does inillucent answer this the way SQLite 3.53.4 does?** Each
case is a whole SQL script, run through `inillucent-shell` and through the pinned `sqlite3`, each
over its own fresh database in its own directory, and every byte of both streams — standard output
*and* standard error — is compared.

That last part is the design. A difference in an **error** is a difference: half of what these
reviews find is a refusal where SQLite answers, or a message naming something else, and a harness
that compared rows would throw exactly that away. It is the same comparison
`crates/inillucent-compat/tests/semantics.rs` makes; this is the wide, exploratory version of it, and
what it finds is meant to *become* cases there.

## Running it

```sh
cargo build --release --bin inillucent-shell     # the engine under test
tools/sqlite-reference.sh                        # the pinned reference, if .sqlite-ref is absent

node tools/feature-probe/run.js                  # all 416 cases
node tools/feature-probe/run.js --filter join    # one area, or one case id
node tools/feature-probe/summarise.js            # the per-area table

node tools/feature-probe/pragmas.js              # every PRAGMA the reference lists, both engines
node tools/feature-probe/vector-features.js      # the vector surface, one pgvector feature at a time
node tools/feature-probe/registers.js            # the completeness audit: SQLites own registers, not our list
```

Everything is written under `_agent_output/feature-probe/`, which is gitignored: the transcripts are
evidence, not source. `results.json` carries the script, both transcripts and the verdict for every
case, which is what you read when a row moves.

## The files

| | |
|---|---|
| `run.js` | the runner, the normaliser and the verdict rule |
| `registers.js` | the **completeness audit**: enumerates every function, pragma, module, collation and dot command SQLite reports, calls each one in both engines, and calls the context-scoped ones properly. It exists because `cases.js` can only find what somebody thought to write down |
| `cases.js` | the main case table, by area |
| `cases-extra.js` | the shell, parameters, limits, and the corners the first table left |
| `pragmas.js` | asks both engines every pragma the reference lists |
| `vector-features.js` | the vector surface against pgvector's, ours only |
| `summarise.js` | `results.json` → the per-area markdown table |
| `paths.js` | where the two binaries are, derived from this file's location |

## The verdicts

| verdict | meaning |
|---|---|
| `same` | byte for byte, including when both refuse |
| `wrong-answer` | both answered and the answers differ |
| `refused` | SQLite answered and this engine refused |
| `accepted` | this engine answered and SQLite refused |
| `both-refuse-differently` | both refused, with different wording |
| `ours-only` | a case with no SQLite equivalent, so only this engine runs it |

`both-refuse-differently` counts as agreement in the comparison document: the feature is absent in
both, and the wording is a separate, much smaller question.

## Adding a case

Append to `cases.js` or `cases-extra.js`:

```js
c('area', 'What it is, in words', 'stable.id', "CREATE TABLE t(a);\nSELECT ...;")
```

Two rules, both learned the hard way:

- **Nothing may read the clock, the process id or the random generator.** A case that cannot answer
  the same way twice cannot answer a question about parity. Use fixed dates, and `round()` a float
  before printing it.
- **Order what you compare.** `ORDER BY` inside the query, or `group_concat` over an ordered
  subquery, unless the case is *about* ordering. An accidental ordering difference is noise that
  costs a reading of two transcripts.

`{ oursOnly: true }` marks a case with no SQLite equivalent; `{ files: { 'data.csv': '1,x\n' } }`
writes a file into the case's directory first, which is how `.import` and `.read` are probed.
