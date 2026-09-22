# task-2066: the test and performance review before release, and what it found

Reviewed at commit `8607adf`, the tip of `main` when this review started. Every file and line cited
below is at that revision. The review ran the code as well as reading it: a full strict run of the
suite in a worktree, one release gate run, the two profilers, 571 SQL statements against the pinned
SQLite 3.53.4 shell side by side, 19 vector checks, about 60 command line and shell probes, a three
process write campaign, and a reproduction attempt on the one incident the audits raised. The five
audit reports, their scripts, the SQL batteries and every database they left behind are under
`_agent_output/task-2066-test-perf-review/`: `survey-prior-round.md`, `audit-tests.md`,
`audit-performance.md`, `audit-correctness.md`, `incident-lost-table.md` and
`strict-run-summary.md`. The reproduction of every SQL defect is `b14-defects.sql` in that folder,
annotated with SQLite's answer, and it runs unchanged on both engines.

This document is the design. Nothing in it is built here. The build is filed for Opus as
**task-2068** on the current sprint, and that ticket may ask Fable 5.1 for guidance when a section
of this document is not precise enough to act on.

## Introduction

The previous round did what it set out to do. task-2000 designed ten performance changes and
task-2006 built four of them; the published headline is 4.53x SQLite on the weighted plan, 4.21x on
the 95% lower bound, against a contract asking 3.00x. task-2035 and task-2036 built the end to end
scenario suite and the nightly tier. Between task-2040 and task-2061 twelve correctness tickets each
closed a defect with a test. A strict run today grades 220 targets and about 3,500 tests, and the
best of those tests, the model based B tree program with its shrinker and retained corpus, is better
than most databases have.

So this review did not look for the things a suite of that shape catches. It looked for what sits
around it, in three places.

**The tools around the SQL are not as good as the SQL.** Of 571 statements compared with SQLite,
16 answered differently and 15 of those are small: a default chosen differently, an error worded
differently. The sixteenth is a wrong answer, and it is the commonest recursive query there is. The
other fourteen items that block release are all outside the dialect: the backup command drops rows,
the integrity command says a corrupt file is fine, the migration command does none of the
verification it documents, a documented vector query fails the moment its index exists, two
language bindings corrupt every large integer, and a single request kills the MCP server.

**The performance gate measures a shape no application produces.** Every published read figure is
taken on a tree imported from SQLite, which is packed, so no figure has ever touched a leaf holding
delta rows. An index created before its data, which is the ordinary order of operations, turns a
join that takes 2 ms into one that takes 3,209 ms. A correlated subquery re-prepares its whole
pipeline for every outer row. A sort larger than memory has nowhere to spill and ends the process.
None of those moves a number on `docs/performance.md`, and all seven workloads that are still slower
than SQLite are the four task-2000 designs that were priced and never built.

**Nothing runs the suite except a person.** There is no continuous integration, the release script
publishes to twelve destinations without running a test, the binding conformance suite skips on any
machine that has not run five commands by hand, twelve fuzz targets exist and nothing runs them, and
the crash campaign reports churn on every run so nobody reads them. Four of the fifteen release
blockers have a test that was written to pass: the vector suite avoids the documented spelling, the
integrity test only checks a healthy file, the Node and Go runners compare two corrupted values, and
recovery counts a dropped record as applied.

The design below fixes the defects, builds the four missing performance designs where they pay,
adds the instruments that would have caught each class of defect, and gates the release on them.

## Goals and Non-Goals

### Goals

1. **Every wrong answer and every data losing path found by this review is fixed, and each fix
   ships with the test that would have caught it.** The fifteen release blockers in §4.1 and the
   eleven items in §4.2. `b14-defects.sql` passes on both engines with no diff.
2. **The three performance defects an application hits and the gate cannot see are fixed**: the
   range probe into a delta leaf, the correlated subquery, and the sort that cannot spill. Each gets
   a benchmark so it cannot come back.
3. **The cheap wins on the shipped API are taken**: the pool scan on every read statement, the
   double parse in `Connection::prepare`, the column names rebuilt per execution, and the sorted
   probe cursor that task-2000 designed as design 3.
4. **The suite runs itself.** A continuous integration workflow on Windows and Linux, a release
   script that refuses to publish on a red or absent run, binding conformance that runs rather than
   reads leftovers, and a fuzz runner with a checked in corpus.
5. **The suite grades what it does not grade today**: aggregate, grouped and join partitions in
   the query generator, a pivoted query arm that needs no oracle, one nightly target larger than the
   buffer pool, a misdirected write and a lying `fsync` in the fault model, and a corruption sweep
   over the shipping format that asserts refusal rather than the absence of a panic.
6. **The written record matches the tree**: changelog sections for 0.1.5, 0.1.6 and 0.1.7, the
   fuzz README naming all twelve targets, and the testing standard's tier table moved with every
   row added.

### Non-goals

- **Re-measuring the published numbers.** That is task-2064, after this work merges, on a quiet
  box. Every fresh number in the audits was taken while task-2065 ran a full strict suite beside
  them and is labelled contended; this document quotes only the published 2026-09-20 figures as
  baselines and uses fresh figures only as ratios and directions.
- **The PostgreSQL parity ladder** (`tasks/task-1998-postgres-parity-tdd.md`): readers that do not
  block a writer across processes, roles, the server. The audits confirm readers wait on the file
  lock today (`audit-performance.md` H1) and this document leaves that where task-1998 put it.
- **The memory bar, the `open.prepare` bar and the `schema` bar.** `docs/closed-items.md` records
  the first as closed by decision and the other two as arithmetically unreachable at their contract
  values. Moving a bar is Jason's decision; it is listed in §9 and not made here.
- **The delta area page format change** (task-2000 design 6, `audit-performance.md` C1 and C2).
  It is the largest remaining write path gain and it is a page format change that redo must
  reproduce byte for byte, on pages task-2065 is changing the release of right now. It is designed
  in §4.3.9 as the next performance ticket and not built in this one.
- **A macOS build machine, the retrieval index's resident footprint plan beyond the two filings in
  §4.3.8, and the 46 missing scalar functions.** Each is on `docs/roadmap.md` with its own owner.

## Problem statement

### What the audits measured

| instrument | what it found |
|---|---|
| strict run, 221 targets, 3,493 tests, in a worktree with the oracle junctioned in and the gate fixtures copied, 46 minutes wall on a contended box | 0 assertions failed; exit 1 because `--strict` named eight suites whose prerequisite was absent. Five are this machine (no Go, no network opt in, no `embed` build, no conformance records, a teacher model on `J:` that fails to parse). Two are suite defects: `setup_embeddings` looks for its binary at a hardcoded `target/debug` and so can never run under the redirected `CARGO_TARGET_DIR` every worktree uses, and three `gates_fail_closed` cases that run a nested runner race with concurrent linking. `strict-run-summary.md` has each failure's output |
| 571 SQL statements against sqlite3 3.53.4, 13 areas | 16 differences, 2 unsupported, 0 crashes in the SQL layer; JSON (33), aggregates and windows (45), FTS5 (40) and savepoints (41) with zero differences; `.dump` byte identical |
| 19 vector checks | the documented JSON literal spelling fails against an HNSW index, and every test avoids that spelling |
| about 60 CLI and shell probes | `dump` drops rows, `integrity-check` exits 0 on corruption, `--params-file` escapes `--root`, a 240 KB request overflows the stack, a BLOB cannot be read back through JSON |
| three process write campaign | 180 acknowledged commits from three processes, 180 rows, integrity ok |
| code read of the ungoverned crates | three C ABI entry points without the liveness check, two index readers allocating from unvalidated on disk integers, a free map walk with no cycle guard, recovery dropping records with no counter |
| one release gate run and two profilers, contended | `gate: NOT MET` on the same two bars and two families as the published run; a one table point query compiles in about 10.9 µs with 90 allocations where every published figure is `SELECT 1` at 13 |
| two targeted measurements, contended | 3,209 ms against 2 ms for one join depending on whether its index was built before or after the load; 13.3 s for a 5,000 row correlated `EXISTS` |
| the test suite, read | no CI, no test in the release script, the binding suite skipping on a clean machine, no correctness evidence above 20,000 rows, a query generator with no aggregate, group, join or pivot arm, twelve fuzz targets nothing runs |

