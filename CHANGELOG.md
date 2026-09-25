# Changelog

Every released version, what it was for, and what it is known not to do. The
dates are the dates the release was cut.

The version is the workspace's, which every package carries: the command line,
the shell, the MCP server, the migration tool, the C ABI library, and the Go,
npm, PyPI and Composer wrappers are all one number. `tools/doc-facts/check.mjs`
fails the build when any copy of it disagrees.

## Unreleased

## 1.0.29 — 2026-09-24

**`DELETE` and `UPDATE` take `ORDER BY`, `LIMIT` and `OFFSET`.** They were refused with
`near "LIMIT": syntax error`, in the words of the pinned SQLite build, which is compiled without
`SQLITE_ENABLE_UPDATE_DELETE_LIMIT`. Apple's SQLite and many application builds have the option, and
`DELETE FROM t WHERE ... LIMIT 1000` in a loop is how a large table is trimmed without one large
transaction. The statement changes exactly the rows a `SELECT` with the same `WHERE`, `ORDER BY`,
`LIMIT` and `OFFSET` would return, which is how SQLite defines it. An `ORDER BY` with no `LIMIT` is
refused with SQLite's message, `ORDER BY without LIMIT on DELETE`. The two cases in the feature probe
that test this clause now answer where the pinned build refuses.

**A bare `REINDEX` runs on a database holding a `WITHOUT ROWID` table.** It failed with
`the index has no catalog row`, and so did `REINDEX` naming the table. A `WITHOUT ROWID` table is
its own primary key tree, so there is no separate index to rebuild. Its secondary indexes are still
rebuilt.

**A scalar subquery can be a value in an insert or update of a virtual table, and in a trigger
body.** `INSERT INTO docs(title, body) VALUES ((SELECT title FROM shelf LIMIT 1), 'x')` into an FTS5
table was refused as "a correlated subquery used as a value", although the same insert into an
ordinary table ran. A statement inside a trigger body was refused the same way, and so was an insert
through a view's `INSTEAD OF` trigger. A trigger body's `WHEN EXISTS (...)` guard and its
`WHERE ... IN (SELECT ...)` also failed when the statement that fired the trigger held a subquery of
its own.

**`inillucent run` reports the same status and exit code as `exec`.** A statement the engine has not
built was reported as `syntax` with exit code 1 when it was part of a `run` script, and as
`unsupported` with exit code 3 under `exec`. Exit code 3 now means the same thing under both.

**`inillucent capabilities` has four more rows**: `update_delete_limit`,
`subquery_value_in_a_virtual_table`, `subquery_in_a_trigger_body`, and `load_extension`, which is
`no` because there is no C extension interface to load a library into.

## 0.1.9 — 2026-09-24

**The macOS `.pkg` opens in Installer.app again.** The 0.1.8 package crashed the macOS Installer
with `abort()` in `_ReadFreeList` as soon as it was opened, because the `Bom` inside it had no free
list after its block table. The `Bom` writer now writes the free list and counts blocks in the
header the way Apple's tools do. It also writes the paths tree as a `Bom` from Apple's tools has
it: one leaf directly under the tree, with entries sorted by parent and name. `install.sh` and
Homebrew were not affected.

## 0.1.8 — 2026-09-24

**The file format is 2, and an index costs a write about 40% of what it did.** A leaf's delta area -
the rows written to it since it was last packed - now opens with a directory kept in key order, so a
lookup in it is a binary search and the area is as large as the page's free space rather than 32
rows. A compaction that finds the new rows fit the page's existing column widths splices them in
rather than packing every row again. On the index count sweep, 5,000 inserts into a 100,000 row
table, the cost of each secondary index went from 5.2 µs a row to 2.0, and at ten indexes the
workload went from 0.43x SQLite to 1.28x. A page's checksum also covers its LSN now, which it did
not.

**A lookup past the last key of a leaf that has been written to costs one comparison in its delta
area**, where it cost a binary search of it. Appending at the end of a table does this twice a row,
and so does a lookup of a rowid past the end, so rows appended since the leaf was packed no longer
slow either down; the gate's `txn.large` went from 2.852 ms to 2.708 pinned to the performance cores,
5.0% faster, and a range
probe into such a leaf now reads only the rows inside its bounds.

**This build reads every earlier file**, written by any release from 0.1.1 on, and a file becomes
format 2 at the first checkpoint after this build writes to it. **No earlier release reads a format
2 file**: 0.1.5 and later refuse it by name as `unsupported`, and 0.1.1 to 0.1.3 report it as
malformed. `docs/relational-architecture.md` section 5a has the details.

**A search table can filter inside the search: `FACET` columns.** A column of an `inillucent_search`
table declared `live FACET` is stored and can be constrained in a search - `WHERE docs MATCH ?1 AND
k = 10 AND live = '1'` - and the constraint is compiled into the filter the scan runs under rather
than applied to what the scan answered. Several narrow rather than widen. A facet's value is not
indexed as text, so it changes no ranking of the prose beside it, and it is an ordinary column
otherwise: it comes back from a `SELECT`, and on a query that is not a search the engine evaluates
it itself.

The difference between filtering inside the search and filtering after it is not small, and it is
why the feature exists. The keyword ranking rescores the best `k * 6` hits by where the query's
terms sit inside them, the rescore only ever lowers a score, and a hit below that window keeps its
full score and competes against rescored ones - so which hits are in the window depends on which
rows the scan admitted. Measured on a 400 row corpus, a constraint applied afterwards shared one hit
of the top ten with the same constraint applied inside. Filtering afterwards also returns fewer rows
than the `LIMIT` asked for.

**A table declaring a facet is stored in format 2** and an older build refuses to open it, by name,
naming the release to install. A table declaring none is stored in format 1 exactly as before, so
nothing already written becomes unreadable.

**`inillucent-migrate` uses it, and its `filter.deleted` check was wrong until it did.** The legacy
engine excludes a tombstoned document's chunks inside the posting scan. The copy had no way to
exclude anything before it ranked, so the migration claimed that joining to `document` and dropping
the deleted rows reproduced the legacy default - which it does not, for the reason above. A migrated
database now carries the flag on the search table's own `live` facet, and the check asks both sides
the same question at the same depth instead of comparing an answer ranked over the live chunks against one ranked over every chunk and
filtered afterwards. A second check goes in beside it, `filter.unreachable`, which asks whether
any chunk of a tombstoned document comes back at all.

**`PRAGMA integrity_check` and `PRAGMA quick_check` account for every page of the file.** They used
to read one tree at a time and each index against its table, and neither of those can see a page two
tables both own: each tree is a well formed tree and neither is an index of the other, so a file
where `SELECT count(*) FROM p` answers with `q`'s rows passed both and was reported `ok`. Two new
answers: `page N is used by table p and also by table q`, and `page N is used by table t but the
free map says it is free`, which is the state the first one grows out of at the next allocation.

