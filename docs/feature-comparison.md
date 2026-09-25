# Feature comparison

**inillucent against SQLite 3.53.4, and its retrieval engine against PostgreSQL + pgvector.**

The project has two goals and this document is the scorecard for both:

1. a **highly performant SQLite replacement offering the same features**, and
2. an **embedding solution that matches pgvector**.

Words used here and not explained here - pragma, collation, affinity, HNSW, BM25, recall - are
in [the glossary](glossary.md), one sentence each.

## The top-line comparison

**The whole scorecard in five rows.** Everything below this section is the evidence for one of
them.

| | | measured |
|---|---|---|
| **Faster than SQLite** | **397% faster** | 4.97x weighted over the contract's ten families, median of four consecutive 30-round runs on `main` on 2026-09-23, with both engines pinned to the performance cores. The 95% lower bound the gate actually grades on is **4.62x**, i.e. **362% faster**, against a 3.00x bound it clears on all four. Unpinned, the same gate reads 4.40x, because Windows then runs this engine on the efficiency cores and SQLite on the performance cores |
| **Faster than pgvector** | **174% faster unfiltered, 6,169% faster filtered** | retrieval p50 0.8462 ms against 2.315, and 0.5820 ms against 36.486 with a `source =` predicate, against the *better* of the two pgvector configurations |
| **Less CPU** | **50% less CPU** | 555 ms of processor against SQLite's 1,082, same plan, one child process each. Ratio 0.500x against a 0.400x bar, missed on all four runs. It was 0.400x on 2026-09-20; the plan has since gained four correlated subquery workloads that took this engine 182 ms a round and SQLite under half a millisecond. At `52c4b5f` they take under 2 ms and the round is 367 ms against 1,102, **67% less**, on passes the gate did not grade because the machine was busy; [Performance](performance.md#measured-again-at-52c4b5f-on-2026-09-24-and-not-graded) has them |
| **Less RAM** | **it is not less. It is 9.5% MORE** | 40.76 MiB peak resident against SQLite's 37.22, on the same 128 MiB budget. It was **102% more** before review 6, **43% more** before review 7 and **14% more** before the index build stopped going through the page pool, and the bar asks for **5% less** - so this is the one headline that is still a loss |
| **Same features as SQLite** | **96.6% byte for byte, 98.5% of what SQLite answers, none refused** | 402 of 416 probed cases produce SQLite's exact bytes. Of the other 14, 6 are vector features SQLite does not have, 6 answer differently, and 2 are `DELETE` and `UPDATE` with `ORDER BY ... LIMIT`, which the pinned SQLite build refuses and inillucent runs. [Why it is not 100%](#why-it-is-not-100) says what each is and which can ever be closed |

**This figure has read 403 twice, with a dip to 391 between.** It was first measured at **403 of 416
with 0 refused** through `inillucent-shell` while that shell still ran an engine this project
retired. `inillucent` re-exports `inillucent-engine` now, so the shell under the probe became a
different program, and re-running it from scratch against the engine that ships gave **391 same, 12
refused**. All twelve were window functions, and the cause was narrow: the compiled-statement path
bailed out on compound selects and had no matching check for windows, so a windowed statement reached
a pipeline builder that refused it, while a correct window implementation sat behind a path only a
test called. Reconnecting the two restored every case. The probe read **403 same, 0 refused, 7
answering differently, 6 features SQLite does not have** after that, and **404 same, 0 refused, 6
answering differently, 6 features SQLite does not have** when it was re-run on 2026-09-23. The
one that moved is `PRAGMA locking_mode`, which reports `normal` as SQLite does now that `normal` is
the default, and `sql.select.window` and
`functions.window` are `pass` in `compat/sqlite-3.53.4.toml` to match. Since `DELETE` and `UPDATE`
began taking `ORDER BY ... LIMIT` it reads **402 same, 0 refused, 6 answering differently, 6
features SQLite does not have and 2 accepted that the pinned SQLite refuses**, re-run on 2026-09-24.

So the sentence this table used to carry — *"there is no case SQLite answers that this engine
refuses"* — was true of a program that is no longer what you install. It is written out here rather
than quietly restated because the correction is the useful part: a parity figure measured against a
component that has since been replaced is a figure about nothing, and this one survived a
rearchitecture without anybody noticing.

**Three of the four performance numbers are wins and the fourth is not, which is why it is written
out rather than rounded off.** The speed is not bought by burning cores - a fifth of the time at half
the processor - and it is not bought by holding the database in memory either: the file on
disk is now **1.036x** SQLite's, within 4%. The extra **3.5 MiB** of RAM is the page pool and one
`CREATE INDEX`'s arena, and [Where the memory goes](#where-the-memory-goes) attributes every megabyte
of it.

Against pgvector the footprint goes the other way: the whole store the queries run against is
**46% less on disk**, and it needs **two fewer processes** because it is a library rather than a
server plus an embedding service.

---

