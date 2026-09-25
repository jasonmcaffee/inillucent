# Coordination plan — inillucent

Several tickets run in this repository at the same time. This file is how they stay out of each
other's way. Read it before you start editing, and add to it when you learn something the next
agent needs.

## You are not the only agent in this repository

`GET /tasks/repo-plan/inillucent` answers with this file **and** the tickets that have an agent
in this repository right now — their keys, their branches and what each one says it needs. Read it
at the start of your ticket and again before a change that reaches outside the files you own.

## Your isolation is a git worktree

Your terminal opened in `.claude/worktrees/task-<N>` on branch `task-<N>`, taken from this
repository's default branch. The main checkout is somebody else's working directory — never edit it,
never run a build in it, and never `git checkout` in it.

- Stage only the paths your ticket touched. `git add -A` in a shared tree commits another agent's
  work under your ticket's message, where nobody will find it.
- A test that fails in a file your ticket never touched probably belongs to another agent. Check it
  against the default branch before you spend time on it.
- Rebase on the default branch and merge when your work is verified. A ticket is not delivered until
  its branch is on the default branch.

## Work that needs this repository to itself

Some work cannot share the repository, and the way to get it is to say so on the ticket rather than
to take a lock:

- **A release.** Cutting a release from a tree where another agent is mid-implementation publishes
  half-finished work.
- **A dependency change.** `npm install` through a junctioned `node_modules` writes into the main
  checkout every other agent is using, so a dependency ticket is dispatched without junctions.
- **A migration or a repository-wide refactor**, where every other branch would conflict on merge.

Write that in the ticket's description, or in its `resourceTags` field as a note to the supervisor
(for example `needs the repo to itself`). The supervisor reads it and holds the other tickets in
this repository back while it runs. Nothing in the backend enforces it — it is a decision, made by
somebody who can read both tickets.

## When two tickets do collide

Say so, on the board, to the other ticket. Post a comment on it naming the files and what you are
about to change, and read its comments before you change them. Two agents that have both said what
they are touching do not collide; two that have not, do.

## Notes for this repository

- **Every sub agent of one session shares one scratchpad directory, and two agents naming a database
  the same thing in it collide.** This looked like data loss and was investigated as one: while the
  performance audit was building a 100,000 row fixture, its `main_table` disappeared and a table
  named `b` it had never created was in the file, with log segment 7 older than segment 5. It was a
  third sub agent running `rm -f t.rdb; inillucent create t.rdb; CREATE TABLE b (i, r, t, z)`
  against the same path two minutes after the auditor's last write. The surviving schema matched
  that statement character for character, and segment 7 outlived its database because `rm -f`
  removed only the main file: the replacement started a fresh chain at one, and two chains were read
  as one. Reproduced five times in fresh directories with six concurrent readers, and it is not an
  engine defect. **Give each sub agent its own folder under the scratchpad, and prefix every scratch
  database name with the agent's own tag.** (task-2066 section 5)

<!-- Add what the next agent needs to know: the hot files, the parts that cannot be edited in
     parallel, the build that takes twenty minutes, the test that only passes on an idle box. -->

- **A worktree has no `.sqlite-ref/`, and every gate binary needs it.** `inillucent-fullgate`,
  `inillucent-writegate` and the oracle suites look for `.sqlite-ref/3.53.4/sqlite-bench.exe` and
  `.sqlite-ref/3.53.4/shell/sqlite3.exe` under the workspace root, which in a worktree is the
  worktree. The directory is gitignored so it exists only in the main checkout. Copy the two
  binaries across before you run a gate; the failure message names the missing program and not the
  reason. (task-2034)
- **~~The whole test suite needs the MSVC environment imported first.~~ `inillucent-testrun` now
  imports it itself.** It was true from task-2034 to task-2047: `onig_sys` compiles oniguruma with
  cl.exe, an agent terminal has no `INCLUDE`, and the run died at
  `regenc.h(39): fatal error C1083: Cannot open include file: 'stddef.h'` after building everything
  else. The runner finds Visual Studio through `vswhere`, runs `vcvars64.bat` and copies the result
  into the environment its cargo children inherit, so `inillucent-testrun` needs nothing from you.
  **Everything else still does** - `cargo build` by hand, `cargo test` by hand,
  `tools/sqlite-reference.ps1` and every program in `packaging/`. For those, dot-source
  `packaging/stage-layout.ps1` and call `Import-MsvcEnvironment`. (task-2047)
- **A cold worktree build costs real time.** The release `inillucent-fullgate` is about three
  minutes from cold and about ninety seconds after a source edit; the debug build the test runner
  needs is much longer, because every test target is its own binary. Start it before you need it.
- **A microsecond measurement in this repository is only trustworthy as a ratio while other agents
  are running.** The gate interleaves its two arms, so a ratio survives a loaded box and an absolute
  microsecond does not. Say in the write-up what else was on the processor. Three tickets building
  at once moved `extension.fts.build`'s absolute time by 40% and left its ratio where it was.
  (task-2034)
- **The files a write path ticket edits are `crates/inillucent-tree/src/write.rs`, `mutate.rs` and
  `leaf/`.** task-2034 changed all three. A performance ticket and a correctness ticket about the
  same page format will land on them together, so say on the other ticket which functions you are
  in before you start. (task-2034)
- **A worktree has no `.sqlite-ref/`, and three test targets panic rather than skip without it.**
  The pinned SQLite 3.53.4 oracle, shell and `sqlite-bench` are gitignored, so they exist in the
  main checkout and not in your worktree. `schema_forms`, `rtree` and `registers` fail with "the
  pinned SQLite oracle is not built", and about twenty more differential targets quietly report
  `needs oracle` and run nothing - which is most of what checks a parser or planner change against
  3.53.4. `inillucent-fullgate` refuses outright with "sqlite-bench is not built". Junction it in
  before you read any result:
  `New-Item -ItemType Junction -Path <worktree>\.sqlite-ref -Target C:\jason\dev\inillucent\.sqlite-ref`.
  Remove the junction before you retire the worktree. (task-2039)
- **There is one copy of that oracle and every worktree junctions to it, so a recursive delete of
  your own junction empties it for everybody.** `Remove-Item -Recurse` and `rm -rf` both follow a
  junction and delete what is on the other side, which is the main checkout's `.sqlite-ref/`. It
  happened on 2026-09-21: the directory was emptied at 09:43, nothing put it back, and the three
  worktrees open at the time were all left pointing at an empty directory. Remove the link without
  touching the target - `cmd /c rmdir "<worktree>\.sqlite-ref"`, or in PowerShell
  `[System.IO.Directory]::Delete($path, $false)`. Both delete the link alone.
  **Run with `INILLUCENT_STRICT=1` or you will not find out.** Without it about twenty differential
  targets report `needs oracle`, run nothing and report success, so a suite that compared nothing
  reads exactly like a suite that passed. To put the oracle back, run
  `pwsh tools/sqlite-reference.ps1`: it downloads the 3.53.4 amalgamation and tools, checks both
  against the SHA3-256 sums SQLite publishes, and compiles the oracle and the benchmark driver. It
  took about a minute, and it needs the MSVC environment imported first. (task-2048)
- **~~`inillucent-testrun` cannot build from an agent terminal until the MSVC environment is
  imported.~~ Fixed in task-2047 — see the entry above.** The half that is still true is
  `cargo test` and `cargo build` run by hand, which import nothing. (task-2039)
- **Before you blame your own change for a `policy` failure, check which file it names.** The
  module-size ceilings are a ratchet over a fixed list, so a file another ticket grew fails for
  everybody who branches after it. `crates/inillucent-sql/src/bind.rs` was 103 lines past its row
  at `b6e79d9`. Raising somebody else's number from your ticket is the diff nobody notices that the
  row exists to prevent - say so on their ticket instead. (task-2039)
- **That `bind.rs` ceiling is settled: task-2048 moved row values into `bind/rowvalue.rs` and the
  row now reads 4,968, the file's exact length.** So the ratchet bites on the next line anybody
  adds to it, and it will bite on whoever adds that line rather than on the next person to branch.
  The number went 5,282 to 5,422 and back to 5,290 inside four days with nobody deciding it should,
  and two tickets spent time working out that the red gate was not theirs. If you need room in
  `bind.rs`, move an idea out the way `cte.rs`, `having.rs`, `literal.rs`, `rowvalue.rs` and
  `scratch.rs` each did; a child module can hold its own `impl Binder` block, and `Binder`'s fields
  are `pub(crate)`, so a move needs no signature or visibility change at all. (task-2048)