The two pragmas stop being one pass under two names. `quick_check` reads every tree and accounts for
every page; `integrity_check` does that and then reads each index against its table. The line is at
the index pass because that is the expensive one, which was counted rather than assumed: over a
table of sixty out-of-line values the whole page walk cost 4 page fetches on top of 133, because it
takes an out-of-line value's pages from the reference in the leaf rather than by reading the value.
The pinned SQLite 3.53.4 draws it in the same place.

**The two page leaks this found are fixed.** A rolled-back `CREATE TABLE` or
`CREATE INDEX` kept its tree's root page, and `DROP TABLE` kept every page the table's out-of-line
values sat on. Both were dead space rather than lost data. Abandoning a transaction now gives back
every tree it built, a commit that drops a table frees the pages its out-of-line values used, and a
`REINDEX` gives back the pages of the index it replaced. A leak is now a state no statement
produces, and `PRAGMA integrity_check` reports one as `Page N: never used`, as SQLite does.
`docs/relational-architecture.md` has the details.

**A commit is one append to the log and one sync of it.** It used to be a
checkpoint: the log folded into the file, and a rollback journal holding the
pre-image of every page the fold was about to overwrite, which is six to eight
`fsync` class calls a statement. The fold is deferred now - until the log passes
four mebibytes, until a caller asks for a checkpoint, or until the connection
closes - and it is made safe without a rollback journal by appending the after
image of every page it is about to write to the log first. Measured on the
gate's own counters, an autocommit `UPDATE`'s hundred statements make **100 log
writes, 100 log syncs, no data file syncs and no folds**, where the same
workload used to make 202 syncs and write 3,252 KiB of log for 50 KiB of rows.

A bulk index build writes each page into the file directly rather than through
a buffer pool frame that then has to be evicted. `count(*)` is one addition a
batch rather than one accumulator call a row. The retrieval index builds on
every core, the two legs of a hybrid search run in parallel, and the distance
kernel dispatches once to an explicit AVX2 and FMA version.

Measured against SQLite 3.53.4, four consecutive 30-round runs either side of
the work on one machine, minutes apart:

| | before | after |
|---|---:|---:|
| weighted over the ten families | 3.55x | **4.53x** |
| 95% lower bound | 3.42x | **4.21x** |
| processor time, ratio to SQLite's | 0.635 | **0.400** |
| peak resident set, ratio to SQLite's | 1.140 | **1.100** |
| an autocommit `UPDATE` of one row | 0.13x | **0.94x** |
| an autocommit `INSERT` | 0.47x | **3.04x** |
| `count(*)` over 100,000 rows | 11.41x | **52.16x** |
| `GROUP BY` over the same | 7.89x | **27.51x** |
| `CREATE INDEX` over 100,000 rows | 0.66x | **1.37x** |
| the retrieval index build, 185,078 chunks | 129.7 s | **16.8 s** |
| vector search p50 | 0.934 ms | **0.8462 ms** |

The retrieval score card's ranking verdicts are unchanged - 15 better, 1
equivalent, 1 inconclusive, 0 worse, every correctness gate passing - which is
the condition the parallel build had to meet, because a parallel build's graph
is not the serial one.

**A statement run outside a transaction no longer reads the meta record twice on its way in.**
Under `locking_mode = normal` a statement takes the file lock, asks whether another process has
folded since this connection last held it, and gives the lock back. Both halves of that question
read both meta slots in full: a buffer one page long allocated and zeroed for each slot, a page read
into each, and a crc32 pass over each before a field could be read. At the 32 KiB default page size
that is four 32 KiB allocations, four 32 KiB reads and four crc32 passes over 32 KiB, per
statement, to compare a record that occupies 116 bytes. The record's own bytes are compared
instead, and the two callers share the one answer they both make under the same SHARED lock, which
a writer cannot hold at the same time. Bytes that differ still go to the full read and its
checksum, which is what decides.

A release from SHARED also unlocks one byte range rather than three. RESERVED is taken only on the
way to RESERVED and `take_shared` gives PENDING back before it returns, so two of the six lock and
unlock calls a statement made were unlocking a range the handle did not hold.

Measured through `Connection` on the medium fixture, `SELECT 1` cost **132,884 nanoseconds outside
a transaction and 1,126 inside one**, and costs **10,095 against 727** now. The two readings were
taken minutes apart on a box with two other agents working, so the ratio going from 118 to 13.9 is
the claim and the nanoseconds are not.

**No published figure moved, which is why this survived.** The scorecard and the performance
contract are measured with `inillucent-fullgate`, which drives the engine's own `plan`, `prepare`
and `pipeline` calls and never opens a connection. Nothing that grades this engine paid the cost or
would have noticed it moving. `inillucent-prepareperf` prints both columns, and
`crates/inillucent/tests/budget.rs` now counts what a statement outside a transaction reads so the
cost cannot come back unnoticed.

**An index a build cannot read is refused by name rather than answered with no rows.** The FTS5
index layout changed in 0.1.2, and 0.1.1 reads a file a later build wrote almost perfectly: the
tables, the `WITHOUT ROWID` entries, a blob stored over a page, the row that exists only in the log,
`SELECT count(*) FROM note_fts` as 5 and `SELECT rowid, title FROM note_fts` as all five rows. The
one thing it gets wrong is `WHERE note_fts MATCH 'segment'`, which comes back as no rows at all -
because it read the new doclist blob as a page number, found no such page, and a term with no
doclist is a term in no documents. That is the worst answer a compatibility break can give: an empty
result set is a legitimate answer to a search, so an application has nothing to tell it apart from
"there are no matching documents".

0.1.1 is published and its answer can never be fixed. What changes is the next one. An FTS5 index
written from this release on carries a layout record - one `%_data` row holding a magic, the layout
number and the release that wrote it - and a reader that meets a layout it has not got refuses with
the status `unsupported`, naming both, on `MATCH`, on any write, and on `fts5vocab`. The record is
stamped at `CREATE VIRTUAL TABLE` and by `rebuild` and `delete-all`, the two places the whole index
is written from scratch, and deliberately not by an ordinary insert: a file 0.1.2 through 0.1.7 wrote
has no record and may hold rows in both layouts at once, so a record stamped on the next write would
claim something the file cannot support. A missing record means "some layout up to and including
this build's" and is read exactly as it was, so no existing database changes behaviour.

An `inillucent_search` table's `%_config` gains a `writer` row beside the format number it already
carried, and its refusal now carries `unsupported` and names that release. The format was also
checked only in `begin`, which is the start of a write transaction, so an ordinary `SELECT` against a
table a later build wrote was answered out of a store whose format had never been checked; the read
path asks now. The database file's own format version already answered this way and is what the two
were made to match.

