# The feature probe

The feature probe produces the numbers in [`docs/feature-comparison.md`](../../docs/feature-comparison.md).

The feature probe asks one question for each of its 416 cases: does inillucent answer this the same
way SQLite 3.53.4 does? Each case is a whole SQL script. The probe runs the script through
`inillucent-shell` and through the pinned `sqlite3`. Each run gets a fresh database in its own
directory. The probe then compares every byte of standard output and standard error from the two
runs.

The probe compares errors too. Many differences are a refusal where SQLite answers, or an error
message that names something else. A probe that compared only rows would miss those.
`crates/inillucent-compat/tests/differential/semantics.rs` makes the same comparison inside the test suite. The
feature probe covers more ground, and a difference it finds should become a case in `semantics.rs`.

## Running it

```sh
cargo build --release --bin inillucent-shell     # the engine under test
sh tools/sqlite-reference.sh                     # the pinned SQLite, when .sqlite-ref/ is missing

node tools/feature-probe/run.js                  # all 416 cases
node tools/feature-probe/run.js --filter join    # the cases whose area or id contains "join"
node tools/feature-probe/summarise.js            # the table of results by area

node tools/feature-probe/pragmas.js              # every PRAGMA the pinned SQLite lists, in both engines
node tools/feature-probe/vector-features.js      # the vector features, one pgvector feature at a time
node tools/feature-probe/registers.js            # the completeness audit
```

On Windows, run `pwsh tools/sqlite-reference.ps1` instead of `tools/sqlite-reference.sh`.

`tools/feature-probe/paths.js` finds `inillucent-shell` in the `release` folder of the directory
cargo builds into. It asks `cargo metadata` for that directory, so it works in a git worktree whose
`.cargo/config.toml` names another target directory. The pinned `sqlite3` is read from
`.sqlite-ref/3.53.4/shell/`.

Every result is written under `_agent_output/feature-probe/`, which is gitignored.
`run.js --out <path.json>` writes the results somewhere else. `results.json` holds, for every case,
the script, both transcripts and the verdict. Read `results.json` when a case changes verdict.

## The files

| File | What it does |
|---|---|
| `run.js` | runs the cases, normalizes the two transcripts, and decides the verdict |
| `cases.js` | the main list of cases, grouped by area |
| `cases-extra.js` | cases for the shell, parameters, limits, and the gaps the first list left |
| `registers.js` | the **completeness audit**. It lists every function, pragma, module, collation and dot command that SQLite itself reports, and calls each one in both engines. It finds features that nobody wrote a case for in `cases.js` |
| `pragmas.js` | asks both engines every pragma the pinned SQLite lists |
| `vector-features.js` | checks which pgvector features can be written in inillucent's SQL. Runs on inillucent only |
| `summarise.js` | reads `results.json` and prints a markdown table of results by area |
| `paths.js` | finds the two binaries and the output folder |

## The verdicts

| Verdict | Meaning |
|---|---|
| `same` | both transcripts are identical byte for byte, including when both engines refuse |
| `wrong-answer` | both engines answered and the answers differ |
| `refused` | SQLite answered and inillucent refused. A case marked `oursOnly` that inillucent refuses also gets `refused` |
| `accepted` | inillucent answered and SQLite refused |
| `both-refuse-differently` | both engines refused, with different messages |
| `ours-only` | a case with no SQLite equivalent, so only inillucent runs it |

`docs/feature-comparison.md` counts `both-refuse-differently` as agreement. Neither engine has the
feature, and the wording of the error is a separate question.

## Adding a case

Add a line to `cases.js` or `cases-extra.js`. The arguments are the area, a description, a stable
id, and the SQL script:

```js
c('area', 'What it is, in words', 'stable.id', "CREATE TABLE t(a);\nSELECT ...;")
```

Two rules keep a case repeatable:

- **Do not read the clock, the process id or the random number generator.** A case whose answer
  changes between runs cannot show whether two engines agree. Use fixed dates, and `round()` a float
  before printing it.
- **Order what you compare.** Put `ORDER BY` in the query, or use `group_concat` over an ordered
  subquery, unless the case tests ordering. An accidental difference in row order makes the case
  report a difference that does not matter.

Two options go in a fifth argument:

| Option | What it does |
|---|---|
| `{ oursOnly: true }` | marks a case with no SQLite equivalent. Only inillucent runs it |
| `{ files: { 'data.csv': '1,x\n' } }` | writes files into the case's directory before the run. `.import` and `.read` are tested this way |