- **Build a second binary for a before-and-after into its own `CARGO_TARGET_DIR`.** cargo takes a
  file lock per target directory, so a release build started while `inillucent-testrun` is
  compiling waits for it rather than running beside it. The release profile is fat LTO with one
  codegen unit, so each arm is a full relink and queueing them doubles an already long wait.
  (task-2039)
- **Close the database and open it again in any test about storage.** Two of the three defects
  task-2033 found were invisible without it: the write returned `Ok`, the row read back in the same
  session because the page was in the pool, and `PRAGMA integrity_check` answered `ok` on a file
  that had lost rows. Recovery learns each table's shape from the `InsertRow` records naming the
  schema tree, so a catalog row that reaches its page any other way - inside a page image, or from
  a bulk build that writes before the record that names it - left recovery not knowing the table
  existed, and the rows written into it were dropped on replay by the unknown-tree tolerance. A
  case that did not reopen passed through all of it. (task-2033)
- **`tests/crash/*.txt` and `*.tsv` show as modified after any run that touched the crash
  campaigns, and the change is only the line endings.** The suites rewrite their recorded schedules
  with LF where the checkout holds CRLF, so `git status` lists eighteen files and `git diff` shows
  nothing in any of them. Do not stage them and do not spend time on them: put them back with
  `git checkout --` naming each path before you commit. (task-2052)
- **`PRAGMA integrity_check` and `PRAGMA quick_check` now account for every page of the file**, so
  a change that gives one page to two trees fails a check rather than losing rows quietly. Two
  things follow for anybody editing the write path. A page a tree reaches that the free map calls
  free is reported as corruption, which is the state task-2043 passed through, so a free that
  happens at the wrong moment now fails the campaign suites at the statement that did it. And
  `ImportedDatabase::check_trees` is what those suites call after every statement, so it walks the
  pages too; counted off the pool, the whole walk cost 4 page fetches on top of 133 over a table of
  sixty out-of-line values, because it takes an out-of-line value's pages from the reference in the
  leaf rather than by reading the value. (task-2052)
- **~~Two ordinary statements leave a page the free map holds and no tree reaches.~~ Both are
  closed, and `PRAGMA integrity_check` now reports a leaked page as `Page N: never used`.** So a
  change that allocates a page and loses track of it fails the pragma, and the pragma runs after
  every statement of the campaign suites - which is the point of having closed them. Two things
  follow for anybody editing the write path. A tree built inside a transaction is recorded in
  `Writing::built` and its pages are given back if the transaction is abandoned, so a new path that
  builds a tree has to go through `build_tree_rows` or say why it does not. And a dropped tree's
  out-of-line values go onto the pending-free list as `ExtentRef`s, freed at the commit by
  `paged::free_extent` - never at the statement, which is the task-2043 rule and is unchanged.
  `crates/inillucent-engine/src/engine/pages.rs` carries the reasoning. (task-2052, task-2065)
- **`inillucent-migrate::corpus` can go red because of prose you wrote, and it is not a defect in
  your change. See task-2067.** Its corpus is gathered **at run time from `crates/` and `docs/`**,
  so it moves whenever the repository's text does: task-2065 tripped it by adding five tests - 226
  lines of test code, not bloated comments - and then rebasing onto task-2053 put that ticket's
  prose in as well and it passed again. It is green on `main` today and it is one edit from either
  side of the line.
  - **Do not spend an hour proving your own change innocent, as task-2065 did.** The corpus is read
    off disk, so hold the test binary constant and revert only the *files*, with no rebuild. Ten
    seconds an iteration, and it bisects to the single file that moved the ranking.
  - What is actually wrong is underneath: `live_filter` compares a legacy ranking taken with the
    filter applied *during* the search against a migrated one that ranks first and filters
    afterwards. Those agree only while the tombstoned rows do not move a document's score, and
    `inillucent-core` says they do. task-2067 has the diagnosis and three options.
  (task-2065, task-2067)
- **A test that shows zero CPU and zero I/O is not necessarily hung - look for a child process
  first.** `inillucent::story_ledger_day_nightly` hands its 100,000-statement script to the pinned
  `sqlite3.exe` and blocks in `Command::output()` for the last third of its run, so the parent's
  counters stop dead while the oracle does the work. It was twice reported as a reproducible hang
  on task-2065, "frozen at the same I/O position" under two different PIDs - which is exactly what a
  parent that has stopped touching the disk looks like. Both runs completed normally. Sample
  `ParentProcessId = <pid>` before concluding anything, and expect this target to take 30 minutes.
  (task-2065)
- **If you build a tree under a new handle, three things have to move with it, and only the first
  is obvious.** `REINDEX` does this - `allocate_root()` then `build_tree_from` - and it got all
  three wrong until task-2065. (1) The tree being replaced has to be released, or its pages leak.
  (2) `release_tree` takes the old handle out of `schema.covering`, and nothing puts the new one
  back: `covering` is maintained by hand in `create_index` and `ALTER TABLE`, and `rebuild_tables`
  does **not** derive it. Miss this and a `SELECT` that picks the index fails with `bad parameter or
  other API misuse`. (3) `rewrite` updates a catalog entry and leaves `Recorded::root` - the handle -
  alone, so the recorded handle goes on naming the tree you just replaced, and reads keep going to
  it until something reopens the file. Nothing caught (3) for as long as it existed, because the old
  tree held the same rows; the leaked page from (1) was its only symptom. (task-2065)
- **Adding a row to `tests/selection.toml` fails `documentation` until the tier table moves with
  it.** `tests/inillucent-testing-tdd.md` records a target count per tier and
  `the_per_tier_table_matches_the_map` compares the two. Update the `targets` cell of the tier you
  added to, in the same commit. The `tests` cell beside it is not asserted, and the sentence in
  `docs/repository.md` about "the 196 rows" is neither asserted nor current. (task-2033)
- **Link the test binaries in one build before running the suite, not during it.** Two
  `inillucent-testrun --changed` runs died at `link.exe returned an unexpected error` on
  `inillucent-compat`'s test targets while other agents were also linking, on a box with 439 GB
  free and 34 GB of RAM - so it is concurrency rather than headroom. `cargo build --workspace
  --tests` first and then `inillucent-testrun --no-build` costs nothing extra and does not throw
  away twenty minutes. (task-2033)
- **In the main checkout, `--strict` still names three suites whose prerequisites are not
  installed**: `inillucent-bench` (`requires = ["onnx"]`), `inillucent-remote::lib` (its download
  cases want `INILLUCENT_NETWORK_TESTS` and a real host) and `rag_verify`. These are not the
  worktree gap task-2034 and task-2039 describe - they fail in the main checkout too. Read the
  message before spending time on one. (task-2033)
- **A worktree has no `_agent_output/fixtures/` either, and `gates_fail_closed` *skips* rather than
  fails without it.** The three SQLite fixtures `tools/build-gate-fixtures.sh` builds are gitignored
  and live in the main checkout, so in a worktree the gate suite reports green with seven of its
  cases never run - which is the exact defect that file exists to catch, one level up. `--strict`
  counts them because the row declares `requires = ["fixtures", "sqlite-bench", "testrun"]`, so read
  what it names. Copy `small.db` across before you trust a green: it is 1.2 MB and every case but
  the fullgate one runs on it. (task-2041)
- **~~`inillucent-testrun` exits 0 when the build fails.~~ It never did. `$?` after a shell
  pipeline is the last command's status, not the runner's.** Measured in task-2047 against the
  build failing on `onig_sys`: `inillucent-testrun --tier smoke >/dev/null 2>&1; echo $?` printed
  **1**, and `inillucent-testrun --tier smoke 2>&1 | tail -1 >/dev/null; echo $?` printed **0** —
  which is `tail`. Reading the tail of a 60 KB log is how a run gets read, so the shape is easy to
  hit. Redirect to a file and read the code from the runner.
  **And read all three codes**: `0` passed, `1` the run happened and was red, `2` the run did not
  happen at all — a build that failed, a `--target`/`--tier` pair that matched nothing, a
  `--filter` that matched no test. `2` is new in task-2047; that case used to be `1`, which is why
  a broken toolchain and a real defect were indistinguishable. (task-2041, task-2047)