Two quieter instances of the same defect went with it. A `%_idx` row naming a `%_data` row that is
not there, or holding a third column that is neither a doclist nor a page number, resolved to "no
doclist", and every reader but `integrity-check` read that as "this term is in no documents" - so a
search over an index full of documents answered nothing, with no error anywhere. `term_row` was
worse: it staged the unreadable doclist as an empty one and the flush wrote it back, so a write to
the table destroyed the postings it could not read. Both refuse now.

`docs/relational-architecture.md` §5a states the promise for all three layouts in one table.
`crates/inillucent-compat/tests/format_refusal.rs` manufactures a record from a build that does not
exist and checks each refusal, including the command line's exit code.

**Every published release can search a graph this release wrote, and nothing was asking.**
`tests/interop/verify.sql` asks an `inillucent_search` table for its rows and its content, which is a
read of `%_content`; it never asked it to search, so no term query, no ranked query and no
nearest-neighbour query had been run by an older binary against a graph a newer build wrote - the
half of the file SQLite has no equivalent of, and the half a format change is most likely to move.
`tests/interop/retrieval.sql` asks it now, of an `approximate` table over twelve vectors compacted
into a stored generation, and 0.1.1 through 0.1.7 answer every question identically to this build.

**Measured against SQLite 3.53.4 on 2026-09-23**, four consecutive
30-round runs at 100,000 rows in a quiet window, with both engines pinned to the
performance cores:

| | 2026-09-20 | 2026-09-23 |
|---|---:|---:|
| weighted over the ten families | 4.53x | **4.97x** |
| 95% lower bound | 4.21x | **4.62x** |
| the `write` family | 2.12x | **3.04x** |
| 2,000 inserts in one transaction | 0.72x | **1.47x** |
| processor time, ratio to SQLite's | 0.400 | **0.500** |
| peak resident set | 40.76 MiB | **40.76 MiB** |

**The benchmark was measuring the two engines on different cores.** The machine
has 8 performance cores and 16 efficiency cores, and unpinned, Windows ran the
gate process, which is this engine's arm, on the efficiency cores and the SQLite
child on the performance cores. The same run unpinned reads 4.40x. The published
figure pins both; `docs/performance.md` has the evidence, and the gates now pin
themselves.

**The processor ratio got worse** because the plan gained four correlated
subquery workloads, which this engine answers once per outer row and SQLite as a
join: 182 ms of each round against under half a millisecond.

**Known not to do.** Six of the thirty weighted workloads are still slower than
SQLite: compiling `SELECT 1` on every call, a join over an index range, building
an FTS5 index, a range scan of the join's shape, `json_extract`, and an
autocommit `UPDATE` of one row. In that run a correlated subquery was 9,395% to
118,020% slower than SQLite, depending on the shape; the correlated fix under
"Speed without a change in answers" below takes a correlated `EXISTS` over 400
outer rows from 59.69 ms to 0.40 ms, measured on passes the gate did not grade
because the machine was busy. The resident set is 9.5% more than
SQLite's against a bar asking for 5% less, and the allocator is measured out of
the difference: a trivial binary's floor is 3.62 MiB with it and 3.62 MiB
without. At 600,000 rows this engine spends 30% more processor than SQLite on the
whole plan while finishing it 434% faster.

**Eleven correctness fixes the audit before release found, and nine smaller
differences from SQLite beside them.**

- A database file shorter than its own header is refused before a page is read
  out of it, saying how many pages the header claims and how many the file
  holds. It used to open and answer queries.
- A zeroed sector in the middle of the redo log stops recovery there. It used to
  end that segment's scan and carry on into the next one, replaying records
  whose predecessors had never been read.
- Creating a file forces the directory entry that names it on POSIX, and
  `PRAGMA synchronous = FULL` is `F_FULLFSYNC` on macOS. A log segment could be
  fsynced, acknowledged, and lost whole to a power loss.
- `randomblob(n)` answers `n` bytes up to the connection's value bound and
  refuses above it by name. It was silently clamped to 1,000,000, so
  `length(randomblob(100000000))` answered `1000000`.
- `CREATE TABLE` and `ALTER TABLE ... ADD COLUMN` are charged against
  `SQLITE_LIMIT_COLUMN`. A 2,100 column table used to be accepted.
- A recursive CTE has no pass limit. The guard at a million passes refused a
  series generator past a million rows, which is an ordinary idiom; what stops a
  recursion that settles neither way is the request budget, charged per row.
- A reader waits for a busy file for as long as its own `PRAGMA busy_timeout`
  says. It waited five seconds whatever the pragma was set to.
- A CSV that is not UTF-8 is refused with the byte that does not decode and its
  offset, instead of `cannot open "<path>"`.
- `PRAGMA aux.user_version` is about the attached database it names. Every
  pragma that is about a file answered about `main`.
- `--readonly` admits the `run` verb and the shell refuses each write inside it,
  so a read only agent can reach the dot commands.
- `run` exits 1 on a statement the shell refused, and `inillucent_run` over MCP
  answers `"isError": true`. Both used to report success.
- A blank line no longer ends an MCP session.
- The `CHECK` named in a refusal is the one that failed. Two unnamed `CHECK`s on
  one table always reported the first.
- `ESCAPE ''` is refused as SQLite refuses it, and an escape character of more
  than one byte is the whole character rather than its first byte.
- `date('2024-1-1')` and `date(' 2024-01-01')` are NULL, as they are in SQLite;
  the field widths are exact and only trailing whitespace is skipped.
- `printf('%.20f', 1.0/3)` answers sixteen significant digits and fills the rest
  with zeros, which is what SQLite prints; `!` raises it to twenty.
- An empty CSV field that is not NULL is written as two quotes, so an exported
  blob is no longer indistinguishable from a NULL.
- A path a confined process may not reach is reported as `invalid_state` rather
  than as a syntax error.

### The release blockers

The review before release named fifteen defects that had to be fixed before the
next release. Fourteen are fixed, each with a test that fails without the fix.

- **`inillucent integrity-check` answered `"ok": true` and exit 0 on a corrupt
  database.** It listed the pragma's rows without reading them. It now returns
  the status `corrupt` for any answer that is not exactly `ok`, including no
  rows at all.
- **`--params-file` read any file on the machine**, past `--root` and
  `--readonly`, and `query` and `exec` both take it over MCP. The path is
  confined now, `-` is refused on a confined server, and the file is capped at
  one mebibyte.
- **One request could stop the MCP server.** A line of 120,000 `[` overflowed
  the JSON parser's stack. Nesting is bounded at 1,000 levels, the server
  answers such a request with `-32700 nested more than 1000 deep`, and it
  answers the next request normally.
- **A free map chain that loops made `Database::open` run forever**, from every
  command including `integrity-check`. The open now refuses in under a second
  and names the chain.
- **`migrate --kind sqlite` did none of the checks it documents.** A database
  whose only content was an FTS5 table migrated to an empty file and reported
  success. The verb now runs the same verified migration as `inillucent-migrate`,
  and it carries `application_id` and `user_version` across.
