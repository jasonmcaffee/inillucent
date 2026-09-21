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

<!-- Add what the next agent needs to know: the hot files, the parts that cannot be edited in
     parallel, the build that takes twenty minutes, the test that only passes on an idle box. -->

- **A worktree has no `.sqlite-ref/`, and every gate binary needs it.** `inillucent-fullgate`,
  `inillucent-writegate` and the oracle suites look for `.sqlite-ref/3.53.4/sqlite-bench.exe` and
  `.sqlite-ref/3.53.4/shell/sqlite3.exe` under the workspace root, which in a worktree is the
  worktree. The directory is gitignored so it exists only in the main checkout. Copy the two
  binaries across before you run a gate; the failure message names the missing program and not the
  reason. (task-2034)
- **The whole test suite needs the MSVC environment imported first.** `onig_sys` compiles oniguruma
  with cl.exe and an agent terminal has no `INCLUDE`, so `inillucent-testrun` fails the build with
  `cannot open include file 'stddef.h'`. Dot-source `packaging/stage-layout.ps1` and call
  `Import-MsvcEnvironment` in a PowerShell session before running it. (task-2034)
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
- **`inillucent-testrun` cannot build from an agent terminal until the MSVC environment is
  imported.** `onig_sys` compiles oniguruma with `cl.exe` and an agent terminal has no `INCLUDE`,
  so a `--changed` run dies at `regenc.h(39): fatal error C1083: Cannot open include file:
  'stddef.h'` after building everything else. AGENTS.md names it for the release path and
  `packaging/stage-layout.ps1` has `Import-MsvcEnvironment`, but nothing in the test path calls it.
  Dot-source that function, or run vcvars64 and copy its variables in, before `cargo test` or
  `inillucent-testrun`. It costs a whole build to find out. (task-2039)
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
- **`inillucent-testrun` exits 0 when the build fails.** A `--changed` run that dies on
  `onig_sys` prints `inillucent-testrun: the build failed` and then exits 0, so a shell that
  branches on the exit code reads a failed build as a passing suite. Read the last line, not `$?`.
  (task-2041)
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
  `crates/inillucent-compat/tests/semantics.rs`. Keep both sides and close the earlier one's last
  case - that file is the busiest merge point in the repository right now. (task-2042)
- **Do not run two copies of a compat test binary in the same worktree at once.**
  `crates/inillucent-compat/tests/cli_commands.rs::area` does `remove_dir_all` on a per case
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
- **`ddl/alter.rs::rebuild_table` has a second, unrelated defect in it - see task-2057.**
  `ALTER TABLE ... DROP COLUMN` on a column that is not the last one fills each surviving column
  from `old_layout.slots[declared]`, where `declared` is the position in the **new** declaration, so
  every column after the dropped one takes its left neighbour's values. It is on `main`, it needs no
  transaction, and dropping the last column is correct - which is why task-2051's own reproduction
  on `(id, n)` did not show it. If you are in that function, the two tickets will collide. (task-2051)