- **`--manifest-path` does not find your worktree's `.cargo/config.toml`.** The backend writes that
  file inside your worktree pointing `build.target-dir` at
  `D:/agent-worktrees/cargo-target/<project>-<task-N>`, but **cargo looks for it from the directory
  the command was run in, not from the directory the manifest is in.** An agent terminal that never
  uses `cd` runs `cargo build --manifest-path <worktree>/Cargo.toml` from wherever it started, so
  cargo reads the main checkout's configuration, finds none, and builds into `<worktree>/target`
  instead. You still get your own target directory, so the file lock the setting exists for is still
  avoided - what you lose is the D: drive and the cleanup that `worktree/retire` does there. Worse,
  a test that shells out to cargo itself disagrees with you about where the binary is:
  `semantics.rs` builds `inillucent-cli` with its working directory set to the worktree, so the
  shell lands on D: while the test binary looks for it beside itself on C: and reports `a shell is
  missing`. Set `CARGO_TARGET_DIR` instead, which cargo reads wherever it was run from. (task-2026)
- **`--trace` on the profilers prints nothing useful from an ordinary release build.**
  `[profile.release]` sets `strip = true`, so `std::backtrace::Backtrace` in
  `inillucent-prepareprofile --trace` and `inillucent-probeprofile` gives frames with no symbols and
  every per-allocation call site comes back empty. Build a second copy with
  `CARGO_PROFILE_RELEASE_STRIP=none` and `CARGO_PROFILE_RELEASE_DEBUG=2`: debug information does not
  change what the optimiser does, so the counts and the nanoseconds are still the release build's.
  Take a **gate** number from an unmodified release build, though - the profile is part of the
  fairness contract in the root `Cargo.toml`. (task-2026)
- **Some numbers are exact on a loaded box and some are not, and the difference decides whether you
  have to wait for a quiet one at all.** Allocation counts, page fetches, compile counts and log writes read
  the same under any load - `inillucent-prepareprofile --sizes` and the counters in
  `crates/inillucent/tests/budget.rs` are the instruments, and task-2026 bounded a compile at
  exactly 15 allocations on that basis. A **mean** nanosecond figure is not: the same binary
  measured a `SELECT 1` compile at 1,389 ns and 2,435 ns in three runs, and the gate's spread
  between passes of one arm was 1.46x at 12 rounds and 1.40x at 30, which is wider than the 15%
  it was being asked to resolve. The fix was a **minimum** rather than a mean -
  `inillucent-prepareprofile`'s `quietest` column, the fastest of 200 batches - because load can
  only ever add time to a sample. It separated the two arms where every mean overlapped. (task-2026)
- **`crates/inillucent-sql/src/bind.rs` and `crates/inillucent-exec/src/physical/chain.rs` are the
  read path's equivalent of the write path's three files above.** Both are large and central and
  have had more than one ticket in them at once; task-2026 and task-2040 were both inside
  `bind_select_core` on the same evening. Say on the other ticket which functions you are in before
  you start. (task-2026)
- **A fix that widens what the engine accepts fails `inillucent-driver::capability`, by design.**
  `drivers/inillucent-driver/src/capability.rs` declares rows `Support::No`, and the suite runs
  every row in both directions - a denied capability that starts working turns the build red.
  task-2042 made a window function in a compound arm run, and the suite went red naming
  `window_in_compound_arm`. It is not a flake and it is not another agent's: grep that file for a
  row describing what you just fixed and move it to `Support::Yes` in the same commit. When you add
  a row, prefer `Probe::Answers` over `Probe::Runs` wherever the old behaviour *ran* and answered
  something wrong - a `Probe::Runs` row would have called the broken engine supported. (task-2042)
- **Twenty-four targets at a time on a busy box produces failures that are not real.** task-2042's
  `inillucent-testrun --changed` reported `schema_forms`, `analyze_reopen` and
  `multi_database_commit` red with seven agents on the machine; all three pass in seconds when run
  alone, and none of them contains a `UNION` that ticket could have touched. Run a suspect target
  by itself before you spend anything on it. (task-2042)
- **`main` moved four times in the hour it took to finish one ticket.** task-2042 rebased onto
  task-2040, task-2026, task-2034 and task-2041 in turn, and `git merge --ff-only` refused twice
  between the rebase and the merge. Do the rebase and the fast-forward in one command so nothing
  lands in between, and expect the merge, not the work, to be what takes the retries. Both conflicts
  were the same shape: two tickets appending a `Case` to the end of `CASES` in
  `crates/inillucent-compat/tests/differential/semantics.rs`. Keep both sides and close the earlier one's last
  case - that file is the busiest merge point in the repository right now. (task-2042)
- **Do not run two copies of a compat test binary in the same worktree at once.**
  `crates/inillucent-compat/tests/e2e/cli_commands.rs::area` does `remove_dir_all` on a per case
  directory and then recreates it, so a second run of that binary deletes the first run's fixture
  while it is being used. task-2044 started `inillucent-testrun` in the background and then ran
  `cargo test --test cli_commands` beside it, and got nine failures that all pointed at the fixture
  builder and none of which were real. Worktrees do **not** collide with each other over this -
  `workspace_root()` is built from `CARGO_MANIFEST_DIR`, which is baked per worktree, so each
  ticket has its own `_agent_output/`. It is only your own two runs. (task-2044)
- **A page a transaction drops is now freed at its commit, not by the statement that dropped it.**
  `release_tree` puts the pages on `Writing::pending_frees` and `flush_pending_frees` releases them
  from `commit_batch` and `seal`. If you write anything that frees a page mid transaction, do the
  same: the free map is durable shared state and this engine's undo buffer holds *row* before
  images, so a rollback has nothing to put a page back with. Freeing as the statement ran let a
  `CREATE` in the same transaction be handed the dropped table's own root page, which is how a
  rolled back `DROP TABLE p; CREATE TABLE p` lost every row durably. The same rule is why the
  `FreePage` log records are written at the commit: the log is redo only. (task-2043)
- **A savepoint records two lengths — `undo` and `pending_frees` — and both are needed.** `marks` is
  `Vec<(name, usize, usize)>`. `DROP TABLE p; SAVEPOINT here` leaves both records at the same undo
  length, so deriving the second from the first cannot say which came first, and `ROLLBACK TO here`
  then leaks the pages of a table that really was dropped. If you add a third append-only per
  transaction record, it needs its own length in `marks` too. (task-2043)
- **The catalog decides what is a virtual table, not `session_state.virtual_tables`.**
  `rebuild_tables` asks a connected module only for the *columns* of a table the catalog already
  calls virtual. It used to convert any table whose name was in that map, so a rolled back
  `CREATE VIRTUAL TABLE` left the connection reading an ordinary table through a dead fts5
  connection — the file had the rows and the session could not see them. (task-2043)
- **`PRAGMA integrity_check` does not look at page ownership or the free map.** `check_trees` walks
  each registered tree in isolation and then checks indexes against tables, so it answers `ok` about
  a database where two tables reach the same page, or where the catalog names a tree that is not the
  one holding the rows. Do not treat a green check as evidence that a storage change is sound —
  assert the values. (task-2043)
- **`crates/inillucent-engine/src/engine/compiled.rs::write` is 4 lines under its recorded ceiling
  of 220.** Adding one line and a comment to it fails `policy`. Put what you need in a helper on
  `ImportedDatabase` instead; `statement_mark` is there as the precedent. (task-2043)
- **`inillucent-testrun` builds the workspace twice, and a number measured by hand can be wrong in
  the binary it actually runs.** After the default `cargo test --workspace --no-run --lib --tests`
  it builds again with every feature the selected suites ask for - for any ordinary selection that
  includes `inillucent-engine/embed` - and runs the second binary. Both are left in
  `target/debug/deps`, so two `budget-*.exe` sit side by side and disagree. `embed` costs three
  allocations on every compile, which put `crates/inillucent/tests/budget.rs`'s allocation guard
  three over a bound that was right every way it had been checked by hand. If an absolute number
  you measured with `cargo test -p <crate>` fails under the runner and you cannot reproduce it,
  build it with the features the run printed before you look anywhere else. The guard asks the
  engine which build it is in rather than carrying two numbers. (task-2039)