- **`PRAGMA cache_size = -1000000000` grew the process to 3.3 GB.** The cache
  has a hard maximum now, `INILLUCENT_LIMIT_CACHE_SIZE`, and a request above it
  is refused in 74 ms.
- **`substr`, `trim`, `ltrim` and `rtrim` replaced bytes that are not valid
  UTF-8** with U+FFFD, and were quadratic in the length. They borrow the bytes
  now and follow SQLite's rule for a character: `substr(x'fffe80',1,2)` is
  `x'fffe80'`, as in SQLite.
- **Recovery dropped log records and counted them as applied.** A dropped record
  is now counted, reported by the driver and printed by the command line.
- **The HNSW and BM25 readers sized allocations from a count in the file**, so a
  damaged file could stop the process. The first fuzzing campaign found one of
  these, reachable from an ordinary `SELECT`.
- **Three C ABI entry points read a caller's pointer before checking it**:
  `inillucent_txn_execute`, `inillucent_txn_commit` and
  `inillucent_clear_bindings`. A freed handle is now answered from a table of
  live handles and is never read.
- **An outer `ORDER BY ... LIMIT` over a recursive CTE applied the limit
  before sorting**, so it returned the first rows generated rather than the
  first rows in order.
- **A vector query failed as soon as its HNSW index existed.** With an index,
  the literal `'[1,0,0]'` was read as seven raw bytes and refused against a
  three dimension index. One vector parser now serves both plans.
- **`dump` left out every row of a table whose name or a column name is a
  reserved word**, at exit 0, and dropped a column named `""`.
- **A blob came out of `--output json` as the text `x'00ff'`**, typed `text`, so
  bytes could be written and not read back by the Node, Go and PHP wrappers or
  the Python subprocess path. A blob is now `{"blob":"<hex>"}` in both
  directions, the column type says `blob`, and all four wrappers decode it.

The fifteenth, how an integer above 2^53 is carried in JSON, is not done. The
wrappers disagree today: given 9007199254740993, Node answers 9007199254740992,
and Python and PHP answer 9007199254740993. Changing the format would change
what the correct callers receive, so it has its own ticket.

### Correctness fixes after the review

Each of these was found by comparing against the pinned SQLite 3.53.4 or by a
test that reopens the file, and each has a test that fails without the fix.

**Rows that were lost or changed.**

- `ALTER TABLE t DROP COLUMN b` on a table `(a, b, c, d)` left `c` holding
  `b`'s values and `d` holding `c`'s, and it survived a reopen. Dropping the last
  column was correct, which is why nothing caught it.
- A rolled-back `DROP TABLE` followed by `CREATE TABLE` lost every row of the
  dropped table, durably, and `PRAGMA integrity_check` said `ok`. A dropped
  tree's pages are now freed when the transaction commits, not while the
  statement runs.
- A rolled-back `ALTER TABLE` left the connection reading and writing a tree
  the catalog no longer named, for the rest of the connection's life.
- An index built on pages that a `DROP` or an `ALTER TABLE ADD COLUMN` had just
  freed could come back damaged after a reopen, because redo replayed what those
  pages held before.
- `ALTER TABLE ADD COLUMN` and `DROP COLUMN` on an attached database failed with
  an I/O error, and altered the table in `main` when one of the same name was
  there. Every `ALTER` on a `TEMP` table was refused. All three work now.
- Below the default 32 KiB page size, an ordinary `INSERT` could answer
  `SQLITE_CORRUPT` or lose rows: two hundred FTS5 documents at a 4,096 byte
  page refused on row 42, and the third `CREATE TABLE` in a 512 byte database
  refused. Every page size the engine accepts now writes and reads back.
- Opening a database whose header was one checkpoint behind cut live pages off
  the end of the file, and every open after that failed. The trim now happens
  only after the open has read the schema, so an open that refuses leaves the
  file as it found it.

**Answers that differed from SQLite.**

- `SELECT count(*) AS n FROM t HAVING n > 0` was a syntax error. A `HAVING`
  without a `GROUP BY` filters the one group an aggregate makes, and the shapes
  SQLite refuses are refused in its words.
- `SELECT 1 UNION ALL SELECT count(*) FROM t` was refused, and with a
  `GROUP BY` on the later arm it returned one blank row per group. An aggregate
  or window function in any arm of a compound select now answers.
- Each `BETWEEN` bound is compared with its own operand's affinity and
  collation, and a `COLLATE` inside an operand, as in `'B' = 'b' || '' COLLATE
  NOCASE`, reaches the comparison around it.
- A `COLLATE` inside an aggregate or window call, as in `max(s COLLATE NOCASE)`,
  and each `CASE WHEN` decide their own comparison.
- An index seek converts its key with the comparison's affinity rather than the
  column's declared type, so a join into an untyped column or one of another
  affinity finds the rows SQLite finds, and a `NULL` key matches nothing.
- A seek key that is a `CAST` or a built in call, as in `id = CAST('8' AS
  INTEGER)` or `k = abs(-4)`, is evaluated rather than refused.

**Command line and migration.**

- `export --out <file>`, `.once` and `.output` wrote an empty file, answered
  `"ok": true`, and returned the rows in the response instead, including inside
  a script run by `run` over MCP. The rows go to the file now.
- Migrating a SQLite database with a `VIRTUAL` generated column failed its
  digest check and deleted a correct result. The digest compares the columns
  the source stores, a new `columns.<table>` check compares the declared list,
  and a digest failure now names the rows that differ.

**Speed without a change in answers.**

- A whole table `DELETE` below the default page size was quadratic: 8,000 rows
  took 1,301 ms and take 17.5 now. A `DELETE` visits the table and each index
  in that tree's own order, which makes the 32 KiB delete about 10 times
  cheaper a row.
- A correlated subquery is evaluated only for rows the query's cheaper
  conditions keep, and it no longer builds and frees a 3.2 MB slot
  array on every execution. `correlated.exists` made 45,414 page
  faults an execution and makes none, and the full gate plan now takes about
  4,200 page faults a round against SQLite's 11,638. Over 400 outer rows a
  correlated `EXISTS` went from 59.69 ms to 0.40 ms and a correlated `IN` from
  118.19 ms to 0.92, measured on passes the gate did not grade because the
  machine was busy.
- `LeafRef::column` reads its directory entry once, and a probe key whose
  affinity changes nothing is no longer copied: `join.range` went from 28.4 ms
  a round to 26.1.
- A statement that uses `%`, `/`, `||` or a bitwise operator is kept and run
  again from its compiled form. It used to be compiled again on every run,
  because those operators read the connection's settings through the same path
  as `changes()` and `random()`. A kept statement now records the settings it
  was built under and is rebuilt only when they change.

### The test and performance review