### The pattern

Three sentences cover most of it.

The dialect was graded against an oracle and is right; the tools were graded against themselves and
are not. Every one of the fifteen blockers outside the planner is in code no oracle checks: the
`dump` verb, the `integrity-check` verb, the `migrate` verb, the vector probe path, the JSON output
renderer, the two process wrappers, the C ABI, the pool's pragma handler, recovery's tolerance.

The gate grades a packed tree read by one thread in a pool that holds the whole file. Applications
build the index first, run correlated subqueries, sort tables larger than memory, and open a
connection from more than one thread. None of those has a number.

A test that exists is not a test that runs. The suite has every mechanism it needs, and the
mechanisms are driven by a person on one machine remembering to type a command.

### What the previous rounds already recorded as open, and this document's answer to each

From `survey-prior-round.md`'s closing list. The answer is where this document puts each one.

| open item | source | here |
|---|---|---|
| `DELETE` is super linear in row count; a fix was tried and reverted | `tests/inillucent-testing-tdd.md` §8 | §4.3.7, profiled first as that section instructs |
| `write.insert.batch` 67% slower; root cause is index maintenance compaction | `docs/roadmap.md` 2 | §4.3.9, designed, next ticket |
| `extension.fts.build` 45% slower; no design against the three cost centres | `docs/roadmap.md` 1 | non goal; the audit found 52% of it is the virtual table write path and prices the dictionary write as the largest stage (`audit-performance.md` F), which is the input the next FTS5 ticket needs |
| HNSW rebuilds the whole graph on every commit | `tasks/findings.md` | non goal; roadmap item 3 territory |
| the 61 item census of tests that silently do not run | task-1969 §4 | mostly closed by task-1970; the two residues found here are §4.4.11 and §4.4.12 |
| end to end gaps: CLI verbs never spawned, MCP never driven by name | task-1969 §5 | closed by task-2036, confirmed by `audit-tests.md` H1; the two edges left are §4.4.13 |
| CHANGELOG has no sections for 0.1.5, 0.1.6, 0.1.7 | `CHANGELOG.md` | §4.5.1 |
| `tests/performance-history.tsv` stale since 2026-09-08 | the file | §4.5.2 |
| fuzz targets have no run cadence; five of twelve undocumented | `fuzz/README.md` | §4.4.7 |
| `bind.rs` split and an `Identifier` type, recommended four times, never filed | task-1979 §11 | not this ticket; §9 asks for the decision |

## Architectural overview

Four principles, and every section below follows from one of them.

**A fix ships with the test that would have caught it, in the tier where it would have run.** The
correctness section lists the test beside each fix, and "done" for a fix is the test failing on
`8607adf` and passing after. This is rule 1.5 of the testing standard applied to a review: the
comment may only claim what its test proves, and the review may only claim a fix its test proves.

**Performance work targets shapes an application produces, and each gets a benchmark before the
change.** The gate stays as it is; task-2064 re-measures it. What this ticket adds are the arms the
gate lacks: an index built before its data, a correlated subquery, a sort past the pool, and the
shipped `Connection` API beside the engine's internal calls. Each change in §4.3 names the arm that
grades it, and each arm is added first so the before number exists.

**The suite drives itself.** Continuous integration is the first item in the order of work, not the
last, because everything else in this document is worthless if nothing runs it. The release script
runs the suite and refuses on a non zero exit, reading the runner's three codes.

**A prerequisite that is missing is named, never skipped past.** The suite already has this rule
and `--strict` already enforces it at the target level. The residues this review found are the
places the rule was met in letter and not in effect: a test whose fixture avoids the documented
spelling, a runner that compares two values corrupted the same way, a corruption sweep that asserts
only the absence of a panic, a recovery counter that counts a drop as a success.

### Where the work lands

| area | crates and files | sections |
|---|---|---|
| planner and execution | `inillucent-sql/src/plan.rs`, `inillucent-exec/src/{correlate.rs,join/loops.rs,ops/order.rs,physical/run.rs}` | §4.1.1, §4.3.1, §4.3.4, §4.3.6 |
| storage and recovery | `inillucent-tree/src/leaf/read.rs`, `inillucent-pool/src/{file.rs,eviction.rs,pool.rs}`, `inillucent-engine/src/{recovery.rs,pragma/}` | §4.1.4, §4.1.8, §4.1.10, §4.1.11, §4.3.2, §4.3.4 |
| retrieval | `inillucent-search/src/store.rs`, `inillucent-core/src/{hnsw.rs,bm25.rs,store.rs}` | §4.1.2, §4.1.12, §4.3.8 |
| command line, MCP, migration | `inillucent-cli/src/{command/verbs.rs,command/outcome.rs,json.rs,mcp.rs}` | §4.1.3 to §4.1.7, §4.1.15 |
| drivers and packages | `drivers/inillucent-driver-capi/src/capi/`, `packages/{npm,go}` | §4.1.13, §4.1.14 |
| scalar functions | `inillucent-scalar/src/builtin.rs` | §4.1.9 |
| the suite | `crates/inillucent-compat/tests/`, `crates/inillucent-sim/src/`, `tests/selection.toml`, `.gitattributes`, `.github/workflows/`, `packaging/ship.ps1`, `tools/` | §4.4 |
| the record | `CHANGELOG.md`, `fuzz/README.md`, `tests/inillucent-testing-tdd.md`, `docs/` | §4.5 |

## Detailed technical sections

### 4.1 Correctness: the fifteen items that block release

Each carries the audit's evidence in one paragraph, the fix, and the test. The full evidence,
including the exact statements and both engines' output, is in `audit-correctness.md` under the
same numbering, and `b14-defects.sql` reproduces every SQL item.

#### 4.1.1 `ORDER BY … LIMIT` over a recursive CTE applies the limit before the sort

```sql
WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM r WHERE n<5)
SELECT n FROM r ORDER BY n DESC LIMIT 1;
-- sqlite3: 5      inillucent: 1
```

`LIMIT 2` returns `2, 1` where SQLite returns `5, 4`: the first two generated rows were taken and
then sorted. Any operator between the CTE and the sort blocks the pushdown, which is why the suite
never saw it; the one query with no such operator is the ordinary hierarchy walk for the deepest
node, which returns the root at exit 0. The engine already refuses a `LIMIT` written inside a
recursive CTE as unsupported, so an outer `LIMIT` becoming an inner one is a transformation it
otherwise declines.

**Fix.** The rule is `source_limit_of` in `crates/inillucent-exec/src/physical/chain.rs`. It
decides the source's bound without asking whether a sorter will be put above it; `build_upper`
computes that answer (`sorted_already`) two lines later. Compute `already_sorted` first and add
`sort_keys.is_empty() || sorted_already` to the condition. The correct general rule: a `LIMIT` may
be pushed below an `ORDER BY` only into an operator that preserves the order the `ORDER BY` names,
and a recursive generation preserves no order.

This applies to every source the bound reaches. A reverse scan keeps its bound, because it is only
chosen when the planner already decided the walk answers the `ORDER BY`. The HNSW probe is
unaffected, because its bound is the `depth` the planner copied from the `LIMIT` when it chose the
probe; the chain limit only reached `iterative_candidates` on plans that have no residual, and
those never read it. (Corrected by task-2069 after task-2068 found that the rule as first written
pointed at the wrong file and left the vector path ambiguous.)

**Test.** The two statements above plus the hierarchy walk, in `differential_part8.rs`'s corpus
under a new `cte` category, and in `b14-defects.sql`. A generated arm follows in §4.4.4.

#### 4.1.2 A vector query fails the moment its HNSW index exists