- **An occupied tree handle is not evidence that it holds the right tree - compare the root page.**
  `reattach_entries` in `crates/inillucent-engine/src/engine/batch.rs` decided whether a rollback
  had to re-attach a table by asking whether `schema.trees` had anything under its handle. That is
  right for a rolled back `DROP`, where the handle is empty, and wrong for a rolled back
  `ALTER TABLE`: `rebuild_table` releases the old tree and registers a new one at a **new root
  page under the same handle**, so the restored catalog row and the attached tree described
  different tables for the rest of the connection's life. If you write any other path that
  re-registers a tree under a handle that is already in use, the catalog row's `root` is what says
  which one it is. `PagedTree`'s own `root` field is assigned only in its two constructors, so the
  comparison is safe - a root page does not move under ordinary writes. (task-2051)
- **A rollback test that only asserts in the session that rolled back can be green against the
  bug.** task-2051's `ALTER TABLE ... ADD COLUMN c INTEGER DEFAULT 9` case passed every in-session
  assertion while broken, because the tree the `ALTER` built carries a *superset* of the catalog's
  columns and every read still found its slot. What it lost was writes: the rebuilt tree is an
  orphan no catalog row names, so an `INSERT` after the rollback reported success, read back in the
  same session, and was not in the reopened file. Assert both ways - in the session, because a
  reopen cannot see a connection's stale state, and after a reopen, because the session cannot see
  its own stranded writes. (task-2051)
- **`_agent_output/fixtures/` is missing from a worktree in the same way `.sqlite-ref/` is, and a
  junction handles both.** task-2041 recorded copying `small.db` across; a junction is one command
  and covers `medium.db` and `large.db` too:
  `New-Item -ItemType Junction -Path <worktree>\_agent_output\fixtures -Target C:\jason\dev\inillucent\_agent_output\fixtures`.
  Remove both junctions before retiring the worktree. (task-2051)
- **A position in the new declaration is not a position in the old one, and `DROP COLUMN` is where
  they part.** `rebuild_table_tree` filled each surviving column from `old_layout.slots[declared]`
  with `declared` taken from the **new** declaration, so every column after the dropped one took
  its left neighbour's values and the last column's values were lost. `alter_table` now passes the
  position it removed and the loop maps a new position at or after it back to one higher. Dropping
  the *last* column was always correct, which is why task-2051's reproduction on `(id, n)` did not
  show it, and why `alter.drop.last` in `semantics.rs` is kept as the control against a mapping
  that shifts unconditionally. Anything else deriving a column's identity from its position across
  an `ALTER` owes the same mapping. (found by task-2051, fixed by task-2057)
- **A `SourceLayout` is derived once and `refresh_catalog` does not derive it again.** It rebuilds
  the catalog *snapshot* and the `TableInfo` list; `schema.layouts` is written by whatever created
  the tree and is otherwise left alone. So a `DROP COLUMN` renumbered the declaration while every
  index on the table went on recording its columns at their old positions, the planner offered such
  an index, and the read failed with `the tree read for FROM term 0 does not carry column N`.
  Reopening the file answered it, because `open` derives every layout afresh - so "it works after a
  reopen" is the signature of a stale derived view rather than of a wrong file, and the two
  `semantics.rs` cases `alter.drop.reopen` and `alter.drop.index` are deliberately one of each.
  `refresh_index_layouts` re-derives them. If you add a schema change that moves a declared
  position, the index layouts are yours to refresh too. (task-2057)
- **`rebuild_table_tree` finds its table by folded name with no schema filter, and so does
  `refresh_index_layouts` beside it.** `alter_table` reads the schema into `at` and filters its own
  existence check by it, then does not pass it down - so `ALTER TABLE side.t DROP COLUMN b` with a
  `t` in `main` as well rebuilt the wrong one, and `SELECT * FROM t` on `main` came back with
  side's row. `ALTER TABLE ... RENAME` is correct on an attached database because it rewrites
  catalog text and never reaches the rebuild, which is how to tell the two apart. Every
  `ALTER TABLE` on a `TEMP` table is `no such table` for the related reason that `rebuild_tables`
  deliberately keeps temporary tables out of `schema.tables`. Being fixed by task-2061; if you are
  in either function before that lands, take the schema as a parameter rather than adding a second
  unfiltered lookup. (found by task-2057)
- **A `WITHOUT ROWID` table's primary key is listed among its indexes and carries the *table's*
  root**, because there is one tree and the key is it. Any loop over `TableInfo::indexes` that
  writes something keyed by `index.root` will write over the table's own entry: re-deriving layouts
  that way replaced a keyed table's layout with an index's and broke reads that had been correct.
  Skip the index whose root is the table's. (task-2057)
- **A page written straight into the data file has to carry an LSN, and the rollback journal has
  to be asked before it is written at all.** `Pool::write_built_page` is the only write in the
  engine that skips the buffer pool and the log both, which is what makes `CREATE INDEX` write its
  index once. Two rules come with it, and neither was there until task-2055: the page carries the
  LSN of its own `AllocPage` record, so redo skips the records its page number carried in a
  previous life; and a page the live rollback journal holds a pre-image of is logged instead,
  because nothing replays a built page forward again after a journal has put it back. If you add a
  second caller of `write_built_page`, it owes both. (task-2055)
- **`Applier::page_lsn` asks the file's length, not `Pool::page_count`.** The pool's count is the
  last checkpoint's, and every page allocated since then is past it - so asking the pool answered
  "the file does not hold that page" about pages it was holding, and the page-LSN rule was off for
  the whole tail of the file during recovery. (task-2055)
- **A connection owes a fold at close for a hot rollback journal as well as for a dirty frame.**
  The two are in tension at a small pool: eviction is what clears dirty flags and what fills the
  journal, so the more a session evicts the more certain it was to reach `fold_on_close` with
  nothing dirty and leave the journal behind. A journal is only ever disposed of by a checkpoint.
  (task-2055)
- **A crash test that snapshots after the connection is dropped is testing a close.**
  `ImportedDatabase::drop` folds, so `vfs.crash()` taken after it holds a cleanly closed database
  and passes against an engine that cannot recover. Take the snapshot while the connection is
  still open - `bulk_build_crash.rs::migrated` is the shape. Both new cases there passed against
  the unfixed engine until that was moved. (task-2055)
- **`durability.rs`'s task-2043 cases run at all six matrix arms now**, through `scenario!`. They
  ran at `Database::open`'s default alone, which is 32,768 byte pages and a 4,096 frame pool -
  nothing evicts, no rollback journal is written and the file is folded at every close, which is
  three of the conditions task-2055 needed. A new storage case in that file should go through
  `scenario!` too. (task-2055)
- **A test file invokes `scenario!` or holds a bare `#[test]`, never both**, and `scenarios`
  refuses the mixture by name: "the file grades one story six ways and another once and the run's
  output cannot tell them apart". `durability.rs` hit it the moment six of its cases went through
  the matrix, which is why there is a `durability_arms.rs` beside it now and an
  `inillucent_compat::durable` holding what the two build. If you add an arm-run case to a file
  that has bare tests, the case goes in the arms file. Adding the target also means a
  `tests/selection.toml` row **and** the tier table in `tests/inillucent-testing-tdd.md`, which
  `documentation` compares against the map. (task-2055)
- **`inillucent-compat::tooling::bindings` fails on this machine and it is nobody's ticket.** It reads
  records the npm, go and php conformance runners write into `_agent_output/conformance/`, and
  that directory exists in neither the main checkout nor any worktree - so the failure is "these
  runners have produced no record", on any branch. Produce them with
  `node --test packages/npm/inillucent/conformance.test.mjs`,
  `go test -C packages/go -run TestConformanceSuite ./...` and
  `php packages/php/tests/conformance.php`, or read past it. Do not spend a run bisecting your own
  change against it. (task-2055)
- **And it gets *worse* after you run the whole suite, which is when you are most likely to blame
  yourself for it.** `bindings.rs` treats no records at all as a skip - "a machine that has never
  run them" - and *some but not all* as a hard failure, because that is a runner that stopped
  running. The rust and python conformance runners **are cargo targets**, so a full
  `inillucent-testrun` runs them and writes `rust.json` and `python.json`; the npm, go and php
  runners are not, so they never run. The suite therefore flips from a counted skip to a red
  failure naming npm, go and php the first time you run everything, with nothing about your branch
  having changed. Measured on task-2047: hollow with `needs conformance-records` at 19:05Z, red at
  21:55Z, and the only thing in between was a full run.
  **`go` is not installed on this machine at all**, so producing all three records is not something
  a run here can do, and `bindings` cannot be made green by trying harder. Read past it.
  (task-2047)