**A retrieval index holds a fifth less of itself in memory.** The BM25 postings
were a `HashMap<String, Vec<Posting>>` beside a second `Vec<String>` of the same
terms in sorted order, so each of 1.7 million terms was charged for three times:
a string in the map and a string in the list, a `Vec` header and its own heap
block however few postings it had, and a hash map slot with its stored hash.
They are flat arrays now - one byte array for the terms, one array for the
postings, a start per term, and a binary search instead of a hash - with an
overflow map that takes appends and is folded back in once it holds an eighth of
the postings. And the chunk text is left in `store.bin` and read a range at a
time, the way `vectors.bin` already was. Measured on a 185,078 chunk index, each
figure taken twice: the postings went from 306.2 MiB resident to 209.4 for
211.1 MiB on disk, and the whole index from 541.3 MiB to 444.3. **Neither
changes the file format**, so an index written by any build opens under this one
and the other way round.

**`NOT INDEXED` reaches the planner.** The clause parsed, an `INDEXED BY` was
checked for a name that exists, and then the hint was dropped: nothing carried it
past the binder, so both were accepted and ignored. `SELECT count(*) FROM h NOT
INDEXED WHERE a = 3 AND b = 100` on a 600 row table with two indexes planned as
`SCAN h` in SQLite 3.53.4 and as `SEARCH h USING INDEX h_b (b=?)` here. It
mattered beyond the plan: `inillucent integrity-check` reads every table
`SELECT * FROM "t" NOT INDEXED` to build its digest, on the argument that the
clause is what makes the digest a fact about the rows - and a table whose index
disagreed with it was being digested through the index.

**`INDEXED BY` forces the index it names.** It used to be checked for
a name that exists and then ignored. The planner now offers the named index and
nothing else, walks it whole when nothing seeks it, and refuses a statement the
index cannot answer with `no query solution`, as SQLite does. `UPDATE` and
`DELETE` obey it on their target, and `INDEXED BY` or `NOT INDEXED` on an
`UPDATE` or `DELETE` inside a trigger body is refused in SQLite's words. Two
planner defects went with it: a partial or expression index was never usable on
a `FROM` term after the first, and `SELECT count(*) FROM s CROSS JOIN h WHERE
h.b > 595` was refused with exit code 3.

**`printf` and every printed double use SQLite's own digits.** The
sixteen digit cap reached `%f` alone at first, so `printf('%.20g',
3.14159265358979)` was `3.1415926535897900074` here and `3.14159265358979` there.
Capping `%e` and `%g` took the disagreements from 222 of 675 generated statements
to 7, all in the last digit of a double in the exponent tail. Those 7 are closed
by printing through a transcription of SQLite's `sqlite3FpDecode`, whose last
digit is not always the correctly rounded one: `1.1304293785495057e251` printed
`...058e+251` here and prints `...057e+251` now, as SQLite does. The `!` and `,`
flags follow SQLite too: `printf('%!.25f', 0.1)` is `0.1000000000000000056` and
`printf('%,.10g', 1234567.0)` is `1,234,567`. The test population grew from 224
values to 1,024 and every statement must now match the pinned shell exactly.

**`.show` reports what was set.** `explain`, `stats` and `output` were written
into its format strings as `auto`, `off` and `stdout`, so `.explain on` then
`.show` answered `auto`, and a shell whose rows were going into a file said they
were going to the terminal.

**A sort that does not fit in memory spills to a run file and merges it back**,
where it used to hold every surviving row and be killed by the operating system.
The sort also encodes each row's key once and compares byte strings, instead of
dispatching on the value's type for every one of the `n log n` comparisons.

**`NOCASE` stops at a NUL, as SQLite's does.** SQLite's `NOCASE`
ends the comparison at a NUL both values hold at the same position and then
compares the byte lengths. This engine used to read every byte. The difference is
reachable only through `CAST(x'..' AS TEXT)` or a bound parameter, because no SQL
string literal can carry a NUL. The comparison, the index key and the `CREATE
INDEX` sort all follow SQLite's rule now. The same change found that a walk of a
`BINARY` index was taken as the order for `ORDER BY`, `GROUP BY` and `DISTINCT`
under `NOCASE`: with rows `b A a B c`, `GROUP BY x COLLATE NOCASE` answered five
groups where SQLite answers three. It answers three now.

**And the suite grew where it could not fail.** Sixteen fuzz targets that had
never been run are run by `tools/run-fuzz.ps1`, with a row per target in
`tests/fuzz-history.tsv`; the first campaign found an allocation sized from an
unvalidated `u32` in the HNSW reader, reachable from an ordinary `SELECT`. The
nightly tier's ledger is checked for freshness, so a scheduled task that stops
running is a failure rather than a silence. `docs/repository.md`'s coverage table
has a floor per crate and a test that reads it. And `semantics.rs`'s 235 probed
constructs are eight parallel groups rather than one serial test, with a guard
that every category belongs to a group - which found seven `fts5` cases being
graded by nothing.

## 0.1.7 — 2026-09-19

**`packaging/install.sh` had been unrunnable for three releases, and the reason it
was the one script nobody noticed is that it was the one script git left alone.**
`.gitattributes` converts every `.sh` in the repository to LF, and git skips a
file that already holds a carriage return - `install.sh` holds a literal one
inside a `tr -d` argument, so the conversion passed over it and it shipped with
CRLF. `sh` on Debian and Ubuntu is dash, which reads the carriage return as part
of the command and dies on the first line with `set: Illegal option -`. Anybody
who ran the `curl | sh` line off inillucent.com got that. The site route now
refuses to publish a shell script that does not parse or that holds a carriage
return, so this cannot ship again without the release stopping.

**Composer read the wrong commit, because a registry pins a version's commit the
first time it sees the tag and never moves it.** Packagist is a route of
`ship.ps1` now rather than something a person remembers, and it runs after the
mirror and after the GitHub release for that reason. The same rule is why
inillucent's Go module at v0.1.5 and its Composer package at v0.1.6 name the
previous release's source and cannot be corrected.

The mirror route pushes. It had never pushed anything, and `gh release create`
against a tag that does not exist makes one at the repository's current HEAD, so
a registry reading the mirror cached whatever was there. `-Only site,pypi` works
when the script is launched with `-File`. The straggler scan stops reporting a
dependency's version as a straggler.

`AGENTS.md` carries what three releases taught, at the top where it is read
before the first command rather than after the first mistake.

## 0.1.6 — 2026-09-19

**The first real release run found four places the plan and the run disagreed,
and this version is what they cost.** `ship.ps1 -WhatIf` printed a plan that
every route would follow, and then two routes could never have run at all: one
named a parameter the function does not take, and one was unreachable. A
`-WhatIf` that prints a route the run cannot take is worse than no plan, because
it is read as evidence.

**The mirror is pushed before the GitHub release is created against its tag.**
proxy.golang.org and Packagist both pin a version's commit the first time they
see the tag, and `gh release create` against a tag the mirror does not have makes
one at that repository's HEAD. Ordering the two routes is the whole fix and it
cannot be retried, which is why it is written down in `AGENTS.md` as well as
fixed here.