The probe path at `crates/inillucent-search/src/store.rs:1033` calls `decode_vector(text.raw())`
on the TEXT literal's bytes and walks `chunks_exact(4)`, so `'[1,0,0]'` (7 bytes) is read as a one
dimension vector and refused against a three dimension index. Without the index the same statement
is answered correctly by a different parser at `crates/inillucent-scalar/src/builtin.rs:1116`;
`--params` works because `verbs.rs:73` rewrites a JSON array to an `x'…'` literal first. There are
three JSON array to vector parsers in the tree and the probe path has none of them.
`docs/vector-search.md:43` documents the spelling that fails. Every test of the indexed probe in
`vector.rs` and `vector_metric.rs` builds its query vector as a hex blob through the local
`literal()` helper, so the suite is green while the documented spelling fails.

**Fix.** Promote one JSON array parser to a shared helper in `inillucent-scalar` and call it from
the `Value::Text` arm at `store.rs:1030`, falling back to raw bytes only when the text is not a
JSON array. Delete the other two copies.

**Test.** The `docs/vector-search.md` example verbatim against an index, in `vector.rs`, plus a
case per spelling (JSON text, blob, bound parameter) asserting identical rows with and without the
index. `doc-facts` should carry that example so the documentation and the test are one thing.

#### 4.1.3 `dump` omits every row of a table whose column or table name is a reserved word, and loses an HNSW index

`CREATE TABLE t1 ("select" TEXT, b INT)` with one row dumps as the `CREATE` and no `INSERT`, exit 0;
a column named `""` dumps with the wrong arity and replays into the wrong column. A table with
`CREATE INDEX pv ON p USING inillucent_hnsw (v)` dumps its five shadow tables as ordinary tables
and never emits the `CREATE INDEX`, so the replayed database has no index and five orphan tables,
and every query still answers by scanning, so nothing reports it. The FTS5 half of `dump` already
does the right thing for a module: it emits the virtual table and suppresses the shadows.