- **`small.db` alone is not the whole fixture gap.** task-2041's note says every case but the
  fullgate one runs on it, and `new_engine_log_lead` does not: it wants
  `_agent_output/fixtures/medium.db`, 17 MB, and skips both its cases without it - which is the
  suite whose own header records that they "skipped for months, and that is why the defect
  survived". The junction task-2051 describes covers all three fixtures and is the thing to do.
  (task-2055)
- **`inillucent-testrun --changed` takes no diff once you have committed**, and answers "nothing
  has changed against HEAD / nothing selected" with exit code 0 - which reads exactly like a clean
  run. Pass the base: `--changed origin/main` selects the branch's whole diff. (task-2055)
  **That 0 is deliberate and task-2047 left it alone**: `--changed` asks what the working tree can
  break and "nothing" is a true answer. What task-2047 changed is the other empty selection - a
  `--target` and `--tier` you named by hand that do not overlap, which used to print
  `nothing selected` and exit 0 and now exits 2 naming both. A request that could not be honoured
  is not a passing run; a derived one that is genuinely empty is. (task-2047)

- **`inillucent-migrate`'s SQLite digest folds only the columns the source file stores.** A
  `VIRTUAL` generated column is declared and stored by neither engine, so `TableInventory::digested`
  leaves it out and a `columns.<table>` check compares the whole declared list beside the digest.
  Before that, every database with one in it failed its own verification with correct rows in the
  staging file. If you add another check to `verify_against`, push it for every table rather than
  only the one that failed - the report is read as a list, and a check that appears once reads as a
  defect in that table. (task-2050)
- **`crates/inillucent-compat/tests/e2e/migrate_realistic.rs` is a merge point now.** task-2036 wrote it
  and task-2050 added three cases to the end of it, which is the same shape as the conflicts on
  `semantics.rs`'s `CASES`. Keep both sides. (task-2050)
- **`sh tools/build-realistic-fixtures.sh --check` says whether each checked-in `.db` is what the
  `.sql` beside it builds.** Run it after editing a fixture's SQL. The `.db` is checked in and
  nothing else in the suite notices the two drifting apart. (task-2050)
- **A microsecond measurement of the shipped API is not a measurement `fullgate` makes.**
  `inillucent-fullgate` drives `database.plan()`, `prepare()` and `pipeline()` and never opens a
  `Connection`, so nothing the scorecard or the performance contract grades pays what
  `Database::open` -> `session()` -> `prepare` -> `step` pays. task-2046 found 132 microseconds a
  statement there that no published number could see. `inillucent-prepareperf`'s breakdown table is
  the instrument for that path; `--repeat` makes a run take a second. (task-2046)
- **Timing anything inside `enter`/`leave` means putting timers in them.** There is no profiler
  wired up here and the primitives measured on their own do not add up to what the path costs -
  four 32 KiB reads timed standalone read 10 us while the same four inside the engine read 89.
  A `pub mod` of `AtomicU64` accumulators in `inillucent-engine`, with `Instant::now()` around
  `begin_read`, `the_meta_moved`, `the_log_moved`, `enter_attached` and `release_if_idle`, answered
  it in one build and came back out before the change was committed. (task-2046)
- **A guard about cost goes in `crates/inillucent/tests/budget.rs` and counts something.** That
  file's header carries the measurement for why a wall-clock ratio cannot be a test here: one
  unchanged commit read between 1.18 and 59.22 depending on what else was on the box. If the thing
  you fixed has no count, add one - task-2046 added `meta_reads` and `meta_probes` to `PoolStats`
  and to the connection's `CacheStats` so the guard could assert what a statement outside a
  transaction reads. Then take the fix back out and watch the guard fail, or it is not a guard.
  (task-2046)
- **`begin_read` and `the_meta_moved` are one question asked twice.** Both ask whether another
  process has folded, of the same two slots, microseconds apart under one SHARED lock. If you add a
  third caller on that path, share the answer through `Database::disk_record_is_as_last_read`
  rather than reading the file again - and read `LastReadSlots::record` first, because a full read
  deliberately does **not** let the next caller short circuit. (task-2046)
- **A change inside `inillucent-engine` or `inillucent-sql` selects almost the whole suite, and
  nine targets are red in a worktree for reasons that are not yours.** `inillucent-testrun
  --changed --strict` picked 178 targets for a four-file change in `ddl/` and `directive.rs`,
  because fifteen packages depend on those two, and took 37 minutes. Every test passed, and the
  run still exited 1, because nine suites had no prerequisite: `setup_embeddings` (`programs`),
  `inillucent-remote::lib` (`network`), `bindings` (`conformance-records`), `rag_verify` (`onnx`,
  `shell`), `new_engine_log_lead` (`fixtures`), `inillucent-remote::transport` (`python`,
  `openssl`), `live_mysql` (`mysql`), `live_postgres` (`postgres`) and `gates_fail_closed`
  (`fixtures`, `sqlite-bench`, `testrun`). That is `--strict` doing its job.
  **Read the report at the end, not the `FAILED` lines scrolling past.** A target whose every
  failure is the strict-skip sentinel is left out of the failure list and printed under
  `suite(s) ran without a prerequisite and evidenced nothing`, with what each was missing, so the
  report already separates a missing prerequisite from a defect. The summary line above it -
  `178 target(s), 2157 test(s), 0 failed, 0 undetermined` - is the one that answers whether
  anything is actually broken. Two of the nine even print `ok` on their own line, because they
  skip cleanly, so counting `FAILED` lines undercounts. The last target runs alone and took 36
  minutes of the 37. (task-2061)
- **Most of the note above no longer holds, since task-2125.** Tiers have a cadence: `--changed`
  runs a `durability` or `perf` target only when a crate you edited is in its `covers`, and never a
  `nightly` one, so an engine or parser change no longer drags in the crash suites by closure or the
  hour long stories. `gates_fail_closed` no longer builds a second workspace or runs alone: its
  nested runners read `INILLUCENT_TESTRUN_ARTIFACTS`. On this box, write the gitignored
  `tests/prerequisites.local.toml` (`absent = ["go", "mysql", "postgres", ...]`, whatever a strict
  run names) and `--strict` stops failing for suites this machine can never run; anything it does not
  declare still fails.
- **`inillucent-compat`'s integration tests are seven binaries, one per tier, since task-2125.** A
  suite is `tests/<tier>/<name>.rs` and its target is `inillucent-compat::<tier>::<name>`. A plain
  `cargo test -p inillucent-compat --test engine` runs all 59 engine suites in one process, which is
  not how the runner runs them (one process per suite, `--exact` names). If a suite behaves
  differently under plain cargo, suspect shared process state first, and use the runner.
- **The nightly lives in `J:/build/nightly` and writes `_agent_output/nightly/latest.json` in the
  main checkout.** `ship.ps1` reads that file instead of running a suite. Do not edit or build in
  `J:/build/nightly`: the scheduled task forces it to `origin/main` every night. (task-2125)
- **Copy `.sqlite-ref/` into the worktree rather than junctioning it.** task-2048 records a
  recursive delete of a junction emptying the one shared copy for every worktree at once.
  `Copy-Item -Recurse C:\jason\dev\inillucent\.sqlite-ref <worktree>\.sqlite-ref` costs about four
  seconds and 40 MB and cannot do that, and the directory goes when the worktree is retired.
  (task-2061)
- **What `--strict` counts as a prerequisite failure on this box, in one place.** The list above is
  the same one task-2053 met, plus `inillucent-bench` (`onnx`). To see what one of them actually
  needs, run that target's own binary with `INILLUCENT_STRICT=1` - each prints the command that
  would fix it. (task-2053)
- **The crash campaigns rewrite `tests/crash/*.txt` and `*.tsv` with different line endings.**
  Eighteen tracked files show as modified after any run that reaches `wal_crash`, `vacuum_crash`
  and their kin, with zero content change - `git diff --numstat` on them prints nothing. Restore
  them by name before committing rather than carrying them into a ticket. (task-2053)