The PyPI route builds a wheel for every platform rather than only the one the
release was cut on - 0.1.5 published a single wheel, so `pip install inillucent`
worked on Windows and found nothing anywhere else. The Windows build imports the
MSVC environment itself: `onig_sys` compiles oniguruma with `cl.exe`, and a shell
that is not a Developer PowerShell has no `INCLUDE`, so the build stopped on
`stddef.h`. The crates.io route needs no prompt. The version phase commits every
file it wrote, rather than leaving some of them for the next run to trip on. The
Go wrapper's pinned release is a version carrier, so it moves with the other
seven. The release deploys inillucent.com rather than printing an instruction to
deploy it.

## 0.1.5 — 2026-09-19

**One command releases inillucent, and says what reached where.**
`packaging/ship.ps1` publishes to twelve destinations - five build targets, the
Linux packages, the signature over `SHA256SUMS`, the tag, the GitHub release, the
public mirror, inillucent.com, and the crates.io, npm, PyPI, Go module and
Homebrew routes - in five phases, and verifies each by asking the destination
what it serves rather than by reading an exit code. A route with no credential
is a skip carrying the sentence that fixes it, because a script that refuses
without all twelve is one nobody runs. Every credential is sealed under
`%LOCALAPPDATA%\inillucent\signing` and unsealed to a RAM disk for the run.

**The macOS release is built on the Windows box, with no Mac involved.** zig
cross-links the Mach-O and `rcodesign` replaces `lipo`, `codesign`, `productsign`,
`notarytool` and `stapler`; Apple's notary is an HTTPS API. The `.pkg` Apple
refused had three things wrong with it, all of them recorded in
`tasks/task-1995-macos-releases-without-a-mac-tdd.md`.

**Two lost writes, both where a file is handed back.** A rollback journal put
back after the pages it describes had already been superseded, and a checkpoint
writing a file it no longer held the lock on. The crash record moved eight cut
points from the new state to the old one, which is the evidence that the fix
changed what the engine does under power loss rather than only what it reports.

**Five things a statement stopped doing on its way out of the file**,
and a tombstoned document counts as one document rather than one per chunk.
An extent reference says what its value reads back as.

The part eight review closed thirteen defects across `inillucent-cli`, the bench
crate and the schema function authorizer, and the suite's own honesty work landed
with it: the PHP round trip ran for the first time, a Python wrapper that
resolved and would not start was fixed, the expected absences come from
`tests/selection.toml` rather than from a list of four, and a run that had every
prerequisite says so. Coverage was measured rather than estimated.

**Known not to do.** The Go module at this version names the previous release's
source, permanently: proxy.golang.org saw the tag before the mirror was pushed
and a registry does not re-read a version it has cached. PyPI carries one wheel
rather than four, so `pip install inillucent` finds nothing on macOS or Linux at
this version; 0.1.6 is the first with all four.

## 0.1.4 — 2026-09-17

**A statement no longer pays for the log's housekeeping on its way out, which
is worth a factor of two to four on every autocommit write.** `locking_mode =
normal` became the default in this version, and under it a connection
checkpoints and releases the file after every statement that wrote. A
checkpoint also makes the catalog's statistics honest, rolls a log segment,
writes a checkpoint record and deletes the segments it has made redundant -
work that belongs to a checkpoint somebody asked for, and that was running once
a statement. Nothing released carries the cost: 0.1.3 shipped with
`locking_mode = exclusive` as the default, under which a connection checkpoints
at close.

Measured on the medium gate against pinned SQLite 3.53.4, the same fixture and
the same disk: an autocommit insert 27.4 ms before and 8.2 ms after, an
autocommit update 22.6 ms and 8.6 ms, `schema.index` 61.5 ms and 54.2 ms. Two
thousand autocommit inserts through `inillucent-shell` took 73.4 s and now take
28.6 s, and the per-statement checkpoint is flat where it used to climb from
20 ms to 38 ms as the run went on.

What is left of an autocommit statement, timed: about 4 ms is the fold - five or
six pages written, three `fsync`s and a rollback journal created and deleted -
and about 4 ms is the statement's own execution and commit sync.

**What this costs, so it is not a surprise.** Between reclamations the log
keeps the segments that would have been deleted, up to four mebibytes - the
same bar SQLite draws at `SQLITE_DEFAULT_WAL_AUTOCHECKPOINT`, which is 1,000
pages of its 4 KiB default. **Nothing in this engine checkpoints when a
connection closes**, so that is also what a process leaves on disk when it
exits without asking for one; it was previously near zero only because the
per-statement checkpoint reclaimed every statement. Measured: 4,000 autocommit
statements against a 320 KB database leave 3.3 MB of log in one segment. A
process killed without closing leaves that same amount for the next open to
replay, where it used to leave at most one statement's worth, so the reopen
after a crash reads more and reports it. `PRAGMA wal_checkpoint`, the
`checkpoint` verb and `Database::checkpoint` all reclaim on demand.

**`Wal::retire_segments_below` was quadratic and unbounded.** It walked every
sequence number from 1, opening a file per sequence to read its header, so
every call re-asked about every segment an earlier call had already deleted.
Two thousand autocommit inserts opened 1.5 million segment headers, 1.49
million of them for a file that is not there. The sequence number is read back
from the meta record, so the cost survived a close: the same database reopened
with 2,030 segments behind it spent 19.5 ms a statement, more than half the
checkpoint, deleting nothing. It now starts at the lowest sequence that might
still be there, and deletes exactly the same files.

**`Wal::sequence_containing` answers for the segment being appended to without
reading its header off disk**, which is the answer almost every call gets.

**The staleness check a statement makes on its way in no longer opens a file.**
Before every statement a connection asks whether another process has written,
and the log half of that question cost a path lookup and a file open: it went
through `tail_on_disk`, which takes a path and walks forward from a sequence,
calling `access` at each one and opening the file to read its header. Its own
doc comment prices the check at "one `file_size` per lock acquisition", and
`Wal::tail_of_open_segment` is what makes that true - the log already holds
that segment open. Measured at 3.2 ms of an 8.8 ms autocommit statement, where
the meta-record half of the same check cost 0.03 ms. Only the open segment has
to be asked: a checkpoint is the only thing that rolls a segment, and it moves
the meta record's generation before it releases the file lock, so the
generation is seen first.

Statistics are written by a checkpoint somebody asked for - a close, `PRAGMA
wal_checkpoint`, `VACUUM`, a backup, an integrity check, a journal-mode switch -
rather than by every statement. Between those, a tree's recorded row and leaf
counts are the shape as of the last one. They were already an estimate rather
than an invariant: `PagedTree::attach` derives the leftmost leaf from the file
instead of trusting the recorded copy, and `PagedTree::check` compares the
sibling chain against the interior levels rather than against the recorded leaf
count.