First written and then re-measured three times as review 5, re-measured again on 2026-09-08 as
review 6, and **the 416-case probe re-run from scratch for review 7**, which is where the parity
number below comes from. It is a measurement taken on this build rather than a
figure carried forward, and re-running it is how the two refusals in
[Why it is not 100%](#why-it-is-not-100) were found. Every row below is a measurement rather than a reading of the
source: each feature is a whole SQL script run through `inillucent-shell` and through the pinned
`sqlite3` 3.53.4, over its own fresh database, with every byte of both streams compared. That is the
discipline `crates/inillucent-compat/tests/semantics.rs` applies - to 208 constructs - widened here to
**416 cases across the whole feature surface**. The harness is checked in as `tools/feature-probe/`
and its transcripts are under `_agent_output/`; [Reproducing this](#reproducing-this) says how to run
it.

**What review 5 added.**

1. **Processor time and resident memory are now measured**, not just elapsed time - for both engines,
   as a whole child process running the same plan. A comparison that reports only elapsed time is
   answering a third of the question, and the two figures it was leaving out do not both point the
   same way.
2. **Every performance figure is written as a percentage as well as a ratio.** Speed is
   **N% faster**, and a workload that is slower says **N% slower** rather than printing a ratio
   below 1.00. Processor time and memory are **N% less CPU** and **N% more memory**, because there
   less is plainly better. The ratio is kept beside every one of them.
3. **The memory is investigated rather than reported**: where it goes, what plausible causes were
   ruled out by measurement, and what it would take to hold less than SQLite.
   See [Where the memory goes](#where-the-memory-goes).

**What review 6 then did about it.**

1. **The contract grades memory and processor time**, with bars written *before* the work that had to
   meet them. Ten elapsed-time families and nothing else meant an engine that doubled its resident
   set still passed the gate.
2. **The resident set went from 75.25 MiB to 53.28** against SQLite's 37.19 - 102% more to 43% more -
   by bounding the redo buffer, stopping the index build holding three copies of the tree, collecting
   a version log nothing was collecting, and capping the allocator's free list in bytes as well as
   blocks. Each is measured on its own below.
3. **The attribution is per workload rather than per family**, with the buffer pool separated from
   everything else the process holds - which corrects one reading of review 5's table and shows that
   the remaining gap is the **file format** rather than another buffer.
4. **The silent difference is closed**, and the audit that found it is now a checked-in test that
   fails a build: `crates/inillucent-compat/tests/registers.rs`.

**Every count and every number below is from this review's own run.** A document that says a feature
works because somebody implemented it is the thing the probe exists to replace.

---

## Why it is not 100%

**Fourteen of the 416 cases are not byte-equal to SQLite, and none of them is refused.** All
fourteen answer - none of them is silent -
split between vector search features SQLite has no equivalent for, three kinds of measured or
structural difference, and one compile option the pinned SQLite build does not have. Two cases are
accepted here that the pinned build rejects.

| how many | what they are | can it ever be closed? |
|---|---|---|
| **6** | **vector-search features SQLite does not have** - the `vec0` table, the distance functions and the operator spellings. There is no SQLite output for them to be byte-equal to, so they cannot count as agreement however well they work. All six work | **No, by construction.** They are extras, not gaps |
| **2** | **one decision this engine made and measured**: `PRAGMA page_size` is 32768 where the reference says 4096, and `.recover` differs on the one line of nineteen that names the page size. Adopting the reference's value was measured, not assumed: 4096 puts the `schema` family at **0.94x**, under the contract's 1.00x floor. `PRAGMA locking_mode` was the third row and is no longer one: it reports `normal`, as SQLite does | **Yes - at a measured cost to the performance bars.** The pragma reports what the file is, which is its job |
| **1** | **the two pinned SQLite artifacts disagreeing with each other.** `.limit` reports `trigger_depth 1000`; the downloaded `sqlite3.exe` says 100 because it was built with `SQLITE_MAX_TRIGGER_DEPTH=100`, and the locally built oracle says 1000. Twelve of its thirteen lines agree | **No.** Whichever value is printed, one of the two references disagrees with it |
| **2** | **a compile option the pinned build does not have.** `DELETE` and `UPDATE` with `ORDER BY ... LIMIT` need `SQLITE_ENABLE_UPDATE_DELETE_LIMIT`. Apple's SQLite and many application builds set it; the pinned 3.53.4 build does not, so it answers `near "ORDER": syntax error`. inillucent runs the statement the way a build with the option does, because `DELETE ... LIMIT 1000` in a loop is how a large table is trimmed without one large transaction | **Yes, by refusing the clause again**, which is what this engine did until a consumer reported the refusal as a gap. It is kept on purpose |
| **3** | **numbers that describe SQLite's own C structures**: `EXPLAIN`'s bytecode program, `.vfslist`'s `szOsFile`, `.stats`' lookaside counters. Each prints the same report in the same shape over the facts *this* engine has | **No.** Printing SQLite's bytes would be a statement about a library that is not linked into this program - a fabrication, not compatibility |

So: **402 of 416 agree byte for byte (96.6%)**, **none are refused**, **6 answer differently
(1.4%)**, and **2 are accepted that the pinned build refuses**. Excluding the six vector cases, which
have no SQLite answer to compare against, and the two the pinned build refuses, 402 of the remaining
408 agree byte for byte - **98.5%**. **Four of the
six that differ cannot be closed by any value** - three because they describe SQLite's own internals,
and one because the two reference artifacts contradict each other. The other two could be closed at
a measured cost to the performance bars.

**A regression in the wording of two refusals was found and fixed at review 7, before the window
function count above existed.** The probe of that era came back **401**, not the review's usual 403:
`DELETE ... ORDER BY ... LIMIT` and the `UPDATE` form answered `Parse error near line 3: ORDER` where
the reference answers `Parse error near line 3: near "ORDER": syntax error`. **Both engines refused,
so nothing that checks only whether a statement fails could see it** - it took a byte comparison of
the message. The cause was one of review 6's own fixes: moving `bind::refused` from
`ParseErrorKind::Unexpected` to `Refused` was right for the forty-seven sentence-shaped refusals in
`directive.rs` and wrong for the one caller that passes a bare token, because
`near "ORDER": syntax error` is the shape that caller wants. It builds an `Unexpected` directly now,
the re-run reproduced **403 / 7 / 6 with no wording difference**, and both shapes are cases in
`crates/inillucent-compat/tests/semantics.rs` so they cannot drift back. That **403** was measured
through `inillucent-shell` while it still ran a retired engine; against the engine that ships it read
**391 / 12 / 7 / 6** until the window path was reconnected, then **403 / 0 / 7 / 6**, then
**404 / 0 / 6 / 6** on 2026-09-23. It reads **402 / 0 / 6 / 6 with 2 accepted** now, as above: the
two are the same two shapes this paragraph is about, which now run.

Detail for every one of the six that differ: [The six rows that are not the same](#the-six-rows-that-are-not-the-same).

**Separately - and this is the more useful number - the register audit found eight names absent**,
which is a different question from whether the 416 cases agree. Auditing against enumerations SQLite
produces itself rather than against a case list somebody wrote, what is missing is
**four functions** (`fts5(...)`, `fts5_locale()`, `fts5_get_locale()`, `fts5_insttoken()`),
**two modules** (`fts4aux`, `fts3tokenize`) and **two dot commands** (`.expert`, `.session`), plus
`fts3_tokenizer()` against the shell. Both modules and `fts3_tokenizer()` are absent from the pinned
SQLite **library** as well, so those three are a gap against the shell rather than against the thing
an application links. See
[Is the feature list itself complete?](#is-the-feature-list-itself-complete).

---

## At a glance

The whole comparison in one table. **Faster and less processor are wins; more memory is a loss** -
and this engine wins two of those three.

| | SQLite 3.53.4 | inillucent | the difference |
|---|---|---|---|
| **SQL features probed** | 416 | 416 | - |
| features that agree byte for byte, answers and error text alike | the reference | 402 | **96.6% of the surface** - and [here is exactly why it is not 100%](#why-it-is-not-100): of the other 14, 6 are vector features SQLite does not have, 6 answer differently, and 2 are accepted where the pinned build refuses. None of the 14 is silent |
| features SQLite answers and inillucent **refuses** | - | **0** | **none**. The last twelve were window functions, and they answer now |
| features inillucent accepts that SQLite rejects | - | **2** | `DELETE` and `UPDATE` with `ORDER BY ... LIMIT`, which the pinned build is compiled without |
| features both answer **differently** | - | 6 | **1.4%**, none of them silent |
| vector features with no SQLite equivalent | 0 | 6 | **6 extra** |
| **the surface audited against SQLite's own registers**, not against our case list | 218 functions, 67 pragmas, 19 modules, 5 collations, 65 dot commands | all called in both engines, and now **compared on every build** | **4 functions, 2 modules and 2 dot commands absent**, and the **silent difference is closed** - see [Is the feature list itself complete?](#is-the-feature-list-itself-complete) |
| **Elapsed time**, weighted over the contract's ten families | the reference | 4.97x the speed | **397% faster** |
| Elapsed time, the 95% lower bound the contract grades on | - | 4.62x | **362% faster** (bar: 200% faster) |
| **Processor time**, same plan, one child process each | 1,082 ms | 555 ms | **50% less CPU** (bar: 60%, missed on all four runs) |
| **Peak resident memory**, same plan, matched 128 MiB budget | 37.22 MiB | 40.76 MiB | **9.5% MORE memory** (was 102%, then 43%, then 14%; the bar asks for 5% *less*) |
| **The database on disk**, the same fixture imported | 16.05 MiB | 16.62 MiB | **1.036x** (was 1.41x) |
| The family that was **under the floor** | - | `transaction`, now **136% faster** (2.36x, lower bound 1.73x) | the release condition is that no required family is below the 1.00x floor, and three runs of the four met it; `schema` went under on the first, at a 0.68x lower bound against a 1.36x ratio, and reads 1.03x, 1.36x and 1.35x on the other three. `transaction` was 1.25x before a commit became one log append and one sync - see [Performance](performance.md#by-family) |
| Retrieval ranking, 17 graded comparisons against pgvector | the baseline | 15 better, 2 not worse | **none worse** |
| Retrieval latency, unfiltered, p50 | 2.315 ms | 0.8462 ms | **174% faster** |
| Retrieval latency, filtered to a minority source, p50 | 36.486 ms | 0.5820 ms | **6,169% faster** |

**Read the performance rows together.** inillucent finishes the same work in **a quarter of the
time** while spending **about a third of the processor**, so the speed is not bought by burning
cores - and it holds **14% more memory** to do it, where it held 102% more before review 6 and 43%
before review 7. The budget handed to the two engines is the same 128 MiB; what differs is how much
of it each chooses to use, and after review 7 the file itself is within **4%** of SQLite's.

**And the family that was under the floor is no longer near it.** `transaction` is **3.41x** with a
lower bound of 2.74x, against a floor asking for no family slower than SQLite that it was below on
all four runs. Two things got it there and only one of them is the engine: a statement stopped
rebuilding its own setup on every execution, and the workload that decided the family stopped
measuring an update that changes nothing. Both are in
[the performance page](performance.md#the-workload-that-was-measuring-nothing), with the number read
three ways so the engine's share and the measurement's share are separate.

That memory row is still the one thing on this page that is worse than SQLite, and it is now graded:
`compat/perf/contract.toml` carries a memory bar and a processor bar, and the gate fails on them.
[Where the memory goes](#where-the-memory-goes) has the per-workload attribution, the changes that
took 28.6 MiB off it, and what is left. Review 6's measurement said the remainder was the **file
format** rather than another buffer; review 7 acted on that - an integer mini-column is now as wide
as its own values - and the file went from 1.41x the `.db` to **1.036x**, taking 6.0 MiB of page
cache with it and making every read family *faster*. Of the 7.3 MiB still between this engine and its
bar, **4.1 MiB is what any Rust binary in this workspace costs before the engine exists** - a 110 KB
one measures the same, and `sqlite-bench` measures 4.2 - and the rest is one `CREATE INDEX`'s own
pages and arena.

---

## The headline

| | | at review 5 |
|---|---|---|
| **416 probed features** | **402 agree with SQLite byte for byte** - [why not 416](#why-it-is-not-100) | 403 |
| features SQLite answers and inillucent refuses | **0** - the last twelve were window functions, and they answer now | 0 |
| features both answer, **differently** | **6** - and none of them is silent | 7 |
| features inillucent accepts that SQLite rejects | **2** - `DELETE` and `UPDATE` with `ORDER BY ... LIMIT`, which the pinned build is compiled without | 0 |
| vector features with no SQLite equivalent | **6**, all working | 6 |
| | **416 of 416 answer** | 409 |

The middle column is the probe as it was re-run on 2026-09-24. A case where both engines
refuse counts as agreement only when the refusal is **the same text**. There is no separate column
for it because there is no case where the wording differs.

**Review 7 re-ran the whole probe on a fresh build over fresh databases, and it did not reproduce
review 5's numbers on the first pass - it came back 401.** Two refusals had drifted in wording; both
engines still refused, so nothing that checks only whether a statement fails could see it, and the
cause was one of review 6's own fixes applied one caller too widely. Corrected, review 7 reproduced
403 case for case, and both shapes are now cases in `semantics.rs`, so the next drift fails a build
instead of a document. **That 403 was measured through `inillucent-shell` while it still ran the
a retired engine.** Against the engine that ships the count read 391 agree, 12 refused and 7 differ
until the window path was reconnected, then 403 agree, 0 refused and 7 differ, then 404 agree,
0 refused and 6 differ on 2026-09-23, and it reads 402 agree, 0 refused, 6 differ and 2 accepted
now -
[why it is not 100%](#why-it-is-not-100) has the full breakdown.

**The five goals, measured:**

| goal | state |
|---|---|
| Same SQL as SQLite | **Yes.** Nothing the probe runs is refused, window functions included: `sql.select.window` and `functions.window` are both `pass` in `compat/sqlite-3.53.4.toml`. Everything agrees byte for byte, or answers with a measured, explained difference in `pragma`, `explain` and `shell`. See [SQL support](sql.md). |
| Same observable semantics | **Nothing SQLite answers is refused silently, and the one silent difference is closed.** `pragma_function_list` and `pragma_module_list` answered fewer rows than SQLite's while the functionality behind the difference worked, so a caller that introspected the register was told less than the truth with no error. It was found by [auditing the list against SQLite's own enumerations](#is-the-feature-list-itself-complete) rather than by the 416 cases, and review 6 closed it and turned the audit into `crates/inillucent-compat/tests/registers.rs`, which compares all four registers on every build. Every one of the seven rows that answers differently reports something a caller can read and act on: a page size and a locking mode this engine chose and can measure the cost of choosing otherwise, a build option the two pinned reference artifacts disagree about, or a number that describes SQLite's own C structures - a VDBE program, `sizeof(sqlite3_file)`, a lookaside allocator's counters - which no engine that is not SQLite can print. The twelve window function refusals are visible too: each returns exit code `3`, not `1`, so a caller can tell "not built" from "your SQL is wrong". |
| The PRAGMA surface an application uses | **59 of the 67 pragmas SQLite lists answer; the other 8 answer nothing in SQLite either.** None is silent here, and none is refused here. |
| Embedding search like pgvector | **The ranking is better and the SQL surface matches**, operator spellings included. Re-graded in full for this review. See [Vector search](#vector-search-against-postgresql--pgvector). |
| Faster than SQLite | **Yes on time and on processor, no on memory.** 397% faster at 50% less CPU over four consecutive runs, at **9.5% more** resident memory - down from 102% before review 6, 43% before review 7 and 14% before the index build stopped going through the page pool, and now graded by a bar in the contract rather than left ungraded. See [Performance](#performance). |

---

## Is the feature list itself complete?

**The 416 cases are a list somebody wrote, and a feature nobody wrote a case for reads on this page
as "no gap".** So review 5 audited the list against enumerations **SQLite produces itself** rather
than against `tools/feature-probe/cases.js`, and the audit found things the probe never asked about.

The method: take every name SQLite lists, call every one of them in both engines, and compare the
whole answer. Nothing here is sampled.

**Review 6 closed the silent one and turned the method into a checked-in test.**
`crates/inillucent-compat/tests/registers.rs` compares all four registers against the pinned
library on every run, as an exclusion list: a name that differs has to be *named there*, with why, or
the build fails. The counts below are re-measured after that work.

| enumeration | source | SQLite | inillucent, before | inillucent, now |
|---|---|---|---|---|
| SQL functions | `pragma_function_list`, then **each of the 218 names called in both shells** | 218 listed | 161 listed | **212 listed**, and against the pinned *library* there is now **no function SQLite answers that this engine does not** |
| PRAGMAs | `pragma_pragma_list`, then each asked of both | 67 | 67 through the shell, **62 through the driver front-end** | **67 through both** |
| virtual-table modules | `pragma_module_list`, then each queried | 19 | 14 through the shell, **67 through the driver front-end** | **20 through both** |
| collating sequences | `pragma_collation_list` | 5 | 5 - identical | **5 - identical** |
| shell dot commands | `.help` from each shell | 65 | 61 | **63** |

### What the audit found

**1. Six functions and two modules were genuinely absent. Two of the six now answer.**

| absent | what it is | now |
|---|---|---|
| `fts5_source_id()` | FTS5's build identifier | **answers** - `fts5:` and this engine's own stamp, which is the shape SQLite answers in. Claiming a particular SQLite build's hash would be a false statement about what is running |
| `optimize(t)` | FTS3/4's segment merge | **answers** `Index already optimal`, which is true here: this index keeps one doclist per term, so there is never a segment to merge |
| `fts5(...)` | FTS5's configuration and rank hook, called as a function inside a `MATCH` query | absent - it returns a pointer to the `fts5_api` structure, a C address for a caller that will call through it, and there is no C API here to point at |
| `fts5_locale()`, `fts5_get_locale()`, `fts5_insttoken()` | the locale and instance-token helpers | absent - FTS5's locale machinery is not implemented, and a stub that answered would be a wrong answer rather than a missing one |
| `fts3_tokenizer()` | FTS3/4's tokenizer registration function | absent, for the same reason as `fts5(...)`. The pinned *library* does not compile it either, so it is a gap against the shell only |
| **module `fts4aux`** | the vocabulary table over an FTS3/4 index. The FTS5 analogue, `fts5vocab`, **is** here | absent, and absent from the pinned library too |
| **module `fts3tokenize`** | the table-valued tokenizer | absent, and absent from the pinned library too |

**2. Four shell commands were absent**: `.expert`, `.load`, `.progress`, `.session`. **Two now
answer**: `.load` reports the reference's own words for a library it cannot open - which is what a
script written for the reference then does, rather than stopping at "unknown command" - and
`.progress` accepts and keeps the same state. `.expert` and `.session` are the two left, and the
shell section below says what each would mean here.

**3. The registers under-reported, and nothing said so. Closed by review 6.**
`pragma_function_list` answered **161 rows where SQLite answers 218**, and `pragma_module_list`
**14 where SQLite answers 19** - yet the functionality behind most of the difference was present and
byte-identical. `dbstat`, `sqlite_dbpage`, `sqlite_stmt`, `bytecode`, `tables_used`, `completion`,
`generate_series`, `matchinfo` and `offsets` all answered here exactly as they do there; they were
simply registered on first use, or answered by the engine rather than by a `Module`, and so were
missing from the list. **A caller that introspects the register to decide what it may use got a wrong
answer, with no error** - which was a silent difference, and the only one this project has found.

**Fifty-one names were present and unlisted**, and each was verified against the engine before it was
added: `current_date`, `current_time`, `current_timestamp`, `->`, `->>`, `if`, `match`, `regexp`,
`subtype`, `unknown`, `unistr`, `unistr_quote`, `sqlite_compileoption_get`,
`sqlite_compileoption_used`, `json_array_insert`, `jsonb_array_insert`, `load_extension`,
`sqlite_log`, `median`, `percentile`, `percentile_cont`, `percentile_disc`, `bm25`, `highlight`,
`snippet`, `matchinfo`, `offsets` and the rest. A name in the register that the binder refused would
be the same defect pointing the other way, so none was added on the strength of looking present.

**Four more things the check found that nobody had asked about**:

- **`->` and `->>` did not bind as function names.** The parser lowered the operators; the spellings
  were missing from `lookup_json`, so `"->"(a, b)` was refused where SQLite answers.
- **Seven arities disagreed.** `narg` is not simply -1 for a variadic function - SQLite encodes a
  minimum, so `coalesce` reads **-4** and `concat` **-3**. This engine said -1 for both, `iif` 3 for
  -4, `max`/`min` -1 for -3, and `rtreecheck` as two overloads for one. That is the column an
  application reads to decide whether a call will bind.
- **The driver front-end's pragma register was missing five names it answers**:
  `checkpoint_fullfsync`, `data_store_directory`, `default_cache_size`, `fullfsync`,
  `temp_store_directory`. One of them, `default_cache_size`, has a whole branch in the engine.
- **`pragma_module_list` answered 67 names through that front-end**, because it registers a
  `pragma_*` shim per pragma and listed all of them. SQLite creates its `pragma` module on first use,
  so its own register names only the one the query provoked. Filtered, and the two front-ends now
  agree with each other and with the reference.

**4. Twenty-three names reported the wrong *reason* out of context. Closed at review 6 for all
twenty-three; the eleven window functions regressed when the engine was replaced.** The eleven window functions
(`row_number`, `rank`, `dense_rank`, `lag`, `lead`, `first_value`, `last_value`, `nth_value`,
`ntile`, `percent_rank`, `cume_dist`) and the FTS5 auxiliary functions (`bm25`, `highlight`,
`snippet`, `matchinfo`, `offsets`, `optimize`, `match`) used to answer `no such function: X` where
SQLite answers `misuse of window function X()` or `unable to use function X in the requested
context`. At review 6, every one of them was present and byte-identical when called properly -
verified in this audit, window frames and `bm25`/`highlight`/`snippet` over a real FTS5 index
included. `SELECT row_number()` answers `misuse of window function row_number()` and
`SELECT bm25(1)` answers `unable to use function bm25 in the requested context`, both still correct
today - `registers.rs::a_name_out_of_context_reports_the_context_and_not_an_absence` compares all
eleven window names against the reference on every run. **Calling a window function properly is a
different matter now.** The engine `inillucent-shell` runs was replaced, and the new engine's
physical pass refuses every `OVER (...)` clause outright, so `SELECT row_number() OVER (ORDER BY a)`
- which review 6 confirmed answered correctly - is refused today. The FTS5 auxiliary functions are
unaffected. See [SQL support](sql.md).

**And the probe found a forty-eighth wording defect on its way past.** Its own scripts create a table
twice, and the second `CREATE TABLE t(a)` answered
`near "table t already exists": syntax error` where the reference answers `table t already exists`.
The cause was one helper: `bind::refused` built a computed refusal as
`ParseErrorKind::Unexpected`, whose whole rendering is `near "X": syntax error` - so every refusal
that has to *name* something took that shape, forty-seven of them in `directive.rs` alone. It builds
a `Refused` now, which is the variant whose purpose is a sentence said in the reference's words, and
the 183-case `semantics.rs` suite still agrees byte for byte.

**5. Forty-one of the "missing" names are not in SQLite at all.** `base64`, `base85`, `decimal*`,
`ieee754*`, `sha1*`, `sha3*`, `regexpi`, `zipfile`, `readfile`, `writefile`, `edit`, `lsmode`,
`realpath`, `usleep`, `stmtrand`, `strtod`, `dtostr` and the `shell_*` helpers are defined in
`shell.c` and **not in `sqlite3.c`** - checked by grepping the pinned amalgamation, both files. An
application that links `sqlite3.h` does not get them, so they are not a gap for a library
replacement. They *are* a gap for a shell replacement.

### What the audit confirms

- **129 of the 218 function names answered identically** when the audit was taken, error text
  included, line endings aside; and every one of the remaining 89 is accounted for above. The
  register itself agrees now: against the pinned library there is no function SQLite
  answers that this engine does not.
- **The PRAGMA surface is complete**: the same 67 names, the same 59 answering, the same 8 silent,
  nothing refused.
- **The collation surface is complete**: the same five.
- The modules that the register omits are nonetheless **byte-identical when used**.

This is where the next audit should start too: an enumeration the engine did not write is the only
kind that can find a feature nobody thought to look for. It no longer has to be started by hand -
`crates/inillucent-compat/tests/registers.rs` runs the comparison on every build, and its own doc
comment says why a suite that probes by calling cannot find this class of gap.

---

## How to read the tables

Each table is one feature per row, side by side. SQLite 3.53.4 is the reference, so its column says
what it does; inillucent's column says whether it does the same.

| symbol | meaning |
|---|---|
| **yes** | byte-for-byte the same answer, including the error message when both refuse |
| **differs** | both answer, and the answers are not the same. The row says what the difference measures |
| **extra** | inillucent has it and SQLite does not |

Six of the 416 rows say **differs**, and every one carries its reason in the row. **None says
refused.** They are collected in
[The six rows that are not the same](#the-six-rows-that-are-not-the-same).

---

## SQL statements and clauses

### SELECT - 19 of 19

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| SELECT with WHERE, ORDER BY, LIMIT | yes | **yes** |
| SELECT DISTINCT | yes | **yes** |
| GROUP BY with HAVING | yes | **yes** |
| GROUP BY expression | yes | **yes** |
| ORDER BY ordinal | yes | **yes** |
| ORDER BY NULLS FIRST / LAST | yes | **yes** |
| LIMIT with OFFSET, both forms | yes | **yes** |
| VALUES as a statement and in FROM | yes | **yes** |
| SELECT with no FROM | yes | **yes** |
| Qualified star and table alias | yes | **yes** |
| Column and expression aliases | yes | **yes** |
| Aggregate over empty set | yes | **yes** |
| GROUP BY with an ORDER BY on an aggregate | yes | **yes** |
| Bare column with an aggregate | yes | **yes** |
| DISTINCT over several columns | yes | **yes** |
| DISTINCT with an ORDER BY on a column not selected | yes | **yes** |
| ORDER BY a window function | yes | **yes** |
| HAVING referring to a select alias | yes | **yes** |
| count(DISTINCT) with two arguments | yes | **yes** |

### Joins - 15 of 15

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| INNER JOIN with ON | yes | **yes** |
| LEFT OUTER JOIN | yes | **yes** |
| RIGHT OUTER JOIN | yes | **yes** |
| FULL OUTER JOIN | yes | **yes** |
| CROSS JOIN | yes | **yes** |
| NATURAL JOIN | yes | **yes** |
| JOIN ... USING | yes | **yes** |
| Self join | yes | **yes** |
| Four table join | yes | **yes** |
| LEFT JOIN with a WHERE on the right table | yes | **yes** |
| Comma join with a WHERE | yes | **yes** |
| LEFT JOIN on a subquery | yes | **yes** |
| LEFT JOIN ... USING | yes | **yes** |
| NATURAL LEFT JOIN | yes | **yes** |
| USING with three tables | yes | **yes** |

A `USING` or `NATURAL` join *coalesces* the named column, and the right-hand copy is suppressed from
`*` and from an unqualified reference while a qualified `b.k` still reaches it - NULL in a
`LEFT JOIN`, where the coalesced column carries the value from the other side.

### Compound selects, subqueries and CTEs - 24 of 24

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| UNION | yes | **yes** |
| UNION ALL | yes | **yes** |
| EXCEPT | yes | **yes** |
| INTERSECT | yes | **yes** |
| Compound with LIMIT | yes | **yes** |
| Three-way compound | yes | **yes** |
| Scalar subquery | yes | **yes** |
| IN with a subquery | yes | **yes** |
| NOT IN with NULLs | yes | **yes** |
| EXISTS and NOT EXISTS | yes | **yes** |
| Correlated scalar subquery | yes | **yes** |
| Derived table in FROM | yes | **yes** |
| Row value comparison | yes | **yes** |
| Row value IN | yes | **yes** |
| Row value with a subquery | yes | **yes** |
| Subquery in SELECT list with a correlated LIMIT | yes | **yes** |
| WITH, one term | yes | **yes** |
| WITH, several terms | yes | **yes** |
| WITH RECURSIVE | yes | **yes** |
| Recursive tree walk | yes | **yes** |
| CTE column list | yes | **yes** |
| MATERIALIZED and NOT MATERIALIZED | yes | **yes** |
| WITH on INSERT | yes | **yes** |
| WITH on UPDATE and DELETE | yes | **yes** |

### Window functions - 11 of 11

This section said "0 of 11, not implemented" between the engine replacement and the review that found the window functions were never missing. That was wrong, and the
way it was wrong is worth recording. The evaluator - `run_windowed` in `inillucent-exec`, about a
thousand lines - answered every form the whole time. The refusal came from one layer above it:
`compiled::try_compile` bailed out on `plan.compounds` and had no matching check for
`plan.select.windows`, so a windowed statement reached `build_upper`, which refused it, and
`run_cached_query` propagated that refusal instead of falling back to the fresh path the way a
compound does. Every entry point an application uses goes through that cached path, so the working
evaluator was reachable only from `run_with`, which nothing but a test calls.

Each row below is graded against SQLite 3.53.4 by `windows_match_the_oracle` in
`crates/inillucent-compat/tests/advanced_sql.rs`: forty-one statements, compared row for row in
order with storage classes included. Tracked as `sql.select.window` and `functions.window` in
`compat/sqlite-3.53.4.toml`, both `status = "pass"`.

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| row_number, rank, dense_rank | yes | **yes** |
| ntile, cume_dist, percent_rank | yes | **yes** |
| lag and lead | yes | **yes** |
| first_value, last_value, nth_value | yes | **yes** |
| PARTITION BY | yes | **yes** |
| ROWS frame | yes | **yes** |
| RANGE frame | yes | **yes** |
| GROUPS frame | yes | **yes** |
| EXCLUDE clauses | yes | **yes** |
| Aggregate with FILTER over a window | yes | **yes** |
| Named WINDOW clause reused | yes | **yes** |

### INSERT, UPDATE, DELETE - 24 of 24

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| INSERT VALUES, multi-row | yes | **yes** |
| INSERT ... SELECT | yes | **yes** |
| INSERT DEFAULT VALUES | yes | **yes** |
| INSERT OR IGNORE | yes | **yes** |
| INSERT OR REPLACE | yes | **yes** |
| INSERT OR ROLLBACK inside a transaction | yes | **yes** |
| INSERT OR FAIL | yes | **yes** |
| INSERT OR ABORT | yes | **yes** |
| REPLACE INTO | yes | **yes** |
| UPDATE with a WHERE | yes | **yes** |
| UPDATE ... FROM | yes | **yes** |
| UPDATE OR IGNORE onto a unique key | yes | **yes** |
| UPDATE OR REPLACE onto a unique key | yes | **yes** |
| DELETE with a WHERE | yes | **yes** |
| DELETE all rows | yes | **yes** |
| DELETE ... ORDER BY ... LIMIT | yes | **yes** |
| UPDATE ... ORDER BY ... LIMIT | yes | **yes** |
| RETURNING on INSERT | yes | **yes** |
| RETURNING on UPDATE and DELETE | yes | **yes** |
| RETURNING with an expression | yes | **yes** |
| INSERT into a WITHOUT ROWID table | yes | **yes** |
| Upsert on a WITHOUT ROWID table | yes | **yes** |
| A correlated UPDATE subquery | yes | **yes** |
| RETURNING beside a trigger | yes | **yes** |

### UPSERT - 7 of 7

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| ON CONFLICT DO NOTHING | yes | **yes** |
| ON CONFLICT DO UPDATE with excluded | yes | **yes** |
| ON CONFLICT DO UPDATE with a WHERE | yes | **yes** |
| ON CONFLICT on a secondary unique index | yes | **yes** |
| Upsert without a conflict target | yes | **yes** |
| Two ON CONFLICT clauses | yes | **yes** |
| Upsert with RETURNING | yes | **yes** |

**Two `ON CONFLICT` clauses on one statement** is the row that moved here. A conflict now carries the
columns of the constraint that reported it, so an arm's target can be matched against it: the write
walks the clauses in order and takes the first whose target names the constraint that fired, which is
SQLite's own rule. The last clause may omit its target and is then the catch-all.

---

## Schema

### CREATE TABLE - 18 of 18

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| CREATE TABLE with typed columns | yes | **yes** |
| CREATE TABLE IF NOT EXISTS | yes | **yes** |
| CREATE TABLE ... AS SELECT | yes | **yes** |
| WITHOUT ROWID | yes | **yes** |
| STRICT | yes | **yes** |
| STRICT with ANY | yes | **yes** |
| Generated column, VIRTUAL | yes | **yes** |
| Generated column, STORED | yes | **yes** |
| DEFAULT expressions | yes | **yes** |
| Quoted and reserved-word identifiers | yes | **yes** |
| Typeless columns | yes | **yes** |
| DROP TABLE and IF EXISTS | yes | **yes** |
| Table-level PRIMARY KEY over two columns | yes | **yes** |
| DEFAULT CURRENT_TIMESTAMP and friends | yes | **yes** |
| CHECK containing a subquery | yes | **yes** |
| INTEGER PRIMARY KEY DESC is not a rowid alias | yes | **yes** |
| A rowid reference in a WITHOUT ROWID table | yes | **yes** |
| A WITHOUT ROWID table with no primary key | yes | **yes** |

### CREATE INDEX - 13 of 13

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| CREATE INDEX | yes | **yes** |
| CREATE UNIQUE INDEX | yes | **yes** |
| Descending index | yes | **yes** |
| Partial index | yes | **yes** |
| Index on an expression | yes | **yes** |
| Index on a WITHOUT ROWID table | yes | **yes** |
| Index with COLLATE | yes | **yes** |
| Composite index | yes | **yes** |
| DROP INDEX | yes | **yes** |
| REINDEX | yes | **yes** |
| NOT INDEXED | yes | **yes** |
| INDEXED BY | yes | **yes** |
| ANALYZE writes sqlite_stat1 | yes | **yes** |

### Views and triggers - 15 of 15

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| CREATE VIEW | yes | **yes** |
| CREATE VIEW with a column list | yes | **yes** |
| DROP VIEW | yes | **yes** |
| Writing through an INSTEAD OF trigger | yes | **yes** |
| A view over a join | yes | **yes** |
| AFTER INSERT trigger | yes | **yes** |
| BEFORE UPDATE trigger with OLD and NEW | yes | **yes** |
| AFTER DELETE trigger | yes | **yes** |
| Trigger WHEN clause | yes | **yes** |
| UPDATE OF column trigger | yes | **yes** |
| RAISE(ABORT) in a trigger | yes | **yes** |
| RAISE(IGNORE) in a trigger | yes | **yes** |
| Recursive triggers | yes | **yes** |
| DROP TRIGGER | yes | **yes** |
| Trigger firing an UPDATE on another table | yes | **yes** |

### ALTER TABLE - 8 of 8

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| ALTER TABLE RENAME TO | yes | **yes** |
| ALTER TABLE RENAME COLUMN | yes | **yes** |
| ALTER TABLE ADD COLUMN | yes | **yes** |
| ALTER TABLE DROP COLUMN | yes | **yes** |
| Rename propagates into a view and a trigger | yes | **yes** |
| ADD COLUMN NOT NULL DEFAULT on a populated table | yes | **yes** |
| ADD COLUMN NOT NULL with no default | yes | **yes** |
| ADD COLUMN UNIQUE | yes | **yes** |

### Constraints - 16 of 16

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| NOT NULL | yes | **yes** |
| UNIQUE | yes | **yes** |
| CHECK on INSERT | yes | **yes** |
| CHECK on UPDATE | yes | **yes** |
| Table-level CHECK over two columns | yes | **yes** |
| PRIMARY KEY AUTOINCREMENT | yes | **yes** |
| A constraint carrying its own ON CONFLICT | yes | **yes** |
| NOT NULL ON CONFLICT REPLACE with a DEFAULT | yes | **yes** |
| Foreign key, immediate | yes | **yes** |
| Foreign key ON DELETE CASCADE | yes | **yes** |
| Foreign key ON DELETE SET NULL and SET DEFAULT | yes | **yes** |
| Foreign key ON UPDATE CASCADE | yes | **yes** |
| Deferred foreign key | yes | **yes** |
| PRAGMA foreign_key_check | yes | **yes** |
| PRAGMA foreign_key_list | yes | **yes** |
| A row colliding on two unique indexes | yes | **yes** |

---

## Values, expressions and functions

### Type affinity and storage classes - 19 of 19

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| Affinity: text into INTEGER | yes | **yes** |
| Affinity: number into TEXT | yes | **yes** |
| Affinity: integer into REAL | yes | **yes** |
| Affinity: BLOB column keeps the class | yes | **yes** |
| Affinity: NUMERIC | yes | **yes** |
| Affinity through an INTEGER PRIMARY KEY | yes | **yes** |
| CAST between every class | yes | **yes** |
| Comparison across storage classes | yes | **yes** |
| Integer overflow becomes real | yes | **yes** |
| Real formatting | yes | **yes** |
| Integer division and modulo | yes | **yes** |
| Hex integer literals and blob literals | yes | **yes** |
| TRUE, FALSE and NULL keywords | yes | **yes** |
| Unicode text round trip | yes | **yes** |
| NULL ordering and arithmetic | yes | **yes** |
| A value wider than a page | yes | **yes** |
| A blob wider than a page | yes | **yes** |
| IEEE special values | yes | **yes** |
| A very large IN list | yes | **yes** |

### Operators - 12 of 12

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| Concatenation and arithmetic | yes | **yes** |
| Bitwise operators | yes | **yes** |
| IS, IS NOT, IS DISTINCT FROM | yes | **yes** |
| BETWEEN and NOT BETWEEN | yes | **yes** |
| IN with a list | yes | **yes** |
| LIKE with and without ESCAPE | yes | **yes** |
| GLOB | yes | **yes** |
| REGEXP without a registered function | yes | **yes** |
| CASE, both forms | yes | **yes** |
| JSON -> and ->> | yes | **yes** |
| Operator precedence | yes | **yes** |
| String comparison and BINARY collation | yes | **yes** |

### Collations - 5 of 5

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| BINARY, NOCASE and RTRIM | yes | **yes** |
| COLLATE in a column definition | yes | **yes** |
| COLLATE in ORDER BY | yes | **yes** |
| COLLATE in ORDER BY, GROUP BY and DISTINCT over a BINARY index | yes | **yes** |
| A unique index under NOCASE | yes | **yes** |
| PRAGMA collation_list | yes | **yes** |
| NOCASE over text holding a NUL | stops at the NUL | **stops at the NUL** |

**The NUL row.** SQLite's `NOCASE` is `sqlite3StrNICmp`, a C string walk whose
loop condition includes `*a != 0`. At a NUL in the left operand it stops and
compares the two bytes at that position. When both are NULs that is a tie, the
bytes after it are never read, and the comparison falls through to the byte
lengths. So `x'0061'` sorts before `x'000079'`, and `x'0061'` equals
`x'0062'`. Until the NOCASE fix this engine read every byte and put `x'000079'`
first. The difference is reachable only through `CAST(x'..' AS TEXT)` or a
bound parameter holding a NUL, because no SQL string literal can carry one.

An index on `s COLLATE NOCASE` still answers an `ORDER BY` by walking, because
SQLite's rule has a byte form whose natural order is the rule's order: the
folded bytes before the first NUL, the NUL, then the value's whole length as
eight big endian bytes. That is `nocase_key_bytes` in
`crates/inillucent-value/src/collation.rs`, and its unit test checks it against
a transcription of `sqlite3StrNICmp` over every pair of 781 short strings.
`inillucent-compat::ordering::nocase_stops_at_an_embedded_nul_as_sqlite_does`
checks the scan, the index walk, a range, `GROUP BY` and `DISTINCT` against
the oracle, and a seek with a bound key against the rows SQLite's rule selects.

**An index on `COLLATE NOCASE` built before the NOCASE fix that holds text with a
NUL is stored in the old order.** Measured on a file written by 0.1.2: the
current `PRAGMA integrity_check` reports `row 2 does not sort after the row
before it`, a seek on that index returned a row it should not have, and
`REINDEX` on the index fixed both. An index whose text holds no NUL is
unaffected, because for that text the key bytes did not change.

**The BINARY index row.** A scan with no `WHERE` reads the narrowest tree that
covers it, which can be an index on `s` under `BINARY`, so the rows arrive in
byte order. Until the NOCASE fix, `ORDER BY s COLLATE NOCASE` then skipped its sort,
`GROUP BY s COLLATE NOCASE` grouped by adjacency and `SELECT DISTINCT s COLLATE
NOCASE` removed only adjacent duplicates, because the check that decides
whether the walk already provides an order compared the columns and not the
collation. With the rows `b A a B c` the three answered `A B a b c`, five
groups and five rows, where SQLite answers `A a B b c`, three and three.
`inillucent-compat::ordering::a_binary_walk_does_not_answer_a_nocase_order`
checks all three against the oracle.

`PRAGMA collation_list` reports five: `decimal`, `BINARY`, `NOCASE`, `RTRIM` and `uint`. `decimal`
compares two numeric strings by value rather than by bytes, and `uint` compares a string of digits by
magnitude; both are the reference CLI's bundled extensions, and both are implemented here so the list
and the ordering agree.

### Scalar functions - 19 of 19

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| abs, sign, round, max, min | yes | **yes** |
| length, substr, instr, replace | yes | **yes** |
| upper, lower, trim, ltrim, rtrim | yes | **yes** |
| printf and format | yes | **yes** |
| quote, hex, unhex, char, unicode | yes | **yes** |
| coalesce, ifnull, nullif, iif | yes | **yes** |
| typeof, likelihood, likely, unlikely | yes | **yes** |
| zeroblob, randomblob length, octet_length | yes | **yes** |
| changes, total_changes and last_insert_rowid | yes | **yes** |
| concat and concat_ws | yes | **yes** |
| glob and like as functions | yes | **yes** |
| sqlite_version and sqlite_source_id exist | yes | **yes** |
| load_extension | yes | **yes** |
| printf %q, %Q and %w | yes | **yes** |
| substr with negative and omitted lengths | yes | **yes** |
| abs of the smallest integer | yes | **yes** |
| round to negative and large digits | yes | **yes** |
| char with zero and out-of-range code points | yes | **yes** |
| instr and length on blobs | yes | **yes** |

### Aggregates - 8 of 8

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| count, sum, total, avg | yes | **yes** |
| max, min, group_concat, string_agg | yes | **yes** |
| DISTINCT inside an aggregate | yes | **yes** |
| FILTER on an aggregate | yes | **yes** |
| group_concat with an ORDER BY argument | yes | **yes** |
| Aggregates over NULLs | yes | **yes** |
| sum of text and of a mixed column | yes | **yes** |
| Integer sum overflowing | yes | **yes** |

### Date and time - 10 of 10

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| date, time, datetime on a fixed instant | yes | **yes** |
| julianday and unixepoch | yes | **yes** |
| strftime, the whole specifier table | yes | **yes** |
| strftime fractional seconds | yes | **yes** |
| Modifiers: days, months, years | yes | **yes** |
| Modifiers: start of, weekday | yes | **yes** |
| Modifiers: ceiling, floor, subsec, auto | yes | **yes** |
| timediff | yes | **yes** |
| Julian day round trip | yes | **yes** |
| Modifiers: localtime, utc | yes | **yes** |

The two time zone modifiers were the one deliberate difference in this table
until later fixes implemented them. `datetime(x, 'localtime')` answered NULL and
`datetime(x, 'utc')` returned its argument unchanged; both now convert between
the machine's zone and UTC, as SQLite's do.

Both answers depend on the operating system's time zone database and on the zone
the process is running in, so the same query answers differently on two machines
and differently again after a daylight saving change. That is true of SQLite too,
and it is why `date_and_time_functions_match_the_oracle` compares the two engines
on one machine rather than asserting a value. If a query has to give the same
answer on two machines, store the offset with the value and convert it in the
application.

### Maths - 4 of 4

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| Trigonometric functions | yes | **yes** |
| Hyperbolic functions | yes | **yes** |
| Logs, powers and roots | yes | **yes** |
| ceil, floor, trunc, mod, pi, degrees, radians | yes | **yes** |

### JSON - 11 of 11

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| json and json_valid | yes | **yes** |
| json_array, json_object, json_quote | yes | **yes** |
| json_extract and json_type | yes | **yes** |
| json_insert, json_replace, json_set, json_remove | yes | **yes** |
| json_patch, json_array_length, json_pretty | yes | **yes** |
| json_group_array and json_group_object | yes | **yes** |
| json_each | yes | **yes** |
| json_tree | yes | **yes** |
| jsonb round trip | yes | **yes** |
| json_error_position | yes | **yes** |
| JSON stored in a column and queried | yes | **yes** |

### Table-valued functions - 5 of 5

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| generate_series | yes | **yes** |
| generate_series with LIMIT and no stop | yes | **yes** |
| pragma_table_info as a table | yes | **yes** |
| pragma_index_list and pragma_index_info | yes | **yes** |
| json_each joined against a table | yes | **yes** |

`json_each(t.d)` joined against a table is a **lateral** join: the argument reads a column of the
outer row, so the module has to be re-driven once per outer row. The physical pass routes a virtual
scan whose constraints read a column into a lateral operator that does exactly that, and the same
path serves `generate_series(1, t.a)` and any other table-valued function given a column.

### The function register

`compat/api/builtins.toml` lists the function names this engine registers, and
`inillucent_sql::function::every_function` is what `pragma_function_list` reports, beside whatever
the connection has registered through `inillucent_ext`'s registry - an application's own functions,
and `embed(TEXT)` in a build carrying the `embed` feature. The registry half arrived when `embed()`
was added to the register, which found the same under-report one more time in the one place the
built-in list could not reach.
Against the pinned **library** - the amalgamation, not the shell - there is no function SQLite
answers that this engine does not. `current_date`, `current_time` and `current_timestamp` used to be the exception: they
worked as keywords but were not named in the register, which is the kind of under-reporting that
review 6 went after. They are named now. `load_extension` *is* registered and refuses in the platform's own
words, because this build has no dynamic loader and a function that quietly answered NULL would be a
function an application believed had worked.

`crates/inillucent-compat/tests/registers.rs` compares the register - names **and** arities - against
the reference on every build, so a function added without being listed, or listed without being
answerable, fails rather than waiting for an audit.

It adds its own, in three groups: the vector measures (`vector_distance_cos`, `vector_distance_l2`,
`vector_dot`, and pgvector's `l2_distance`, `cosine_distance`, `inner_product`, `l1_distance`,
`hamming_distance`, `jaccard_distance`, `vector_dims`, `vector_norm`, `l2_normalize`,
`binary_quantize`, `subvector`, `vector_add`, `vector_sub`, `vector_mul`, `vector_concat`), the
geometry (`geopoly_area`, `geopoly_bbox`, `geopoly_blob`, `geopoly_ccw`, `geopoly_contains_point`,
`geopoly_debug`, `geopoly_group_bbox`, `geopoly_json`, `geopoly_overlap`, `geopoly_regular`,
`geopoly_svg`, `geopoly_within`, `geopoly_xform`), and the R-Tree diagnostics (`rtreecheck`,
`rtreedepth`, `rtreenode`).

---

### PRAGMA - 33 of 35

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| PRAGMA table_info | yes | **yes** |
| PRAGMA table_xinfo with a generated column | yes | **yes** |
| PRAGMA table_list | yes | **yes** |
| PRAGMA index_list, index_info, index_xinfo | yes | **yes** |
| PRAGMA database_list | yes | **yes** |
| PRAGMA integrity_check and quick_check | yes | **yes** |
| PRAGMA user_version and application_id | yes | **yes** |
| PRAGMA page_size, page_count, freelist_count | 4096 and 2 | **differs** - 32768 and 2. The page size is this engine's, and it is a measured choice rather than a spelling: a 32 KiB page is what the performance gate is set at, and moving to 4096 to make this row agree cost the weighted headline 3.83x down to 2.60x when it was measured. `PRAGMA page_size` reports what the file actually is, which is the whole job of the pragma |
| PRAGMA cache_size and synchronous | yes | **yes** |
| PRAGMA journal_mode | six modes, `delete` first | **yes** - all six switch, and `delete` is the default here as it is there. Review 5 measured what that costs: the medium gate reads 3.78x weighted with `wal` as the default and 3.70x with `delete`, lower bounds 3.45x and 3.44x over 30 paired rounds. It costs nothing, so the reference's default is the default |
| PRAGMA locking_mode and temp_store | `normal` | **agrees** - `normal`. Both modes work. `normal` is the default because `exclusive` never releases the file between statements, so a second process either waits out the whole life of the first or reads state from before it - which is how two writer processes lost 43% of their acknowledged commits. `exclusive` is still a real switch for a program that never opens a second connection: releasing the file between statements means re-reading the meta record and the log's tail before each one |
| PRAGMA encoding | yes | **yes** |
| PRAGMA auto_vacuum and incremental_vacuum | yes | **yes** |
| PRAGMA secure_delete and cell_size_check | yes | **yes** |
| PRAGMA foreign_keys, defer_foreign_keys, ignore_check_constraints | yes | **yes** |
| PRAGMA recursive_triggers and legacy_alter_table | yes | **yes** |
| PRAGMA case_sensitive_like and reverse_unordered_selects | yes | **yes** |
| PRAGMA schema_version and data_version | yes | **yes** |
| PRAGMA optimize, shrink_memory, wal_checkpoint | `optimize`, `shrink_memory`, `wal_checkpoint` | **yes** - all three, including `wal_checkpoint`'s `0|-1|-1` over a database that is not in WAL |
| PRAGMA busy_timeout, threads, query_only | yes | **yes** |
| PRAGMA mmap_size, soft_heap_limit, hard_heap_limit | yes | **yes** |
| PRAGMA max_page_count | yes | **yes** |
| PRAGMA trusted_schema and writable_schema | yes | **yes** |
| PRAGMA analysis_limit and automatic_index | yes | **yes** |
| PRAGMA module_list, function_list, pragma_list exist | yes | **yes** |
| PRAGMA compile_options exists | yes | **yes** |
| PRAGMA schema.table_info qualified by database | yes | **yes** |
| PRAGMA user_version round trip | yes | **yes** |
| PRAGMA application_id round trip | yes | **yes** |
| PRAGMA table_info on a view | yes | **yes** |
| PRAGMA index_info on an implicit primary-key index | yes | **yes** |
| PRAGMA journal_mode reported by default | `delete` | **yes**. The write-ahead log is still here and `PRAGMA journal_mode = wal` still selects it - and a database left in WAL **reopens in WAL**, because the meta record now carries the flag the way SQLite's header carries its read/write version |
| PRAGMA wal_checkpoint(TRUNCATE) | `wal_checkpoint(TRUNCATE)` over a rollback journal | **yes** |
| PRAGMA count_changes and other deprecated ones | yes | **yes** |
| PRAGMA collation_list after a CREATE | yes | **yes** |

`tools/feature-probe/pragmas.js` asks every pragma the reference lists, of both engines:

```
67 pragmas SQLite lists
{ answers: 59, silent: 8 }

answers in SQLite, silent here:      (none)
answers in SQLite, refused here:     (none)
```

The eight that answer nothing do so in **both**: `case_sensitive_like`, `data_store_directory`,
`foreign_key_check`, `foreign_key_list`, `incremental_vacuum`, `optimize`, `shrink_memory` and
`temp_store_directory`. A pragma that answers nothing is one whose whole effect is what it does, and
each of those does it.

One of the eight is on Windows only. SQLite compiles `data_store_directory` under `SQLITE_OS_WIN`,
so a Linux build's `pragma_list` is one name shorter than the list above, and so is this engine's since the round two code review -
`registers.rs::the_pragma_register_agrees_exactly` compares the two on both platforms
now, which it could not do until the Linux runs had an oracle to compare against.

---

## EXPLAIN, transactions and multi-file work

### EXPLAIN - 4 of 5

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| EXPLAIN QUERY PLAN, full scan | yes | **yes** |
| EXPLAIN QUERY PLAN, index search | yes | **yes** |
| EXPLAIN QUERY PLAN, join | yes | **yes** |
| EXPLAIN QUERY PLAN, sort | yes | **yes** |
| EXPLAIN, the bytecode form | the VDBE program, one row per opcode | **differs** - the same eight columns under the same widths and the same header rule, holding this engine's operator chain. The *layout* matches now: review 5 gave the shell the reference's `MODE_Explain`, so a listing prints as a table rather than as `0|Init|0|1|0||0|Start at 1`. What the rows hold is what the statement actually runs, framed by the `Init` and `Halt` that begin and end an execution here as they do there; SQLite lists the opcodes of a bytecode program and this engine compiles none |

### Transactions - 11 of 11

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| BEGIN, COMMIT | yes | **yes** |
| BEGIN, ROLLBACK | yes | **yes** |
| DEFERRED, IMMEDIATE and EXCLUSIVE | yes | **yes** |
| SAVEPOINT, RELEASE, ROLLBACK TO | yes | **yes** |
| Nested savepoints | yes | **yes** |
| DDL rolled back | yes | **yes** |
| DROP TABLE rolled back | yes | **yes** |
| A statement failing part way leaves nothing behind | yes | **yes** |
| COMMIT with no transaction | yes | **yes** |
| Nested BEGIN | yes | **yes** |
| END as a synonym for COMMIT | yes | **yes** |

### ATTACH and temporary objects - 11 of 11

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| ATTACH a second file and query across it | yes | **yes** |
| Join across two databases | yes | **yes** |
| A transaction spanning two databases | yes | **yes** |
| A rollback spanning two databases | yes | **yes** |
| ATTACH an in-memory database | yes | **yes** |
| PRAGMA database_list after ATTACH | yes | **yes** |
| CREATE TEMP TABLE | yes | **yes** |
| CREATE TEMP VIEW and TEMP TRIGGER | yes | **yes** |
| Temporary table is not in the main schema | yes | **yes** |
| CREATE TEMP TABLE ... AS SELECT | yes | **yes** |
| A temp table shadowing a main table | yes | **yes** |

---

## Extensions

### FTS5, FTS4 and FTS3 - 14 of 14

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| CREATE VIRTUAL TABLE ... fts5 and MATCH | yes | **yes** |
| FTS5 phrase and NEAR queries | yes | **yes** |
| FTS5 boolean operators and prefix | yes | **yes** |
| FTS5 bm25 ranking | yes | **yes** |
| FTS5 highlight and snippet | yes | **yes** |
| FTS5 column filter and multiple columns | yes | **yes** |
| FTS5 delete and update | yes | **yes** |
| FTS5 rank and the rowid | yes | **yes** |
| FTS5 external content table | yes | **yes** |
| FTS5 contentless table | yes | **yes** |
| FTS5 'optimize' and 'rebuild' commands | yes | **yes** |
| FTS5 tokenizer options | yes | **partly** - `unicode61` with `remove_diacritics`, `tokenchars` and `separators`, `ascii`, and `porter` over either. `trigram` is not implemented, and a name this build has not got is now refused by name rather than read as `unicode61`: the tokenizer decides what `MATCH` means, so substituting one turned `tokenize='trigram'`'s substring search into a whole-word search with no error anywhere |
| fts5vocab | yes | **yes** |
| FTS3/FTS4 | yes | **yes** |

Three of these moved in this run. **`highlight()` and `snippet()`** mark per *phrase instance*:
`MATCH 'quick brown'` marks two ranges and `MATCH '"quick brown"'` marks one, because the first is
two phrases of one term and the second is one phrase of two - and the reference draws exactly that
distinction. **An external content table** (`content='c'`) reaches the owner's rows through one
explicit grant: a shadow table with no suffix *is* the named table, looked up rather than created,
which is the same mechanism `fts5vocab` uses to read another index. **FTS3 and FTS4** are a second
front on the same index: `docid`, `snippet(t, start, end, ellipsis, column, tokens)`, `offsets(t)`
and `matchinfo(t, format)`, over the tokenizer, dictionary and doclists FTS5 already had.

### R-Tree and geopoly - 2 of 3

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| CREATE VIRTUAL TABLE ... rtree and a window query | yes | **yes** |
| rtree_i32 | yes | **yes** |
| An R-Tree with an auxiliary column | yes | **yes** |

`geopoly` is the R-Tree with a different front: two dimensions, real coordinates, and a `_shape`
column whose bounding box is computed rather than written. SQLite implements it the same way, in the
same file, for the same reason - a second copy of the node splitting is a second place for it to be
wrong. The thirteen functions and the one aggregate are ported from `ext/rtree/geopoly.c`, including
two things that are exact rather than equivalent: the sweep's tie-breaking, and the fifth-order sine
approximation that makes `geopoly_regular(0,0,10,4)` `10.0007` wide in both engines rather than
exactly `10` in one of them.

### The other modules - 6 of 6

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| dbstat | yes | **yes** |
| sqlite_dbpage | yes | **yes** |
| geopoly | yes | **yes** |
| The CSV module | yes | **yes** |
| sqlite_offset | yes | **yes** |
| The session extension (changeset) | yes | **yes** |

`dbstat` and `sqlite_dbpage` describe the file's pages, and are the engine's rather than a module's
because a module reaches its own shadow tables and a pager is not one of them. `bytecode`,
`tables_used`, `sqlite_stmt` and `completion` are the engine's for the same reason: each is a
question about the *connection*. `fsdir` is the **shell's**, which is where the reference puts it too
- a library that read the file system on behalf of any statement would be a library an untrusted
query could read a password file through, and `Database::register_module` is how a program that wants
one says so.

### VACUUM

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| `VACUUM` | rebuilds the file, reclaiming free space | **yes, and it is a rebuild.** The schema is captured, replayed into a fresh file beside the original, every row copied through it, and the result renamed over the source. Measured on a database of 246 pages and 8.06 MB: 127 pages and 4.16 MB afterwards, `PRAGMA integrity_check` ok |
| `VACUUM INTO 'copy.db'` | writes a compacted copy | **yes** - the same rebuild, written to the named file. It opens the copy and checks every tree before returning, so a file it produced is a file something has read, and it refuses an existing output file in SQLite's own words |
| `VACUUM` inside a transaction | `cannot VACUUM from within a transaction` | **yes**, in the same words |
| `PRAGMA auto_vacuum`, `incremental_vacuum` | settable before the first table, a no-op afterwards | **yes** - `auto_vacuum` is accepted on an empty database and ignored on one that has tables, which is SQLite's own rule, and `incremental_vacuum` moves free pages off the end of the file when the mode is `incremental` |

---

## Syntax, limits and the shell

### Syntax - 9 of 9

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| Comments, both forms | yes | **yes** |
| Keyword case insensitivity | yes | **yes** |
| Identifier quoting, all four forms | yes | **yes** |
| Reserved words as column names | yes | **yes** |
| String literals with embedded quotes | yes | **yes** |
| Bare double-quoted string falling back to a literal | yes | **yes** |
| Deeply nested expression | yes | **yes** |
| Statements without a trailing semicolon | yes | **yes** |
| A very long identifier and a very long string | yes | **yes** |

### Limits - 7 of 7

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| A table with 1000 columns | yes | **yes** |
| A 100 term compound select | yes | **yes** |
| A 100 deep nested expression | yes | **yes** |
| A 40 term join | yes | **yes** |
| Recursive CTE bounded by a LIMIT | yes | **yes** |
| Thirty attached databases | yes | **yes** |
| A 2 MB text value | yes | **yes** |

`.limit` reports the register itself, thirteen lines of it, and twelve of the thirteen agree. The
thirteenth is `trigger_depth`, and it is the two pinned reference artifacts disagreeing with each
other rather than a difference in this engine - see
[The six rows](#the-six-rows-that-are-not-the-same).

### The shell - 37 of 39 probed, and 63 of the reference's 65 commands

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| .tables | yes | **yes** |
| .schema and .schema TABLE | yes | **yes** |
| .fullschema | yes | **yes** |
| .indexes | yes | **yes** |
| .databases | yes | **yes** |
| .headers on | yes | **yes** |
| .mode csv | yes | **yes** |
| .mode json | yes | **yes** |
| .mode line | yes | **yes** |
| .mode column | yes | **yes** |
| .mode insert | yes | **yes** |
| .mode quote, markdown, box, table, html | yes | **yes** |
| .separator and .nullvalue | yes | **yes** |
| .dump | yes | **yes** |
| .import a CSV file | yes | **yes** |
| .output to a file and back | yes | **yes** |
| .once | yes | **yes** |
| .read a script file | yes | **yes** |
| .backup and .restore | yes | **yes** |
| .save | yes | **yes** |
| .clone | yes | **yes** |
| .changes on | yes | **yes** |
| .echo on | yes | **yes** |
| .bail on | yes | **yes** |
| .eqp on | yes | **yes** |
| .width | yes | **yes** |
| .parameter set and a named parameter | yes | **yes** |
| .sha3sum | yes | **yes** |
| .lint fkey-indexes | yes | **yes** |
| .limit | `trigger_depth 100` | **differs on one line** - `trigger_depth 1000`. The two pinned reference artifacts disagree with each other: the downloaded `sqlite3.exe` was built with `SQLITE_MAX_TRIGGER_DEPTH=100`, which its own `PRAGMA compile_options` says, and the locally built `sqlite-oracle.exe` reports the amalgamation's default of 1000. `compat/limits.toml` names the oracle as authoritative and `differential::every_limit_matches_sqlites_default` checks against it. Every other line of the thirteen agrees |
| .vfsinfo / .vfslist | six VFSes: `win32`, `apndvfs`, `memdb` and three long-path variants | **differs** - the same four lines per entry, over the two file systems this build has. `szOsFile` is the size of a C struct in a library that is not linked here, and `apndvfs` is a shim over a format this engine does not write |
| .stats on | twenty-four lines of allocator and pager counters | **differs** - the same two-column shape, over the counters this engine keeps. Lookaside slots, pcache overflow bytes and the size of a prepared statement are facts about SQLite's allocator; what a caller reads `.stats` for is what a statement cost, and the cost here is the page cache |
| .timeout | yes | **yes** |
| .recover | `PRAGMA page_size = '4096'` | **differs on one line of nineteen** - `PRAGMA page_size = '32768'`, which is the page size row above. The recovered SQL is otherwise identical, statement for statement |
| .selftest | yes | **yes** |
| .log | yes | **yes** |
| .open a second file | yes | **yes** |
| .help exists | yes | **yes** |
| An unknown dot command | yes | **yes** |
| .dbconfig, and the 22 flags it lists | yes | **yes** - byte for byte, listing and setting. `defensive` is on by default here as it is there, which is what makes `PRAGMA journal_mode = OFF` refuse in both |
| .cd, .shell, .system, .excel, .www | yes | **yes** |
| .crlf, .prompt, .explain, .nonce | yes | **yes** |
| .testcase and .check | yes | **yes** - including the tally line, the `Got:` that shows a trailing newline, and the two-line complaint about a `.check` with no `.testcase` |
| .scanstats, .trace | yes | **yes** |
| .auth ON\|OFF | yes | **yes** - `sqlite3_set_authorizer` is a real connection setting here now, and an installed authorizer takes the connection off the plan cache so its callback runs every time |
| .dbinfo, .dbtotxt, .intck, .filectrl | yes | **the same reports over this file's own numbers.** `.dbinfo`'s twenty-two lines are the reference's names in the reference's column; five of them describe SQLite's header fields, which this format does not have, and read as the values a database using none of them has |
| .connection [close] [#] | yes | **yes** - five slots, `ACTIVE` on the one statements run on, an in-memory database opened in a slot that was closed |
| .imposter INDEX TABLE | yes | **yes.** An index's entries are the indexed columns followed by the row's identity, which is a `WITHOUT ROWID` table - so the declaration reads the index's own b-tree, and `.imposter off` takes it away again |
| .expert | `EXPERIMENTAL. Suggest indexes for queries` | **not here.** Its answer is whichever candidate index its *cost model* prefers - `WHERE a>1 ORDER BY b` recommends `(b)` and `WHERE a=1 ORDER BY b` recommends `(a, b)` - so an implementation over this planner would recommend this planner's answer, which is a different tool wearing the same name. SQLite marks it experimental for the same reason |
| .load FILE ?ENTRY? | `Error: The specified module could not be found.` for a library it cannot open | **yes, and it answers exactly that** - the command exists, `.help` lists it, and a script written for the reference runs to the same message rather than stopping at "unknown command". What it cannot do is *find* a library: a SQLite extension is a shared object against `sqlite3_api_routines`, and no crate in this workspace presents that C ABI - `inillucent-driver-capi` is a bespoke API of its own rather than a `sqlite3_api_routines` workalike. The extension surface this engine has is Rust-native, through `inillucent_ext::registry` |
| .progress N | `Invoke progress handler after every N opcodes`; prints nothing unless `--limit` is given | **yes, and it keeps the same state** - the words, `--once`, `--quiet`, `--limit` and `--reset`, reported by `.show`. What it does not do is interrupt a statement part-way: the unit is a VDBE opcode and this engine compiles none, so there is no per-opcode callback to hang one on. The reference's visible effect on an ordinary script is none either, which is why keeping the state is the whole of the parity worth having |
| .session ?NAME? CMD ... | yes | **not here.** The session extension is changesets, patchsets, conflict resolution and a rebaser - a subsystem beside the engine rather than a command, and the largest single thing on SQLite's surface this engine has not got |

`.help TOPIC` is `showHelp`'s own search: a prefix that matches one command prints its long-form
usage, several print one line each, and a pattern that matches no command is looked for *inside* the
help text - which is what makes `.help wal` find the commands that mention it. The entries are the
reference's own words, and only for the commands this shell has: help that described a command that
is not here would be worse than no help.

---

## Schema introspection and integrity

### sqlite_schema, table_list and the integrity checks - 9 of 9

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| sqlite_schema and sqlite_master | yes | **yes** |
| The schema of an index and a trigger | yes | **yes** |
| VACUUM | yes | **yes** |
| VACUUM INTO | yes | **yes** |
| Rowid, oid and _rowid_ aliases | yes | **yes** |
| sqlite_sequence after AUTOINCREMENT | yes | **yes** |
| integrity_check over an index and a WITHOUT ROWID table | yes | **yes** |
| integrity_check with a row limit argument | yes | **yes** |
| Parameters through .parameter set, all spellings | yes | **yes** |
---

## The six rows that are not the same

There were seven until `PRAGMA locking_mode` stopped differing; it is kept below as item 3 because
the decision behind it was measured. Every one, with what it measures. Nothing here is refused and nothing here is silent: each answers,
and each reports something a caller can read.

**Two are one decision, measured.** Both of the experiments in this section were run when the
weighted headline stood at 3.83x rather than today's 4.97x, and neither has been re-run since. What
each measures is the *difference* between two settings, which is why it is still quoted.

1. **`PRAGMA page_size` is 32768** where the reference is 4096. Measured both ways on the medium
   gate: 32768 gave 3.83x weighted with the `schema` family at 1.15x; 4096 with a cache-matched
   pool gave 3.44x with `schema` at **0.94x**, under the 1.00x floor the contract requires. The
   pragma reports what the file is, which is its job.
2. **`.recover`** differs on exactly that line and no other: nineteen statements, one of which names
   the page size.

**One is a second decision, measured the same way.**

3. **`PRAGMA locking_mode` is `normal`**, which is what SQLite reports too, so this row is no longer
   a difference. `exclusive` works and is a real switch. What grades multi-process access is
   `crates/inillucent-compat/tests/process_concurrency.rs`: two real writer processes, rows present
   equal to commits acknowledged, under both modes and through `ATTACH`. Five defects were found and
   fixed on the way there - an upgrade
   deadlock, a PENDING lock read as a failure, a lock released while the pool was still dirty, a
   schema cache outliving the pages it described, and a file opened between its creation and its
   first meta record. The default used to be `exclusive`, because review 5 ran the gate with `normal`
   as the default and it read **3.03x with a lower bound of 2.95x - under the contract's 3.00x bar**,
   taking `write` from 1.94x to 1.19x, `transaction` from 0.89x to 0.37x and `schema` from 1.34x to
   0.66x. Releasing the file also folded the log into it then. A commit is one append to the log and
   one sync now, and the fold is deferred, so `normal` is the default and the headline is measured
   under it.

   **The journal mode used to be four more rows and is now none of them.** `PRAGMA journal_mode`
   reported `wal` by default, and three other rows asked the same question in other words. The same
   measurement was made - 3.78x weighted with `wal` as the default, 3.70x with `delete`, lower
   bounds 3.45x and 3.44x - and it came back the other way: the reference's default costs nothing,
   so it is the default here too. The write-ahead log is still there, `PRAGMA journal_mode = wal`
   still selects it, and a database left in WAL reopens in WAL.

**One is the two pinned reference artifacts disagreeing with each other.**

4. **`.limit` reports `trigger_depth 1000`** and the downloaded `sqlite3.exe` reports 100. That
   shell was built with `SQLITE_MAX_TRIGGER_DEPTH=100`, which its own `PRAGMA compile_options` says;
   the pinned amalgamation's default is 1000, and the locally built `sqlite-oracle.exe` - which
   `compat/limits.toml` names as authoritative and which
   `differential::every_limit_matches_sqlites_default` checks against - reports 1000. Twelve of the
   thirteen lines agree. No value closes this row: whichever of the two artifacts is agreed with,
   the other one disagrees.

**Three print numbers that describe SQLite's own C structures.**

These three are the ones no engine that is not SQLite can match. What the reference prints in them is the size of a C struct, the contents of a bytecode
program, and the counters of a particular memory allocator. Reproducing the *bytes* would mean
printing numbers about a library that is not linked into this program - which is not a measurement,
it is a fabrication. Each of the three prints the same report over the facts this engine has.

5. **`EXPLAIN`** answers with SQLite's eight columns, under SQLite's own column widths and header -
   Review 5 closed the layout, so a listing here lines up under the same `addr opcode p1 p2 p3 p4
   p5 comment` rule as the reference's. What the rows hold is this engine's operator chain: SQLite
   lists the opcodes of a bytecode program and this engine compiles none, so the listing is what the
   statement actually runs, and `bytecode('...')` reads the same rows.
6. **`.vfslist`** prints four lines per file system, in the reference's format, over the two this
   build has rather than the six SQLite's registry holds. `szOsFile` is the size of a C struct in a
   library that is not linked here, and `apndvfs` is a shim over a format this engine does not
   write.
7. **`.stats`** prints the same two-column shape over the counters this engine keeps - page cache
   fetches, hits, misses and rewarms, frames cooled and evicted, pages read and written. Lookaside
   slots, pcache overflow bytes and the size of a prepared statement are facts about SQLite's
   allocator, and its numbers are its own memory use rather than anything about the query.

`sqlite_offset(X)` used to be a twelfth row, refused. It answers now, and the answer is the offset of
the **page** the row is read from rather than of the record: a row here is not a record, because a
PAX leaf stores each column as its own run of bytes and one row therefore occupies several places on
its page. The reference's value is opaque to a caller either way - SQLite's own documentation says it
may name the table or an index depending on the plan - and what a caller can test is that a column of
a real table has an offset and a literal does not. Both engines agree on that, row for row.

---

## Architecture and operations

These cannot be probed with SQL. They are read from the tree and from the design documents.

| | SQLite 3.53.4 | inillucent |
|---|---|---|
| file format | the SQLite format, readable by every tool | its own `.rdb` plus `RDBWAL01` log segments. **A SQLite file cannot be opened** - `database disk image is malformed` - it is imported |
| import from SQLite | - | `inillucent-migrate --sqlite-file src.db dest.rdb`: copy, then verify by count and digest, then publish by rename. Tables, views, triggers, FTS5 indexes and `sqlite_sequence` all carried |
| export to SQLite | - | `.dump` produces SQL a `sqlite3` can replay |
| processes per file | many, byte-range locks | **many**, over the same SHARED / RESERVED / PENDING / EXCLUSIVE protocol, under `PRAGMA locking_mode = NORMAL`, which is the default. One writer holds the file at a time and a second is refused with `busy` after `PRAGMA busy_timeout` |
| writers | one at a time; readers block in rollback mode, not in WAL | **one at a time; a reader waits for a writer too**, and is refused with `busy` after `PRAGMA busy_timeout`. There is no shared-memory log index for a reader to find a snapshot through; `docs/roadmap.md` has the protocol that would add one |
| threading | single-thread, multi-thread and serialised modes | **single-threaded** |
| journal modes | `DELETE`, `TRUNCATE`, `PERSIST`, `MEMORY`, `WAL`, `OFF` | **all six**, and `delete` by default, as SQLite is - review 5 measured the alternative and the reference's own default cost nothing. They mean something different here: an application's `ROLLBACK` is undone from the log under every one of them, including `OFF`, so what the journal mode selects is how a **checkpoint** is protected rather than how a transaction is. `MEMORY` and `OFF` are therefore the same choice here, where SQLite distinguishes them |
| durability | rollback journal or WAL, `synchronous` OFF/NORMAL/FULL | **both**: a redo WAL with group commit, crc32c per record, fuzzy checkpoints that retire segments and ARIES-style recovery; and a rollback journal that takes a page's pre-image before its new image is written. `synchronous` OFF/NORMAL/FULL |
| isolation | serialisable, one writer | serialisable, one writer: the connection holds the file for its transaction, so a second process sees the state before it or after it and never inside it. `inillucent-txn` implements snapshot isolation with a version log and garbage collection, and the shipped engine does not use it for this |
| page size | 512-65536, 4096 default | 8-64 KiB, **32 KiB default** |
| default page cache | `PRAGMA cache_size` **-2000**, so 2 MiB | `PRAGMA cache_size` **-131072**, so **128 MiB - 64x SQLite's**. It is a real switch here and setting it to SQLite's default takes a scanning shell from 23.1 MiB resident to 9.3; see [Where the memory goes](#where-the-memory-goes) |
| C API | `sqlite3.h`, ~290 functions | **`inillucent_driver.h`, 53 symbols**, per-symbol stability in `drivers/abi.toml`, plus a capability table a caller can ask before composing a statement |
| language bindings | dozens, everywhere | **Python**, in the standard library only, as the reference binding |
| backup API | `sqlite3_backup_*` | `inillucent_backup_to` in the driver, `.backup` in the shell |
| serialize / deserialize, incremental blob I/O, authorizer, update / commit / rollback / preupdate hooks, progress handler, tracing, `unlock_notify`, snapshots, custom VFS | yes | **not on the new engine** |
| encryption at rest | SEE, a commercial add-on | none. `ATTACH ... KEY` refuses by name rather than parsing the key and ignoring it, which is what a build without an encryption extension does |
| user-defined functions and collations | yes | **yes**, scalar and aggregate, through the driver |
| virtual-table modules a program registers | `sqlite3_create_module` | **`Database::register_module`**, which is how the shell adds `fsdir` |
| assurance | TH3, `testfixture`, ~600 tests per line of code | **the same four red binaries as `bd9fedf`**, all pre-existing and accounted for; a differential oracle against the pinned build; SQLLogicTest; a `BTreeMap` model reference; a fault-injecting VFS; 8 fuzz targets; all 29 crates deny `unwrap`/`panic`/indexing and 21 of 29 forbid `unsafe` |

### The migration path

The supported route from an existing SQLite application is `inillucent-migrate --sqlite-file`, and it
works for tables, `WITHOUT ROWID` tables, generated columns, partial indexes, foreign keys, views,
triggers, FTS5 indexes and `sqlite_sequence`.

Each table is verified three ways before the database is published: its row count, its column list,
and a digest over every column the source file stores. A `VIRTUAL` generated column is stored by
neither engine - both compute it on read - so the digest leaves it out and the column list is what
says it crossed. `crates/inillucent-compat/tests/migrate_realistic.rs` compares those columns value
for value against the pinned SQLite build, and checks that the migrated table recomputes them after
an `UPDATE` rather than holding the value they had on the day of the migration.

The one refusal is an FTS5 index declared with `content=` pointing somewhere else. It keeps no copy
of the text, so there is nothing in the file to rebuild the index from, and the tool says so by name
rather than publishing a database whose search returns nothing.

---

## Vector search, against PostgreSQL + pgvector

**Re-graded 2026-09-20, in full.** The whole suite was re-run at commit cd53317 - the index rebuilt
from the cache, 2,713 queries embedded with `nomic-embed-text-v1.5` in process on the GPU, and every
scenario measured against both pgvector configurations. The card at the repository root is this run's
own output file, so the two agree row for row.

**The ranking figures reproduce.** Every ranking row below is within a thousandth of what the same
suite measured at commit 0c14f71d three weeks earlier, and eight of them are identical to four
decimal places. The 2026-09-19 run at commit e2a81e1 agrees with this one on the same terms: its
verdicts are the same 15 better, 1 equivalent, 1 inconclusive, 0 worse, and where the two differ it is
in the latency rows, which are wall clock on a live PostgreSQL and move by tenths of a millisecond
between sittings on both engines' arms. A set of figures published from a 2026-09-08 run did not reproduce here, and that run
embedded 1,109 queries where this one and the 0c14f71d one embed 2,713; a family measured on fewer
queries is a different measurement, which is the likeliest reason its margins sat where they did.
Its own output file did not survive, so that cannot be checked, and nothing published now depends on
it.

**17 primary comparisons: 15 better, 1 equivalent, 1 inconclusive, 0 worse. Correctness gates: all
pass.** Both engines read byte-identical vectors and are handed the same embedded query, so the model
cancels out and a difference measures indexing and ranking. Each comparison is decided by a 95%
paired bootstrap interval and a paired randomisation test against a threshold declared before the
run.

### Ranking quality

Against the **better of the two pgvector configurations**, never the misconfigured one. Higher is
better on every row except abstention, where the metric is how often an engine confidently answers a
question the corpus cannot answer - there, lower is better.

| family | measurement | inillucent | best pgvector | the difference |
|---|---|---|---|---|
| Lexical | rare identifiers, MRR | **0.5455** | 0.1358 | **302% higher** |
| Multi-source | evidence in two sources, evidence recall@10 | **0.6237** | 0.1923 | **224% higher** |
| Filtered | `source = jira`, recall@10 in filter | **1.000** | 0.3280 | **205% higher** |
| Filtered | `source = github`, recall@10 in filter | **1.000** | 0.3320 | **201% higher** |
| Passage | one transposed character, graded nDCG@10 | **0.6794** | 0.3969 | **71% higher** |
| Filtered | `source = slack`, recall@10 in filter | **1.000** | 0.6120 | **63% higher** |
| Passage | three keywords, graded nDCG@10 | **0.6266** | 0.4651 | **35% higher** |
| Lexical | natural language headings, MRR | **0.7222** | 0.5896 | **22% higher** |
| Hybrid | document identity, nDCG@10 | **0.9756** | 0.8148 | **20% higher** |
| Hybrid | natural language headings, nDCG@10 | **0.7478** | 0.6271 | **19% higher** |
| Passage | passage evidence, graded nDCG@10 | **0.7045** | 0.6027 | **17% higher** |
| Filtered | `source = miro`, recall@10 in filter | **1.000** | 0.8840 | **13% higher** |
| Filtered | `source = confluence`, recall@10 in filter | 0.9960 | 0.9760 | 2% - **inconclusive** |
| Filtered | `source = figma`, recall@10 in filter | 1.000 | 1.000 | **equivalent, at the ceiling** |
| Abstention | questions with no answer, confident answer rate | **0.0050** | 1.000 | **99.5% fewer confident wrong answers** |

**The correctness gate is the row that matters most and it is not a percentage.** inillucent returned
every row its predicate admits, on every source. pgvector did not: at the extension's defaults it
returned fewer than the 50 rows the predicate admits on **25 of 25** queries for every one of the six
sources, and even correctly configured it fell short on github (12 of 25), jira (9 of 25) and miro
(1 of 25). An engine that returns fewer rows than the filter allows is not a faster engine.

### Latency

Median over the same queries, measured inside the calling process. Both pgvector columns are given
because they are two different bargains: the defaults are quick and return incomplete results, and
the configured one returns the rows and pays for them.

| query | inillucent | pgvector, configured | vs configured | pgvector, defaults | vs defaults |
|---|---|---|---|---|---|
| no predicate, p50 | **0.8462 ms** | 2.315 ms | **174% faster** | 1.492 ms | **76% faster** |
| no predicate, p95 | **1.389 ms** | 3.359 ms | **142% faster** | 2.046 ms | **47% faster** |
| `source = slack`, p50 | **0.5820 ms** | 36.486 ms | **6,169% faster** | 1.114 ms | **91% faster** |
| `source = slack`, p95 | **0.8172 ms** | 89.097 ms | **10,803% faster** | 2.015 ms | **147% faster** |

The filtered row stands for the whole comparison: pgvector's cost of *being correct under a
filter* is to repeat the scan, and it is two orders of magnitude. inillucent's probe widens itself
instead - ask the graph for *k*, run the residual predicate, and if fewer than *k* survive ask for
four times as many - which needs no setting, and is why the filtered query here takes **less** time
than the unfiltered one rather than sixty times more.

**inillucent pays no network cost because it is a library and pgvector pays a loopback round trip.**
That is a real difference in the deployed system rather than a measurement artefact, and it is not a
difference in index quality; it is named here so the percentages are read for what they are.

### Memory and footprint

| | inillucent | PostgreSQL + pgvector | the difference |
|---|---|---|---|
| index on disk, 185,078 chunks | 952 MB, int8 quantised | 800 MB - 722 MB HNSW plus 78 MB GIN | **19% more on disk** |
| the whole store the queries run against | the 952 MB index | a 1,750 MB database | **46% less on disk** |
| resident set of the serving process | **1,216 MiB**, one process, opening the saved index in **0.8 s** | a PostgreSQL server; `shared_buffers` alone is configured at 10,240 MiB on this machine | see the note |
| processes to run | **none** - it is a library in the caller | PostgreSQL, plus an embedding server | **2 fewer** |

**Why the resident-set row is a note rather than a percentage.** PostgreSQL's memory is not one
number that can be put beside a single process's: it is a shared-memory segment charged to every
backend that touches it, spread over 35 processes on this box, two instances of which are running.

The retrieval index's resident set is the one cost on this side of the project that nothing has tried
to reduce; it is item 22 of [What is still missing](#what-is-still-missing).

### The SQL surface

| pgvector | inillucent |
|---|---|
| operators `<->` `<#>` `<=>` `<+>` `<~>` `<%>` | **all six.** Lexed as three-byte operators before the two-byte forms - or `v <=> q` would lex as `v <= (> q)` - and bound at PostgreSQL's own slot for a user-defined operator: tighter than a comparison and looser than `+`, so `WHERE v <=> q < 0.5` and `ORDER BY v <=> q` parse the way anybody writing them means. `<#>` answers the *negative* inner product, as it does in pgvector, so that smaller is always better |
| `l2_distance`, `inner_product`, `cosine_distance`, `l1_distance`, `hamming_distance`, `jaccard_distance` | **all six**, under pgvector's names and under this engine's own three |
| `vector_dims`, `vector_norm`, `l2_normalize`, `binary_quantize`, `subvector` | **all five.** `binary_quantize` writes one bit per component, most significant bit first within each byte, which is how pgvector's `bit` type is laid out and therefore what `hamming_distance` counts over |
| `avg(vector)`, `sum(vector)` | **both**, component by component. The binder chooses the vector fold where the argument's declared type says the column is one, which is where the type is known; at run time a blob is just a blob |
| vector arithmetic `+`, `-`, `*` and concatenation | **the operators and the functions**, element-wise, plus `vector_concat`. The operator form is chosen from the *declared type* - the same thing PostgreSQL uses when it overloads `+` for its own `vector` - so `v + v` over a `VECTOR(n)` column is a vector and `x'00' + x'00'` is still SQLite's integer `0`. A number on one side scales: `v * 2` is pgvector's scaling and so is `vector_mul(v, 2)` |
| `vector`, `halfvec`, `bit`, `sparsevec` types | **`VECTOR(N)`.** `HALFVEC(4)`, `BIT(8)` and `SPARSEVEC(4)` are accepted as declared type names; the storage behind all of them is the one 32-bit float vector, so what the three narrower spellings buy a caller today is that a schema written for pgvector is a schema this parses |
| HNSW and IVFFlat index types | **both.** `CREATE INDEX ... USING inillucent_hnsw (v)` is the graph the retrieval engine builds; `CREATE INDEX ... USING ivfflat (v) WITH (lists = 20, probes = 3)` is k-means centroids and an inverted list per centroid, written as its own module because an inverted file needs no lexical half. Probing every list is **exhaustive and exact** - a graded test asserts it returns the exhaustive plan's ten rows, row for row - and three lists of twenty over four hundred vectors returned the same ten |
| `WITH (m = ..., ef_construction = ...)`, `SET hnsw.ef_search` | **`WITH ( ... )` takes them all**: `m`, `ef_construction`, `ef_search`, `metric`, `threads` and `compact`, checked against the structure that reads them - a name it has not got is refused rather than ignored, and so is `WITH` on an index that is not `USING` a module. `ef_search` is set on the index rather than on the session, so there is no equivalent of `SET hnsw.ef_search`. The search also widens itself, which is the part that needs no knob: see below |
| an ordering on any distance planned onto the index | **cosine and L2**, named by `WITH (metric = 'cosine' \| 'l2')` on the index and defaulting to cosine. The index is probed only when the `ORDER BY` function matches the metric it was built under; a mismatch plans as a scan and a temp b-tree, which is what pgvector's operator classes express - an `hnsw (v vector_l2_ops)` index answers an L2 ordering and a `vector_cosine_ops` one does not. `vector_dot` has no index of its own and always plans as a scan |
| a mismatched dimension raises | **raises** - `different vector dimensions 4 and 3`, and a non-vector argument raises `vector_distance_cos: argument 2 is not a vector`. A NULL argument is still NULL, which is what every other scalar function answers |
| **filtered search** (`WHERE ... ORDER BY v <=> ? LIMIT k`), with `hnsw.iterative_scan` to keep recall | **yes**, and it needs no knob. See below |
| embedding generation | pgvector has none. inillucent has **`embed(TEXT)`** - `nomic-embed-text-v1.5` through ONNX Runtime, in the database process, answering the 3,072 bytes of a 768-component vector ready to store in a `VECTOR(768)` column. It is behind `--features embed`, off by default, for the reason the retrieval engine's own `onnx` feature is off: a SQL engine that linked a native machine-learning runtime whether or not anybody asked would cost the binary and the load time to every caller who supplies their own vectors, and most do |
| ACID, replication, backups, many writers, many processes | PostgreSQL's. Here: one writer, and many processes under `locking_mode = NORMAL` |

### The filtered vector search

**The probe widens itself until it has *k* survivors or the graph is exhausted.** It is the same idea
as pgvector's `hnsw.iterative_scan`, with the loop inside the engine rather than behind a setting:
ask the graph for *k*, run the query's own residual predicates over what comes back, and if fewer
than *k* rows survive, ask for four times as many and try again.

**Measured, at both scales, over the whole grid** - filters keeping 100%, 50%, 5% and 1% of the rows,
at `LIMIT` 1, 10 and 100, the indexed plan's rows compared against the exhaustive plan's row for row:

| corpus | comparisons | recall | rows |
|---|---|---|---|
| 400 rows, 16 dimensions | 12 of 12 | **1.000** | identical to the exhaustive plan |
| 20,000 rows, 16 dimensions | 12 of 12 | **1.000** | identical to the exhaustive plan |

Three of the cases are checked in as tests in `crates/inillucent-compat/tests/vector.rs`.

---

## Performance

**Four consecutive runs, one machine, one fixture.** `inillucent-fullgate`, medium scale
(100,000 rows), 30 paired rounds each, every workload's answer digested and compared with SQLite's
before a timing counts - **34 of 34 workloads agreed in every round of every one of the four runs**.
Both arms are given **the same memory budget** - a 4,096-frame pool of 32 KiB pages here,
`PRAGMA cache_size = -131072` for SQLite, 128 MiB each - and both run under `synchronous = FULL`.

**Measured 2026-09-23 on `main` at `6f84ce6` with `7f93661` applied, in a quiet window,
with both arms pinned to the performance cores.** The run files are kept alongside a summary, and
every figure in this section is read out of them by `summarise.mjs` rather than typed.

**The pinning is new and it is part of the result.** This machine has 8 performance cores and 16
efficiency cores. Unpinned on 2026-09-23, Windows ran the gate process, which is this engine's arm,
on the efficiency cores and the SQLite child on the performance cores, and the medium headline read
**4.40x**. Pinned so that both arms run on the performance cores it reads **4.97x**, and pinned so
that both run on the efficiency cores it reads 5.72x. [Performance](performance.md) has the per
workload evidence and the mask. The published figure is the one where both engines ran on the same
hardware. The gates now pin themselves and print the mask they ran on.

`compat/perf/contract.toml` carries a `[memory]` bar of 0.95x and a `[cpu]` bar of 0.40x, both ratios
of SQLite's on the same plan and the same budget, and both **written before the work that had to meet
them**. Until they existed the contract held ten elapsed-time families and nothing else, so an engine
that doubled its resident set would still have passed the gate outright - which is exactly what had
happened.

**A note on how these medians are taken.** Four runs have no single middle value, and every figure on
this page is the average of the two middle runs. A summary script used in an earlier round took the
*upper* of the two instead, which reported that round's headline as 3.82x where its median was 3.79x.
The difference is small and it went in the flattering direction, which is why the rule is written
down rather than assumed.

**Where the numbers have been.** The engine has been measured in full six times, and the headline
has moved every time; nothing here is a figure carried forward from a previous document.

| | review 7 | before the durability fix | after the setup fix | v0.1.3 | `main`, 2026-09-20 | **`main`, 2026-09-23** |
|---|---|---|---|---|---|---|
| weighted headline | 3.79x | 4.26x | 4.30x | 4.39x | 4.53x | **4.97x** |
| 95% lower bound (bound 3.00x) | 3.65x | 4.13x | 4.06x | 4.09x | 4.21x | **4.62x** |
| `transaction` (floor 1.00x) | 0.84x - `UNDER THE FLOOR` | 3.41x | 2.52x | 2.50x | 2.36x | **2.37x** |
| `write` (bar 1.50x) | 1.71x | 1.92x | 2.08x | 2.17x | 2.12x | **3.04x** |
| `extension` (bar 1.50x) | - | 1.30x | 1.52x | 1.58x | 1.57x | **1.73x** |
| `read.analytical` (bar 5.00x) | - | - | 6.67x | 5.36x | 10.48x | **10.71x** |
| `schema` (bar 3.00x) | - | - | - | 1.36x | 1.37x | **1.31x** |
| processor time | 445 ms | 422 ms | 390 ms | 391 ms | 461 ms | **555 ms** |
| peak resident set | 42.61 MiB | 42.61 | 42.40 | 42.45 | 40.76 | **40.76 MiB** |
| the imported `.rdb` | 17,432,576 B | 17,432,576 B | 17,432,576 B | 17,432,576 B | 17,432,576 B | **17,432,576 B** - 1.036x the `.db` |

The column from the measurement taken after the log resume fix is dropped to make room; it measured 3.83x weighted, a 3.60x bound,
`transaction` 0.90x and `write` 1.60x. Review 6 measured 3.81x weighted, a 3.55x bound, `transaction`
1.14x, `write` 1.93x, 406 ms of processor and 53.28 MiB resident. Review 5 measured only the resident
set, at 75.25 MiB.

**The columns before 2026-09-23 were not pinned**, and nothing recorded which cores their two arms ran
on. The 2026-09-20 run read `scan.aggregate` at 1.70 to 1.73 ms against SQLite's 92.5 to 93.8, which
is the 2026-09-23 performance core figure on both arms, so it is compared here as a run where both
landed on the performance cores. Every ratio in this table is
also against a denominator that moves with the box: the same pinned SQLite binary on the same fixture
has read **2.16x faster on a quiet box than on one that has just run the test suite**, which may be
the same effect as the core placement.

**What moved from 2026-09-20 to 2026-09-23 is mostly `write`**, 2.12x to 3.04x. A later change sized a
leaf's delta area by the page's free space and spliced a compaction's new rows into the page's
existing column widths, and `write.insert.batch` went from about 0.60x to 1.47x. **Processor time went
the other way**, 461 ms to 555, and the reason is four `read.correlated` workloads the plan gained in
between: this engine spends 182 ms of each round on them and SQLite under half a millisecond. They
are outside the ten weighted families, so they do not touch the elapsed time headline, but the
processor figure is one round of the whole plan.

The two rows that moved most before the durability fix are the two that had been failing. `transaction` was
**under the contract's floor on all four runs** and reached 3.41x; `write` missed its bar and cleared
it. What that took is in
[the performance page](performance.md#the-workload-that-was-measuring-nothing), and one part of it
was a defect in the workload rather than in the engine.

**`transaction` moved backwards once, from 3.41x to 2.52x, and it was the price of a durability
fix.** That fix found that the rollback journal never synced its page images before the pages they
protect were overwritten, so a crash during a checkpoint could destroy a database the power loss
itself had left whole. [Closed items](closed-items.md#eight-items-closed-together) has that fix and three
more that came out of the same thread.

**How to read every percentage below.** A workload that takes 1 second where SQLite takes 4 is
written as **300% faster**, and its ratio is 4.00x. A workload that takes 4 seconds where SQLite
takes 1 is **300% slower**, ratio 0.25x. The two directions are never mixed, and a ratio below 1.00
is never left as the only statement of a loss. Memory and processor time are written as *less* and
*more*, where *more* means this engine costs more than the reference.

### The three numbers

| | SQLite 3.53.4 | inillucent | the difference | the contract |
|---|---|---|---|---|
| **elapsed time**, weighted geometric mean over the ten families | the reference | 4.97x the speed | **397% faster** | - |
| **elapsed time**, the 95% lower bound the contract grades on | - | 4.62x | **362% faster** | bound 3.00x - **MET on all four runs** |
| **processor time**, one round of the whole plan | 1,082 ms | 555 ms | **50% less CPU** | bar 0.40x, measured **0.500x** - **MISSED on all four runs** |
| **peak resident set**, one round of the whole plan | 37.22 MiB | 40.76 MiB | **9.5% more memory** | bar 0.95x, measured **1.095x** - **MISSED on all four** |

The processor and memory figures are the gate's *comparable pair*: **one child process each**, both
opening a finished file the parent built, both running one round of the same plan, neither figure a
delta.

**The memory row is the one that has moved furthest over the reviews, and it did not move this
time.** Review 5 measured 75.25 MiB - 102% more than SQLite - and named four consumers. Review 6
reached 53.28, review 7 reached 42.61 by shrinking the file rather than a buffer, and a later change
reached **40.76** by stopping the index build going through the page pool. It reads 40.76 again on
2026-09-23. [Where the memory goes](#where-the-memory-goes) has the per-workload attribution and the
measured reason the last 3.5 MiB is not going to come from another buffer either - including the
trivial binary that measures the process floor at 3.62 MiB with this engine's allocator installed and
3.62 MiB without it.

**The four runs, one by one.**

| | run 1 | run 2 | run 3 | run 4 | median |
|---|---|---|---|---|---|
| weighted geometric mean | 4.94x | 5.03x | 4.93x | 5.01x | **4.97x - 397% faster** |
| weighted lower bound (bound 3.00x) | 4.72x | 4.70x | 4.46x | 4.53x | **4.62x - `MET` on all four** |
| floor: every required family above 1.00x | `schema` 0.94x | `schema` 0.81x | `schema` 0.95x | **met** | **met on one of four** |
| digests | 34 of 34 equal | 34 of 34 | 34 of 34 | 34 of 34 | **all equal** |
| processor time, ours / SQLite's | 469 / 1,000 ms | 563 / 1,078 | 563 / 1,125 | 547 / 1,086 | **0.500x - `MISSED` on all four** |
| peak resident set, ours / SQLite's | 40.73 / 37.21 MiB | 40.79 / 37.22 | 41.38 / 37.21 | 40.71 / 37.22 | **1.095x - `MISSED`** |
| **SQLite's own `txn.batched`** | 805 ms | 813 | 838 | 837 | **steady across the four, see below** |

**`schema` is the only family that went under the floor, on three runs of the four.** Its median
ratio is 1.31x, and its lower bounds read 0.94x, 0.81x, 0.95x and 1.30x. It is a family of one
workload, so its bootstrap has three values and the widest interval on the page; the ratio is above the floor
and the bound is not. On 2026-09-20 it went under on one run of four.

**That last row is a fact about the volume rather than about either engine, and it is in the table
because a reader has to be able to tell the two apart.** `txn.batched` is 200 commits and 200
`fsync`s, so it is the workload the disk decides. An earlier sequence recorded the reference's own
arm going **309 ms to 895 ms part-way through four runs**, because a run writes a couple of gigabytes
through `%TEMP%` and the volume stops keeping up. This sequence read 805 to 838 ms, steady to within
4%, which is the slower of that volume's two states for both arms and every run. It is a reason to
read `txn.batched`'s **absolute** milliseconds with suspicion and no reason to doubt the ratios,
which pair the two arms round by round on the same disk in the same second.

The gate also removes its own scratch directory now, which it never did: 279 abandoned run
directories holding 60 GB had built up in `%TEMP%`, one per gate run ever taken.

### Elapsed time by family

Median of the four runs in the table above. **Bar** is what `compat/perf/contract.toml` asks of the
family; **weight** is what the contract gives it in the headline.

| family | weight | measured | the difference | the bar asks | verdict | lower bounds |
|---|---|---|---|---|---|---|
| `read.point` | 16% | 29.85x | **2,885% faster** | 100% faster | **MET**, 4 of 4 | 26.01-26.99 |
| `large.values` | 4% | 12.35x | **1,135% faster** | 50% faster | **MET**, 4 of 4 | 8.82-9.37 |
| `read.analytical` | 10% | 10.71x | **971% faster** | 400% faster | **MET**, 4 of 4 | 8.22-8.55 |
| `read.range` | 12% | 5.02x | **402% faster** | 200% faster | **MET**, 4 of 4 | 3.89-3.98 |
| `read.join` | 8% | 4.21x | **321% faster** | 200% faster | **MISSED** on the lower bound, 4 of 4 | 2.73-2.90 |
| `write` | 20% | 3.04x | **204% faster** | 50% faster | **MET**, 4 of 4 | 2.18-2.81 |
| `transaction` | 10% | 2.37x | **137% faster** | no slower than SQLite | **MET**, 4 of 4 | 1.83-1.92 |
| `extension` | 8% | 1.73x | **73% faster** | 50% faster | **MET** on 3 of 4, missed at 1.48x on one | 1.48-1.57 |
| `open.prepare` | 8% | 1.69x | **69% faster** | 400% faster | **MISSED**, 4 of 4 - the bar asks for more than SQLite itself reaches, see below | 1.22-1.29 |
| `schema` | 4% | 1.31x | **31% faster** | 200% faster | **MISSED**, 4 of 4, and under the floor on 3 of 4 | 0.81-1.30 |

**`extension` clears its bar on the lower bound for the first time**, at 1.55x against 1.50x.
`extension.fts.build` went from 0.69x to 0.95x with the write path, since an FTS5 build is four
ordinary row writes a document, and it is still the slowest workload in the family.

**`read.join` still misses on the lower bound**, at 2.73x to 2.90x against a 3.00x requirement, with
the family reading 4.21x. `join.range` at 0.84x is the half that holds it down. A later measurement
checked that workload's time on this engine across three builds and 27 passes and found it unchanged;
the bound follows SQLite's arm of the same workload.

**Every lower bound in the table above is the pooled statistic**, which the gates used until a later
change: every workload's every round in one list, bootstrapped. That bound mostly measures how
far apart a family's workloads are. The gates now resample rounds, and four pinned passes of `main`
read `read.join`'s lower bound at 4.08x to 4.27x, which meets the bar, and `extension`'s at 1.54x to
1.67x. [Performance](performance.md#how-a-familys-interval-is-computed) has both statistics
from the same samples for every family.

**`read.analytical` is 10.71x**, with a lower bound of 8.45x against the 5.00x the bar grades.
`scan.aggregate` reads **52.09x** and `scan.group` **27.72x**, where they read 11.54x and 7.96x before
`count(*)` became one addition a batch rather than one accumulator call a row. `scan.sort`
reads 5.43x and `scan.distinct` 1.69x, and `scan.distinct` is still what holds the bound furthest
from the family's ratio.

### The three bars, and what each would take

Two of the missed bars are not gaps to close; they ask for more than the workload can give, and the
arithmetic says so rather than an opinion. The third is now met on the median.

- **`open.prepare`, bar 5.00x.** The family is `prepare.point` at 5.10x and `prepare.trivial` at
  0.57x, whose geometric mean is 1.69x. For the family to reach 5.00x, `prepare.trivial` would have
  to reach **4.90x** - and SQLite compiles, binds, steps and resets `SELECT 1` in **407 ns** here, so
  that is a demand for **83 ns**. No SQL front end does that; SQLite's own number is five times it.
  What is reachable is parity, and the allocations are most of the way there: ours is **13**,
  measured by `inillucent-prepareprofile`, where it was 24 - one change removed three and a later
  one removed eight more. **Nine of the thirteen leave with the compiled statement** and cannot go without
  changing what a compile returns, so the remaining scratch is four allocations: the parser's
  lookahead buffer, its result-column vector, the literal's own text, and one the profiler cannot
  attribute. `prepare.trivial` is 729 ns against 407 on 2026-09-23, from 837 against 460 on
  2026-09-20.
- **`schema`, bar 3.00x.** One workload. Ours is 26.47 ms against SQLite's 34.63, and its stages on
  2026-09-23 are `scan 3.6 ms, sort 4.8, pack 15.1 to 16.3, catalog 0.2, seal 1.1`. `pack` is where
  the pages are written, and it is 4 MB of file at a 32 KiB page; a packer that cost **nothing at
  all** leaves about 10.5 ms, which is about 3.3x, so the bar is reachable only by writing the index
  for almost nothing. The stage that came out of this workload was the pool: a bulk build used to
  write each page into a frame that then had to be evicted, and it writes to the file directly now,
  which took the workload from 0.66x to 1.37x and the plan's peak resident set from 42.45 MiB to
  40.76.
- **`extension`, bar 1.50x.** **Met on the median lower bound**, at 1.55x, and on three runs of
  four. `extension.fts.build` is 0.95x - 10.9 µs a document against 10.4 - where it was 0.69x, and
  the index itself is still the smaller part of it: the gate's stage timer puts the whole index build
  at 2.3 ms of the workload's 5.44 ms a round, so **58% of the workload is the SQL and virtual table
  write path around the index**. Inside the index the stages are `content 0.5 ms`, `tokenize 0.4`,
  `docsize 0.3`, `dict write 0.6`, `flush 0.6` and `new terms 0.4` over 507 terms.

**The gate prints `NOT MET`, and it is exact about which tests that is.** The headline clears its
bound on all four runs. What is missed is the **memory bar** - 1.095x against 0.95x, taken apart
below - the **processor bar**, 0.500x against 0.400x on all four runs, and three per-family **bars**,
which are targets rather than requirements. The **floor** - no required family slower than SQLite -
is met on one run of the four, because `schema`'s lower bound went under 1.00x on the other three.
It was missed by `transaction` on all four before the setup fix.

### The workloads that are slower than SQLite

Thirty weighted workloads, median of four runs. Twenty four are faster; these six are slower.

| workload | family | ratio | how much slower | per operation | why |
|---|---|---|---|---|---|
| `prepare.trivial` | `open.prepare` | 0.57x | **75% slower** | 729 ns against 407 | `SELECT 1` compiled per iteration, in **13 allocations** |
| `join.range` | `read.join` | 0.84x | **18% slower** | 57.1 µs against 48.6 | an index range and a probe per entry, where SQLite amortises one statement's overhead over two hundred rows |
| `extension.fts.build` | `extension` | 0.95x | **5% slower** | 10.9 µs a document against 10.4 | 58% of it is the write path around the index. It was 45% slower on 2026-09-20 |
| `range.lookaside` | `read.range` | 0.96x | **4% slower** | 57.4 µs against 55.7 | the same shape as `join.range`, 200 rowid probes |
| `extension.json` | `extension` | 0.97x | **3% slower** | 289 ns a call against 282 | the extraction, plus one uncontended mutex and two comparisons a call; a repeated document and path are already cached |
| `txn.autocommit` | `transaction` | 0.98x | **2% slower** | 1.17 ms a statement against 1.15 | one `fsync` each, and on this device an `fsync` is most of the millisecond. It was **0.13x** before a commit became one log append and one sync |

**`write.insert.batch` left this list.** It was 0.72x on the 2026-09-20 list and is **1.47x**, 7.0 µs
a row against 10.2, after a leaf's delta area was resized by its free space and its compaction
changed to splice rows in.

**Four more workloads are far slower, and the contract does not weight them.** They are the
`read.correlated` family, a subquery that names a column of the outer query, and each is graded
against the join that asks the same question rather than against SQLite:

| workload | this engine | SQLite | how much slower |
|---|---|---|---|
| `correlated.exists` | 59.69 ms | 0.29 ms | **21,332% slower** |
| `correlated.in` | 118.19 ms | 0.10 ms | **118,020% slower** |
| `correlated.exists.selective` | 2.11 ms | 0.022 ms | **9,395% slower** |
| `correlated.scalar.selective` | 2.11 ms | 0.021 ms | **9,839% slower** |

SQLite answers these as a join; this engine runs the subquery once per outer row. **That table is
the last graded run, and it is out of date by two orders of magnitude.** At `52c4b5f`, on four
passes the gate refused to grade because the machine was busy, the four read 0.40 ms against 0.30,
0.92 ms against 0.10, 19.5 µs against 23.7 and 17.9 µs against 22.3. A later change stopped every
execution of a correlated block building and freeing a 3.2 MB slot array. The two selective forms
are faster than SQLite there, and a correlated `IN` over 400 outer rows is still 809% slower.
[Performance](performance.md#measured-again-at-52c4b5f-on-2026-09-24-and-not-graded) has the
passes, and [the slower workloads](performance.md#the-workloads-that-are-slower) has what earlier
work did before that.

**`txn.autocommit` got seven times faster to reach this list.** It was 0.13x - 10.91 ms a
statement - because a commit was a checkpoint: the log folded into the file, a rollback journal holding
the pre-image of every page the fold was about to overwrite, six to eight `fsync` class calls a
statement. A commit is one append to the log and one sync of it now, measured on the gate's own
counters as **100 log writes and 100 log syncs for 100 statements, with no data file syncs and no
folds**, where the same workload used to make 202 of them and write 3,252 KiB of log for 50 KiB of
rows. Both engines now do exactly one `fsync` a commit, so what is left is everything either does
around that one call.

**`txn.large` has left this list.** It was the slowest workload on the board at 0.09x and it decided
the `transaction` floor; it is now **3.80x**, where the median round takes 2.76 ms against SQLite's
10.50. What that took is in
[the performance page](performance.md#the-workload-that-was-measuring-nothing), including the part
that was a defect in the measurement rather than in the engine.

And the other end of the same table, as ratios: `point.miss` 52.48x, `scan.aggregate` **52.09x**,
`large.read` 44.69x, `point.rowid` 30.74x, `scan.group` **27.72x**, `join.selective` 20.94x,
`point.index` 16.34x, `range.reverse` 15.63x, `range.covering` 8.41x. The two in bold are where
`count(*)` stopped being one accumulator call a row.

**Four of the six had one cause, and it was not the storage engine.** The whole tree write was
ablated out of the in-place update path - the statement found its row, decided what to write, and
returned without writing - and `txn.large` measured **1.54 microseconds against 1.53**. What was left
was what a statement costs before it reaches a tree: the operator chain, the column names, the
`EXPLAIN` description, the row space, the assignments and the declarations were all rebuilt on every
execution.

The setup fix took most of that out. A parameter is now read when its expression is evaluated rather than
folded in when the expression is built, which is what let anything compiled outlive one execution's
values; a write statement's setup is built once per compiled statement rather than once per
execution; and `update_in_place` takes the caller's before-image instead of descending the tree a
third time. `UPDATE side_table SET note = ?2 WHERE id = ?1` went from **2,219 ns and 33.7 heap
allocations to 1,235 and 13.3**, and `txn.large` from 0.09x to 3.70x.

What is left of that cost is why `join.range` and `range.lookaside` still sit just under SQLite while
`point.rowid` sits twenty-six times over it: a query that spreads one statement's overhead across two
hundred rows shows the remainder, and one that does not, hides it. The remaining lever is measured
and not connected - `physical::build_statement` holds an operator chain across executions, and
reusing it is worth **738 ns to 358** on `SELECT 1`, **1,413 to 786** on a point lookup and
**71,672 to 59,983** on the 200-row range scan those two workloads are shaped like. It is not wired
up because an index nested loop holds a borrow of the tree it reads, and the engine keeps its trees
in a map the write path takes mutably, so caching a chain means reference counting the trees - and
getting that borrow discipline wrong is a runtime panic rather than a wrong number.

---

## Where the memory goes

**The one row on this page that was worse than SQLite, taken apart and then acted on twice.**
Review 5 measured **75.25 MiB against SQLite's 37.19** - 102% more - and named four consumers.
Review 6 went after them and reached **53.3**. Review 7 then went after what review 6's attribution
said was left, which was not a buffer at all but the **file**: it is now **42.61 MiB against 37.19**,
**15% more**, and the `.rdb` has gone from **1.41x** the `.db` to **1.036x**. Every number below is
re-measured rather than edited.

### How the attribution is taken now

Review 5 found where the memory was by running the gate once per family and reading one number off
each run: seven runs to answer one question, at family granularity. The gate's memory child already
walks the whole plan in one process, so it now reads its high-water mark **after every workload** and
prints the steps, with the pool's own bytes beside them. One run, per-workload granularity, and the
pool separated from everything else the process holds. Both columns are the same plan at the same
128 MiB budget, on the same box, minutes apart:

| workload | before review 7 | | | after | | |
|---|---|---|---|---|---|---|
| | **peak** | pool | other | **peak** | pool | other |
| `(open)` - the file opened and the pool warmed | 31.50 | 22.59 | 8.90 | **26.74** | **17.84** | 8.90 |
| every read workload | 31.54 | 22.59 | 8.95 | 26.79 | 17.84 | 8.94 |
| `write.insert.batch` | 34.73 | 22.94 | 11.55 | 30.92 | 18.19 | 12.20 |
| `write.update.indexed` | 35.39 | 22.94 | 12.45 | 31.23 | 18.19 | 13.05 |
| `write.upsert` | 36.59 | 22.94 | 12.53 | 32.49 | 18.19 | 13.18 |
| `schema.index` | **51.35** | 29.84 | 12.23 | **46.61** | 24.66 | 12.78 |

**Read the `other` column.** It does not move: 8.90 MiB of process floor at open in both, 12.2-12.8
MiB at the peak in both. The whole 4.74 MiB is the **pool**, and the pool fell because the file did.
That is the claim review 6 ended on - *the cache costs what the file costs* - measured directly
rather than inferred.

### What was fixed, each measured on its own

Review 6's four changes took 75.20 MiB to 53.4: the redo buffer bounded to 512 KiB, the index build
no longer holding three copies of the tree, a version log nothing was collecting, and the allocator's
free list capped in bytes as well as blocks. They stand and are not repeated here.

Review 7 added one change and it is a **format** change:

| change | what it was | measured |
|---|---|---|
| **An integer mini-column is as wide as its own values** | an `Int64` slot was eight bytes whatever the value held, so `main_table`'s three integer columns cost 24 bytes a row where SQLite's record varints cost about five. The column directory has always carried a `slot_width` `u16` that no reader ever read; the builder now picks the narrowest of 1, 2, 4 and 8 that holds every typed value in the leaf, and every reader honours the directory - so a file written before this change reads unchanged, because its pages say eight | the `.rdb` **22.66 → 17.90 MiB**, the peak resident set **51.33 → 46.61**, and every read family *up*: `read.point` 27.06x → **30.49x**, `read.join` 3.98x → **4.44x**, `read.analytical` 6.08x → **6.68x** |
| **A compaction that cannot use its preferred fill splits no more** | `BULK_FILL` is 0.9 and `COMPACT_FILL` is 0.75, and `make_room` refused a compaction unless every live row fitted in 0.75 of a page - which a leaf packed at 0.9 never does. So every bulk-built leaf **split on its first write, whatever the write was**, and the space was never recovered | one `UPDATE main_table SET key = key + 1 WHERE id % 20 = 0` took `main_key` from 57 pages to **113** and `main_category` from 85 to **117**; both now stay where they are after four rounds of it. SQLite, same statement: 307 pages before and after |

### The file, tree by tree

`dbstat` over a freshly imported medium fixture at 32 KiB pages, both builds, same fixture:

| tree | shape | pages before | pages after | entries/page after |
|---|---|---|---|---|
| `main_table` | `(rowid, key, category, label, payload)` | 468 | **388** | 241 |
| `main_category` | `(category, key, rowid)` | 85 | **34** | 2,941 |
| `main_key` | `(key, rowid)` | 57 | **23** | 3,704 |
| `side_table` | `(rowid, owner, note)` | 30 | **16** | 1,190 |
| `side_owner` | `(owner, rowid)` | 15 | **4** | 4,167 |
| `wide` | `(rowid, body)` - a text column | 58 | 58 | 6.9 |
| **the file** | | **23,756,800 B** | **17,432,576 B** | |

`main_category`'s leading column holds 64 distinct values and is therefore **one byte**, which is why
it is the biggest winner. `wide` does not move at all, which is the check that nothing narrowed that
should not have.

### What it cost, and what would not buy it back

The narrow slot is not free. Four 30-round runs each way, medians, lower bound in brackets:

| family | wide | narrow |
|---|---|---|
| `read.point` | 27.06x (25.19x) | **30.49x (27.77x)** |
| `read.join` | 4.08x (2.90x) | **4.44x (3.17x)** |
| `read.analytical` | 6.08x (5.07x) | **6.68x (5.75x)** |
| `read.range` | 4.18x (3.40x) | 4.19x (3.48x) |
| `large.values` | 13.40x (9.35x) | 13.01x (9.33x) |
| `schema` | 1.35x (1.31x) | 1.36x (1.30x) |
| `extension` | 1.20x (1.02x) | 1.21x (1.04x) |
| **`write`** | **2.03x (1.85x)** | 1.52x (1.31x) |
| **`transaction`** | **1.45x (1.04x)** | 1.37x (1.00x) |
| headline | **3.85x (3.71x)** | 3.71x (3.63x) |
| processor time | 0.33x | 0.35x |
| peak resident set | 51.33 MiB | **46.61 MiB** |

The cost is in exactly three workloads and they have one cause: `write.insert.batch`
**39.8 ms → 69.4**, `write.update.indexed` **48.6 → 95.6**, `write.delete` **15.4 → 22.4**. All three
write `main_table` and its two indexes inside one transaction, and a compaction is one pass over
every live row of a leaf - of which an index leaf now holds twice as many. `write.upsert`, which
writes the one table that did **not** narrow, is unchanged at 2.86 → 2.84 ms.

**`DELTA_LIMIT = 64` does not buy it back, and the negative result reproduces on both arms**, which
is what makes it a result. Four runs each:

| family | wide @32 | wide @64 | narrow @32 | narrow @64 |
|---|---|---|---|---|
| `write` | 2.03x (1.85x) | 2.13x (1.93x) | 1.52x (1.31x) | 1.54x (1.35x) |
| `transaction` | 1.45x (1.04x) | 1.20x (0.88x) | 1.37x (1.00x) | 1.22x (0.85x) |
| `large.values` | 13.40x (9.35x) | **6.84x (6.02x)** | 13.01x (9.33x) | **6.70x (5.81x)** |
| headline | 3.85x (3.71x) | 3.74x (3.62x) | 3.71x (3.63x) | 3.58x (3.48x) |

A larger delta area halves the compactions and makes every read of a written-to leaf walk twice as
far, and the second effect is the larger one. `DELTA_LIMIT` stays at 32.

### The five follow-ups, each measured

Review 7's first pass shipped the narrow slot and named five follow-ups. All five were then worked,
and two of them turned out to be worth less than they looked, which is why each one was measured on its own.

| # | what | outcome |
|---|---|---|
| 1 | **A compaction that does not rewrite the whole leaf** | `make_room` called `LeafRef::live`, which allocates a `Vec<Datum>` per row plus one outer vector - 3,701 allocations to repack an index leaf, for values already on the page. It now materialises **once, flat**: one allocation of `rows * width`, indexed directly. `write.insert.batch` **39.8 → 30.6 ms**. Reading straight through the mini-columns instead was tried first and was *worse* - the per-value class check and slot decode cost more than the allocations on a leaf of many small rows |
| 2 | **The process floor** | the number this document carried was wrong. A **trivial 110 KB Rust binary from this workspace has a 4.1 MiB floor**, and `sqlite-bench` has 4.2 - so almost all of it is the operating system, not the engine. What is ours is about 2.2 MiB of code and statics. `panic = "abort"` and `strip = true` take the binary from **8.49 MB to 6.50** and the floor from 8.90 MiB to **8.49**; SQLite is C and does not unwind, which is the same argument the profile already makes about LTO |
| 3 | **The index build's arena, made cheaper rather than spilled** | the high-water mark *is* the arena, and the sort prefix - sixteen bytes an entry, 1.6 MiB - is dead the moment the order exists. Freed between the sort and the pack: **1.59 MiB off the peak at no cost**, `schema.index` 26.5 → 26.1 ms. `shrink_to_fit` on the payload arena was tried in the same call and **removed**: it returned nothing and cost 2 ms |
| 4 | **Frame of reference for integers** | a per-column base in a sixteen-byte directory entry, so a width comes from a column's *range* rather than its magnitude. `main_key` 27 → **23** pages, `side_owner` 6 → **4**. Worth **0.33 MB** of file. The first decode went through `i128` and cost `read.analytical` **6.57x → 2.77x**; matching the width once and adding with `i64` put it back to 6.79x |
| 5 | **The `(u32, u32)` heap pair narrowed to `(u16, u16)`** | a page of 64 KiB or less addresses every value in two `u16`s. `main_table` 415 → **388** pages, `side_table` 21 → **16**. Worth **1.02 MB** of file |

Together: the `.rdb` **17.90 → 16.62 MiB**, the peak resident set **46.61 → 42.61**, and every read family
higher than the arm without them.

### What it cost, and the family it put under its floor

**This is review 7's own A/B and it is left as it was taken**: four 30-round runs each way, on the
build that shipped the narrow slot, medians with the 95% lower bound in brackets. Every figure in it
predates the setup fix, which is why `transaction` reads 0.84x here and 3.41x at the top of this page.

| family | wide | narrow (shipping) |
|---|---|---|
| `read.point` | 25.96x (23.15x) | **31.83x (28.20x)** |
| `read.range` | 4.78x (3.76x) | **5.03x (4.04x)** |
| `read.join` | 3.77x (2.60x) | **4.24x (2.87x)** |
| `read.analytical` | 5.88x (4.72x) | **6.79x (5.57x)** |
| `schema` | 1.25x (1.20x) | **1.38x (1.35x)** |
| `extension` | 1.26x (1.15x) | **1.43x (1.23x)** |
| `large.values` | 11.25x (7.88x) | 11.18x (8.50x) |
| `open.prepare` | 1.58x (1.17x) | 1.61x (1.20x) |
| **`write`** | **2.08x (1.87x)** | 1.71x (1.50x) |
| **`transaction`** | **1.23x (0.80x)** | 0.84x (**0.59x**) |
| weighted headline | 3.75x (3.62x) | **3.79x (3.65x)** |
| peak resident set | 49.17 MiB | **42.61 MiB** |
| memory ratio | 1.32x | **1.15x** |

**That put `transaction` under the 1.00x floor, which is a release-blocking condition, and it stayed
there for two tickets.** One workload did it: `txn.large` replaces a ten-byte `note` with a
fifty-byte one, two thousand times, in one transaction. The lengths differ, so the in-place slot
write refused every time and each statement became a delta insert - and every thirty-second one a
compaction over the whole leaf. `side_table` went from 30 pages to 16, so a leaf holds twice as many
rows and a compaction costs twice as much: **4.1 ms → 10.2**. `write.upsert`, which writes the one
table whose columns did not narrow, was unchanged at 2.86 → 2.84 ms.

**`DELTA_LIMIT = 64` did not buy it back** - measured on both arms, it costs `large.values` half and
moves `write` by 0.01x - and neither did a cheaper compaction on its own. What cleared it was
the setup fix, and none of it was the compaction: a longer value is now relocated inside its own leaf
rather than refused, a statement stopped rebuilding its own setup on every execution, and the
workload itself was found to be updating rows to the values they already held. `txn.large` is
**3.70x** and the family **3.41x** with a 2.74x lower bound.
[The workload that was measuring nothing](performance.md#the-workload-that-was-measuring-nothing)
reads the number three ways so the engine's share and the measurement's share stay separate.

### Where the rest of it is

**This engine's transient is smaller than SQLite's and its file is now within 12% of SQLite's.** What
is left is two quantities, and one of them is no longer the file:

| | inillucent | SQLite | why |
|---|---|---|---|
| **the cached database** | 16.56 MiB | ~16 MiB | the `.rdb` is **1.036x** the `.db`, down from 1.41x. What remains is the class array's two bits a row and the page directory |
| **the process floor** | 8.65 MiB | ~4.2 MiB | of which **4.1 MiB is what any Rust binary in this workspace costs before the engine exists** - a 110 KB one measures the same. About 2.2 is this engine's code and statics; the rest is the gate child's plan and the opened catalog, which `sqlite-bench` does not build |
| **`schema.index`'s rise** | 9.33 MiB | ~15.9 MiB | the pages the new index occupies plus the sort's arena. Ours is the *smaller* of the two |

**The bar is still missed: 1.095x against 0.95x, 40.76 MiB against the 35.36 it would need.** The file
is no longer where it is: at 1.036x of SQLite's there is under 0.6 MiB left in the cached database.
What is left is 4.2 MiB of process - most of it the operating system's, which neither engine escapes -
and one `CREATE INDEX`.

**And the engine's own allocator is measured out of it.** A 130 KB Rust program whose `main` reads its
own working set and returns peaks at **3.62 MiB with `inillucent-alloc` installed as its global
allocator and 3.62 MiB without it**, 0.66 MiB private either way, over five runs each. It has no
initial reservation to size down, so the two to three mebibytes an earlier review went looking for
are not there.

### Two causes, ruled out in review 5 and still ruled out

| suspected cause | the test | the result |
|---|---|---|
| **the allocator** - a size-classed free list that recycles rather than returning pages | `inillucent-allocarm`, one binary, one code path, the allocator swapped by a flag | **86.7 MiB pooled against 84.3 MiB on the system allocator - 2.4 MiB**, and the free list is **15% faster**. What review 6 changed is its *cap*, not the allocator: bounding the large classes recovered 1.6 MiB and left the headline where it was |
| **the 32 KiB page size** - eight times SQLite's 4 KiB | the gate at four page sizes, the budget held at 128 MiB on both arms | **78.6 MiB at 4 KiB against 75.2 MiB at 32 KiB.** The larger page is *slightly smaller* in memory, and 4 KiB costs the headline as well - 3.51x against 3.89x |

The third, **the process floor**, is now the largest single item left: 4.7 MiB of the remaining 11.3
is exactly it. `sqlite3` opens a database and runs `SELECT 1` in **4.2 MiB** and `inillucent-shell`
in **6.0 MiB**, and the gate's child carries the plan on top of that.

### The pool default, re-walked on the smaller file and left where it was

`PRAGMA cache_size` answers **-131072** here - 128 MiB - and **-2000** in SQLite, and it is a real,
working switch. Review 6 walked this ladder with a 22.66 MiB file; the file is now 17.90, so a 20
MiB pool holds the whole database and the question is a different one. Seven budgets, twelve rounds
each, on the shipping build:

| budget, both arms | inillucent | SQLite | ratio | weighted headline (lower bound) |
|---|---|---|---|---|
| **128 MiB (the default)** | 46.32 MiB | 37.20 MiB | 1.25x | **3.61x (3.49x)** |
| 32 MiB | 45.96 | 37.20 | 1.24x | 3.81x (3.58x) |
| 24 MiB | 46.05 | 37.18 | 1.24x | 3.77x (3.73x) |
| 20 MiB | 43.23 | 36.15 | **1.20x** | 3.61x (3.46x) |
| 16 MiB | 40.52 | 32.97 | 1.23x | 3.37x (3.27x) |
| 12 MiB | 36.51 | 29.19 | 1.25x | **2.40x (2.34x) - under the bound** |
| 8 MiB | 32.23 | 24.60 | 1.31x | **2.47x (2.43x) - under the bound** |

**The answer is the same as review 6's and for the same reason: the budget is matched.**
`fullgate` derives SQLite's `cache_size` from the pool's bytes, so lowering ours lowers theirs, and
the ratio never comes near 0.95 - the best rung is 1.20x and it costs the headline. Below 16 MiB the
headline falls under the 3.00 bound outright. **So the default stays at 4,096 frames**, and
`PRAGMA cache_size` remains the switch for a caller who wants the other end of this table.

### Two defaults, decided by measurement rather than by preference

Settled in review 5, unchanged since, and repeated here because they are two of the seven
rows that differ from SQLite:

| default | alternative measured | decision |
|---|---|---|
| `journal_mode` | 3.78x weighted / 3.45x low with `wal`; **3.70x / 3.44x with `delete`** | **`delete`**, the reference's own default. It costs nothing, so there is no reason not to have it |
| `locking_mode` | 3.78x / 3.45x with `exclusive`; 3.03x / 2.95x with `normal` on the build that lost writes | **`normal`**, SQLite's own default, chosen on correctness rather than on the gate: `exclusive` holds the file for a connection's whole life, so a second process reads state from before that connection started. `PRAGMA locking_mode = exclusive` is still a real switch for a program that never opens a second connection. What `normal` costs a single process is a checkpoint and a lock release between statements: 300 autocommit inserts through `inillucent-shell` take 10.70 s against 0.46 s under `exclusive`, on a debug build. An application that never opens a second connection sets `exclusive` and pays none of it |

---

## What is still missing

Review 5 filed this list; **Review 6** worked it, and this is where it stands. Nothing
here is a guess: every row names the measurement or the file it comes from.

### Against the first goal - a highly performant SQLite replacement

The headline gap was **memory**: 102% more than SQLite on the same plan under the same budget, with
the goal being to hold **less**. It is now **9.5% more**, the contract grades it, and
[Where the memory goes](#where-the-memory-goes) says exactly what the remaining difference is made
of - which is no longer the leaf format, because review 7 fixed that.

| # | gap | state | measured |
|---|---|---|---|
| 1 | **The contract graded no memory and no processor time at all** | **closed** | `compat/perf/contract.toml` has a `[memory]` bar of **0.95x** and a `[cpu]` bar of **0.40x**, both ratios of SQLite's on the same plan and the same budget, both read off the gate's child-process pair and printed in its verdict, and both folded into `passed`. They were written **before** the optimisation work, which is the only order in which a bar is a bar |
| 2 | **The index build is the largest single consumer of memory on the board** | **most of the way, and now the largest item left** | 28.87 MiB of rise at a 128 MiB budget, now **14.11** - of which about half is the pages the new index legitimately occupies. The leaf images no longer accumulate (`LeafBuilder::fit` counts the run without encoding it) and the flat `Vec<Datum>` and its slice vector are gone (`LeafBuilder::pack_rows` reads the arena). What is left is the sort's own arena - **spilling it costs about 8 ms of a 27 ms statement**, which would put `schema` under its 1.00x floor, so it is a follow-up rather than a change made here |
| 3 | **The redo buffer was held whole** | **closed** | `SPILL_BYTES` = 512 KiB. One round of `write.update.indexed` writes 7,927 KiB of log and now holds at most half a megabyte of it; **75.20 → 68.04 MiB**, fifteen extra writes, no extra syncs |
| 4 | **Large values were said to be materialised where SQLite streams them** | **not a gap where it was thought to be** | `large.values` at 37.48 MiB in the family-by-family run is 31.5 MiB of open-and-warm plus about 6. Run in plan order, `large.read` and `large.write` **do not move the high-water mark at all**. The 6 MiB is worth having and is a follow-up; it was never the 405% the family table implied |
| 5 | **FTS5's build accumulates** | **open** | 1.6 MiB above the baseline in isolation, and 0.28-0.48x on time. Both have one cause: **2,000 tree row writes per 500 documents** - `%_content`, `%_docsize`, then 507 dictionary rows and 507 doclists at the flush - where SQLite writes about 1,000 rows and one segment blob. The fix is the segment format, and it is the same change for the time bar |
| 6 | **The page pool's default was not a decision anybody made** | **decided, and left where it was** | seven budgets, twelve rounds each, both numbers read off every one - the table in [Where the memory goes](#where-the-memory-goes). The ratio is worst at both ends and best at 20 MiB (1.28x), and 20 MiB costs the headline 3.91x → 3.45x while 16 MiB puts it **under the 3.00x bound**. Trading a met headline for a missed memory bar is not a trade; `PRAGMA cache_size` remains the switch |
| 7 | **The memory bar itself is missed** | **open, and the file half of it is closed** | **1.095x against a 0.95x bar** on 2026-09-23, down from 1.15x when this row was written and 1.43x before that. Review 6 said the remaining gap was the file rather than a buffer, and review 7 acted on it across six changes: an integer mini-column is as wide as its own values, a per-column frame of reference makes that width a function of the column's *range*, and a heap slot is `(u16, u16)` on a page of 64 KiB or less. The `.rdb` went **22.66 → 16.62 MiB** (1.41x the `.db` → **1.036x**) and the pool fell with it, so the attribution was right. Of the 7.3 MiB left, **4.1 is what a 110 KB Rust binary in this workspace already costs** - `sqlite-bench` costs 4.2, so almost none of it is this engine - and the rest is `schema.index`'s own pages and arena. The pool budget was re-walked on the smaller file and still cannot reach the bar |
| 8 | **`open.prepare` straddles the floor** and misses its bar on every run | **open, and the allocations are down by half** | `prepare.trivial` - `SELECT 1`, compiled per iteration - took 1,258 ns against SQLite's 420, in 25 allocations when this row was written. The count is **13** now: one change removed three and another removed eight, and nine of the thirteen left leave with the compiled statement. The nanoseconds here are this sitting's and have not been re-measured; `docs/performance.md` carries the current ratio |
| 9 | **`schema` misses its elapsed-time bar by an order of magnitude** | **open** | 1.29-1.34x against a bar of 3.00x. The stage breakdown says where: `scan 3.6 ms, sort 5.6, flatten 0.0, pack 11.5, catalog 0.2, seal 5.6` of 27 ms. `flatten` was 1.0 ms before this ticket and is now free; `pack` absorbed 4.5 ms of it, which is what the second sizing pass costs |
| 10 | **`txn.large` and `write.insert.batch` were the two slowest workloads on the board** | **closed** | they read 0.18-0.23x and 0.52-0.62x when this row was written, and **3.80x and 1.47x** on 2026-09-23. `txn.large` moved with the setup fix's per statement work and a corrected workload; `write.insert.batch` with the delta area sized by its free space |
| 10a | **A compaction costs one pass over a leaf, and a leaf now holds twice as many rows** | **closed**, by sizing the delta area by the free gap and splicing a compaction's rows in; the `write` family reads **3.04x** on 2026-09-23. What follows is the history | Review 7's narrow integer slot took the `write` family from 2.03x to **1.52x**: `write.insert.batch` 39.8 → 69.4 ms, `write.update.indexed` 48.6 → 95.6, `write.delete` 15.4 → 22.4. All three write `main_table` and its two indexes in one transaction; `write.upsert`, on the one table that did not narrow, is unchanged. **`DELTA_LIMIT = 64` does not buy it back** - measured on both arms, it costs `large.values` half and `transaction` its floor and moves `write` not at all |
| 11 | **The C API is 53 symbols against SQLite's ~290** | open | `drivers/abi.toml`. Serialize/deserialize, incremental blob I/O, the authorizer, the hooks, the progress handler, tracing, `unlock_notify`, snapshots, a caller-supplied VFS |
| 12 | **Single-threaded.** SQLite has three threading modes; access from several *processes* landed in review 5 and threading did not | open | the architecture table below |
| 13 | **One language binding.** Python, standard library only | open | `drivers/bindings` |
| 14 | **A SQLite file cannot be opened**, only imported | out of scope by the rearchitecture's own decision | recorded so the comparison is not read as claiming otherwise |
| 15 | **No encryption at rest.** `ATTACH ... KEY` refuses by name | parity | the same position a build of SQLite without SEE is in |
| 16 | **Linux is 1.53x where Windows was 3.85x** at the time; Windows reads 4.97x on 2026-09-23 and Linux has not been re-measured | open | the same absolute work rather than a fix specific to Linux; the experiment is in [Performance](performance.md#linux) |

**Two things ruled out, so no ticket spends itself on them.** The allocator accounts for **2.4 MiB**
of the memory gap and swapping it costs **13% more elapsed time**; the 32 KiB page size accounts for **none**
of it - 4 KiB pages are *larger* in memory (78.6 MiB against 75.2) and cost the headline 3.89x to
3.51x. Both were measured in review 5 and both still hold.

### Found by auditing the list, not by running it

These are the gaps the 416 cases could not have found, because no case asked. The method and the
evidence are in [Is the feature list itself complete?](#is-the-feature-list-itself-complete), and it
is now a checked-in test - `crates/inillucent-compat/tests/registers.rs`.

| # | gap | state | measured |
|---|---|---|---|
| 17 | **`pragma_function_list` and `pragma_module_list` under-report, silently** - the project's one silent difference | **closed** | 161 names → **212**, and against the pinned library there is now no function SQLite answers that this engine does not. **Fifty-one names were present and unlisted**, each verified before it was listed. The check also found `->`/`->>` not binding as function names, **seven arities** that disagreed (`narg` encodes a minimum: `coalesce` is -4, not -1), **five pragmas** the driver front-end answered but did not name, and a `pragma_module_list` that answered 67 names through that front-end because it listed its own `pragma_*` shims |
| 18 | **Six FTS functions are absent** | **two closed** | `fts5_source_id()` and `optimize()` answer, both faithfully - `optimize` reports `Index already optimal`, which is true here because this index keeps one doclist per term. `fts5(...)`, `fts5_locale()`, `fts5_get_locale()` and `fts5_insttoken()` hand out C pointers or belong to FTS5's locale machinery, and a stub would be a **wrong** answer rather than a missing one. `fts3_tokenizer` is absent from the pinned library too |
| 19 | **Two modules are absent**: `fts4aux` and `fts3tokenize` | open | both absent from the pinned library as well, so they are a difference against the shell. The FTS5 analogue `fts5vocab` **is** here |
| 20 | **Four dot commands are absent** | **two closed** | 61 → **63 of 65**. `.load` answers the reference's own words for a library it cannot open, and `.progress` accepts and keeps the same state. `.expert` and `.session` are the two left, and the shell table says what each would mean here |
| 21 | **Twenty-three names give the wrong reason out of context** | **closed at review 6; the eleven window functions regressed when the engine was replaced** | `SELECT row_number()` answers `misuse of window function row_number()` and `SELECT bm25(1)` answers `unable to use function bm25 in the requested context`, which is what SQLite answers. At review 6 every one of the twenty-three was byte-identical when called properly, message and result alike. The engine `inillucent-shell` runs was replaced, and the new one refuses `OVER (...)` outright: the eleven window functions still answer the bare, out-of-context message correctly, but calling one properly - inside `OVER (...)` - is now a refusal rather than an answer. `bm25`, `highlight`, `snippet` and the rest are unaffected. See [SQL support](sql.md) |
| 22 | **The register comparison was a thing somebody had to think to do** | **closed** | `registers.rs`: six cases over the four enumerations, run on every build, written as an exclusion list so a name that differs has to be named there with why. Its own doc comment says why a suite that probes by calling cannot find this class of gap |

**Forty-one further names are not a gap**: `base64`, `base85`, `decimal*`, `ieee754*`, `sha1*`,
`sha3*`, `regexpi`, `zipfile`, `readfile`, `writefile`, `edit`, `lsmode`, `realpath`, `usleep`,
`stmtrand`, `strtod`, `dtostr` and the `shell_*` helpers live in `shell.c` and not in `sqlite3.c`, so
an application linking the library never had them. They are a gap for the *shell* only, and are
recorded here so a later reader does not re-discover them as library gaps - which is also why
`registers.rs` compares against the pinned **library** rather than against `sqlite3.exe`.


### Against the second goal - an embedding solution that matches pgvector

The ranking goal is met and then some: 15 of 17 graded comparisons better, none worse, re-graded in
full for this review. What is open is cost rather than quality.

| # | gap | measured | what "closed" looks like |
|---|---|---|---|
| 22 | **The retrieval index's resident set.** 3.83 GB for a 3.1 GB index of 598,560 chunks in production; 1,216 MiB for the 185,078-chunk corpus here | the production deployment, and this review's `inillucent-childcost` run | nothing has tried to make it smaller; a measured attempt, with a number |
| 23 | **`embed()` is behind a feature flag and off by default**, so the "one library, no second process" claim needs a build to be true | `--features embed` | a decision recorded either way, rather than a default nobody chose |


---

## Reproducing this

The probe is checked in as [`tools/feature-probe/`](../tools/feature-probe/README.md), so this document can be
regenerated from a clone rather than trusted.

```sh
cargo build --release --bin inillucent-shell   # the engine under test
tools/sqlite-reference.sh                      # the pinned reference, if .sqlite-ref is absent

node tools/feature-probe/run.js                # 416 cases through both shells, one fresh database each
node tools/feature-probe/summarise.js          # the per-area table
node tools/feature-probe/pragmas.js            # every PRAGMA the reference lists, asked of both
node tools/feature-probe/vector-features.js    # the vector surface, one pgvector feature at a time
```

Everything it writes lands in `_agent_output/feature-probe/`, gitignored. `results.json` carries the
script, both transcripts and the verdict for every case, which is what to read when a row moves.
`tools/feature-probe/README.md` says how to add a case and the two rules that keep one meaningful.

### The completeness audit

[Is the feature list itself complete?](#is-the-feature-list-itself-complete) does not read
`cases.js` at all. It asks SQLite what it has, then asks both engines for each of it, and it is
checked in beside the probe:

```sh
node tools/feature-probe/registers.js
```

It diffs the four registers and the two `.help` outputs, calls every one of the 218 function names
SQLite lists in both shells, and then calls the context-scoped ones properly, because a bare call
reports the wrong thing in *both* engines: `bm25`, `highlight` and `snippet` over a real FTS5 index
answer correctly there. Window functions are the exception in the other direction - a bare call
answers the same `misuse of window function` message SQLite gives, and calling one properly inside
`OVER (...)` answers what SQLite answers. Its transcripts land in `_agent_output/feature-probe/registers/`.

The three rules that keep it meaningful are in the file's own header: **the enumeration comes from
SQLite, never from us**; **every name is called, not just listed**, because a register can
under-report in either direction; and a name SQLite only has in `shell.c` is checked against the
pinned amalgamation before it is called a gap.

**It does not have to be remembered**, because the same comparison is a test:

```sh
cargo test --release -p inillucent-compat --test registers
```

Six cases over the four enumerations, against the pinned **library** rather than the shell - which is
the right basis for an embedder, and is why the forty-one `shell.c` names are not in it. It is
written as an exclusion list, so a name that differs has to be named there with the reason, or the
build fails.

### The performance, memory and vector numbers

The feature tables come from the probe; every number in [Performance](#performance),
[Where the memory goes](#where-the-memory-goes) and
[Vector search](#vector-search-against-postgresql--pgvector) comes from these, run in this order.

```sh
cargo build --release

# the fixtures, one fresh copy per gate run - schema.index leaves an index behind
bash tools/build-gate-fixtures.sh <dir>

# elapsed time, processor time and peak resident set, both engines, matched budget
target/release/inillucent-fullgate <dir>/medium-run1.db --scale medium --rounds 30 --page-size 32768 --frames 4096
#   run it four times, on four fresh copies of the fixture
#   the gate exits 1 whenever any bar is missed, and the memory bar always is, so read the
#   sections rather than the exit code
#   a summary script reads the four transcripts and prints every figure this document
#   publishes, taking each median as the average of the two middle runs

# the memory attribution: the gate's own child prints its high-water mark after every workload,
# with the pool's bytes beside it, so one run answers what seven family-filtered runs used to
target/release/inillucent-fullgate <dir>/m.db --scale medium --rounds 3 --frames 640

# the budget ladder: the gate derives SQLite's cache_size from the pool's bytes, so every arm is
# matched. --frames 4096 / 1024 / 768 / 640 / 512 / 256 / 128 is the table in "Where the memory goes"
target/release/inillucent-fullgate <dir>/m.db --scale medium --rounds 12 --page-size 32768 --frames 640

# the allocator, ruled out: one binary, one code path, the allocator swapped by a flag
target/release/inillucent-childcost target/release/inillucent-allocarm <dir>/m.db --rounds 8 --scale medium
target/release/inillucent-childcost target/release/inillucent-allocarm <dir>/m.db --rounds 8 --scale medium --system

# the memory a read costs: two shells, 200,000 rows each. The cache_size ladder is the same
# script with a leading `PRAGMA cache_size = -N;`, through inillucent-childcost
target/release/inillucent-shellrss

# the retrieval engine, re-graded in full against pgvector. Every flag goes AFTER the verb.
# Run it from the repository root: the card records the commit by reading the working directory.
target/release/inillucent-bench embed-check --cache <cache> --model-dir <models>/nomic-embed-text-v1.5 --device cuda:0
target/release/inillucent-bench grade --database-url postgres://postgres:<password>@127.0.0.1:5433/inillucent_synth     --cache <cache> --model-dir <models>/nomic-embed-text-v1.5 --device cuda:0     --per-source 100 --runs-dir <runs> --out inillucent-scorecard.md
target/release/inillucent-bench save --cache <cache> --dir index.inillucent --quantized
target/release/inillucent-childcost target/release/inillucent-bench open --dir index.inillucent
```

Review 5's own transcripts - four gate logs, the eight-arm budget sweep, the seven family
attributions, the allocator pair, the `cache_size` ladder and the regenerated score card - are under
`_agent_output/`, alongside review 6's: the four accepted gate runs, the two four-run sets a
contended volume spoiled - kept, because the reference arm's own `txn.batched` in them is the
evidence for what contention looks like - the seven-budget ladder, the allocator pair, and the
per-workload attributions the memory work was steered by. That folder is not checked in; the
commands above rebuild all of it.

The cases the engine's own suite carries are `crates/inillucent-compat/tests/semantics.rs` - **208
now**, up from 164, every one of them a construct this document moved - plus `vector.rs` for the
filtered-search recall and `new_engine_writes.rs` for the write path. They run in the selected suite
and fail when a construct changes its mind in either direction. The probe is wider than the suite
deliberately: it is the instrument that *finds* a difference, and a difference it finds becomes a
case there.

---

## Where the rest of the documentation is

| | |
|---|---|
| [`docs/README.md`](README.md) | the documentation index |
| [`README.md`](../README.md) | what the project is, in one page |
| [`docs/sql.md`](sql.md) | this document's findings, told as what runs and what differs |
| [`docs/performance.md`](performance.md) | the SQLite comparison, summarised |
| [`docs/retrieval-quality.md`](retrieval-quality.md) | the graded comparison with pgvector |
| [`docs/architecture.md`](architecture.md) | how the retrieval engine works |
| [`docs/roadmap.md`](roadmap.md) | what is not there yet |
| [`drivers/README.md`](../drivers/README.md) | the driver, for somebody writing a binding |
| [`compat/README.md`](../compat/README.md) | the parity manifest and the harness that fills it |
