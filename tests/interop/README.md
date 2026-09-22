# `tests/interop/` — a database from every release we have shipped

Each subdirectory holds a database **written by that release's own binary**, the log segment it
left, and the answers it gave. `crates/inillucent-compat/tests/release_format.rs` opens them with
the build under test and asks the same questions again.

This is the only check in the tree that can answer *"can today's engine still read what we shipped
last month"*, and it can only answer it because the file was written by that release rather than
described by one.

## The releases

| version | published | signature | log segment | note |
|---|---|---|---|---|
| 0.1.1 | 2026-09-12 | none | 456 B | the oldest fixture. Releases were not signed until 0.1.3, so only the `SHA256SUMS` digest is checked for this one. **It cannot read an FTS5 index written by a later build** — see below. |
| 0.1.2 | 2026-09-14 | none | 33,792 B | |
| 0.1.3 | 2026-09-19 | minisign | 33,792 B | the first signed release |
| 0.1.5 | 2026-09-20 | minisign | 512 B | |
| 0.1.6 | 2026-09-20 | minisign | 512 B | |
| 0.1.7 | 2026-09-20 | minisign | 512 B | |

There is no 0.1.4. It was never published: the npm packages `inillucent@0.1.3` and `@0.1.4` shipped
broken and an npm version number can never be reused, so the release after 0.1.3 was 0.1.5.

Every `app.rdb` is **622,592 bytes**, which is 19 pages of 32,768. Git stores each one at about
6.6 KB. Every `expected.tsv` is 3,812 bytes, LF, and all six are byte for byte identical — which is
the point of keeping one per release rather than one shared file: a release that answered
differently would be the finding, and a shared file could not say which release it was.

## Building one

```powershell
pwsh tools/build-interop-fixture.ps1 -Version 0.1.6
```

It downloads that release's Windows archive and `SHA256SUMS` into `tools/cross/bin/releases/`,
which is gitignored and shared with the main checkout the way the cross toolchain is. It verifies
the archive against `SHA256SUMS`, and `SHA256SUMS` against `packaging/inillucent.pub` with minisign
when the release published a signature. Then it runs `build.sql` with that release's
`inillucent.exe`, checkpoints, writes one more row, and records the answers to `verify.sql`.

`packaging/ship.ps1` calls it in the publish phase for the version being shipped, in a commit of
its own after the tag — the fixture is built from the *published* archive, so it cannot exist
before the release does. `release_format.rs::the_release_script_builds_the_shipped_versions_fixture`
is what stops that call being quietly removed.

## Two things the design asked for that are not here, and why

**Both page sizes.** The design asked for a fixture at 4,096 bytes and one at 32,768. A released
binary cannot choose a page size: it is an argument to `Database::open_at`, the command line has no
flag for it, and `PRAGMA page_size` reports the page size rather than setting one. So every fixture
is at the engine's own 32,768, and the smaller page is covered where it can be — by `matrix.rs`'s
`sqlite_page` arm, which drives the library directly.

**Files under 300 KB.** 19 pages of 32,768 is 622,592 bytes and the rows are a small part of it:
each tree root costs a page whether it holds five rows or five thousand. The fixture holds one of
each kind of storage rather than a lot of any of them, and what it costs in the repository is the
6.6 KB git stores it as.

## The row that only the log holds

Each fixture was written, checkpointed, and then written to once more. That last row — `note` 9001,
`the row that only the log holds` — is in `app.rdb-wal.*` and nowhere else. A build that opened the
database and ignored the log would answer 120 rows where `expected.tsv` says 121. Replaying an
older release's log is the part of the format most likely to move and the part an ordinary read
would never reach.

This is also why nothing ever opens a fixture in place: opening a database replays its log and
writes the row into the file, which would leave the checked-in fixture different from the one the
release produced. Every reader stages a copy first, and `inillucent_compat::interop::stage` is what
they all call.

## What 0.1.1 cannot read

`release_format_history.rs`, in the `nightly` tier, asks the other direction: an older binary
reading a file **this** build wrote. Every release answers every question identically except one.

```
0.1.1  SELECT rowid FROM note_fts WHERE note_fts MATCH 'segment'   ->  (no rows)
0.1.2 and later                                                    ->  1;4
```

The FTS5 index layout changed in 0.1.2. 0.1.1 reads everything else in the same file — the tables,
the `WITHOUT ROWID` entries, the blob over a page, the `inillucent_search` rows, the row that only
the log holds — and it reads `count(*)` and `SELECT rowid, title` out of `note_fts` itself. Only
the `MATCH` comes back empty, and it comes back **empty rather than refused**, because nothing in
the index said which layout wrote it. That silence is **task-2053**. The 0.1.1 half is history: a
published binary's answer can never be fixed, so it is recorded in `KNOWN_GAPS` as a difference
that is asserted to *still happen*, and a change that made 0.1.1 read the new index turns that
suite red and gets the row deleted.

What task-2053 changed is the next one. An FTS5 index written from this build on carries a layout
record — one `%_data` row naming the layout and the release that wrote it — an `inillucent_search`
table's `%_config` names its release beside the format number it already carried, and a reader that
meets a layout or a format it has not got refuses with the status `unsupported` naming both, rather
than answering nothing. `docs/relational-architecture.md` §5a states the promise for every layout in
the file; `crates/inillucent-compat/tests/format_refusal.rs` checks each refusal, including the
command line's exit code.

## The retrieval half, asked backwards

`verify.sql` asks an `inillucent_search` table for its rows and its content, which is a read of
`%_content`. It never asks it to **search**, so until task-2053 nothing in the suite had run a term
query, a ranked query or a nearest-neighbour query from an older binary against a graph a newer
build wrote — the half of the file SQLite has no equivalent of, and the half a format change is most
likely to move.

`retrieval-build.sql` and `retrieval.sql` are that question. They are **not** part of a checked-in
fixture and have no `expected.tsv`: the six recorded files hold what `verify.sql` asked, and adding
rows to a table those questions count would change every one of their answers. Instead
`release_format_history.rs` writes one database with the current build, creates a
`mode = 'approximate'` table over twelve vectors beside the rest of `build.sql`'s tables, compacts
it into a stored generation, and asks both builds the same questions of the same file. The reference
answer is the current build's own, which is the point: the comparison is between two builds, not
between a build and a recording.

Every release from 0.1.1 on answers all of them identically, so `KNOWN_RETRIEVAL_GAPS` is empty.

## Format 2: every release above refuses a file this build writes

task-2074 moved the file format from 1 to 2: a leaf's delta area has a directory kept in key order,
and a page's checksum covers its LSN. Every fixture here is format 1, and this build still reads all
of them - `release_format.rs` reads each one, writes to it, kills the writer and recovers. The other
direction does not hold any more, and cannot: no release up to 0.1.7 can read a format 2 file.

What each of them does instead is refuse, and none of them answers from the file:

```
0.1.1, 0.1.2, 0.1.3   ->  database disk image is malformed: neither meta page is readable
0.1.5, 0.1.6, 0.1.7   ->  Error [unsupported]: this database is format version 2 and this build
                          reads version 1; upgrade inillucent to open it
```

The first three predate the refusal by name, which task-1979 added. `release_format_history.rs`
asserts both answers against the released binaries, and it decides which a release gets by reading
the format number out of that release's own fixture - so the first release that writes format 2 is
graded on its answers again with nothing to edit.