**`embed(TEXT)` is `direct_only`, which is a behaviour change to a shipped
function.** A schema may no longer name it: a `CHECK` constraint, an index
expression, a generated column, a `DEFAULT`, a view or a trigger that calls
`embed` is refused with "may only be used from top-level SQL". A statement may
call it exactly as before.

It was registered with `FunctionFlags { deterministic: true, ..Default::default() }`,
and the `Default` derive is every flag false - so the flag said a schema may
name it while the function's own doc comment said "It stays `direct_only`: a
function that loads a 275 MB model has no business being called out of a `CHECK`
constraint or an index expression". Nothing published promised the old
behaviour: `PRAGMA function_list` does not report the bit, and no document said
a schema could call it. A `CREATE INDEX i ON t (embed(body))` would load the
model once per row of the table, inside the statement that creates the index.

`UserFunction::external` is the constructor a registrant should use for this;
`FunctionFlags::default()` exists for `builtin()`'s sake and is not what
anything registered from outside wants.

**And the binder consults it**, which it did not before this.
`Registry::authorize_function` had no caller anywhere in the workspace, so
`direct_only`, `innocuous` and `PRAGMA trusted_schema` were a policy with a
passing unit test and no effect on the engine: a `CHECK`, an index expression,
a generated column, a `DEFAULT`, a partial-index predicate, a view and a trigger
could each name any registered function whatever its flags said.

The rule itself moved down to `inillucent_sql::function::schema_refusal`, below
the binder that enforces it, and `Registry::authorize_function` calls that same
function - so an application asking the registry directly and a statement the
binder compiles cannot answer differently. `FunctionFlags` and `CallSite` moved
with it and are re-exported from `inillucent_ext::registry`, so every path an
application already writes resolves to the same type it did.

The binder carries a call site that is set at seven places: a `DEFAULT`, a
`CHECK`, a generated column's expression, an index expression, a partial-index
predicate, a view's body and a trigger's body. Two of those paths - the nested
binders in `Binder::bind_alone` and `dml.rs::bind_schema_expr` - also dropped
the connection's registered functions and collations on the way, so a schema
expression naming a registered function did not resolve at all; they inherit
them now.

**`CREATE INDEX` on an expression is refused when the index is created**, not on
the next write of the table. Such an index is filled by a `SELECT` the engine
builds out of the index's own expression, and a `SELECT` is a statement - so
that one query was the place a schema expression reached the machine with a
statement's permissions, and `CREATE INDEX i ON t (embed(body))` loaded the model
once per row before anything was refused.

**`PRAGMA trusted_schema` reports and sets the connection's own policy.** It
used to answer a constant 0 from the fixed-answer table while the connection's
policy said the opposite, which was harmless only for as long as nothing read
either one. The library's default is on, which is SQLite's; turning it off
refuses every registered function a schema names unless the registration said
`innocuous`.

**And `inillucent-shell` turns it off at startup**, which is what the reference's
shell does and why `.dbconfig` on the reference prints `trusted_schema off` on a
connection whose library default was on. A shell is a program that opens files it
did not write, which is the case the flag exists for. `.dbconfig trusted_schema`
reads and writes that setting now instead of printing a constant beside it.
`semantics.rs`'s `shell.dbconfig` case grades the whole listing against the
pinned SQLite and is what caught the difference.

**`PRAGMA defensive` refuses a write to a module's shadow table**, which is the
same defect in the same file: `Registry::authorize_shadow_write` was the whole of
that promise, it read a `Policy::defensive` nothing ever set, and nothing called
it. The shell turns defensive on for every connection it opens, so what
`.dbconfig defensive on` actually refused was `PRAGMA journal_mode = OFF` and
nothing else. Which names are shadow tables is derived from the roots each module
was connected with rather than from the spelling, so `docs_backup` is still an
ordinary table beside `docs_data`.

`Registry::authorize_extension` still has no caller and that is not the same
defect: nothing in this engine loads a shared library. `load_extension(path)`
refuses every path and so does the shell's `.load`, and those two refusals are
what `crates/inillucent-compat/tests/schema_function_policy.rs` checks, because
they are the guarantee a caller has. There is no `authorize_module` at all.

### The census: sixty-one places that could report success having checked nothing

The rest of this release is this review's answer to one question - how
many places in this repository can print a green result without having checked
anything - and the answer was 61, against a page that named 5.

- **One skip helper.** `inillucent_base::testing::skipping` prints the one
  marker and panics under `INILLUCENT_STRICT`, and it is below every crate, so
  the three production crates that could not reach the test harness no longer
  print their own sentence. Twenty-four raw prints across nine files are gone,
  including a local `announce_skip` in `tests/differential.rs` that shadowed the
  library one and let nine of that file's ten tests pass on a machine with no
  SQLite oracle.
- **The map and the suites have to agree.** `tests/selection.toml` gained a
  prerequisite on seventeen rows and lost one from six that could not skip, and
  `selection.rs` now fails in both directions: a suite that can skip without a
  declared prerequisite, and a declared prerequisite whose suite cannot skip.
- **An instrument can answer about an older tree, which looks exactly like an
  answer about this one.** `tools/doc-facts/check.mjs` measured the published
  test count with `target/release/inillucent-testrun.exe`, because its binary
  lookup prefers a release build - while both validate scripts build the runner
  into `target/debug` and run it from there. The release copy on the machine
  that cut this was four days old and reported 3,010 tests where the current one
  reports 3,016. The check now takes the newer of the two and refuses one older
  than any source it was built from, naming the file and the rebuild command.
- **A contract file is only as good as the lines its parser reads.**
  `tests/selection.toml` was carrying a bare array and a repeated key, left by an
  edit that removed half a row. `toml_lite` drops the first and keeps the last of
  the second, so the file parsed, 188 rows came back and every check over it
  passed. `every_line_of_the_map_is_one_the_parser_reads` compares the file to
  itself rather than through the parser, because a line the parser drops is a
  line no other check looks at.
- **The checks outside cargo fail when they cannot check.**
  `tools/doc-facts/check.mjs` treats an instrument that cannot answer as a
  failure rather than a skip - ten of its sixteen facts were skipping on any
  fresh clone - refuses a feature-probe result recorded at another commit, and
  runs as a stage of both validate scripts and of `packaging/release.sh`.

### End to end

The layer a user touches was the layer nothing exercised. Eighteen of the thirty
command line verbs had never been passed to a spawned binary, no test had seen
exit code 3 from outside a process, no MCP tool had been called by name over
real pipes, thirty of the seventy-one dot commands appeared in no test file, and
no durability test had ever killed a real writer.

- `cli_commands.rs`: one subprocess test per verb, asserting a named field of
  parsed `--output json` or a specific exit code, and one that drives a built
  binary to exit code 3.
- `mcp_wire.rs`: one handshake, twenty-eight `tools/call` requests, one process.
- `dot_commands.rs`: every dispatched name through a real shell, and the
  63-of-65 claim held to the pinned `sqlite3`'s own list in both directions.