- **A new `tests/*.rs` target that can skip needs `requires` in the same commit.**
  `selection::every_target_that_can_skip_declares_it_and_vice_versa` reads the source for a call to
  the skip helper - which includes `let Some(binary) = program("inillucent") else { return; }` -
  and fails if the row declares no prerequisite. Adding one then moves the count in
  `docs/repository.md`'s `<!-- requires:begin -->` table, which
  `documentation::the_prerequisite_table_names_every_value_in_the_map` checks, and a new row in the
  `engine` tier moves the per-tier table in `tests/inillucent-testing-tdd.md`. Three files, and the
  build tells you about them one at a time. (task-2053)
- **`node tools/doc-facts/check.mjs` needs no release build for the two counts about the suite.**
  It reports `test targets` and `selection map rows` from `tests/selection.toml` alone, and both
  were stale by 3 and 24 before task-2053. Run it after touching `selection.toml`; the seven
  `instrument could not answer` lines about verbs and MCP tools need
  `cargo build --release -p inillucent-cli` and are not about your change. (task-2053)
- **An FTS5 index carries a layout record now, in `%_data` row 2.** `fts5/layout.rs` writes it at
  `CREATE VIRTUAL TABLE` and in `wipe_index`, and nowhere else on purpose - an ordinary insert must
  not stamp one, because a file 0.1.2 through 0.1.7 wrote may hold rows in both layouts at once. If
  you change the dictionary layout again, raise `LAYOUT` in the same commit, or a build that cannot
  read what you wrote will answer no rows for it rather than refusing. (task-2053)
- **`inillucent-migrate::corpus` going red is no longer a reason to suspect your own change - but
  read which check it names.** The suite builds its corpus out of `crates/` and `docs/` at run time
  and stops at 400 documents, so adding a file anywhere early in that walk shifts every document
  after it and pushes some off the end. That sensitivity is deliberate and is stated in the suite's
  own header; it is what proved task-2065's engine change innocent. What was **not** deliberate was
  that `filter.deleted` compared two different operations and so could go red on an ordinary commit
  that added a test file: task-2065, task-2066 and task-2068 each met it, and task-2068 measured its
  phase 1 turning the suite red on its own. task-2067 fixed the comparison, so a red `filter.deleted`
  now means a real difference. The other checks are still corpus-sensitive by design, and the way to
  tell a corpus effect from a defect is the method task-2065 used: hold the test binary and vary only
  the prose. (task-2067)
- **Do not edit a file under `crates/` or `docs/` while that suite is running.** It reads every one of
  them with `read_to_string` and skips any whose prose is under 400 characters, so a file caught
  mid-rewrite reads short, gets skipped, and takes one document out of a walk that stops at 400 -
  which shifts every document after it. task-2067 got a red `corpus` in a full run that way and a
  green from the same binary a minute later; a whole sweep of deliberately built corpora passed in
  between. Start the run, then leave the tree alone until it is finished. (task-2067)
- **Copy `.sqlite-ref/` into your worktree rather than junctioning it.** `cp -r
  C:/jason/dev/inillucent/.sqlite-ref <worktree>/.sqlite-ref` costs 42 MB and about ten seconds, and
  retiring the worktree deletes it like any other file. A junction has to be removed with a command
  that does not follow it, and the entry above records the day somebody's `rm -rf` followed one and
  emptied the only copy for every worktree at once. The copy has no way to do that. (task-2067)
- **A full test run leaves thirteen files under `tests/crash/` reported as modified, and they are
  not.** The crash campaign rewrites them with different line endings, so `git status` lists them
  while `git diff --numstat` shows nothing. Restore them by name rather than staging them, and do not
  spend time working out what changed. (task-2067)
- **A test binary's own CPU is not a progress signal when it shells out to the oracle.**
  `inillucent::story_ledger_day_nightly` sits with flat CPU and every thread in `Wait` for minutes at
  a time, and that is the correct state: it replays its day through
  `.sqlite-ref/3.53.4/shell/sqlite3.exe` and blocks on it, so the work is in the child. task-2067 read
  the parent's counter, called it wedged and killed it, and was wrong. Before you conclude anything
  from a flat counter, look for children:
  `Get-CimInstance Win32_Process -Filter "ParentProcessId = <pid>"`, and read *that* process's CPU.
  (task-2067)
- **A migrate or retrieval flake is not reproducible by re-running it, and the corpus is why.**
  `HnswParams::default().build_threads` is `available_parallelism()`, and `Hnsw::build_parallel` says
  itself that it does not produce the same graph as the sequential build: "the order in which nodes
  link to each other is whatever the thread pool produced". Measured on 2026-09-22: one legacy index
  built from a fixed seed in five processes gave five different directories, and one *fixed* index
  migrated in three processes gave three databases of 2,293,760 bytes differing on eleven pages. So
  `inillucent-migrate::corpus` migrates a different file every run, and "ran it again and it passed"
  is not evidence of anything. Set `build_threads` to 1 when you need a run you can compare.
  (task-2070)
- **Two copies of one test binary that share a directory under `_agent_output` produce convincing
  false failures.** Two loops of the corpus binary against `_agent_output/migrate/release` failed
  three times in five runs, two of them with the exact message task-2070 is about. One run's
  `scratch()` removes the directory while the other is mid-migration; the running one keeps writing
  through its now-unlinked handle, its manifest records land in a *new* file at the same path - so
  the manifest begins halfway through with no header lines - and its reopen finds the other run's
  fresh empty database. Before believing a failure from a loop, check there is only one loop.
  (task-2070)
- **A stalled process with no children is not always a hang in that process.** The entry above is
  right and it is not the whole rule. When `inillucent-testrun` itself is the stalled process with no
  children, what is holding it is a grandchild it cannot see: `Command::output()` waits for the
  child's pipes to close, not for the child to exit, and anything the child started with inherited
  standard output keeps that pipe open after the child has gone. The runner now waits on the child
  instead, so this shape should not recur - but the reasoning is the part to keep, because the same
  trap is in any code that reads a child's output. (task-2071)
- **libtest captures what a test prints, so a suite's pipe is not a heartbeat.** A `println!` inside a
  test never reaches the parent while the test is running; what does is libtest's own
  `test <name> ... ok` line as each test finishes. A suite that is one long test - which
  `story_ledger_day_nightly` is - says nothing at all for its whole run. Do not write a check that
  reads silence on a test binary's pipe as trouble. (task-2071)
- **`inillucent::story_ledger_day_nightly` has no row in `tests/timings.toml`.** Nothing in the
  `nightly` tier does, because an ordinary run never selects it, so `--record` has never written one.
  It takes 1800.37s on an idle box and **3568.50s** when two other tickets are running theirs -
  measured on 2026-09-22, both times. Anything that draws a bound from that file has to have a floor
  that clears the loaded number, not the idle one: an hour would have missed it by 31 seconds.
  `inillucent-testrun`'s floor is two hours for that reason. (task-2071)
- **A leaf now comes in two layouts, and the write path has to keep them apart.** Since task-2074
  the file format is 2: a leaf's delta area opens with a directory in key order, a delta index means
  a directory position, and the page checksum covers the LSN. A leaf format 1 wrote has no directory
  (`LeafRef::has_delta_directory` is false) and is still read and written by format 1's rules until
  a compaction or split rewrites it - and that rewrite has to be logged with its page image, because
  recovery replays a format 1 log onto format 1 pages by format 1's rules. So a new write path that
  changes a leaf's delta area either goes through `LeafMut` (which picks the rules from the page) or
  logs an image. `crates/inillucent-model/tests/redo_bytes.rs` compares every page byte for byte
  after a crash, and `release_format.rs` crashes a write into every shipped release's file; run both
  after touching `leaf/`, `mutate.rs`, `write.rs` or `redo.rs`. (task-2074)
- **`perfhistory --only <prefix> --label <name>` takes one series on its own and says which build it
  was.** The index count sweep is `--only insert.indexes`; `inillucent-writeprofile --sweep` is the
  same sweep in process with the write counters. A run whose calibration drift is far from 1.0 is
  worth taking again after a warm up: the box settles for a minute after the other agents pause, and
  the calibration loop is only about 4 ms long, so its drift is noisy on its own. (task-2074)