**Fix.** In the row emitting half of `dump` (`crates/inillucent-cli/src/command/verbs.rs`, the
dump verb, and the shell's `.dump`), quote every identifier the way the `CREATE` half does, emit the
column list explicitly instead of relying on `SELECT *` arity, and treat a module index the way the
FTS5 path treats a module table: emit `CREATE INDEX … USING …` and suppress its shadow tables.

**Test.** A round trip in `cli_commands.rs`: a reserved word column, a reserved word table name, an
empty column name and an HNSW index; assert row counts, `PRAGMA index_list` and the query plan after
replay, on a database the test built.

#### 4.1.4 `integrity-check` reports `"ok": true` and exit 0 on a corrupt database

The pragma returns the failure as a row of text, as SQLite does. The verb at `verbs.rs:1018` lists
the rows and `outcome.rs:317` hardcodes `("ok", Json::Bool(true))` for any outcome. The only test,
`integrity_check_answers_ok_on_a_healthy_file` (`cli_commands.rs:1113`), runs the verb on a file
the test just wrote.

**Fix.** In the verb, inspect the returned rows and return `Err(Failed::said(Status::Corrupt, row))`
when the first row is not `ok`, so the exit code and the `ok` field follow the answer. Leave the
pragma returning rows.

**Test.** Corrupt one page's checksum of a database the test built, run the verb as a subprocess,
assert exit 1 and `"ok": false`, in `cli_commands.rs` beside the healthy case.

#### 4.1.5 `--params-file` reads any file on the machine, past `--root` and past `--readonly`

`verbs.rs:336` calls `read_to_string(path)` with no confinement check, and `params-file` is a
parameter of `query` and `exec`, both served over MCP. Against a server started
`--root <root> --readonly`, a request naming `C:/Windows/Temp/probe.json` returned its contents.
`confinement.rs` covers `ATTACH`, `VACUUM INTO`, backup, restore, import and export and has no case
for this parameter.

**Fix.** Put the path through `context.confine(path)?` at `verbs.rs:336`, and refuse `-` when
`context.confined()` the way `resolve_source` at `verbs.rs:1365` already does.

**Test.** Both cases in `confinement.rs`, over a spawned `inillucent-mcp`.

#### 4.1.6 One request kills the MCP server: unbounded recursion in the command line JSON parser

`crates/inillucent-cli/src/json.rs:290`: `value()` calls `object()` and `array()`, each of which
calls `value()`, with no depth counter. It is reached from the MCP request line, from `--params`
and from `--params-file`, which is `read_to_string` of a whole file with no size cap. A 240 KB line
of 120,000 `[` overflowed the stack at exit 127 in both the CLI and the server; with
`panic = "abort"` the session dies. The sibling parser in `inillucent-scalar/src/json/parse.rs:59`
already has `MAX_DEPTH = 1000`, which is why `json_valid()` on the same document returns 0 cleanly.

**Fix.** A `depth` field on `Reader`, charged in `object()` and `array()`, refusing past 1000 with
the same status the scalar parser uses; a size cap on `--params-file`. Bounding at parse also stops
the recursive `Drop` of a deep `Json` from overflowing on the way out.

**Test.** The 120,000 level line over a spawned `inillucent-mcp` in `mcp_session.rs`, asserting the
request is refused and the next request is answered; the same file through `--params-file` in
`cli_commands.rs`, asserting exit 1 and not 127.

#### 4.1.7 `migrate --kind sqlite` performs none of the verification it documents

`inillucent_migrate::sqlite::migrate` (`sqlite.rs:385`) does inventory, stage, checkpoint, reopen,
`verify_against` and refuses to publish on a failed report. The shipped verb
(`verbs.rs:1546`, `migrate_sqlite_file`) calls `Database::import_sqlite_into` and renames the file.
Consequences measured on the shipped binary: an FTS5 table is dropped and the migration exits 0
with no warning even under `--output json`, so a database whose only content is an FTS5 table
migrates to an empty file and reports success; `application_id` and `user_version` are dropped;
`sqlite_stat1` rows naming FTS5 shadow tables absent from the target are carried; and every
successful migration leaks its staging segment because `remove_staged` runs only on the error path.

**Fix.** Point `migrate_sqlite_file` at `inillucent_migrate::sqlite::migrate`, report `checks`,
`rows`, `tables` and `notCarried` the way the PostgreSQL path does, fail rather than exit 0 when a
table was not carried, and run `remove_staged` on both paths.

**Test.** In `inillucent-migrate/tests/cli.rs`: a fixture holding an FTS5 table, a `user_version`,
an `application_id` and `sqlite_stat1` rows; assert the FTS5 table and its rows arrive, the two
pragmas match, no stat row names an absent table, and no staging file remains beside the
destination. This is the same fixture §4.4.18 extends.

#### 4.1.8 One `PRAGMA cache_size` grows the process without bound

`pragma/tuning.rs:66` converts the request to a frame count with no ceiling and
`pool.rs:816` eagerly allocates per frame; `frames.resize` at `:838` is a bare resize where the
three vectors beside it use `try_reserve`. `PRAGMA cache_size = -1000000000` reached a 3.3 GB
working set in five seconds; a larger value was still climbing at 84 GB when killed. The pragma is
reachable from any application that passes user SQL and from the MCP server.

**Fix.** A `CacheSize` row in `compat/limits.toml` with a hard maximum, checked by name in the
pragma handler, and `try_reserve` in place of the bare resize.

**Test.** The pragma at one past the maximum refuses with the limit's name; the compat report
already checks `limits.toml` rows against the oracle's defaults and the new row joins it.

#### 4.1.9 `substr`, `trim`, `ltrim` and `rtrim` replace bytes that are not valid UTF-8

`builtin.rs:926` runs `from_utf8_lossy` before splitting, so `hex(substr(CAST(x'fffe80' AS TEXT),1,2))`
returns `EFBFBDEFBFBD` where SQLite returns `FFFE80`. `UPDATE t SET c = trim(c)` over a latin1
derived column rewrites every non ASCII value irreversibly. The same function is quadratic: it
builds a `Vec<Vec<u8>>` with one allocation per character. Everything else about encoding is
correct: no `from_utf8_unchecked` anywhere, TEXT carried as bytes, and `length`, `upper`, `lower`,
`replace`, `instr`, `printf`, `||`, `unicode` and `char` all matching SQLite on invalid bytes.

**Fix.** Walk the byte slice counting characters by the bytes whose top two bits are not `10`, find
the start and end offsets, slice once. No allocation, no replacement character.

**Test.** The two `hex()` statements above in `functions.rs`, plus a 1 MB `substr` under the
`perf` tier's count based guard.

#### 4.1.10 Recovery drops log records naming a tree it does not know, and does not count the drops

`recovery.rs:1125`, `tolerate_unknown_tree`, turns "the log names tree N, which this recovery was
not told the shape of" into `Ok(())` and drops the record. The argument that such a tree was
dropped before the end of the window has been wrong twice, in task-1932 and task-2033. The
task-2033 repair is asked at most once per tree and marks the tree refreshed before it knows the
read helped (`recovery.rs:1169`), so a record that arrives before its catalog page image is dropped,
and so is every later record naming that tree. `Recovered` has no field for skipped records, and
`recover.rs:631` counts a tolerated drop as applied.

**Fix, two parts.** (a) Push onto `refreshed_for` only when the refresh read a catalog row, or key it
on the catalog LSN so it retries once per catalog change. (b) Count every tolerated drop on
`Recovered`, print it from the CLI's open path, expose it through a pragma, and fail a strict
recovery on a non zero count.

**Test.** A campaign in `new_engine_recovery_shapes.rs` that builds a transaction whose first record
naming a new tree precedes the catalog page image, crashes after commit, reopens, and asserts both
the row count and that the drop counter reads zero. A second case asserts the counter is non zero
when a record is deliberately orphaned, so the counter is proven to count.

#### 4.1.11 A free map chain that does not terminate hangs the open

`file.rs:1361`, `read_free_map`, follows `page::right_of` with no visited set and no bound, pushing a
page image per hop. Pointing a free map page's right link at itself produced a query that never
returned. The leaf sibling walk in `paged/cursor.rs:71` already carries the guard.

**Fix.** The same guard: a hop counter bounded by `pool.page_count()` and a visited set, returning
`corrupt("a free map chain that does not terminate")`.

**Test.** The rewritten page in `corruption.rs`, asserting a corruption status within the test's
timeout. Note task-2065 is editing the free map's release path; this change is to the read path and
the two should be merged in whichever order lands first, with a comment on the other ticket.

#### 4.1.12 The HNSW and BM25 index readers size allocations from unvalidated on disk integers

`hnsw.rs:537` (`vec![0u8; n_nodes]` and three siblings) and `bm25.rs:477`
(`Vec::with_capacity(n_terms)` from a raw `u64`), each from a header field with no ceiling and no
check against the bytes remaining, reached from an ordinary `SELECT` over an `inillucent_search`
table. `binio.rs:10` already states the rule and `read_records` already follows it.

**Fix.** Route both through `binio::read_records` or `read_pod_vec`, or check against the
`MAX_PART_LIST` ceiling `persist.rs:912` defines, before any allocation.

**Test.** A segment blob with each header field set to `u64::MAX`, opened through the search table,
asserting a corruption status rather than an abort. Add the segment header to the fuzz targets
(§4.4.7).

#### 4.1.13 Three C ABI entry points dereference a caller's pointer without the liveness check

task-1979 gave every handle a magic word read through `held()`. `capi/db.rs:632`
(`inillucent_txn_execute`), `:682` (`inillucent_txn_commit`) and `capi/stmt.rs:232`
(`inillucent_clear_bindings`, which writes into freed memory) were missed. Begin, roll back, then
commit the same pointer returned a syntax status having followed a freed `Rc` to a live database
and issued a real `COMMIT`. A Python `Transaction.__del__` after an explicit `commit()` is that
sequence.

**Fix.** The `held()` guard `bind` already has at `lib.rs:387`, on all three.

**Test.** Three `check_status(..., INILLUCENT_MISUSE)` cases in `tests/c/lifecycle.c`, and a Python
case in `python_conformance` that commits then lets the object be collected.

#### 4.1.14 Node and Go corrupt every integer past 2^53, and neither conformance runner can fail

`--output json` emits raw i64. `packages/npm/inillucent/index.mjs:174` reads it with `JSON.parse`,
which turns `9007199254740993` into `9007199254740992`; `packages/go/inillucent.go:236` decodes
without `UseNumber()` so every number is a `float64`. `suite.json:1290` deliberately carries
`9007199254740993` to catch this, and both runners compare two values corrupted the same way:
`conformance.test.mjs:107` falls back to `String(want) === String(got)` where both came through
`JSON.parse`, and `conformance_test.go:186` casts both sides `float64 → int64`.

**Fix.** An opt in rendering in `--output json` for an integer outside the safe range as
`{"int":"9007199254740993"}`, matching the `{"blob":"hex"}` convention the write path uses; a
`BigInt` from Node and a `json.Number` from Go; both runners comparing the raw text of the number.

**Test.** The existing suite case, once the runners compare text, fails on `8607adf` and passes
after. That is the proof the runner can fail.

#### 4.1.15 A BLOB cannot be read back through `--output json`

`SELECT x'00ff'` under `--output json` returns the string `"x'00ff'"` typed `text` while `typeof`
says `blob`; nothing distinguishes it from a TEXT column holding that literal. Bytes go in as
`{"blob":"<hex>"}` and cannot come back, for Node, Go, PHP and the subprocess half of Python.

**Fix.** Render a blob as `{"blob":"<hex>"}` in `--output json`, and decode it in each wrapper.

**Test.** A bytes in, bytes out case in `drivers/conformance/suite.json`, graded by all five
runners.

### 4.2 Correctness: fix before release

Eleven items `audit-correctness.md` grades below the line, with the fix in one line each; the
evidence is under the same number there.

| # | item | fix | test |
|---|---|---|---|
| 16 | the declared page count is never checked against the file length, and `integrity-check` says ok on a file that is short | compare at open and in the check; refuse by name | a truncated file in `corruption.rs` asserting the message names the page count |
| 17 | the page LSN sits outside the page checksum, so a flipped bit there is undetectable | a format change; record it in §9 as a decision and add the detection test to the format ticket | a flipped LSN byte asserting corruption once the format covers it |
| 18 | recovery's scan can jump a zeroed hole at a segment boundary | treat a zeroed header at a boundary as the end of the log, not as a gap to step over | a campaign case in `recovery.rs` |
| 19 | no directory fsync after a file is created on POSIX; macOS gets `fsync` rather than `F_FULLFSYNC` | fsync the parent directory in the VFS after create and rename; `F_FULLFSYNC` on macOS | the VFS conformance suite gains a case counting directory syncs per create |
| 20 | `randomblob(n)` is silently clamped to 1,000,000 bytes | honour the value length limit and refuse above it by name, as SQLite does | one case in `functions.rs` |
| 21 | `Limit::Column` is enforced on the result set and not on `CREATE TABLE` | enforce at `CREATE TABLE` and `ALTER TABLE ADD COLUMN` | `hostile.rs` |
| 22 | a recursive CTE is capped at a million passes and a legitimate query is refused | raise to SQLite's behaviour (no cap; the row budget is the limit) or make the cap a named limit | `hostile.rs` |
| 23 | a reader fails rather than waits while a writer holds the file | honour `busy_timeout` on the reader's lock acquisition | `busy_timeout.rs` gains the reader arm |
| 24 | a CSV that is not valid UTF-8 is refused with "cannot open" | name the byte offset and the reason | `cli_import.rs` |
| 25 | no pragma honours a database qualifier | route `PRAGMA db.name` to the named database | `new_engine_pragma.rs` over an attached database |
| 26 | `--readonly` refuses the `run` verb wholesale | let `run` execute read only statements and refuse each write by name | `mcp_wire.rs` |

Item 27 in the audit, the smaller verified differences (four defaults chosen differently, six error
wordings, five minor behaviours), is recorded as a table there. The defaults stay as they are and are
documented in `docs/sql.md`'s differences table; the five behaviours are fixed if the fix is a line
and left as documented differences otherwise, each with a test asserting whichever it is (rule 1.3).

### 4.3 Performance: the shapes the gate cannot see, then the cheap wins

Baselines are the published 2026-09-20 run in `docs/performance.md`. Where a section quotes a fresh
number it is a ratio from an interleaved run on a contended box, and it is used for direction only.

#### 4.3.1 A correlated subquery is prepared once, not per outer row

`correlate.rs:165` clones `Params` and calls `run_any`, which calls `prepare_any` then
`run_any_prepared`, per outer row. `prepare_any` runs the covering candidate trial at
`stages.rs:478`, a full speculative pipeline build per candidate, and `run_prepared` builds the
chain again. `EXISTS` collects every row when it needs one. Measured (contended): 13.3 s for
`SELECT count(*) FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.g = a.g)` over 5,000 rows each on an
indexed column, 2.7 ms per outer row for one index probe.

**Change, in order.** (1) Call `prepare_any` once in `Correlation::new`, store the `Prepared`, and
call the already public `run_any_prepared` (`run.rs:247`) per row. (2) Answer `SubqueryKind::Exists`
with `CollectInto::with_limit(1)`. (3) Push outer rows in batches, only after (1) and (2) are
measured, because it changes the order a subquery with a side effect runs in.

**Why it is safe.** A `Prepared` is bound to the schema it was built against and the plan cache
already keys on that (`connect.rs:859`).

**Benchmark.** A `correlated` workload in the read gate: the `EXISTS` above and its `IN (SELECT …)`
form, graded against the equivalent join. Target: within 2x the join.

#### 4.3.2 A read statement no longer walks 4,096 pool frames to ask whether anything is dirty

`dirty_pages()` at `eviction.rs:339`, called from `locks.rs:487`, past a short circuit that fires
only for writers. About 4 µs on statements whose whole cost is 1 to 2 µs (`point.rowid` is 1.66 µs
published).

**Change.** A dirty frame counter maintained where a frame's dirty bit changes; the scan stays
behind a debug assertion that the counter equals it.

**Benchmark.** `point.rowid` outside a transaction through the shipped `Connection` API (§4.3.10).

#### 4.3.3 `Connection::prepare` parses a statement once

`connect.rs:952` and `statements.rs:663` parse the same text twice on the shipped API; the comment
at `connect.rs:947` records the hazard. About 560 ns of a cached prepare.

**Change.** Parse once and hand the AST to the second consumer.

**Benchmark.** `prepare.trivial` through the shipped API (§4.3.10).

#### 4.3.4 A range probe into an index whose leaves hold delta rows answers from the leaf

`join/loops.rs:521` calls `leaf.live_between(...)` when `needs_materialising()`, and `live_between`
(`leaf/read.rs:639`) filters after merging the whole leaf, where `live()` is quadratic in the delta
count (`read.rs:483`). `equal_span` (`paged/cursor.rs:389`) visits a written leaf whose sorted run is
empty and continues to the right sibling, so a probe into an index whose sorted region is empty
visits every leaf of the index. Measured (contended) on identical 5,000 row tables and the identical
join with plan `SCAN a` + `SEARCH b USING COVERING INDEX bg`:

| index built | time |
|---|---:|
| before the inserts, so the leaves hold delta rows | 3,209 ms |
| after the inserts, so the leaves are packed | 2 ms |

Halving the rows moved the delta case to 34 ms: the per probe cost is linear in the index, so the
probe is not a probe. `probe_one` was fixed for this and says so at `cursor.rs:526`; `probe_range`
never was. The gate cannot see it because `readgate.rs:359` imports from SQLite and an import
builds packed trees. `CREATE INDEX` before the load is the ordinary application order, and
`needs_materialising` is also true for any leaf holding an out of line value (`read.rs:105`).

**Change.** Give `probe_range` the shape `probe_one` has: find the matching span inside the sorted
region without materialising, and emit delta matches as constants merged on the way out. Stop
`equal_span` following the right sibling on a written leaf whose sorted run is empty unless the delta
area holds a key past the probe.

**Why it needs a differential test first.** A delta row shadows a sorted row and the visitor's span
is computed over the sorted region only, so a wrong replacement returns duplicate or stale rows. The
test in §4.4.4 (join TLP with the corpus mutated during the run, index built first) is written and
red on the timing arm before this change starts, and it is the correctness guard for it.

**Benchmark.** An `index-first` arm of the read gate: the same fixture loaded after `CREATE INDEX`,
every read workload run on it. Target: within 2x the packed arm on every workload.

#### 4.3.5 A cached `SELECT`'s column names are built once per compile

`plans.rs:476` and `explain.rs:30` allocate one string per column per execution on every
`Connection` driven read.

**Change.** Build `Outcome.names` at compile and share it by reference.

#### 4.3.6 A sort that does not fit in memory spills instead of ending the process

There is no external sort. `Sort` and `Distinct` hold every surviving row (`ops/order.rs:341`,
`rows: Vec<Vec<OwnedDatum>>`); `budget::materialise` exists (`budget.rs:311`) and is a no op when no
request armed a budget, the library default is `Limits::unbounded()` by design (`budget.rs:87`),
and `Sort` and `TopN` are the only buffering operators that never call it even when a budget is
armed (`budget.rs:298` enumerates what it covers). So `SELECT * FROM t ORDER BY label` over a table
larger than memory is an out of memory kill where SQLite spills to a temporary file and finishes.

**Change, in order.** (1) One line each: `Sort` and `TopN` charge `budget::materialise`, so a served
connection gets the refusal every other breaker gives. (2) An external merge sort: runs sorted in
memory to a threshold, written through the VFS's temporary file the index build already uses, k way
merged on the way out; a size bounded `GROUP BY` hash that falls back to a sorted aggregate past the
same threshold. (3) Sort `(encoded_key, row_index)` pairs from `key::encode_into_with` instead of
re dispatching on the datum discriminant per comparison; descending terms reverse the comparison
and `NULLS FIRST`/`LAST` stays separate, as `order.rs:445` handles it today.

**Why the threshold matters.** task-1869 measured a spill at 8 ms on a 27 ms `CREATE INDEX`, which
put `schema` under its floor. The threshold sits far above anything the gate reaches, and the
external path is graded by the benchmark below and never by the gate.

**Benchmark.** `scan.sort` and `SELECT * ORDER BY label` over a 10 GB fixture with the pool at 1/8 of
the file. Target: the sort completes, and scan throughput within 30% of SQLite at the same
`cache_size`.

#### 4.3.7 `DELETE` is super linear in row count

`tests/inillucent-testing-tdd.md` §8: 2,000 rows 11 ms, 4,000 rows 272 ms, 8,000 rows 2,075 ms,
against SQLite at 1 to 2 ms throughout. A capacity precheck in `write.rs::merge_if_small` was tried
and reverted at 18%. That section's own instruction stands: profile with `inillucent-writeprofile`
and `inillucent-hotprofile` before reasoning, because the model the reverted fix was built on was
wrong. `audit-performance.md` C6 names `live_order`'s quadratic shadow loop at `leaf/read.rs:394` as
a candidate and C1 names the delta area compacting on a count of 32 regardless of leaf size
(`mutate.rs:406`); a `DELETE` of half a table drives both.

**Change.** Profile the 8,000 row delete, fix what the profile names, and record the profile in the
ticket. C6 is a one day change with `live_order_agrees_with_live` as its proof and is done
regardless.

**Benchmark.** `delete.half` in the performance history already records it at 0.068 relative to
SQLite; the target is linear in row count and within 3x SQLite at 8,000 rows.

#### 4.3.8 Retrieval: the two filings that halve residency, and the default that is a linear scan

`core/src/store.rs:231` holds the store's text arena in memory, about 500 MiB of a 1,665 MiB
retrieval process, where `vectors.bin` is already filed and read on demand. `core/src/bm25.rs:131`
holds postings as a map plus a second copy of every term, 890 MiB resident for 615 MiB on disk.
And vector search defaults to `mode = 'exact'`, so a user who follows `docs/vector-search.md` gets a
linear scan and the HNSW graph is opt in.

**Change.** File the text arena the way `vectors.bin` is filed; flat arrays for postings with an
overflow map for the in place delta append. The default mode is a decision (§9); until it is made,
`docs/vector-search.md` says in its first example which mode the index is used in.

**Benchmark.** Resident MiB per million chunks after open, and p50 under both modes at the current
corpus; the retrieval card already measures p50.

#### 4.3.9 Designed and not built here: the delta area sized from the gap, and the compaction splice

task-2000 design 6 as the audit re-priced it. The delta area compacts on a count of 32 whether the
leaf holds 241 rows or 3,704 (`mutate.rs:406`, `leaf.rs:139`), which is why the two secondary
indexes are 69% of `write.insert.batch`; sizing it from the free gap is worth 18 to 20% of that
workload. A compaction re-encodes every kept row (`write.rs:446`); splicing the delta rows in is
worth 16% of the transaction. Both are page format changes that redo must reproduce byte for byte,
both land on `write.rs`, `mutate.rs` and `leaf/` where task-2065 is working, and a spliced page
never re narrows its widths. They are the next performance ticket, after task-2065 merges, with the
index count sweep benchmark (0, 2, 5, 10 secondary indexes) added first so the gain is graded per
index rather than on the fixture's two.

#### 4.3.10 The shipped API gets an arm

`inillucent-fullgate` never opens a `Connection` or steps a `Statement`; it drives `plan`, `prepare`
and `pipeline` directly. `docs/performance.md:529` already names this. §4.3.2, §4.3.3 and §4.3.5 are
invisible to every published figure for that reason.

**Change.** A `--api connection` arm of the ten families beside the pipeline driven one, in the same
binary, interleaved the same way. Target: the two arms within 20%, or the difference explained on
the page.

#### 4.3.11 Two benchmarks that cost nothing

`tests/workloads/nikaya/statements.sql` holds 211 real application statements already replayed for
correctness by `story_workload_replay` and never timed; time it. `inillucent-checkpointperf` exists
and is in no release command list; run it from `release.rs` and gate the worst commit under 10x the
median.

### 4.4 The suite: instruments, then the gaps each instrument grades

#### 4.4.1 Continuous integration

`.github/workflows/tests.yml`: on every push and pull request, on `windows-latest` and
`ubuntu-latest`, build `inillucent-testrun`, fetch the SQLite reference with
`tools/sqlite-reference.ps1` (or its shell twin), build the gate fixtures, then run
`inillucent-testrun --strict`, `cargo fmt --check`, `cargo clippy` with the governed crates' denies
(today the panic ban is evaluated only by `tools/validate.ps1`), and the contract tests. The
runner's exit code is the job's: 2 is reported as "the run did not happen" with the reason, not as a
failure of a test. The Linux job is the first time the suite has run anywhere but this machine, and
the first run will name what is Windows only; each such target gets a `requires` row or a fix.

#### 4.4.2 The release script runs the suite

`packaging/ship.ps1` gains a `tests` phase between `preflight` and `version` that runs
`inillucent-testrun --strict` and refuses on any non zero exit, reading the three codes and saying
which. `-Only` cannot skip it; a `-SkipTests` switch exists, prints a sentence saying the release
is untested, and writes that sentence into the GitHub release notes.

#### 4.4.3 Binding conformance runs rather than reads leftovers

`bindings.rs:322` skips when `_agent_output/conformance/` is empty, and that directory is
gitignored; `packages/php/tests/conformance.php` is invoked by nothing. Make `bindings.rs` run the
five runners itself when the built binaries and the interpreters are present, with a `requires` row
naming each interpreter, and add the PHP runner to `tools/validate.sh` and `validate.ps1`.

#### 4.4.4 The query generator gains four arms, and the corpus moves under it

`tlp_differential.rs` generates one table, four columns, predicate depth two, no aggregate, group,
join or pivot, over a corpus built once in order. In order of value:

1. **Aggregate TLP.** `SELECT sum(a) FROM t` against the union of the three partitions' sums; the
   same for `count`, `min`, `max`, `avg` and `total`.
2. **`DISTINCT` and `GROUP BY` TLP.**
3. **Join TLP** over a second table, with the index on the join column created **before** the load,
   so the range probe path of §4.3.4 is under the generator from the first run.
4. **PQS.** `pqs_differential.rs` in the `engine` tier: pick a row, build a conjunction of atoms
   each verified true of it in Rust, assert the row comes back, before and after `ANALYZE` and with
   each index dropped in turn. It needs no oracle and it catches the failure TLP cannot: a
   comparison rule wrong the same way on all three partitions.
5. **Widen the atoms** to `LIKE`, `GLOB`, `CAST`, arithmetic and `COLLATE NOCASE`, with a
   `TEXT COLLATE NOCASE` and a `BLOB` column in the fixture.
6. **Mutate the corpus during the run**: every 50 cases delete a range and reinsert it, so the tree
   has split and merged.
7. **Depth** generated in `1..=5`, with seed and depth in every failure message.
8. A **recursive CTE arm** for §4.1.1: a generated hierarchy, `ORDER BY depth DESC LIMIT k` against
   the oracle.

#### 4.4.5 One nightly target larger than the buffer pool

`story_large_table_nightly.rs`: a table an order of magnitude past the pool, sized from
`PagerOptions`' cache size so it scales with the default, TLP and NoREC over it, then
`PRAGMA integrity_check`. Nothing above 20,000 rows is graded today, and every differential case
runs on six rows.

#### 4.4.6 The corruption sweep asserts refusal, over the shipping format

`phase2_campaigns::corrupt_pages_never_panic` (`:440`) flips every byte of a leaf, interior and meta
page and asserts nothing beyond no panic. Change it to assert every outcome is an exact match with
the undamaged read or a status in the corruption family, counting both, reusing
`corruption::assert_expected`. Then a sweep over the shipping `.rdb` format and its log, with
per damage shape detection rates recorded in a checked in file the way `tests/crash/` records cut
counts, and each rate asserted not to have fallen; `truncated` asserted near 1.0 directly. The
existing `corruption.rs` sweep runs against SQLite format fixtures through the storage pager and
its floor is `refused > total / 20`.

#### 4.4.7 The fuzz targets run, and three new ones exist

`tools/run-fuzz.ps1` installs nightly beside the pinned toolchain, runs each target for a bounded
time, writes a row per target into `tests/fuzz-history.tsv`, and the minimised corpus is checked in
under `fuzz/corpus/`. New targets, ranked: `sql_text` over `Connection::prepare` (the largest
untrusted input in the product), `fts5_query` over `MATCH`, `sqlite_file` over the migration
reader, and the segment header for §4.1.12. `fuzz/README.md` names all twelve existing targets; it
names seven today.

#### 4.4.8 Two failure shapes the fault model lacks

`Failure::Misdirected` in `inillucent-sim/src/failpoint.rs`: a valid page written to the wrong
offset, the one damage a per page checksum cannot catch, with a test asserting the reader refuses
the page by number. `MediaModel::sync_is_a_lie` in `media.rs`: `sync` returns `Ok` and leaves the
cache unresolved, with a campaign asserting the loss is detected rather than served. Both are what
consumer drives and container file systems actually do.

#### 4.4.9 The crash reports stop churning

`/tests/crash/** text eol=lf` in `.gitattributes`, and `crash_reports.rs` in the `tooling` tier
parsing each report's header and failing when a recorded cut count is below the checked in one.
Today every run marks seven files modified with an empty diff, the review signal the directory
exists for is noise, and the dirty tree blocks `ship.ps1` and `cargo publish`.

#### 4.4.10 A WAL replay model

`wal_model.rs` in the `durability` tier: a `BTreeMap<PageId, Vec<u8>>` updated by the same record
stream, compared against replay after every record, with `inillucent-compat/src/model.rs`'s
`Shape`/`Op`/`shrink` structure. The B tree has this and is the best tested component in the tree;
the log has 1,603 lines of hand written cases and no model.

#### 4.4.11 The skip guard accepts a multi line message and `return None`

`live_postgres.rs:42` and `live_mysql.rs:38` announce with a bare `eprintln!` whose literal starts
on the next line and end `return None;`; `policy::every_skip_site_goes_through_the_one_helper`
reads the message from the `eprintln!(` line and requires `return;` within four lines, so it sees
neither, and `INILLUCENT_STRICT` never panics there. The runner catches it at the target level
through `requires`; a plain `cargo test -p inillucent-remote` does not. Fix the guard to read a
multi line literal and accept `return None;`, then move both sites onto
`inillucent_base::testing::skipping`.

#### 4.4.12 Tests that were written to pass, made able to fail

The four the review found, each already named above: the vector probe test using the documented
spelling (§4.1.2), the integrity check on a corrupt file (§4.1.4), the Node and Go runners comparing
text (§4.1.14), and the recovery counter proven to count (§4.1.10). Plus the corruption floor of
§4.4.6 and the allocation campaign of `tests/crash/allocation.txt`, which records `points: 0` because
the write path allocates with plain `Vec` rather than through `inillucent_base::buffer`; route those
allocations through the buffer so the existing failpoint reaches them.

#### 4.4.13 The edges of end to end

`semantics.rs` is one `#[test]` over 2,397 lines, so the first disagreement stops the run; split it
per category or collect every disagreement before asserting. Unknown method and protocol version
mismatch are proven only in process (`mcp.rs:1013`, `:869`); drive both over a spawned server in
`mcp_session.rs`. `.import` with an embedded newline in a quoted field and CRLF rows is tested on
export and not on import. `inillucent-cli` is at 50.1% line coverage (`docs/repository.md:248`);
raise the floor and add the test that fails when it drops.

#### 4.4.14 Generated collation ordering, generated numeric text, and the migration values a real database holds

A generated arm in `ordering.rs`: random strings with case variants, combining marks, surrogate
pairs and embedded NULs through a `NOCASE` and a `BINARY` index, index order against scan order
against the oracle. `numeric_text.rs`: `printf('%s', <real>)` and the default rendering against the
pinned shell over random doubles including subnormals, negative zero and the 15 to 17 digit boundary,
because `differential.rs` compares tagged bytes and never grades the text a user reads.
`migrate_sqlite.rs` gains expression indexes, a blob with an embedded NUL, NaN and infinity reals,
a unicode table name and the affinity edges, in the same fixture §4.1.7 uses.

#### 4.4.15 Freshness

A case in `workload_freshness.rs` failing when the newest row of `tests/nightly-history.tsv` is
more than seven days old, and `tools/run-nightly.ps1 -Register` run and recorded. The ledger holds
two rows from the commit that created the script.

#### 4.4.16 Two targets that cannot run in a worktree

The strict run in this review found them, and both read as "missing prerequisite" when neither is.
`setup_embeddings.rs`'s `binary()` runs `cargo build -p inillucent-cli --bin inillucent` and then
looks for the result at the hardcoded `workspace_root()/target/debug/inillucent.exe`, so under the
redirected `CARGO_TARGET_DIR` every worktree uses the build succeeds and the lookup fails, and all
six cases skip on every worktree run. Resolve the binary through `env!("CARGO_BIN_EXE_…")` or by
reading cargo's `--message-format json` output, the way `gates_fail_closed.rs` already does and
says so in its comment. Second, the three `gates_fail_closed` cases that spawn a nested
`inillucent-testrun` (`testrun_passes_a_real_run_with_code_zero` and two siblings) race with
whatever else is linking into the same target directory and skip with the message their own panic
carries. Give the nested run its own `CARGO_TARGET_DIR` under the test's temporary directory, or
run those three under a `serial` marker the runner honours, so a strict run in a worktree has no
named skip that a person on this machine could not clear.

The teacher model at `J:/inillucent-embeddings/models/qwen3-embedding-4b-teacher` fails to parse
("Protobuf parsing failed") and takes five `inillucent-bench` cases with it. That is a file on this
machine and not the tree; it is listed in §9 for a person to re-fetch.

### 4.5 The record

1. `CHANGELOG.md` gains `## 0.1.5`, `## 0.1.6` and `## 0.1.7` from the tags' commit ranges, and
   this ticket's work goes under `## Unreleased`.
2. `tests/performance-history.tsv` is appended by `inillucent-perfhistory` from the same run that
   grades §4.3.7, so the super linear `DELETE` has a current number beside its fix.
3. `tests/inillucent-testing-tdd.md`'s tier table moves with every row added to `selection.toml`,
   in the same commit, because `the_per_tier_table_matches_the_map` fails otherwise.
4. `drivers/README.md` states which bindings link the C ABI (Python, C) and which spawn the command
   line (Node, Go, PHP), so the testing consequence in §4.1.14 and §4.1.15 is written down.
5. `docs/performance.md` gains no new number here (task-2064), and its "What is not measured"
   section names the arms §4.3.10 and §4.4.5 add, so the next reader knows they exist.
6. `.claude/repo-plan.md` gains a note that every sub agent of one session shares one scratchpad,
   and that two agents naming a database the same thing in it collide (§5).

## 5. The incident, and what the reproduction found

While the performance audit was building a 100,000 row fixture, its `main_table` disappeared and a
table named `b` it had never created was in the file, with log segment 7 older than segment 5. It
looked like data loss. `incident-lost-table.md` reconstructs it from the session transcripts and
reproduces it both ways.

**Verdict: not an engine defect.** Every sub agent of one session shares one scratchpad directory,
and a third sub agent ran `rm -f t.rdb; inillucent create t.rdb; CREATE TABLE b (i, r, t, z)` against
the same path at 20:44:03, two minutes after the auditor's last write. The surviving schema matches
that statement character for character. Segment 7 outlived its database because `rm -f` removed
only the main file; the replacement started a fresh chain at one; two chains were read as one. The
reported sequence, run five times in fresh directories with six concurrent reader processes, gave
`main_table` and `t2`, 100,000 rows, integrity ok, and ended at segment 7 every time. The collision
variant reproduces the incident's file listing to the byte.

**What the engine did right.** `inillucent create` refuses an existing path (`verbs.rs:497`), so it
destroyed nothing. `belongs_to` checks the database uuid, `read_chain` stops at a foreign segment,
`open_segment` truncates and replaces one; driving the replacement database past sequence 7
replaced the orphan cleanly with integrity ok. A mid flight delete on Windows is deferred while a
handle is open.

**The gap is silence, and it gets three tests.** A foreign uuid segment at a database's own path is
direct evidence the path was reused, and the engine discards it without a word; `--db` on a deleted
path silently creates a new empty database.

1. A CLI test that `create` on an existing path refuses: the guarantee is stated in two places and
   asserted nowhere.
2. An end to end test for a reused path driven past the orphan's sequence, asserting the orphan is
   replaced and integrity is ok.
3. A warning on stderr, and a field in `--output json`, when a foreign segment is found at open.

And a note in `.claude/repo-plan.md` (§4.5.6) that sub agents share one scratchpad, so a database
name in it must carry the agent's own prefix.

## Alternatives considered

**Adopt a property testing crate.** `proptest` and `arbitrary` are absent by policy
(`docs/dependency-policy.md`), and `btree_model.rs` shows the house generator with its own shrinker
and retained corpus is at least as good. The four new generated arms follow that template.

**Import SQLite's SQLLogicTest corpus wholesale.** `tests/conformance/` runs any upstream `.test`
file with no new code and holds exactly one. The `evidence/` and `index/` suites are thousands of
files and would add hours to a run; they belong in the nightly tier behind the same `allow.list`
discipline `differential_part8.rs` uses. That is the right second step and it is not in this
ticket, because the generated arms grade more of the planner per minute than a recorded corpus
does.

**Fix the delta area format now.** It is the largest write path gain left and it is a page format
change that redo must reproduce byte for byte, landing on the files task-2065 is in. The
correctness items in §4.1 and the three application shapes in §4.3 are worth more per day and carry
less risk. §4.3.9 is designed so the next ticket starts from a priced plan.

**Make the gate see delta leaves by changing the import.** Tempting and wrong: the gate would then
grade a different tree from the one every published number was taken on, and task-2064 could not
compare. An `index-first` arm beside the packed one keeps both numbers.

**Spill early to buy back the memory bar.** Declined twice already (`docs/closed-items.md`);
task-1869 measured it at 8 ms on a 27 ms statement. The threshold in §4.3.6 sits above the gate.

**Fix the four "tests written to pass" by deleting them.** Rule 1.3: a known gap is a test that
asserts it. Each is made able to fail instead.

## Testing strategy

- **Every fix in §4.1 and §4.2 is proven by reverting it.** The implementation ticket records, per
  item, the test name, that it fails on `8607adf`, and that it passes after. `b14-defects.sql` is
  the acceptance run: both engines, no diff.
- **Every change in §4.3 has its benchmark arm added first**, so a before number exists, and every
  number the implementation ticket quotes says what else was running on the box. If a number is
  load bearing, the ticket asks for a quiet window with the `QUIET REQUEST` protocol rather than
  quoting a contended figure. The published page is not edited; task-2064 does that.
- **The strict run is the gate at every step.** `inillucent-testrun --strict` from a worktree with
  the oracle junctioned in and the gate fixtures copied, redirected to a file, the runner's own exit
  code read. `2` means the run did not happen and nothing in it may be read as a pass.
- **The suite's own honesty tests stay green**: `policy`, `selection`, `documentation`,
  `command_parity`, `gates_fail_closed`. A new `tests/*.rs` file has its row and its tier count in
  the same commit.
- **The continuous integration workflow is proven by a deliberately red push** on a scratch branch
  before it is trusted, on both operating systems.
- **The Linux job's first run is expected to name Windows only targets.** Each gets a `requires`
  row with the reason, or a fix, and never an unconditional skip.

## Order of work

Phases, each mergeable on its own, each leaving `main` releasable. Sizes are the audits' estimates
and are days including the test.

| phase | what | sections | size |
|---|---|---|---|
| 1 | continuous integration, the release script's test phase, the crash report line endings, the skip guard fix, the two targets that cannot run in a worktree, the changelog sections | §4.4.1, §4.4.2, §4.4.9, §4.4.11, §4.4.16, §4.5.1 | 3 |
| 2 | the fifteen release blockers, each with its test, in the audit's order; `b14-defects.sql` clean | §4.1 | 10 |
| 3 | the generator's four arms and the recursive CTE arm, PQS, the corpus mutation, the large table nightly | §4.4.4, §4.4.5 | 6 |
| 4 | the correlated subquery, the pool scan, the double parse, the column names, the range probe with its join TLP guard, the shipped API arm | §4.3.1 to §4.3.5, §4.3.10 | 8 |
| 5 | the sort that spills, `DELETE` profiled and fixed, the two free benchmarks | §4.3.6, §4.3.7, §4.3.11 | 8 |
| 6 | the eleven fix before release items | §4.2 | 5 |
| 7 | the corruption sweep over the shipping format, the two fault shapes, the fuzz runner and three targets, the WAL model, binding conformance that runs | §4.4.3, §4.4.6, §4.4.7, §4.4.8, §4.4.10 | 8 |
| 8 | the retrieval filings, the end to end edges, the generated collation and numeric arms, the migration values, freshness, the rest of the record | §4.3.8, §4.4.13 to §4.4.15, §4.5 | 6 |

Phase 1 is first because it is small and everything after it is graded by it. Phase 2 is second
because it is what stops a release. Phase 3 precedes phase 4 because §4.3.4 must not start without
its guard. The next performance ticket (§4.3.9) is filed when phase 5 merges and task-2065 has
merged.

## Coordination with the tickets beside this one

- **task-2065** is changing page release and the free map in `write.rs`, `mutate.rs`, `leaf/` and
  `pool/file.rs`. §4.1.11 touches `file.rs`'s read path and §4.3.7 may touch `leaf/read.rs`; the
  implementation ticket posts on task-2065 naming the functions before editing them, and §4.3.9 does
  not start until task-2065 merges.
- **task-2064** re-measures the published numbers after the implementation merges. Nothing in this
  ticket edits `docs/performance.md`'s figures or inillucent.com.
- **A quiet window** is needed only for a number the ticket intends to quote as a result. The
  correctness work, the suite work and the profiling that names a cost need none; the before and
  after arms of §4.3 are ratios from the interleaved gate and survive a loaded box.

## Decisions for Jason

1. The default vector search mode. `exact` is a linear scan; `approximate` is what the index is
   for. §4.3.8 documents whichever it is and does not change it.
2. The page LSN outside the checksum (§4.2 item 17) is a format change. This ticket adds the test
   that shows the gap; the format ticket closes it.
3. The three bars `docs/closed-items.md` records as unreachable or closed by decision. task-2064
   will grade against them again; if they are to move, `compat/perf/contract.toml` is where.
4. The `bind.rs` split and the `Identifier` type, recommended in four reviews, still have no ticket.
5. Whether the SQLLogicTest import (Alternatives) is wanted as its own nightly ticket.
6. The teacher model at `J:/inillucent-embeddings/models/qwen3-embedding-4b-teacher` fails to
   parse on this machine and takes five `inillucent-bench` cases with it; re-fetching it is a
   person's job, and until then every strict run names it.

## Done means

- [ ] `b14-defects.sql` runs on `inillucent` and `sqlite3` with no diff, and every item in §4.1 has
      a named test that fails on `8607adf` and passes on the branch.
- [ ] Every item in §4.2 is either fixed with its test or recorded in `docs/sql.md`'s differences
      table with a test asserting the difference.
- [ ] `.github/workflows/tests.yml` exists, has been proven red on a scratch branch on Windows and
      Linux, and is green on the branch.
- [ ] `packaging/ship.ps1 -WhatIf` prints the `tests` phase, and a run with a failing test refuses.
- [ ] `inillucent-testrun --strict` from a worktree with the oracle and fixtures in place names no
      skip that a person on this machine could not clear: `setup_embeddings` and the three nested
      runner cases in `gates_fail_closed` run and pass (§4.4.16), and the only named skips left are
      the machine's own (no Go, the network opt in, the `embed` build, the live servers, the
      teacher model).
- [ ] `tlp_differential.rs` has aggregate, group, join and recursive CTE arms and mutates its corpus;
      `pqs_differential.rs` exists; `story_large_table_nightly` exists and has a row in the ledger.
- [ ] The `index-first`, `correlated` and `--api connection` arms exist in the gate, each with a
      before number recorded in the ticket, labelled with what else was running.
- [ ] §4.3.1, §4.3.2, §4.3.3, §4.3.4, §4.3.5 and §4.3.6 are built, and `SELECT * ORDER BY label` over
      a fixture larger than the pool completes.
- [ ] The 8,000 row `DELETE` profile is in the ticket and the fix it named is built, with
      `delete.half` re-recorded in `tests/performance-history.tsv`.
- [ ] `git status` is clean after a full run: no crash report churns.
- [ ] `tools/run-fuzz.ps1` has written at least one row per target to `tests/fuzz-history.tsv`, and
      `fuzz/corpus/` is checked in.
- [ ] `CHANGELOG.md` has sections for 0.1.5, 0.1.6 and 0.1.7.
- [ ] The next performance ticket (§4.3.9) is filed with the index count sweep as its first step.
- [ ] The branch is merged to `main`, the worktree retired, and the ticket's closing comment lists
      each item above with the commit that closed it.
