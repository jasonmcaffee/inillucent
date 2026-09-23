# The inillucent testing standard

**What a test in this repository is for, how the suite is organised, how to run
only the part a change can break, and what all of it costs.**

Written on 2026-09-08, with the target/test counts and §6's timings refreshed on
2026-09-13 after task-1911. Every number below was measured on the machine
described in [Timings](#6-timings), by the tools this document describes, and the
commands that produce them are given so they can be taken again.

---

## 1. The seven rules

Everything else here follows from these. They are written as rules because each
one was arrived at by finding a test that broke it.

### 1.1 A test asserts a value, not the absence of a crash

`assert!(result.is_ok())` is not a test. It passes for a function that returns
the wrong answer, and it passes for a function that was deleted and replaced
with `Ok(())`. Every assertion names the value it wants, and a refusal counts as
a value: a case the engine is allowed to decline asserts *which* error code it
declines with.

### 1.2 A test that cannot fail is worse than no test

The suite is full of prerequisites the workspace cannot build — the pinned
SQLite oracle, a C compiler, the ONNX runtime — and every suite that needs one
prints a line and returns success when it is absent. That is correct for a fresh
clone and a trap for anybody reading the green as evidence, so the runner counts
those suites, names them, and `--strict` turns the count into a non-zero exit.

The same rule applies inside a test. A loop that asserts nothing because its
input was empty has not tested anything; assert the input first.

### 1.3 A known difference is recorded as a test that asserts it

When the engine does something wrong and the fix is not this ticket's, the
behaviour is written down as a **test that asserts what happens**, with a
comment naming the defect and a failure message telling a future fixer which
lines to rewrite. `crates/inillucent-compat/tests/semantics.rs` has done this
for some time now, and it is the discipline this repository runs on: a fix that
lands turns the test red, so a fix cannot land unnoticed and a regression cannot
either.

The alternative — deleting the test, or `#[ignore]`ing it — makes the defect
invisible and the suite quietly smaller.

### 1.4 Durability is asserted by a handle that did not write the data

A write that is only in a page pool satisfies every assertion a single
connection can make. Any test claiming that something persists **drops the
`Database` and opens the path again**, so what it reads has been through the
write-ahead log and recovery.

This is not theoretical. The virtual-table defect that prompted this suite was invisible
to every test that did not reopen: the *file* was correct throughout, and only
the live connection was wrong.

### 1.5 A comment may only claim what its test proves

If a test's doc comment says it pins a particular defect, that has to be checked
by **reverting the fix and watching it fail**, not by reading the test and
believing it. This ticket wrote a test whose comment claimed to pin a
savepoint-level bug and which passed perfectly well with that fix reverted; the
comment now says so instead. A test that does not discriminate is still worth
keeping - it pins the behaviour - but a comment that oversells it is worse than
no comment, because the next person will trust it.

### 1.6 The same question, asked two ways

An index the insert path maintains and the delete path forgets answers a
covering query wrongly while every other query about the same row is right. So
the write suites ask each question twice — once in a shape the planner answers
from the table, once in a shape it answers from an index — and a difference
between the two answers is reported as an index that has drifted rather than as
a query difference. `new_engine_writes.rs` is built entirely on this, and
`durability::churn_leaves_the_index_agreeing_with_the_table` is the small
version of it over the public API.

---

### 1.7 A test asserts a count, not a duration

A wall-clock reading is a measurement of the machine as much as of the code, and
on this box the machine is four agent terminals, a training run, and the
runner's own 24-wide pool. Six assertions in three files were decided that way
and every one of them eventually failed on a working engine:

| file | what it asserted | what it read under load |
|---|---|---|
| `crates/inillucent/tests/budget.rs` | one transaction beats 2,000, ≥ 2x | 1.3x |
| the same | re-preparing is cached, ≤ 4x | 6.6x, "the cache is not being consulted" |
| `crates/inillucent-compat/tests/new_engine_vtab_stream.rs` | a bounded series answers in < 500 ms | 636 ms |
| `crates/inillucent-txn/tests/transactions.rs` | a refused writer gave up in < 50 ms | — |

**Widening the number is not the answer.** It has a floor — the ratio reaches
1.0, where it asserts nothing — and it reaches it: that guard went 4x, 2x, 1.3x.
Neither is running the tier alone, which the runner already does and which says
nothing about the rest of the box.

**Ask what the claim counts.** Almost every cost claim is a count in disguise,
and the count reads the same on an idle machine and on a saturated one — which
was checked, at 100% processor load, with byte-identical results:

| the claim | the count |
|---|---|
| batching a commit is cheaper | log writes: 1 against 2,000 |
| the plan cache answers a second prepare | statements compiled: 1 for 200 prepares |
| an index beats a scan | pages fetched: 33 against 2 |
| a `LIMIT` stops the scan below it | whether it returns at all, over a series too long to walk |
| a `busy_timeout` of zero refuses rather than waits | the slot's `timed_out`, with `waited` unmoved |

Where a count genuinely cannot see the defect — a quadratic that does no extra
I/O — a clock is allowed, as a **ceiling** with three orders of magnitude of
headroom and a comment saying what the headroom is against. The worst load
factor measured in this repository is about 50x. A bound that a 50x machine can
cross is a bound the machine decides.

**And an assertion reports its measurement rather than naming a cause.** The
plan-cache guard printed "the cache is not being consulted" while the cache was
fine, and would have sent its reader hunting a defect that did not exist. Print
the two numbers and what was expected of them.

## 2. The shape of the suite

**221 test targets, 3,493 tests, in ten tiers.** A target is one binary
`cargo test` builds; a tier is a band you can ask for by name. Every target is
in exactly one tier, so the tiers partition the suite rather than overlapping
it. (Was 129 targets, 2,336 tests when this document was written; task-1911's
re-point of 36 files onto the shipping engine, its free-map durability fix
and its other roadmap work added 20 targets and 309 tests, mostly to `engine`,
`differential`, `unit` and `durability` — counted fresh against
`tests/selection.toml` and a full run rather than carried forward by hand.
Then the two code reviews: task-1932 and task-1946 took it to 169 and
2,789, task-1946 adding the suites for `ANALYZE` on an open connection,
`VACUUM` on a file system that is not the disk, trigger depth against the
oracle, and the rollback journal's ordering, and deleting 1,284 lines that
nothing called.)

**The target column is the `[[target]]` row count in `tests/selection.toml`, and
`cargo test -p inillucent-compat --test documentation` fails when it is not.**
It said 169 here and 170 in `docs/repository.md` while the map held 181, and its
`engine` and `differential` cells said 53 and 30 against the map's 58 and 31.
Nothing compared any of the three to the map: `tools/doc-facts/check.mjs` held a
written count against what the *runner* last reported, so a document that agreed
with a stale run passed (task-1969, 4.14). The test count per tier is a property
of a run rather than of the map and is not checked here.

**The test counts below were taken from one full run rather than maintained by
hand** (task-2066). Nothing reads them, so they had drifted: `differential` said
334 where the run reported 349, `durability` 220 where it reported 234, and
`unit` 1,430 where it reported 1,446. They are what a `--changed` selection of
225 of the 228 targets reported, plus `numeric_text`'s three, which that
selection predated. Read them as the size of a tier rather than as a number to
check a run against.

| tier | targets | tests | what it is for |
|---|---:|---:|---|
| `smoke` | 1 | 10 | the ten-second answer: a real file opened, written, reopened, read |
| `unit` | 31 | 1,446 | every crate's own `#[cfg(test)]` modules |
| `engine` | 68 | 447 | SQL and storage behaviour over real database files |
| `differential` | 34 | 349 | graded against the pinned SQLite 3.53.4 |
| `durability` | 34 | 234 | crashes, injected faults, corruption and concurrency |
| `e2e` | 36 | 432 | the public surfaces an application binds to, end to end |
| `perf` | 1 | 8 | the cost guards — **runs alone**, see §5 |
| `retrieval` | 7 | 570 | the embedding and retrieval engine, and its graded harness |
| `tooling` | 16 | 147 | the checks that keep the repository's own rules true |
| `nightly` | 3 | 6 | the long forms, run on a schedule rather than on a change |

The map that assigns them is `tests/selection.toml`, and it is data rather than
code so that a person can read the whole arrangement in one file.

### 2.1 Where a new test goes

| what you are testing | where it goes |
|---|---|
| one function, one module | `#[cfg(test)]` in the crate — tier `unit` |
| a construct SQLite also has | `inillucent-compat/tests/`, graded against the oracle — tier `differential` |
| SQL or storage behaviour with no SQLite equivalent | `inillucent-compat/tests/` — tier `engine` |
| what an application does with the public API | `crates/inillucent/tests/` — tier `e2e` |
| **a sequence an application performs, at every configuration** | `crates/inillucent/tests/story_*.rs`, through `scenario!` — tier `e2e`, see §2.2 |
| what survives a crash or an injected fault | `inillucent-compat/tests/`, under the simulator — tier `durability` |
| **what a language binding must answer** | a case in `drivers/conformance/suite.json`, which all five runners read |
| **what an earlier release wrote, or will read** | a fixture in `tests/interop/<version>/` — tier `e2e`, see §2.2 |
| **a defect that escaped, and what holds it now** | a row in `tests/escapes.toml` — tier `tooling` |
| **the same question at a size nobody waits for** | a second target in tier `nightly`, see §2.2 |
| a cost that must not change shape | `crates/inillucent/tests/budget.rs` — tier `perf` |

**The public facade is the newest of these and the one most easily forgotten.**
`crates/inillucent/tests/` did not exist until this suite was written, on the reasoning that
`inillucent::Database` is a re-export of a thoroughly tested engine. The hole in
that reasoning is that an application does not depend on the engine, it depends
on **the name**: a re-export that stops compiling, a type that stops being
public, a method that moves down a layer — none of those are engine defects,
none of them fail an engine test, and every one of them breaks every caller.
The facade was moved from one engine to another and nothing in the suite would
have noticed if it had moved to neither.

---

### 2.2 Stories, the configuration matrix, and the long forms

**Every test above asks about a construct. A story asks about a sequence.** The
distinction is not stylistic: `CREATE TABLE`, `ALTER TABLE ADD COLUMN`, `CREATE
INDEX` and `ANALYZE` each have their own tests and each of those passes, and the
four of them in the order an application's startup performs them corrupted a
table beside the one being migrated. A suite made only of construct tests cannot
find that, because there is nothing wrong with any of the constructs.

**And every test above runs at one configuration.** Until the matrix, every
story in the tree ran at a 32,768 byte page with 4,096 frames and an untouched
journal, because that is what `Database::open` gives — so a defect that needs a
4,096 byte page was invisible to all of them, and one of those was found the day
the matrix was written.

`crates/inillucent-compat/src/matrix.rs` declares the arms and expands them:

```rust
scenario!(a_startup_migration_leaves_every_neighbour_readable);
```

That is one `#[test]` per arm — `default`, `sqlite_page`, `small_pool`,
`truncate_journal`, `persist_journal`, `waiting` — each with its own scratch
directory, its own database and the arm's geometry written into the open. A
story that calls `scenario!` is asking its question six times; a story that
writes a bare `#[test]` is asking it once, and
`tooling::scenarios_run_every_quick_arm` is what notices.

**The long forms live in tier `nightly` as a second target**, because a target
is in exactly one tier and the tiers partition the suite. The pattern is a pair:
`story_ledger_day` issues eight hundred transactions in `e2e`, and
`story_ledger_day_nightly` issues a hundred thousand and replays them through
the pinned SQLite shell; `release_format` reads the newest interop fixture in
`e2e`, and `release_format_history` reads all six and hands a file this build
wrote to every released binary. `pwsh tools/run-nightly.ps1` runs the tier under
`--strict` and appends a row per target to `tests/nightly-history.tsv` — the
date, the commit, the machine, the verdict and the seconds — because a green run
nobody recorded cannot answer "when did this last actually pass".

**The interop fixtures are evidence rather than input.** `tests/interop/0.1.1/`
holds a database written by 0.1.1's own binary, the log segment it left and the
answers it gave; nothing but `pwsh tools/build-interop-fixture.ps1` ever writes
one, and every reader stages a copy first, because opening a database replays
its log and would change the thing being measured. `packaging/ship.ps1` builds
the fixture for each version it publishes.

**`tests/escapes.toml` is the ledger of what got out.** One row per escaped
defect: the ticket, the surface, one sentence about what happened, and the test
that holds it now. A row with `held_by = []` must say why in `open`, and
`tooling::escapes` fails when a `held_by` names a test that does not exist — so
the ledger cannot quietly become a list of tests somebody deleted.

---

## 3. Running only what a change can break

Running everything takes minutes; running the right subset takes seconds. The
mechanism is `inillucent-testrun --changed`.

```sh
# build it once - the feature is explained in §4.3
cargo build -p inillucent-compat --bin inillucent-testrun --features testrun

# what the working tree's changes can break
target/debug/inillucent-testrun --changed

# ...against a particular revision
target/debug/inillucent-testrun --changed origin/main

# see the selection without running it
target/debug/inillucent-testrun --changed --list
```

### 3.1 How it decides

1. **What changed.** `git diff --name-only <rev>` plus `git ls-files --others`,
   so a brand new file counts — a selector that could not see an untracked file
   would run nothing for exactly the change most likely to need a run.
2. **Which packages those paths belong to.** A path inside a member directory
   belongs to that member. A path *outside* every member is looked up in the
   map's `[[path]]` rules, and **a path matching no rule selects everything** —
   the safe answer, and the one that makes a new top-level directory loud rather
   than invisible.
3. **What sits above them.** The seed packages are closed over **reverse**
   dependencies, so a change to `inillucent-base` reaches every crate built on
   it. Development dependencies are edges too: a change to the simulator has to
   re-run the campaigns that inject faults through it.
4. **Which targets cover those packages.** Each row's `covers` list names the
   **top** of the stack that suite drives, not everything underneath it — the
   closure has already walked upward, so a suite that drives the shell declares
   `inillucent-cli` and is selected by a change to the page pool.

Plus two direct rules: editing a crate's own source selects that crate's own
tests, and editing `tests/wal_crash.rs` selects `wal_crash` and not the other
seventy-three suites in the same crate.

### 3.2 Why `covers` is declared rather than derived

Two thirds of this workspace's assertions live in one crate, and every one of
its 75 suites depends on `inillucent-compat` — so a graph walk answers "a change
to the tree layer selects all 75", which is the same as having no selector.

Nor can it be read out of each suite's `use` statements, because the suites that
cover the most import the least: `semantics`, `cli`, `pragma`, `json` and `fts5`
drive the *shell* over a pipe, so their imports name `inillucent-compat` and
nothing else while what they exercise is the whole engine end to end.

So coverage is declared once per target, and the two failure modes a declaration
invites are both closed by `crates/inillucent-compat/tests/selection.rs`:

- **a target with no row** — it would never be selected, by `--changed` or by a
  tier, so it would sit in the tree looking like coverage and never run;
- **a row naming a target that does not exist** — caught at build time rather
  than the next time somebody runs it;
- **a test hiding where the runner does not look** — every source file carrying
  a `#[test]` is attributed to the target that compiles it, and a target that
  has no row fails the check. This is the one that matters: the obvious
  optimisation is to skip bin targets, because 43 of this workspace's 45 hold no
  tests — and the other two are `inillucent-bench` and `inillucent-shell`, whose
  crates have **no library at all** and which hold 179 tests between them. The
  check caught `inillucent-perfhistory` the day it was written.

### 3.3 What it is allowed to get wrong

Selecting too much costs time. Selecting too little costs a defect that reaches
`main`, so every judgement call is biased toward running more. The tier a target
is in never affects whether it is *selected*: tiers are for asking "run the
quick ones" and selection is for asking "run what this change can break", and
conflating them would let a tier choice quietly narrow a correctness question.

### 3.4 The pragmatic ladder

| you are | run | costs |
|---|---|---|
| mid-edit, want to know it still works | `--tier smoke` | ~1 s |
| about to commit | `--changed` | seconds to a minute |
| about to commit something structural | `--tier unit --tier engine --tier e2e` | ~55 s |
| about to push | everything except `retrieval` | ~35 s after a warm build |
| changing the planner, the tree, or the log | everything | ~155 s |
| claiming a speedup | the gates, not the suite — see §5.3 | minutes |

---

## 4. Running it in parallel

### 4.1 Why `cargo test` is not enough

`cargo test` runs test binaries **one at a time**. That is right for a crate and
wrong for a workspace of 149 targets on a 24-core machine, and the shape of this
suite makes it especially wrong: most binaries finish in under a tenth of a
second (131 of 194 when this was last measured, before task-1911 added targets;
not re-counted this pass), while five account for most of the clock. Running
them in sequence leaves the machine idle for almost all of a run.

It also starts 43 bin harnesses that hold no tests, to be told they have
nothing to run.

### 4.2 What the runner does

One build, then every selected binary at once, longest-first from a committed
timing ledger (`tests/timings.toml`). Longest-processing-time-first is the
classic answer and the reason is the tail: whatever starts last decides when the
run ends, so the worst thing to start last is the slowest suite.

```sh
target/debug/inillucent-testrun                 # everything
target/debug/inillucent-testrun --tier engine   # one tier
target/debug/inillucent-testrun --target inillucent-compat::semantics
target/debug/inillucent-testrun --jobs 12 --test-threads 4
target/debug/inillucent-testrun --record        # update the ledger
target/debug/inillucent-testrun --strict        # a missing prerequisite fails
```

**It never decides that a test passed.** It builds what cargo builds, runs the
same executables cargo would run, and reports their exit status; where it
differs is only in *which* binaries it runs and *how many at a time*.

### 4.3 Three things it had to be taught

**Do not rebuild yourself.** Cargo builds a package's binaries whenever that
package has integration tests, and `inillucent-compat` has 75 — so the runner's
own `cargo test --no-run` tried to replace the executable of the process that
asked for it, which Windows refuses. The bin is therefore behind
`required-features = ["testrun"]`, which is cargo's own way to say "do not build
this unless somebody asks". Copying the binary elsewhere and re-executing does
**not** work: the parent still holds the image.

**Two artifacts per binary.** Under `--no-run` cargo emits both the program and
the test harness compiled from the same sources, with the same name and kind.
Only `profile.test` tells them apart, so a reader that took the first would run
`inillucent-shell` as a program and hang the run on standard input.

**Some tiers cannot share a machine** — §5.

### 4.3.1 Three exit codes, because a run that did not happen is not a red run

Rule 1.5 says a test that cannot fail is worse than no test, and §9 exists
because a suite whose prerequisite was absent reported green. The runner's own
answer had the same defect one level up: `the build failed` exited **1**, which
is the code a failing test exits, so a caller branching on the status could not
tell a broken toolchain from a real defect. task-2041 read one as the other.

| code | what it means |
|---|---|
| `0` | every selected target ran and passed |
| `1` | the run happened and was red — a target failed, a target could not be read, or `--strict` found a suite that evidenced nothing |
| `2` | **the run did not happen.** The build failed, a `--target`/`--tier` pair matched nothing, `--filter` matched no test, or cargo could not say what it had built |

`2` is the code that carries the rule: nothing in that run was graded, so
nothing in it may be read as a pass. The command line spends a third code on
`unsupported` for the same reason — a caller should be able to branch without
matching on a message.

**The report and the exit status have to agree**, or one of the two stops being
read. A run that graded no test prints `not ok` and exits 2 from the same
question, `nothing_was_graded`; there is no path that prints `ok` over a
non-zero code.

**Two empty selections, and only one of them is a failure.** `--changed` that
finds nothing is a true answer and stays green. A `--target` and `--tier` the
caller named by hand that do not overlap is a request that could not be
honoured, and it refuses naming both — it used to print `nothing selected` and
exit 0, with both names real so neither existing guard fired.

The five cases holding this are in `crates/inillucent-compat/tests/gates_fail_closed.rs`,
and each asserts an **exact** code rather than "non-zero". A test that only
asserts non-zero on a broken build passes against a runner that refuses
everything, which is a worse program than the one being fixed.

### 4.4 How many at a time

Measured over `engine + differential + durability`:

| `--jobs` | `--test-threads` | wall |
|---:|---:|---:|
| **24** | **2** | **34.2 s** |
| 12 | 2 | 49.2 s |
| 24 | 1 | 57.6 s |
| 8 | 3 | 59.7 s |

24×2 is the default. One thread per binary is worse because a suite with many
tests loses its own parallelism; three threads across eight binaries is worse
because the long tail is back.

---

## 5. Performance

Three instruments, for three different questions. Using the wrong one is how a
performance claim becomes untrustworthy.

### 5.1 The cost guards — `crates/inillucent/tests/budget.rs`, tier `perf`

Ordinary tests, run with the suite, that assert the **shape of the cost curve**
rather than a time. Every one of them is a ratio between two counts taken in the
same run:

| guard | what it counts | measured here | asserted |
|---|---|---|---|
| an index makes a point lookup cheaper than a scan | pages fetched | 33 against 2 | scan ≥ 4x seek |
| one transaction around 2,000 inserts beats 2,000 of them | log writes | 1 against 2,000 | batched ≤ 1/100 of singly |
| preparing the same statement 200 times is answered from the plan cache | statements compiled | 1 | ≤ 1, **and** the cache grows by ≤ 1 entry |
| a keyset page costs the same wherever it starts | pages fetched | 502 against 502 | start ≤ 4x end |
| rewriting the same 5,000 rows ten times does not grow the file | bytes on disk | — | ≤ 3x |
| a full scan of 20,000 rows stays proportional to the table | pages fetched, **and** the clock | 20 pages, 2.99 ms | ≤ 200 pages, under 10 s |
| a statement outside a transaction rereads nothing the file has not changed | full meta reads, record reads | 0 and 200 over 200 statements | 0 full reads, ≤ 1 record read a statement |

Every threshold is a fraction of what it measures. That is the trade: these
catch a change of *kind* — an index dropped, a commit per row, a cache turned
off — and deliberately do not notice twenty per cent.

**These used to be wall-clock ratios, and widening them was the wrong answer.**
This section used to say "a guard that flaps gets widened, never tightened",
and that advice has a floor it reaches: "one transaction beats many" went from
4x to 2x and then read **1.3x** under a full run, where the next step down
asserts nothing. §7.3 has the whole history, including the second attempt — a
median of interleaved rounds, which is a better clock and is still a clock.

The rule that replaced it is §1.7. `budget.rs` now reads a count everywhere it
can, and the one place it cannot — a quadratic that does no extra I/O, which
no count in the file can see — keeps a clock at 3,300x headroom and says so.

**And the tier runs alone.** `perf` is declared `exclusive = true` in the map;
the runner finishes everything else, then runs it one binary at a time and with
one thread inside each binary. That second half was missing until task-1886, so
the tier's six guards ran two at a time against each other. It costs about half
a minute. It is worth having and it is not the fix: four agent terminals and a
training run are outside the runner's reach, and that is the load that failed
these guards twice.

### 5.2 The history — `inillucent-perfhistory`, `tests/performance-history.tsv`

What each workload costs **beside SQLite, in processor time and memory as well
as wall clock, appended over time**.

```sh
cargo build --release -p inillucent-cli          # the engine under test
cargo run -p inillucent-compat --bin inillucent-perfhistory -- --rounds 5
cargo run -p inillucent-compat --bin inillucent-perfhistory -- --dry-run
```

Eight workloads, each with an **untimed** setup and a timed operation:
`insert.10k`, `index.build`, `lookup.indexed`, `scan.aggregate`, `update.churn`,
`join.two-tables`, `text.like`, `delete.half`.

It accounts for the rest of the machine in four ways, because no one of them is
enough:

- **Interleaving.** Each round runs inillucent, then SQLite, back to back — so a
  background job that arrives halfway slows both arms and the *ratio* survives
  it. The ratio is the column to read across time; the absolutes catch the case
  where both moved together.
- **A calibration loop, before and after.** A fixed arithmetic loop that touches
  nothing. Its size says how fast this machine is, so rows from two machines can
  be compared; the difference between the two readings says whether the machine
  changed *during* the run, and a row whose drift is far from 1.000 says so
  itself rather than looking like a regression.
- **Statistics chosen per number.** Wall clock takes the median, because one
  round that lost the processor moves a mean. Processor time takes the mean of
  the total, which is the opposite choice and is forced by the clock: Windows
  accounts CPU in ~15.6 ms units, so a median of five short rounds is one of two
  values. Peak resident set takes the largest, because that is what a peak is.
- **Startup is subtracted.** Both shells are measured as child processes, which
  is the only fair arrangement when one of them is a separate program — and
  starting them costs ~18 ms and ~8 ms, which is more than some workloads spend
  on their data. The subtraction is recorded in its own columns rather than
  hidden, and the absolute columns are left unadjusted.

**Two things this instrument taught while being built, both of which were the
instrument's fault and not the engine's**, and both of which are worth knowing
before writing another one:

- The first version generated its rows inline with `WITH RECURSIVE`, and
  everything came out 3x–8x slower than SQLite. A 200,000-row recursive CTE that
  touches no table at all costs 201 ms here against SQLite's 85 ms — so the
  workloads were a measurement of recursive-CTE throughput with a little storage
  engine underneath. The generator moved into the untimed setup. *(That 2.4x is
  real and is a separate finding.)*
- The second version copied the database file between rounds and left the log
  behind, because this engine writes a **segmented** log — `app.db-wal.0000000001`,
  a new file per segment — and SQLite writes `-wal` and `-shm`. Every round after
  the first opened a database beside somebody else's log. Each database now lives
  in its own directory and the *directory* is what gets copied, which needs to
  know nothing about either engine's file naming.

### 5.3 The gates — `inillucent-fullgate` and its siblings

Thirty rounds against the pinned SQLite with a geometric mean and a lower bound,
correctness-qualified by digest. **This is where a speedup claim belongs**, and
it is deliberately not a `cargo test`. Neither §5.1 nor §5.2 is a substitute:
the guards assert shape, the history keeps a series, and the gate decides
whether a number holds.

---

## 6. Timings

Measured on Windows 11, a 24-core processor, 127.5 GB of memory, NVMe, with a
warm build. `cargo build` time is excluded except where stated.

### 6.1 The whole suite

Re-measured after task-1911 (149 targets, 2,642 tests, up from 129/2,336) on a
box that was not quiet — several agent terminals were active in this tree at
the time, which is exactly the load §1.7 and §7.3 warn a wall-clock reading
is sensitive to. The counts are exact; the wall-clock figures below are this
run, not a guaranteed one, and worth a re-read on a quiet box before they are
leaned on for anything more than a rough sense of scale.

A later pass in the same ticket fixed a real checkpoint data-loss defect
Fable's review found and added three regression tests plus a fourth crash
sweep pinning it and the two independent bugs the fix itself needed (§2 above
has the current total, 2,645); the wall-clock figures below were not
re-measured against that count, since three more tests change nothing at this
scale.

| | wall | note |
|---|---:|---|
| `cargo test --workspace` (serial) | **315 s** *(not re-measured this pass)* | includes ~70 s of build; 164 binaries + 30 doc-test targets |
| `inillucent-testrun` (parallel, everything) | **~302 s** | 149 targets, 2,642 tests, 2,676 s of processor time — **8.9x** |
| `inillucent-testrun` without `retrieval` | **~89 s** | 143 targets, 2,136 tests |

The retrieval tier is 213.8 s on its own (§6.2), and its two largest targets —
`inillucent-core::lib` and `inillucent-bench` — are 250 s+ each on their own in
a full run because of the same contention. **They are the floor of a full
parallel run**: no scheduling improves on the longest single binary. Everything
else finishes in the time they take.

### 6.1.1 What the hardware has to be, and what contention means

**Every figure in this section is from one machine**: Windows 11, a 24-core
processor, 127.5 GB of memory, an NVMe disk, and an RTX 5090. The parallel
runner uses all 24 cores, so a machine with fewer scales the wall clock roughly
by the ratio while the processor-time total stays where it is.

Two tiers want hardware the others do not, and until task-1961 the table above
did not say so:

| tier | what it needs | without it |
|---|---|---|
| `retrieval` | **a CUDA GPU for the embedding arm** - the figures here are an RTX 5090 - and ONNX Runtime plus the model weights, which `inillucent setup-embeddings` installs | `inillucent-core::lib` and `inillucent-bench` report success having embedded nothing. `--strict` counts them and names them, which is the only reason a green run on a machine without a GPU is not mistaken for a green run on one with it. |
| `retrieval` | **the disk**, for the index store: the 600,589-chunk corpus is 3.1 GB on disk and the suite writes and re-reads it | the corpus-backed targets skip; the smaller ones run from a generated corpus and are disk-bound rather than GPU-bound |
| fuzzing | **a nightly toolchain**, because libFuzzer needs one, and hours rather than seconds | nothing runs. `rust-toolchain.toml` pins stable, so `cargo fuzz` is a deliberate, separate step on a machine that has installed a nightly beside the pin. The seeded twins in `crates/*/tests/fuzz_seeded.rs` are what runs under the pinned compiler, in under a second each. |

**"Under contention" means other processes on the same machine**, and in this
repository that is nearly always other agent terminals building or testing in
the same tree. It is not contention between the runner's own 24 jobs, which is
what the phrase reads as: the runner schedules one binary per core and the
binaries do not share a file. A quiet box and a busy one differ by about a
factor of three on the two longest targets:

| target | quiet box | busy box |
|---|---:|---:|
| `inillucent-core::lib` | ~110 s | 261 s |
| `inillucent-bench::inillucent-bench` | ~105 s | 249 s |
| `inillucent-testrun` (everything) | ~300 s | 771 s |

The quiet-box figures are what to expect from a checkout on an idle machine;
the busy-box column is §6.3's table, taken while several agents were working in
this tree. **A timing read on a busy box is not a defect and is not worth
chasing**, which is why §1.7 says a count is the thing to assert on and a
duration is not.

### 6.2 Tier by tier

Each tier run in isolation (`--tier <name>` alone), not carved out of the full
run above - so these do not sum to 6.1's total, and each one is its own
build-plus-run rather than a slice of one shared build.

| tier | wall | targets | tests |
|---|---:|---:|---:|
| `smoke` | 0.8 s | 1 | 8 |
| `unit` | 7.8 s | 31 | 1,388 |
| `e2e` | 25.4 s | 36 | 412 |
| `perf` | 35.1 s | 1 | 6 |
| `differential` | 36.1 s | 29 | 292 |
| `engine` | 37.8 s | 44 | 299 |
| `retrieval` | 213.8 s | 6 | 506 |
| `tooling` | 242.4 s | 13 | 122 |
| `durability` | 988.2 s | 31 | 216 |
| `nightly` | 1,650.3 s | 2 | 3 |

`perf`'s 35.1 s agrees with §5.1's "about half a minute" where the old 9.1 s
in this table did not; that inconsistency predates this pass and is corrected
here rather than carried forward. Each figure includes the runner's own
startup and its `cargo` target listing, about 1.2 s.

**`unit`, `e2e`, `tooling`, `durability` and `nightly` were measured again for
task-2036**, which added 26 targets across them; the other four rows are
carried forward from the pass that measured them. **`e2e` was measured again
for task-2055**, which split `durability` into two targets so that the cases
running at every arm sit apart from the cases picking their own geometry - 36
targets and 412 tests, at 25.4 s on a box that had one other agent on it.

**`e2e` is 23.5 s and the design asked for fifteen.** A tier's wall is its
slowest target, and three of them are within a second of the whole tier:
`story_nikaya` at 23.5 s, `cli_commands` at 23.2 s and `story_edges` at 21.8 s.
`cli_commands` is not one of this ticket's suites and is over fifteen seconds on
its own, so cutting the stories would not bring the tier under it. What the
number bought is 370 tests where the tier had 130: nine stories, each asked at
six configurations.

**`durability` and `tooling` are minutes rather than seconds, and both are one
target.** `vacuum_crash` is the whole of `durability`'s wall and
`gates_fail_closed` is the whole of `tooling`'s; the other suites in each finish
while those two are still going.

### 6.3 The eight that matter

Re-measured post task-1911. Three of the original five (`inillucent-migrate::corpus`,
`inillucent-compat::corruption`, `inillucent-compat::semantics`) are still slow
but no longer the tail; the free-map durability fix's own crash campaigns and
the differential re-point's new fixture-heavy suites now dominate it, so the
list is longer rather than swapped one-for-one. Under the parallel runner,
where contention inflates each, and on a box that was not quiet (see §6.1):

| target | wall |
|---|---:|
| `inillucent-core::lib` | 261 s |
| `inillucent-bench::inillucent-bench` | 249 s |
| `inillucent-compat::search_crash` | 224 s |
| `inillucent-compat::durability` | 213 s |
| `inillucent-compat::wal_crash` | 179 s |
| `inillucent-compat::new_engine_recovery_shapes` | 152 s |
| `inillucent-compat::semantics` | 122 s |
| `inillucent-migrate::corpus` | 98 s |

Serial figures for this list were not re-measured this pass — the original
five's serial numbers (40 s, 59 s, 20 s, 20 s, 10 s) do not apply to the three
new entries, which did not exist when they were taken. What is still true is
the shape: these are the floor of a full parallel run, and running twenty-four
at once is worth paying for regardless of the exact serial-versus-parallel
ratio on any given day.

---

## 7. What this standard found

The suite described here found four defects while it was being written; reviewing the fixes found three more, in the fixes; and running it
under load found three more after that, all of them in the gate rather than in
the engine. All ten are fixed, and each is now a test that asserts the fix. The
first seven are below, the last three are in 7.3.
**Virtual tables did not participate in their transaction.** A rolled-back
insert into an `fts5` or `rtree` table stayed, a rolled-back delete was gone, and
`ROLLBACK TO` did nothing — so one query answered differently before and after a
reopen with nothing written in between, wrong in whichever direction the
abandoned transaction had written, and silent. The clue was that the *file* was
always right, which is what a missing undo record looks like rather than a stale
cache: `change_module` built its write log with `undo: None` where every ordinary
write passes `Some(&self.undo)`. The write path now records before-images, the
engine now calls `rollback`/`rollback_to` on every connected module — it only
ever called `begin`, `sync` and `commit`, while the *old* engine has dispatched
`Moment::Rollback` all along — and FTS5 implements the `rollback` it never had.

**`Statement::bind` accepted any index.** `Params::set` did
`index.saturating_sub(1)` into a vector it then resized to fit, so index 9 on a
one-parameter statement grew the set and returned `Ok`, and **index 0 silently
aliased `?1`** — a caller who believed index 0 was a no-op had overwritten its
first parameter with nothing to say so. SQLite answers `SQLITE_RANGE` to both.

**`execute_batch` could not create a trigger.** It cut the script at every
semicolon outside a string literal and a trigger body contains one. It now walks
the script with the parser's own `statement_length`, so there is one opinion
about statement boundaries instead of two.

**Four files were unformatted on `main`**, failing the repository's own
`policy::the_governed_crates_are_formatted`.

### 7.1 One that fixing exposed

The first version of the `bind` fix routed the engine's *internal* `Params::set`
through the same range check, and three correlated-subquery tests went red.
`correlate::Correlation::answer` feeds an outer row's columns into a correlated
block through parameter slots the **binder** invents above the statement's
declared count — so the check silently dropped them and every correlated
`EXISTS` answered against an unbound slot. The two paths are now separate: the
engine may write any slot it invented, an application may only write the ones
its statement declared.

It is recorded here because it is the shape this whole document is about. A
check that is correct at the boundary and wrong one layer in produces a **wrong
answer rather than an error**, and the only reason it was caught in minutes
rather than months is that the differential suites ask SQLite the same question.


### 7.2 Three the review found in the fix itself

The virtual-table rollback fix was reviewed again in task-1857, and that
review found three defects **in the fix**, all on the same two lines. They are
listed because they are more instructive than the original bug:

- **A rollback could return early and leave a transaction with no data in it.**
  The module notification's `?` sat between `undo_to` and the bookkeeping, so a
  module whose `rollback` failed left `marks` and `batch` untouched while the
  rows were already undone. A rollback is not a step that can be declined: every
  step now runs and the first failure is returned afterwards.
- **`ROLLBACK TO` told the modules the wrong number.** It passed the current
  nesting depth rather than the level of the savepoint being returned to, so
  `SAVEPOINT a; SAVEPOINT b; ROLLBACK TO a` said "two" where the answer is
  "zero", and a module numbering its own marks by what it was given would keep
  the state belonging to the savepoint just abandoned.
- **A failed `ROLLBACK TO` had a side effect.** The level was resolved with
  `unwrap_or(0)`, so a name no savepoint held told every module to discard and
  *then* reported the error. A typo in a savepoint name threw away a buffered
  virtual table's pending writes.

Two lessons, and both are rules elsewhere in this document now.

**A test's comment may only claim what the test proves.** The nested-savepoint
test written for the second finding *passes with that fix reverted* - FTS5's
`rollback_to` discards its whole buffer and ignores the level, and there is no
module in the tree that keeps marks of its own. That was found by reverting the
fix and running it, not by reading it, and the test's comment now says so. The
test for the third finding was checked the same way and does go red.

**Review the fix, not just the bug.** Three of the seven defects this ticket
fixed were introduced by fixing the other four, and every one of them was in
error handling - the paths the tests exercise least. A review pass over the diff
is part of finishing, not a formality.

### 7.3 The gate itself, three times over

The three faults after those seven were all in the machinery that decides
whether a run proved anything, and none of them was in the engine.

**A `--strict` skip could never be detected.** The runner matched on
`has not been built`, `is not available` and `no reference`; what the suites
actually print is `the pinned SQLite oracle is not built; skipping` and its
siblings, so the standard and the code had been describing two different lists.
On a machine without the oracle, thirty-odd differential suites skipped every
case and the run reported `ok`. `; skipping` is the phrase that carries it now,
because it is the one §9 tells authors to print.

**A target was recorded FAILED with all 156 of its tests passing.** The runner
read the exit status and nothing else, and `inillucent-bench` sometimes dies in
the ONNX runtime's teardown after libtest has printed its summary. That is not a
contradiction to be discarded — it is a process that died after its tests passed
— so `inillucent_compat::verdict` now reads the transcript and the status
together and has three answers, the third being "the runner could not tell", with
its reason named. A target it could not read is run once more, alone; a target
that failed a test is never re-run. The `FAILED:` list prints each exit status
beside the name, which is the one fact that separates the two cases and which
somebody had to work out by hand the first time.

**Six assertions were decided by the machine rather than by the code.** This is
§1.7, and it is the one that took three attempts. The first answer was to widen
the threshold: 4x to 2x, and then a 1.3x reading under a full run. The second was
to keep the clock and make it fair — five interleaved rounds, median taken, so a
load spike lands on both arms — and the next contended run moved the failure to a
different test in the same file, which read 6.6x against a bound of 4 and printed
`the cache is not being consulted` about a cache that was fine. The third answer
was to stop reading a clock. Every one of those assertions is now a count, and
the counts were re-read at 100% processor load with the same digits.

**Fixing the named test is not fixing the defect.** `budget.rs` had eight
measurement sites; the first report named one of them, and the second run failed
on a different one. The sweep that followed found the same shape in
`new_engine_vtab_stream.rs` and `transactions.rs` — one of which then failed, on
a working engine, during this ticket's own verification run. A defect in how a
gate decides is worth grepping the whole tree for.

---

## 8. Open, and not fixed here

**`DELETE` is super-linear in the number of rows.** Measured through both
shells, deleting every row of a table with one text column:

| rows | inillucent | SQLite |
|---:|---:|---:|
| 1,000 | 3 ms | ~1 ms |
| 2,000 | 11 ms | ~1 ms |
| 4,000 | 272 ms | ~1 ms |
| 8,000 | 2,075 ms | 2 ms |

The row count quadruples from 2,000 to 8,000 and the time grows 189-fold. It is
not the transaction boundary — the same delete inside an explicit `BEGIN`
behaves the same — and it is not history-dependent, since a freshly built table
shows it. It is visible in the history as `insert.10k` at 0.005x and
`delete.half` at 0.068x, the two workloads that delete.

A candidate cause was investigated and **the attempted fix was reverted**:
`write.rs::merge_if_small` asks whether the *left* leaf has underflowed and then
packs both leaves to see whether they fit, so a tree emptied in key order packs
two leaves and throws the work away on every delete once the left leaf is half
empty. Adding a cheap capacity pre-check on the right sibling moved 8,000 rows
from 2,075 ms to 1,695 ms — an 18% improvement where a decisive one was
predicted, which means the model was wrong. Shipping an unvalidated change to a
B-tree on a wrong model is not worth 18%, so the tree is untouched and this is
written down instead.

Whoever takes it should profile rather than reason: `inillucent-writeprofile`
and `inillucent-hotprofile` exist for it.

---

## 9. Adding to the suite

**A test.** Put it where §2.1 says. If it is a new `tests/*.rs` file, add its row
to `tests/selection.toml` — `crates/inillucent-compat/tests/selection.rs` will
fail until you do, and its message names the target.

**A tier.** Add a `[[tier]]` row and move targets into it. A declared tier that
holds nothing fails `every_tier_is_declared`, so a tier cannot be added
speculatively.

**A story.** Put it in `crates/inillucent/tests/story_*.rs` and write it as a
function taking `(&Arm, &Path)`, then `scenario!(its_name);`. It runs at all six
arms, and `tooling::scenarios_run_every_quick_arm` fails if a story in that
directory asks its question once. Give it a sequence an application actually
performs rather than one invented to look like one: the escape a story is shaped
around was in the order the real program goes in.

**A long form.** Write it as a second target in tier `nightly`, not as a bigger
number in the `e2e` one — a target is in exactly one tier. Name it
`<the short form>_nightly` so the pair is obvious, and state its cadence rather
than deriving it: the short form's phase boundaries are tuned for a run that
finishes in seconds, and the same cadence over a hundred thousand transactions
spends its night on `VACUUM`. `pwsh tools/run-nightly.ps1` picks it up with no
further wiring.

**An interop fixture.** `pwsh tools/build-interop-fixture.ps1 -Version <version>`
downloads that release, verifies it against the published `SHA256SUMS` and its
minisign signature, and writes `tests/interop/<version>/`. Never write one by
hand and never open one in place. A question every release should answer goes in
`tests/interop/verify.sql`, and every fixture is then rebuilt — one list, two
readers, and `expected.tsv` is what each release answered.

**A conformance case.** Add it to `drivers/conformance/suite.json` with the
group it belongs to and the capabilities it needs. All five runners read that
file, `tooling::bindings` fails when a runner ran fewer cases than the suite
holds, and a runner that filters a group must name the group and the reason in
`skipped_by` rather than skipping quietly.

**An escape.** When a defect gets out, its fix adds a row to `tests/escapes.toml`
in the same commit: the ticket, the surface, one sentence, and the `held_by` test
that would now catch it. If nothing holds it yet, `held_by = []` and `open` says
why. `tooling::escapes` checks that every `held_by` resolves to a test that
exists.

**A performance workload.** Add a `Workload` to `perfhistory.rs` with an untimed
`setup` and a timed `script`, and a `repeat` large enough that the timed part is
an order of magnitude more than the ~18 ms process startup. **Never rename one**:
a renamed workload is a new series with the old one's history thrown away.

**A prerequisite.** If a suite needs something the workspace cannot build, add
it to that row's `requires` and make the suite print one of the phrases the
runner recognises — otherwise `--strict` cannot tell a real pass from an empty
one. Every `differential` row must declare one;
`every_differential_target_declares_what_it_needs` enforces it.

The phrases are `; skipping`, `is not built`, `is missing`, `has not been
built`, `is not available` and `no reference`, and **`; skipping` is the one to
use**: every existing message already ends with it.

> **This paragraph and the runner disagreed for a while, and the disagreement
> was the exact failure `--strict` exists to prevent.** The list here has always
> read `is not built`, `is missing`, `; skipping`; `missing_prerequisites` in
> `testrun.rs` matched `has not been built`, `is not available` and
> `no reference`. What the suites actually print is `the pinned SQLite oracle is
> not built; skipping`, `the pinned shell is not present; skipping` and `no
> usable C compiler; skipping` — which matched **none** of the three the code
> looked for. So on a machine without the pinned oracle, thirty-odd differential
> suites would skip every case and `--strict` would still print `ok`. The runner
> now matches all six, which makes this paragraph true rather than aspirational.
> Read it as a warning about the shape rather than about the strings:
> a check whose *documentation* is the only place its contract is written down
> is a check nothing verifies.

---

## 10. The commands, in one place

```sh
# build the runner (once)
cargo build -p inillucent-compat --bin inillucent-testrun --features testrun

target/debug/inillucent-testrun --list-tiers        # what the tiers are
target/debug/inillucent-testrun --tier smoke        # ~1 s
target/debug/inillucent-testrun --changed           # what your edits can break
target/debug/inillucent-testrun --changed --list    # ...without running it
target/debug/inillucent-testrun                     # everything, ~155 s
target/debug/inillucent-testrun --strict            # fail on a missing prerequisite
target/debug/inillucent-testrun --record            # update tests/timings.toml

# the performance history
cargo build --release -p inillucent-cli
cargo run -p inillucent-compat --bin inillucent-perfhistory -- --rounds 5

# the prerequisites the differential tier needs
tools/sqlite-reference.ps1          # or tools/sqlite-reference.sh
```

`cargo test --workspace` still works and still means the same thing. The runner
is faster and more selective; it is not a different definition of a pass.