- `process_crash.rs`: the operating system ends a real writer twenty times and
  the file is reopened from the parent.
- `crates/inillucent-migrate/tests/cli.rs`: the migration tool as a process,
  which it had never been.
- Round trips for the npm and PHP wrappers, which had only ever read their own
  source as text, and the Python conformance runner, which `drivers/README.md`
  calls the proof that a second language can implement the driver and which was
  run by nothing.

**Two defects those tests found**, both in verbs nothing had spawned:
`inillucent --db app.rdb shell` ignored `--db` and opened `:memory:`, and
`inillucent restore <file>` accepted a backup that is not there, exited 0 and
created an empty database.

### Known not to do

- A `CHECK`, a `DEFAULT`, a generated column or an index expression that names a
  function a schema may not name is refused when the statement that reads it is
  bound, not when the schema object is created. SQLite refuses the `CREATE`
  itself. `CREATE INDEX` is the exception and is refused at creation, because
  that is the one form this engine binds while it builds it.
- The Go wrapper's engine tests did not run on the machine that cut this: Go
  is not installed there, and `winget install --id GoLang.Go -e` downloaded
  1.27.0, verified its hash and ended with `Installer failed with exit code:
  1603`, which is the MSI declining to install without elevation. The `wrappers`
  validate stage names the absent toolchain and the install URL rather than
  passing over it silently. The other three wrappers ran: the npm suite 8 of 8,
  both PHP suites, and the Python conformance runner's 18 cases and 69 steps.
- The retrieval baseline names 19 files under `crates/inillucent-bench` that
  moved without an amendment, so `the_retrieval_baseline_is_unchanged` fails and
  with it the `contracts`, `tests` and `doc-facts` stages of both validate
  scripts. Every other stage of `tools/validate.ps1` passes. The amendment
  belongs to whoever changed those files.

## 0.1.3 — 2026-09-15, published 2026-09-19

**Published.** https://github.com/Black-Rainbow-Labs/Inillucent/releases/tag/v0.1.3
carries eleven assets and inillucent.com serves nine downloads, all naming 0.1.3.
This is the first inillucent release with macOS binaries: `inillucent-0.1.3.pkg`
is signed with a Developer ID and notarised by Apple, and
`inillucent-0.1.3-universal-apple-darwin.tar.gz` holds the same universal
binaries. It is also the first with signed `.deb` and `.rpm` packages, for x86-64
and aarch64, and the first whose `SHA256SUMS` carries a signature anyone can
check: `SHA256SUMS.minisig`, against `packaging/inillucent.pub`.

It was tagged on 2026-09-15 and left unpublished for four days. The GitHub
release stayed a **draft**, which is worse than nothing having happened: `gh
release view` finds a draft, so the release step uploaded every asset into it and
reported success while the release stayed invisible and untagged.
`Publish-GitHubRelease` publishes a draft it uploaded into now, and
`packaging/ship.ps1`'s preflight asks GitHub who it is rather than checking that
`gh` is installed — `gh` had never been logged in on the release machine.

This entry is written after the fact, because the release that cut the tag did
not write one and a hole between 0.1.2 and 0.1.4 is the kind of thing a reader
assumes is a mistake in their checkout.

What is in it is the public Rust surface reduced to one - the facade
is a re-export of the driver rather than a second API over the same engine -
`Connection::begin`, the engine's `lib.rs` from 7,307 lines to 1,279 and
`physical.rs` from 5,708 to 297, the parameter lists that were really types,
`ImportedDatabase`'s 63 fields in six groups behind their own cells, six
roadmap items, and coverage measured at 77.2% of regions.

Seven version pins moved together, because nothing downstream can tell which is
the real one, and `tools/doc-facts/check.mjs` fails the build when any copy
disagrees.

## 0.1.2 — 2026-09-13

**`embed(TEXT)` answers in a published binary.** Every archive up to 0.1.1 was
built without `--features inillucent-cli/embed`, so
`inillucent setup-embeddings all` downloaded 620 MB of ONNX Runtime and weights
and the program that downloaded them then answered `no such function: embed`.
The feature is in the release scripts now; the published Windows and Linux
archives both answer `SELECT length(embed('hello'))` with `3072`, the Linux one
after `setup-embeddings all` on a machine that had never run it.

Cutting it found three defects in the release gate, none of them reachable by
building the workspace:

- Five release checks spoke an MCP handshake the server no longer accepts. It
  enforces the lifecycle now — `initialize` needs `protocolVersion`,
  `capabilities` and `clientInfo`, and every other method answers `-32002` until
  `notifications/initialized` arrives — and `packaging/release.ps1`'s smoke test
  refused to build the archive at all.
- Two of those read the answer through `grep -q`, which stops at its first match
  and closes the pipe; the server's next write then failed and `set -o pipefail`
  reported a pipeline that had answered correctly as failed.
- No release had ever carried an ARM Linux archive, because `rust-toolchain.toml`
  named only the two x86-64 targets.

A fourth was not about the gate: a clean checkout on Windows turned every shell
script into CRLF, because `core.autocrlf` is true and the repository carried no
`.gitattributes`. `*.sh` is pinned to LF.

## 0.1.1 — 2026-09-11

**0.1.0 is withdrawn rather than patched.** Its archives carried `README.md`,
`docs/getting-started.md` and the quickstart skill from before the Go command
was renamed, so all three told a reader to run
`go install .../packages/go/cmd/inillucent@latest` — and `@latest` resolves to a
module where that directory no longer exists. The archive the site handed out
contained an install command that failed. Replacing those archives in place
would have left two different archives both called 0.1.0, so the version was
withdrawn instead; its archives answer 404 and its GitHub release is marked
*withdrawn — use 0.1.1*.

Also in this release, each found by running a command the release ships rather
than the same command from the repository:

- `SHA256SUMS` was written with CRLF, so Linux `awk` kept the carriage return
  and `curl -fsSL .../install.sh | sh` on Ubuntu said Linux had no build and
  then listed the Linux archive on the next line.
- Two one-liners pointed at `raw.githubusercontent.com`, which answered 404.
- `install.sh` used `set -o pipefail` and `${BASH_SOURCE[0]}`, both bash-only,
  against a documented command that pipes into `sh` — which on Debian and Ubuntu
  is dash. The script is POSIX now.
- `release.ps1` could produce an empty `SHA256SUMS` and exit 0, when an inherited
  `PSModulePath` shadowed `Microsoft.PowerShell.Utility` and `Get-FileHash`
  resolved to nothing. It checks for the cmdlets it needs before doing anything.

## 0.1.0 — 2026-09-10, withdrawn

The first release: the command line, the `sqlite3`-shaped shell, the MCP server
and the migration tool, with Windows and Linux archives on inillucent.com and
the Go module published as a tag.

Withdrawn the next day for the reason above. Its archives are removed from the
site and its GitHub release keeps its assets attached, because deleting them
would remove the record of what was published.