- **A write that is slow only past the pool is usually an order problem, and the pool's read count
  says so.** `inillucent-writeprofile --spread <rows> <pool MiB>` builds the table of
  `story_large_table_nightly` at both page sizes and prints `reads`, `writes`, `cooled`, `evicted`
  and `rewarms` for `DELETE FROM big WHERE a % 2 = 0`, once with a pool that holds the file and once
  past it. Equal cost in the first and a gap in the second means page traffic, not leaf work. The
  32 KiB delete was 5.4 times the 4,096 byte cost per row because the keys came from the covering
  index in `(d, a)` order; it read one page per deleted row. (task-2077)
- **A leaf whose parent is pinned cannot be cooled, so a descent that loads leaf after leaf of one
  root can fill the pool.** `take_frame` then hands out frames past `PRAGMA cache_size` while any are
  free, and reports "every frame in the buffer pool is pinned" when none are. A pass that reads
  every row of a table before changing any of it hits this; `vacuum_on_vfs` (64 frames) is the test
  that catches it. (task-2077)
- **A test binary launched directly has no `cargo` parent.** Running the prebuilt
  `target/.../deps/story_large_table_nightly-*.exe` keeps a build out of a quiet window, but a check
  that looks for cargo to see whether a measurement is running will miss it. List processes by name
  and CPU, without truncating the list. (task-2077)
- **A test that asserts a defect is still there has to be inverted by the ticket that fixes the
  defect, in the same change.** This repository records known gaps as tests that fail once the gap
  closes: task-2052's page leak cases, the LSN checksum case in `durability.rs`, and the
  `does_not_yet` cases in `planner.rs`. That is a good record, but it only works if the fixing ticket
  runs the strict gate before it merges. task-2074 closed the LSN checksum gap in format 2 and did not
  run it, so `inillucent-compat::durability::durability` was red on `main` from d142334 until task-2078 inverted
  the case, and two agents each spent a diagnosis on it. Before merging a fix, search the tests for the
  gap's name or ticket number. (task-2078)
- **`INDEXED BY` forces the named index and nothing else, and the binder refuses what it cannot
  answer.** `choose_path` returns an `AccessPath` and cannot refuse, so `plan::unanswerable_index_hint`
  (in `plan/hint.rs`) asks the same question first with the same terms, from `bind_arms` and from the
  UPDATE and DELETE binders. If you change which terms a path may use, whether that means
  `statement_terms`, `outer_terms` or `index_usable`, change them there, or the binder's refusal and
  the planner's choice disagree. (task-2078)
- **A field added to `scalar::Context` has to be one of two kinds, and the kind decides whether a
  statement can be re-run.** `changes`, `total_changes`, `last_insert_rowid` and `seed` move between
  two executions, so reading them goes through `Params::context()`, which counts as a parameter read
  and stops `Statement::rebindable`. `length_limit` and `like_case_sensitive` are settings, read
  through `Params::settings()`, which does not count; instead a kept chain (`Compiled`, `Statement`,
  `UpdateSetup`) records the settings it was built under and is rebuilt when they differ. A new
  counter goes in `Context::settings()`'s zeroed half and in `builtin::reads_execution_constants`; a
  new setting goes in the kept half. Before this, every `%`, `/`, `||` and scalar call counted, and
  `inillucent-fullgate` refused `WHERE a.id % 100 = 0`. (task-2081)
- **Before chasing a gate ratio that slipped, read this engine's own time in the same row.** The
  `ours ns` and `theirs ns` columns are both printed. task-2074 reported `join.range` falling from
  0.90x to 0.84x and `read.join`'s lower bound going under 3.00x; this engine's time for it was
  33.0 to 33.6 ms on every build across 27 quiet passes, and 27.2 to 27.6 ms pinned to the performance
  cores, while SQLite's arm drifted from 24.8 to
  31.6 ms inside one window, and the lower bound followed SQLite. A round also reads before it
  writes, so no write change can reach a read workload's leaves at all. And pin the gate before you
  compare it with anything: unpinned, the scheduler put this engine on the efficiency cores and SQLite
  on the performance cores (task-2064), and task-2085 makes the gates pin themselves. (task-2082)
- **A family's lower bound printed before task-2093 is mostly the gap between its workloads.**
  Until then every gate bootstrapped one list of every workload's every round; for `read.join` (21x
  and 0.87x) that gave [2.8x, 6.5x] around two workloads each measured to within 2%. Every gate now
  calls `perf::family_interval`, one value a round, rounds resampled. Do not compare a family bound
  from before task-2093 with one from after it; compare the family ratio, which is the same under
  both. (task-2086, task-2093)
- **A family's per round interval covers one pass, not the next one.** On four pinned full gate
  passes of `main`, eight of ten families moved between passes by more than their interval's half
  width; `read.join` moved 5.2% against a half width of 1.5%. A family within about 5% of its bar
  needs several passes before its verdict means anything. `extension` is that family now. (task-2093,
  follow up task-2095)
- **Before reading a family's move between passes as the engine, read the SQLite arm's.** It is the
  same `sqlite-bench.exe` in every pass whatever build is under test, so its time over the read
  workloads says how fast the box was. In task-2093's window it ran 5% to 15% slow on some passes,
  and those are the passes that moved the ratios. In task-2095's settled window it stayed within
  1.4% for 29 minutes. With other processes using 16% to 37% of the CPU it was 8% to 20% slow while
  this engine's arm was 5% to 11% slow, so **a busy machine inflates every ratio**, `read.join` to
  4.57x against 4.19x to 4.27x quiet. Several passes in a row on a busy machine are all inflated
  together, so averaging them does not help; check the SQLite arm instead. `inillucent-fullgate --samples <file>` writes every round's raw times for both
  arms with timestamps, and `_agent_output/task-2095-between-pass/rolling.js` (main checkout) turns a
  pass's samples into the SQLite arm's speed index per round. (task-2095)
- **`inillucent-fullgate --engine-child` is a diagnostic, not a better gate.** It runs this engine's
  arm in a new process each round, as the SQLite arm always runs. It removed most of what our arm
  carries from pass to pass in one process (the short read workloads, `point.miss` 6.4% to 1.3%),
  but it also times the first touch of every page, which put `schema` under the floor and made
  `extension` miss its bar on five of six passes. Use it to ask whether a move comes from the gate's
  process, and never to take a published figure. (task-2095)
- **D: filled up on 2026-09-24 from worktrees that were never retired.** Fifteen finished tickets'
  target directories held 1.64 TB under `D:/agent-worktrees/cargo-target`, task-2068's alone 295 GB.
  A build failing with os error 112 means that again; check `Get-PSDrive D` before blaming the
  build, and a build can go to C: through `CARGO_TARGET_DIR`. (task-2095)
- **To print two statistics from the same samples, build twice, not run twice.** task-2093 built
  `main` with an extra line after each family line, copied the binaries to
  `_agent_output/task-2093-family-statistic/bin/both/` (main checkout), then restored the sources by
  name. The older builds got the same one extra line through `perf.rs` and the gate's family loop.
  Two separate runs would leave every difference open to the objection that the samples differed.
  (task-2093)
- **To walk a gate back through history, pin from outside and use the read gate.** Builds before
  `d389021` cannot pin themselves, so `_agent_output/task-2086-read-join/pinned-walk.ps1` (main
  checkout) sets `0xC03C03` on the gate process and reads the SQLite child's mask back on every pass.
  HEAD gets the same external pin, so no build is treated differently. `inillucent-readgate` takes
  21 s a pass against 2.5 minutes for fullgate, so a 36 pass bisection fits in 13 minutes. A detached
  worktree checkout needs `git checkout -- Cargo.lock` first, because some builds rewrite it and the
  next checkout then fails and leaves the old binary in place. `.sqlite-ref/3.53.4` has to be copied
  in whole, the shell included. (task-2086)
- **To find which change inside one commit costs the time, switch each change off at that commit,
  and remember that a format switch reverts only what is written.** task-2091 built
  `inillucent-readgate` at `a8f45b1` with `FRAME_OF_REFERENCE = false`, with `heap_slot_width`
  forced to 8, and with `CARGO_PROFILE_RELEASE_PANIC=unwind`, one binary each, by editing the
  detached worktree and restoring the files by name between builds
  (`_agent_output/task-2091-join-range/build-variants.ps1`, main checkout). The format switches read
  zero, and the real cost was the code that reads the new format, which those switches leave in
  place. What found it was one more binary with every switch off, whose file had the same page count
  as the commit before, so the only difference left was code. A release profile can be ablated
  without a source change through `CARGO_PROFILE_RELEASE_*`. Five binaries build in about eight
  minutes into one shared target directory. (task-2091)
- **Three in four of the `transaction` family's updates match no row.** `Bind::Scatter` picks rowids
  up to `main_table`'s row count and `side_table` holds a quarter of that, so those statements measure
  a lookup past the end of `side_table`'s last leaf - the leaf `write.insert.autocommit` appended 100
  rows to earlier in the round. A change to how a leaf's delta area is searched shows up in
  `txn.large` for that reason, not because of the in-place update it describes. (task-2082)
- **Count before you time, when the box is loaded.** `inillucent-fullgate --families write,transaction
  --put-split` prints how many writes of each workload went through the tree's put path, and that
  count does not move with load. It separated two hypotheses in minutes that timing on a box at 30%
  could not: the same binary varied 20% run to run there. (task-2082)
- **The shared `.sqlite-ref/` was empty again on 2026-09-23 (last written 05:07).** Every worktree
  junctioned to it then had no oracle and the gates refused with "sqlite-bench is not built".
  task-2082 restored it by copying a worktree's intact 3.53.4 directory back; `pwsh
  tools/sqlite-reference.ps1` rebuilds it from the published sources if no copy exists.
  The 2026-09-21 emptying was a recursive delete through a junction; remove one with `cmd /c rmdir`.
  (task-2082)
- **Every program that times this engine against SQLite now pins itself to the performance cores,
  and a figure from before that is an efficiency core figure until shown otherwise.** This box has 8
  performance cores (mask `0xC03C03`) and 16 efficiency cores (`0x3FC3FC`), and unpinned Windows
  sometimes ran the gate process on the efficiency cores and `sqlite-bench` on the performance cores.
  Read the `cores` line of a report before comparing it with another: two runs are comparable only
  when both say the same class and mask. `--cores any` takes the unpinned figure on purpose.
  Do not set the mask from outside with a script any more; the program sets its own and prints it,
  and on Windows a mask set from outside is replaced unless the run passes `--cores any`. A new timing program calls
  `inillucent_compat::affinity::pin_from_arguments` at the top of `main` and starts its reference
  arm through `affinity::spawn_on_same_cores`. (task-2085)
- **The correlated workloads get faster with more cores, so a pinned run reads them slower.** On
  2026-09-23 `correlated.exists` took 59.69 ms on the eight performance cores, 46.35 ms on the sixteen
  efficiency cores and 38.74 ms with all twenty four. Every other workload was fastest on the
  performance cores. Compare the `read.correlated` rows only between runs that name the same mask.
  (task-2064)
- **`perfhistory` rows record `dirty` whenever the working tree has any edit**, documents included,
  so a history row taken while a doc rewrite is open reads `<sha>-dirty` although the engine is
  clean. Commit or stash the documents first if the row should name a clean commit. (task-2064)
- **Putting a source file back with PowerShell `Copy-Item` keeps its old modification time, and
  cargo then does not rebuild.** task-2087 built the old version of `constant.rs` to prove a test
  failed before the fix, put the fixed file back with `Copy-Item`, and the next `cargo test` ran the
  old code: the corpus reported nine refusals the fix had already removed. After copying a file
  back, set `(Get-Item <path>).LastWriteTime = Get-Date` or write it with a bash redirect, and look
  for `Compiling <crate>` in the output before trusting the run. Do not restore a file with
  `git show ... | Set-Content` either: PowerShell joins the lines and the crate stops compiling.
  (task-2087)
- **`bind.rs` and `expr/tree.rs::compile` sit at their `policy.rs` ceilings, and a unit test
  module counts toward the file.** task-2088 added four fields to `BETWEEN` and a test module to
  `bind.rs`, and `inillucent-compat::tooling::policy` failed at the end of a 30 minute `--changed` run on
  `no_module_grows_past_the_size_it_is_recorded_at` and
  `no_function_grows_past_the_length_it_is_recorded_at`. Put binder tests in a `bind/` submodule
  next to the code they test, and run `cargo test -p inillucent-compat --test tooling policy::` on its own
  before the full run when an edit adds lines to either. (task-2088)
- **A suite that shells out to `inillucent-shell` builds it while the run is going, so edits made
  during a baseline run end up in that baseline.** `numeric_text.rs` calls `cargo build -p
  inillucent-cli` from inside the test. A baseline started before an edit therefore measures the
  edit if the target runs after it. On task-2080 that made the baseline show 3 differences where
  the code at `HEAD` had 7. Do not edit engine code while a baseline is running, or take the
  baseline in a second worktree. (task-2080)
- **A double is printed through `inillucent_value::fpdecode`, which transcribes SQLite's
  `sqlite3FpDecode`.** `real_to_text` and every `printf` real conversion go through it. Do not use
  Rust's `{:e}` or `{:.N}` to print a value SQLite would print. Those are correctly rounded, and
  SQLite's last digit sometimes is not. `numeric_text.rs` fails on any difference. (task-2080)
- **A test that holds a file from a second process waits for that process to say so, never for a
  fixed time.** `process_concurrency.rs` slept 1.5 s after writing `BEGIN; INSERT` to a shell and
  then expected a second writer to be refused. Under the runner's full parallel load the shell had
  not run the `INSERT` yet, the second writer took the file first, and the case reported the engine
  admitting two writers. Send a `SELECT '<marker>'` after the statements and read the shell's output
  until it appears: `held_after` in `process_concurrency.rs` and `Fed::until` in `busy_timeout.rs`
  both do this. A `BEGIN` alone holds nothing, so the marker goes after the first write. (task-2090)
- **Timing an old build of a gate means applying `column-once.patch` to it, or the step you find is
  where that change landed.** Every build before `3acd7ac` pays four directory lookups per
  `LeafRef::column` call, and the cost of those lookups moved by a factor of about six between
  `a07036b` and `dcc65f2` without the number of calls moving. So a walk across that span with the
  change on one end only reports a step that is the compiler's, not the commit's.
  `_agent_output/task-2099-join-range/apply-fix.js` makes the same change on the three older shapes
  of the leaf reader, and its `count` variant counts `column` calls. (task-2099)
- **A gate binary looks for `sqlite-bench` under the worktree it was compiled in, not beside
  itself.** Remove or move that worktree and every binary built there prints "sqlite-bench is not
  built" and exits in a second, while a gate built elsewhere keeps working. Keep the build tree's
  `.sqlite-ref` junction in place for as long as you run its binaries. (task-2099)
- **Seven first-parent commits from 2026-09-07 do not compile**: `e1a245b`, `52d68f0`, `21efbf8`,
  `6e19c0b`, `81855a7`, `0f24df5` and `889c1ff`. The two whose errors were read fail in
  `inillucent-vm`: `e1a245b` on a missing `like_case_sensitive` in `builtin::Context`, `6e19c0b` on
  `StaticCatalog` against `CatalogSnapshot` in `machine.rs`. A bisect has to step over them.
  (task-2099)
- **`git checkout -- <file>` on a foundation crate costs a full rebuild even when the content did
  not change.** Cargo reads the mtime, and checkout rewrites the file. Measured: an edit to
  `crates/inillucent-base/src/lib.rs` recompiled 28 crates and relinked 280 test binaries in 91 s,
  and the checkout that reverted it cost the same 92 s again. A rebuild with nothing edited is 0.4 s,
  and an edit to `inillucent-cli/src/main.rs` is 1.9 s. There is no cheap revert, because any
  write moves the mtime, so do not probe a foundation crate to see what rebuilds. Where the time
  of a ticket goes, with the numbers: `tasks/task-2114-inillucent-build-times-tdd.md`. (task-2114)
- **To measure `examples/rag-agent/rust-example` against the engine in your worktree, copy the example
  to a scratch folder and add a `[patch.crates-io]` table** naming `inillucent` and
  `inillucent-engine` by path into the worktree. The workspace version equals the published one, so
  Cargo takes the patch and the other internal crates follow by path. Build with the MSVC environment
  imported. A first `sync` of the corpus embeds 3,696 chunks and took 11 minutes on the processor;
  keep the synced database and copy it for each variant rather than syncing again. (task-2130)
