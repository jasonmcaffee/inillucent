# task-1816: Inillucent rearchitecture: a fast engine, not a SQLite-compatible one

Status: accepted design, implementation starts from this document  
Supersedes: the performance half and the storage/VM design of `task-1781-sqlite-feature-parity-tdd.md`  
Reference engine for correctness and speed comparisons: SQLite 3.53.4 (unchanged pin)  
Scope: this document changes no runtime behavior; every phase below is a separate work package

Every number in this document is one of two kinds, and each is labelled: **measured** means it was
read off a scorecard run or a profile on this machine; **estimate** means it is a prediction from the
design and from published numbers for comparable engines. An implementer must not treat an estimate as
a promise; the phase gates exist to turn estimates into measurements early.

## Decision in one page

inillucent stops being a reimplementation of SQLite and becomes a fast embedded relational engine that
speaks SQLite's SQL dialect. The engine is rebuilt from the storage layer up:

- **Storage**: a B+tree with PAX (column-within-page) leaves, pointer swizzling, optimistic
  version-latches, a buffer pool with a cooling FIFO, and an out-of-line blob extent store. No SQLite
  page format, no cell format, no overflow chains, no rollback journal, no pointer map, no VACUUM.
- **Execution**: a push-based vectorised executor over batches of column vectors that borrow directly
  from pinned leaf pages, with closure-compiled expressions and a separate compiled point-probe path.
  No bytecode VM.
- **Transactions**: snapshot isolation by MVCC with a single writer at a time, a physiological redo
  WAL with group commit, fuzzy background checkpoints, ARIES-style recovery under a no-steal policy.
- **Front end**: the existing lexer, parser, binder and planner algebra survive; prepare gets a
  per-statement arena and a plan cache.

Two things that were non-negotiable in task-1781 are explicitly abandoned: **byte-level SQLite
file-format compatibility** and the **pinned C ABI**. One thing from task-1781 is kept without
change: the SQLite **SQL dialect** and its documented semantics (types, affinity, collation, NULL
rules, error messages where tests depend on them, built-ins, PRAGMAs that still make sense).

The correctness bar does not fall with the format. The differential digest gate against real SQLite
3.53.4 continues over an **imported fixture** (same SQL, same logical data, different files), a
**model reference** (`BTreeMap`-backed) replaces SQLite as the oracle for storage and crash
behaviour, `inillucent-sim`'s failpoints and crash snapshots apply to the new storage layer unchanged
because it keeps the `inillucent-vfs` trait boundary, and the SQLLogicTest corpus stays as the semantics
oracle.

The performance target moves from "1.50x SQLite" to a design target of a **3.0x weighted geometric
mean** with **no family below 1.0x**, and a per-family contract in which the CPU-bound families are
expected at 3-10x and the fsync-bound ones at parity. The first phase is a falsifiable go/no-go: a
PAX leaf plus a vectorised scan must reach **5x SQLite on `read.analytical`** in four weeks or the
program stops and reports.

The existing hybrid-search/retrieval consumer keeps a stable Rust API, its legacy-index import path,
and its "at least 1.50x a configured baseline" gate. Nothing it depends on is deleted until its
replacement is green on that gate.

## Goals and non-goals

### Goals

1. A single-process embedded relational database in Rust whose CPU-bound paths are several times
   faster than SQLite 3.53.4 on the ten scorecard families, measured by the existing
   correctness-gated paired harness (`crates/inillucent-compat/src/bin/scorecard.rs`) with the
   checked-in weights in `compat/perf/contract.toml`.
2. SQLite's SQL dialect and observable SQL semantics, verified by the same SLT corpus and the same
   differential digests as before.
3. ACID with the same durability policy vocabulary as SQLite (`synchronous` `OFF`/`NORMAL`/`FULL`)
   so that paired benchmarks stay fair, verified by `inillucent-sim` crash and fault campaigns and a
   model reference.
4. A stable Rust API (`inillucent::Database`, `Connection`, `Statement`) for the hybrid-search engine
   and the CLI, and a migration path from every existing on-disk artifact (legacy retrieval index
   generations and the SQLite-format files produced during task-1781).
5. Every phase shippable: at the end of every phase the repository builds, the surviving tests pass,
   and a scorecard can be produced.

### Non-goals, stated so nobody has to infer them

- **SQLite file-format compatibility.** SQLite will not open our files and we will not open
  SQLite's, except through a test-only and migration-only reader (see triage).
- **The C ABI.** `sqlite3.h` does not link against us. `inillucent-capi` is deleted. A C shim can be
  written later against the Rust API if a consumer appears; it is not part of this ticket.
- **Multi-process access to one database file.** One process opens a database; inside it, many
  connections. File locks exist only to refuse a second process. Removing this deletes the
  rollback-mode lock state machine, the WAL-index shared memory, and most of `inillucent-vfs/locks.rs`.
- **Multiple concurrent writers.** One writer transaction at a time, readers never block. The
  version-chain design leaves room for optimistic multi-writer later; it is not built now.
- **A JIT with a code generator.** Expressions are compiled to closures. Cranelift/LLVM are out of
  scope; see "Execution engine" for why.
- **`ATTACH` across files, `VACUUM`, backup-to-SQLite, `sqlite3_serialize` byte compatibility,
  incremental blob I/O on the SQLite API shape.** `ATTACH` returns later as attaching another
  inillucent file; the rest is dropped.
- **Async I/O (io_uring/IOCP) in this ticket.** The scorecard is single-connection and
  warm-cache; async I/O buys overlap for checkpointing and cold reads, not single-statement latency.
  Designed for (the I/O boundary is a trait), not delivered.

### What is thrown away

The bytecode VM and its verifier; the SQLite page codec, cell codec, record codec, overflow chains,
freelist, pointer map and vacuum; the pager in its current shape; the rollback journal, super-journal
and hot-journal recovery; the SQLite WAL frame format and WAL-index; the multi-process lock state
machines; the C ABI; every test that asserts a SQLite byte layout, an opcode sequence, a journal or WAL
frame, or a lock transition. The triage table at the end names each crate.

### Measurable success criteria

| Gate | Release criterion |
|---|---|
| Phase 1 go/no-go | `read.analytical` lower 95% bound at least 5.0x SQLite at medium scale, digest-equal |
| SQL semantics | 100% pass on the pinned SLT corpus through the new engine; exclusions name the SQLite-specific reason |
| Differential corpus | Zero unexplained result, type, column-name, row-count or error differences against SQLite over the imported fixture |
| Model reference | Zero divergences between engine and model across the operation-trace corpus, including after every simulated crash |
| ACID | Zero torn, lost-acknowledged, dirty, non-repeatable or forked-history outcomes across the `inillucent-sim` fault matrix |
| Robustness | No panic, UB, leak, hang or out-of-bounds access on malformed SQL, corrupt pages, corrupt WAL, injected OOM and I/O faults |
| Performance | Weighted geomean lower 95% bound at least 3.0x SQLite; no family below 1.0x; per-family contract met |
| Search consumer | Hybrid-search scorecard at least 1.50x its configured baseline with no quality regression, on the new engine |
| Migration | Every legacy generation and every task-1781 SQLite-format fixture imports with verified counts and digests |
| Portability | Windows x64 and Linux x64 pass |

## Definitions

- **Page**: the unit of the buffer pool and the file. One size per database, chosen at creation,
  default 32 KiB, allowed 8, 16, 32, 64 KiB. *Estimate*: 32 KiB is the right default for PAX scans
  at this row size; Phase 1 measures 16/32/64 and fixes the default.
- **Frame**: a page-sized slot in the buffer pool with a stable address.
- **Swip**: an 8-byte child reference in an interior page. In memory it is either a frame pointer
  (low bit 0) or a page id (low bit 1, "unswizzled"). On disk it is always a page id.
- **Tree**: one B+tree. Every table is a rowid-clustered tree; every index is a key-to-rowid tree;
  the catalog is a tree; blob extents are not a tree.
- **Batch**: up to 2048 rows as column vectors, the unit of the executor.
- **Commit timestamp (cts)**: a global monotonic `u64` assigned at commit; snapshots read as of one.
- **LSN**: a `u64` byte position in the WAL stream, monotonic across segments.
- **Scale**: the scorecard's small/medium/large (5k/100k/600k rows); gates are stated at medium and
  reported at all three.

## Architectural overview

```
inillucent (public API: Database / Connection / Statement)
  └── inillucent-session      connections, statements, pragmas, plan cache, result sinks
        ├── inillucent-sql     lexer, parser, binder, planner algebra, physical planning   (survives, extended)
        ├── inillucent-exec    batches, operators, closure compiler, point probe            (NEW)
        ├── inillucent-catalog schema objects stored in the engine's own catalog tree       (rewritten)
        ├── inillucent-txn     MVCC, snapshots, undo buffers, commit, group commit          (NEW; replaces inillucent-transaction)
        ├── inillucent-tree    B+tree over PAX leaves, swizzling, latches, bulk build       (NEW; replaces inillucent-storage)
        ├── inillucent-pool    buffer pool, cooling FIFO, writeback, blob extents, free map (NEW)
        ├── inillucent-wal     WAL segments, record codec, checkpoint, recovery             (NEW)
        └── inillucent-vfs     file I/O trait, OS backends, memory VFS                      (survives)
              └── inillucent-sim  failpoints, media model, crash snapshots                  (survives, wired to the new crates)
inillucent-ext / inillucent-search / inillucent-core   JSON, FTS5, R-Tree, hybrid retrieval          (storage adapters rewritten)
inillucent-model                               BTreeMap reference engine for traces         (NEW, test-only)
inillucent-sqlite-reader                       read-only SQLite 3 file reader               (NEW, from inillucent-storage's read path; test + migrate only)
inillucent-compat / inillucent-bench / inillucent-migrate / inillucent-cli                              (survive, repointed)
```

Dependency direction is strictly downward. `docs/invariants/layering.toml` is updated in Phase 1 to
the new crate set; the existing layering check keeps enforcing it.

## Storage engine

### File layout

One data file `<name>.rdb` plus WAL segments `<name>.wal/<seq:08>.log`. The data file is an array of
pages. Page 0 is the meta page. Pages are addressed by `PageId(u64)`; page 0 is never a tree page.

Meta page (page 0), little-endian throughout:

| offset | size | field |
|---|---|---|
| 0 | 8 | magic `b"RDB2\0\0\0\0"` |
| 8 | 4 | format version, `1` |
| 12 | 4 | page size in bytes |
| 16 | 8 | page count |
| 24 | 8 | catalog tree root page id |
| 32 | 8 | free-map first page id |
| 40 | 8 | checkpoint LSN: every page write with `page.lsn <= this` is durable in the data file |
| 48 | 8 | commit timestamp watermark at the checkpoint |
| 56 | 8 | WAL segment sequence at the checkpoint |
| 64 | 8 | database generation (incremented on every checkpoint; used by the plan cache and by `Database::open` to refuse mismatched WAL segments) |
| 72 | 16 | database uuid |
| 88 | 4 | crc32c over bytes 0..88 and 92..page size |
| 92 | .. | reserved, zero |

The meta page is written twice per checkpoint (a shadow copy at page 1 with the same layout; the
reader takes whichever of page 0 and page 1 has a valid checksum and the higher generation). This is
the only double-write in the design.

Every page after the meta pages carries a 32-byte common header:

| offset | size | field |
|---|---|---|
| 0 | 8 | page LSN of the last modification |
| 8 | 4 | crc32c over bytes 12..page size, computed on writeback, verified on read from disk |
| 12 | 1 | kind: `1` interior, `2` leaf, `3` blob extent, `4` free map, `5` unused/free |
| 13 | 1 | flags (leaf: bit 0 `has_exceptions`, bit 1 `has_tombstones`, bit 2 `has_delta`) |
| 14 | 2 | level (0 for leaves) |
| 16 | 8 | tree id |
| 24 | 8 | right sibling page id (leaves and interiors; `0` for none) |

### PAX leaf layout

A leaf stores up to `row_count` rows sorted by key in a column-major region, plus a small row-major
**delta area** for recent inserts, plus a tombstone bitmap for deletes. The key columns are the first
`key_columns` entries of the column directory: for a rowid tree, one `Int64` column; for an index
tree, the indexed columns followed by the rowid.

Leaf header, after the common header:

| offset | size | field |
|---|---|---|
| 32 | 2 | `row_count`: rows in the sorted region |
| 34 | 2 | `delta_count`: rows in the delta area |
| 36 | 2 | `column_count` |
| 38 | 2 | `key_columns` |
| 40 | 4 | `heap_start`: offset of the variable-length heap (grows downward from the page end) |
| 44 | 4 | `delta_start`: offset of the delta area |
| 48 | 8 | `max_cts`: commit timestamp of the last modification (MVCC fast path, see below) |
| 56 | 8 | low fence key page-order hint (rowid trees: the smallest rowid; index trees: unused) |
| 64 | .. | column directory, `column_count` entries of 8 bytes |

Column directory entry:

| offset | size | field |
|---|---|---|
| 0 | 1 | physical type: `1` Int64, `2` Float64, `3` Text, `4` Blob, `5` Any (row-major tagged values; used for columns with no affinity fast path) |
| 1 | 1 | flags: bit 0 nullable, bit 1 part of key |
| 2 | 2 | fixed width in bytes (`8` for Int64/Float64, `0` for variable) |
| 4 | 4 | offset of the mini-column within the page |

Mini-column, fixed-width type (Int64, Float64): a **class array** of 2 bits per row, padded to 8
bytes, followed by `row_count` values of the fixed width, 8-byte aligned. Class values: `0` NULL,
`1` typed value present in the array, `2` **exception** (the row holds a value that does not fit the
physical type, for example text in an INTEGER-affinity column; the value lives in the heap as a tagged
value and the array slot holds its heap offset as a `u32` in the low half). Class `3` is reserved.

Mini-column, variable-width type (Text, Blob): the same class array, then `row_count` entries of
`(u32 offset, u32 length)` into the heap. Length with the top bit set means the entry is a 16-byte
out-of-line reference in the heap (see blob extents). Text is stored as UTF-8 bytes with no
terminator; the database encoding is UTF-8 only.

Mini-column, `Any`: the class array, then `row_count` `u32` heap offsets of tagged values (the tagged
value encoding is: 1 type byte, then Int64/Float64 little-endian, or a varint length and bytes).

The `has_exceptions` flag on the leaf is set when any class array holds a `2`. The vectorised scan
checks the flag once per leaf: a leaf without exceptions yields column vectors that borrow the
arrays directly; a leaf with exceptions goes through the per-row generic path for that leaf only.
*Estimate*: exceptions are rare enough in every scorecard workload that the fast path covers 100% of
leaves; the design is still correct if they are common, just slower for those leaves.

Tombstones: a bitmap of `row_count` bits at `delta_start - ceil(row_count/8)` (padded to 8), present
only when `has_tombstones` is set. A tombstoned row is invisible; its space is reclaimed on
compaction.

Delta area: `delta_count` rows, row-major, each `u16 length` followed by the row encoded as tagged
values in column order (keys first). Rows in the delta area are unsorted. Limits: `delta_count <= 32`
and the delta area may not exceed 1/8 of the page (*estimate*: 32 is enough that a random-insert
workload compacts every 32 inserts per leaf, which is a few hundred bytes of memmove amortised across
32 inserts; Phase 3 measures 16/32/64).

Readers merge the delta: point lookup binary-searches the sorted region, then linearly scans the delta
(at most 32 comparisons); range and full scans sort the delta rows by key once per leaf visit (at most
32 elements) and merge. A leaf with an empty delta and no tombstones is the fast path and is the
common case after compaction.

**Compaction** rewrites the leaf in place into a fresh frame image: sorted rows minus tombstones plus
delta rows, resorted, mini-columns rebuilt, heap rebuilt without holes. Triggered when the delta
limit is hit, when the heap has no room for an insert, when tombstones exceed 25% of `row_count`, or
before a split. It is a single-page operation logged as one WAL record (`CompactLeaf`, see WAL).

**Insert** into a leaf: if the row's key falls in the sorted region's range or after it, and the delta
has room and the heap has room, append to the delta; otherwise compact; if after compaction the row
still does not fit, split. **Split**: compact, then move the upper half of rows (by count, or by bytes
for variable-width-heavy leaves so that both halves are at most 60% full) to a new leaf, link
siblings, insert the separator into the parent. **Delete**: set the tombstone bit, or remove the row
from the delta. **Update**: for a fixed-width, same-class, non-key column, write in place; otherwise
tombstone plus delta insert. Leaves merge with a sibling when below 25% full after a delete; the
merge is a compaction of both into one.

Invariants (checked by `debug_assert!` and by the integrity checker):

1. Within a leaf, sorted-region keys are strictly increasing and no delta key equals a live
   sorted-region key.
2. `heap_start >= end of the last mini-column` and `delta_start + delta bytes <= heap_start`.
3. A class-`1` slot holds a value of the column's physical type; a class-`2` slot's heap offset
   points at a tagged value whose type is not the column's physical type.
4. The union of live rows in a leaf lies in `[low fence, parent separator)`.

### Interior pages

Interior header (after the common header): `u16 count`, `u16 key_columns`, `u32 heap_start`, then a
slot array of `count` entries `(u32 key offset, u32 key length, u64 swip)` and a final
`u64 rightmost_swip`. Keys are encoded with the **memcmp-comparable key encoding**: Int64 as
big-endian with the sign bit flipped, Float64 as the IEEE bits with the usual order-preserving
transform, Text as bytes with `0x00` escaped as `0x00 0xFF` and terminated by `0x00 0x00`, NULL as a
single `0x00` byte before everything, with a leading type byte per column so mixed-type SQLite
ordering (NULL < numeric < text < blob) is preserved. Collations other than BINARY are encoded through
the collation's key function (NOCASE: case-folded; RTRIM: trailing spaces stripped) so that every
interior comparison is a `memcmp`. Rowid trees store only the 8-byte transformed rowid.

### B+tree, pointer swizzling, and latches

Each frame carries a 64-bit **version latch**: bit 63 exclusive, bits 0..62 version. Readers take an
optimistic read (`v = latch.load()`, fail if exclusive, do the work, re-check `latch.load() == v`);
writers CAS the exclusive bit and increment the version on release. Readers who fail validation
restart from the root; after 4 restarts they descend with shared latches (a reader count in a
separate `u32`) so a hot writer cannot starve them. This is LeanStore's scheme.

Descent: start at the root frame (cached in the tree object), optimistic-read the interior, binary
search the slot array, read the swip. If the swip is a frame pointer, follow it; if it is a page id,
take the exclusive latch on the parent, ask the pool to load the child, store the frame pointer into
the swip (swizzle), release. Validation of the parent read happens before the child is used. A leaf
read for a point lookup is fully optimistic: copy the projected values out, then validate; on failure
retry.

Because there is one writer transaction at a time, tree mutations never race each other; the latches
exist for the reader/writer and eviction/reader races. Eviction of a frame requires its exclusive
latch and unswizzles the parent's swip under the parent's exclusive latch; a reader validating the
parent afterwards restarts.

Invariants:

5. A frame pointer swip is only ever stored while the child frame is pinned by the pool as
   "referenced by parent"; the pool cannot evict a frame whose parent still holds a pointer swip.
6. A page written to disk contains no frame pointers; writeback translates every swizzled swip in the
   I/O copy back to a page id (the in-memory frame is left as is).

### Buffer pool and cooling FIFO

The pool is one virtual reservation (`VirtualAlloc` reserve / `mmap` `PROT_NONE` + commit on demand)
of `frames * page_size` bytes so that frame addresses are stable for swizzling. Default size: 25% of
physical RAM or 4 GiB, whichever is smaller; configurable by `PRAGMA cache_size` in pages for dialect
continuity.

Frame states: **hot** (swizzled, in use), **cooling** (in the cooling FIFO, swip unswizzled, page
still resident), **free**. A background clock samples random hot frames when the free list drops
below 2% of the pool and moves 10% of the pool into cooling. A descent that hits an unswizzled swip
whose page is in the cooling FIFO removes it from the FIFO and re-swizzles it without I/O. The tail
of the FIFO is evicted: if dirty and its LSN is above the durable WAL LSN, the WAL is flushed first
(write-ahead rule); the page is written back with translated swips; the frame goes to the free list.

Dirty tracking: a per-frame dirty bit plus `first_dirty_lsn`; the checkpointer consumes the dirty
list in page-id order to write sequentially.

For every scorecard workload the whole database fits in the pool; the eviction path therefore only
has to be correct in this ticket, not fast. It is exercised by the `inillucent-sim` campaigns with a pool
of 64 frames.

### Blob extents and large values

A value longer than `page_size / 8` (4 KiB at the default page size) is stored out of line. An extent
is a run of pages of kind `3`; each carries the common header plus `u64 next_extent_page` and
`u32 payload_length`; payload follows. The allocator asks the free map for a contiguous run large
enough for the whole value and falls back to chaining runs. The in-leaf reference is 16 bytes:
`u64 first page id, u64 total length`. Reading a large value is one descent plus a contiguous read;
`large.values` is memcpy-bound on both engines (*estimate*: 1.5-2x from avoiding SQLite's 4 KiB
overflow chain walk and per-page checksumless copies; measured in Phase 4).

### Free map

Free pages are tracked in a bitmap spread over kind-`4` pages, one bit per page, chained by the common
header's sibling field. Allocation scans from a per-tree hint; freeing clears the bit. Every
allocation and free is a WAL record so recovery reconstructs the map; the map pages are ordinary
pool pages and are checkpointed like any other.

### Trees per table and index

A table is a rowid-clustered tree keyed by `Int64 rowid` (`INTEGER PRIMARY KEY` is the rowid;
`WITHOUT ROWID` tables are keyed by their primary key columns with the same leaf layout and
`key_columns > 1`). A secondary index is a tree keyed by the indexed columns plus the rowid, with no
non-key columns unless the index is declared with `INCLUDE`-style covering columns (dialect extension,
off by default). `UNIQUE` is enforced by probing before insert.

### Bulk build

`CREATE INDEX` and bulk loads use a bottom-up builder: read the source tree, encode the keys, sort in
memory (or an external merge with 64 MiB runs through a temp file on the VFS), then pack leaves left
to right at a 90% fill factor, then build interior levels from the separators, then publish the root
in the catalog. Every packed page is a WAL record (`WritePage` with the full image, so redo is a
copy). *Estimate*: 3-5x SQLite on `schema.index`, because SQLite sorts then does per-key
append-inserts through its cursor, and packing avoids the per-key path entirely.

## Transactions and MVCC

### Model

Snapshot isolation. A transaction takes a snapshot `s = latest committed cts` at its first read
(`BEGIN DEFERRED`) or at `BEGIN IMMEDIATE`. Reads see every version with `cts <= s` and nothing
later. One writer at a time: `BEGIN IMMEDIATE`, or the first write of a deferred transaction, acquires
the writer slot (`busy_timeout` semantics apply; a second writer waits or returns `SQLITE_BUSY`'s
dialect equivalent). Readers never wait.

### Versions

Pages hold the newest version of every row, including uncommitted rows written by the active writer.
A per-writer **undo buffer** holds, for every modified row, the before-image (`tree id, key, old row
bytes or "absent"`), in order. On commit the writer's undo buffer is appended to the global
**version log** with the transaction's `cts`; on rollback it is applied in reverse to the pages.

A reader with snapshot `s` reading a leaf checks `leaf.max_cts`: if `max_cts <= s` (the common case:
no writer has touched the leaf since the snapshot), the page is read as is. Otherwise the reader
consults the version log for entries on that tree with `cts > s` and substitutes before-images for
the affected keys (and hides rows the entry says were absent). The version log is an in-memory
`BTreeMap<(tree id, key), Vec<(cts, before-image)>>` plus a per-cts index; it is garbage-collected
when the oldest active snapshot passes an entry's `cts`.

Uncommitted rows from the active writer are visible only to that writer: a reader with `s` less than
the writer's not-yet-assigned `cts` always sees `max_cts > s` on touched leaves and finds the
before-images in the writer's own undo buffer, which the version log exposes as "pending" entries.

Invariants:

7. A leaf's `max_cts` is greater than or equal to the `cts` of every committed change on it and is
   set to `u64::MAX` while the active writer has an unflushed change on it.
8. A version-log entry is never discarded while any snapshot older than its `cts` is active.
9. Rollback restores every modified page to a byte-for-byte equal live-row set as before the
   transaction (layout may differ).

### Savepoints

A savepoint is a position in the undo buffer; `ROLLBACK TO` applies the undo buffer in reverse down to
that position and truncates it; `RELEASE` drops the position. Nested savepoints are positions.

### Commit and group commit

Commit: the writer appends a `Commit{txn, cts}` record to the WAL buffer, publishes its undo buffer
to the version log tagged `cts`, then waits on the **commit gate**. The log writer thread drains the
WAL buffer, writes it, calls `sync` according to the `synchronous` policy (`FULL`: sync every commit;
`NORMAL`: sync at checkpoint and every 64 MiB; `OFF`: never), then wakes every waiter whose commit
LSN is at or below the durable LSN. Concurrent committers share one write and one sync. On a single
connection this is still one write and one sync per commit, exactly what SQLite's WAL mode does under
the same policy, which is why `transaction.autocommit` is expected at parity and not above.

The `cts` is assigned under the commit gate in LSN order so that commit order equals visibility
order equals log order.

## WAL, checkpoint and recovery

### Segments and records

The WAL is a sequence of 64 MiB segments. Segment header: `magic b"RDBWAL01"`, `u32 format`,
`u64 segment sequence`, `u64 first LSN`, `u128 database uuid`, `u32 crc32c`. A segment whose uuid or
sequence does not match the meta page is refused.

Record header, 32 bytes: `u32 total length`, `u32 crc32c over the rest of the record`, `u64 lsn`,
`u64 txn id`, `u8 kind`, 7 bytes zero. Records are 8-byte aligned. Kinds:

| kind | payload | redo action |
|---|---|---|
| `InsertRow` | tree id, page id, encoded row | insert into the named leaf (delta or compaction path); if the page's LSN is at or above the record's, skip |
| `DeleteRow` | tree id, page id, key | tombstone or delta removal |
| `UpdateInPlace` | tree id, page id, key, column index, new fixed value | write the slot |
| `CompactLeaf` | tree id, page id, full page image | copy the image |
| `SplitLeaf` | tree id, left page id, right page id, parent page id, full images of all three | copy each image whose page LSN is below the record's |
| `MergeLeaf` | same shape as split | same |
| `WritePage` | page id, full image | copy (bulk build, free map, interior rewrite) |
| `AllocPage` / `FreePage` | page id | set/clear the free-map bit |
| `Commit` | txn id, cts | mark the transaction committed |
| `Abort` | txn id | mark aborted (written when a rollback happens after records were flushed) |
| `Checkpoint` | checkpoint LSN, cts watermark | recovery start marker |
| `CatalogChange` | serialized catalog delta | applied to the catalog tree via the ordinary row records; this record only invalidates plan caches on replay |

Redo is idempotent by page LSN: a record is applied to a page only if `page.lsn < record.lsn`, and
applying sets `page.lsn = record.lsn`. Multi-page records apply per page under the same rule.

Policy: **no-steal, redo-only.** A page dirtied by an uncommitted transaction is never written to the
data file. Rollback is done in memory from the undo buffer, so the WAL needs no undo records and
recovery needs no undo pass. Consequence and limit: a transaction can dirty at most the pool
(the reservation grows on demand up to the configured maximum; beyond that the transaction fails
with the dialect's `SQLITE_FULL` equivalent). *Estimate*: `txn.large` at the scorecard's largest
scale dirties well under 1 GiB. A steal policy with undo records is the documented follow-up if a
consumer needs transactions larger than memory.

### Checkpoint

A background thread checkpoints when the WAL since the last checkpoint exceeds 256 MiB or 30 seconds,
or on `PRAGMA wal_checkpoint`. Steps: read the durable LSN `d`; take the dirty list; write every dirty
page whose changes are all from transactions with `cts` committed at or below `d` (no-steal makes
this "every dirty page not touched by the active writer"); sync the data file; write the meta page and
its shadow with `checkpoint LSN = d` and the new generation; delete segments entirely below `d`.
Foreground work continues throughout (fuzzy checkpoint); a page written by the checkpointer and dirtied
again afterwards is simply dirty again.

### Recovery

On open: read the meta page (or its shadow), verify segment chain from the checkpoint's segment
sequence, first pass: scan records from the checkpoint LSN to the end, stopping at the first record
whose length or crc fails (torn tail) and collecting the set of committed txn ids; second pass:
replay every record of a committed transaction in LSN order under the page-LSN rule; truncate the WAL
after the last valid record; rebuild the free map if any `AllocPage`/`FreePage` was replayed;
set `latest cts` from the last commit. Recovery is idempotent: running it twice yields the same file.

Invariants:

10. Durability order: a data page is never written with `page.lsn > durable WAL LSN`
    (write-ahead), and a commit is never acknowledged before its record is durable under the
    configured policy.
11. The checkpoint LSN in the meta page is never above the durable WAL LSN, and every page whose LSN
    is at or below it is in the data file.
12. Recovery after a crash at any point yields exactly the committed prefix: every acknowledged
    commit present, no unacknowledged one visible.

Invariant 12 is the one the model reference checks after every simulated crash.

## Memory and allocation

- **Prepare arena**: one bump arena per statement (`bumpalo` or an in-tree bump allocator; the
  dependency policy already allows allocators). AST nodes, bound expressions, plans and the compiled
  operator tree are arena-allocated and freed together at `finalize`. Target: a point-query prepare at
  under 2 us and under 10 heap allocations (*estimate*; measured 12 us and 246 allocations today).
- **Execution arena**: one bump arena per statement execution, reset on `reset()`; batch buffers,
  hash tables' growth and sort runs come from it, so a `step()` loop allocates nothing after warm-up.
- **Value representation**: a 16-byte `Value` (tag + payload; text and blob payloads are
  `(ptr, len)` borrowing a page, the arena or the statement's bind storage). Owned values exist only
  at the API boundary (`column_text` returns a borrow valid until the next `step`, as SQLite does).
- **Plan cache**: per connection, LRU of 256 entries keyed by `(sql bytes, catalog generation,
  prepare flags)`, holding the compiled operator tree template; a hit clones the template into the
  new statement's arena (a few hundred bytes) and binds parameters. Invalidated on every
  `CatalogChange`.

## Execution engine

### Batches

```
struct Batch<'p> {
    len: u32,
    sel: Option<&'p [u16]>,          // selection vector; None means all rows
    pins: PinSet,                     // leaf frames this batch borrows from
    columns: &'p [Vector<'p>],
}
enum Vector<'p> {
    Int64  { values: &'p [i64], class: &'p ClassBits },
    Float64{ values: &'p [f64], class: &'p ClassBits },
    Text   { offsets: &'p [(u32,u32)], heap: &'p [u8], class: &'p ClassBits },
    Blob   { .. same .. },
    Any    { values: &'p [Value<'p>] },   // generic path: exceptions, delta rows, computed mixed types
    Const  ( Value<'p> ),
}
```

A `TableScan` over a leaf without exceptions, tombstones or delta produces vectors that point straight
into the frame; nothing is copied until an operator needs to materialise. The `PinSet` keeps the frame
pinned and its optimistic version is validated when the batch is released; a failed validation
re-reads the leaf (the scan is restartable per leaf). Batches never cross a leaf boundary in the fast
path; small leaves are coalesced by copying into the arena only when a downstream operator asks for
`min_batch` (hash join build does).

### Operators

Push model: every operator implements `push(&mut self, batch: &Batch) -> Flow` and
`finish(&mut self) -> Flow`, where `Flow` is `Continue` or `Stop` (for `LIMIT`). Pipelines are built
by the physical planner as a chain from a source to a sink; pipeline breakers (`HashAggregate`,
`Sort`, `HashJoin` build side, `Materialize`) end one pipeline and start another.

| operator | notes |
|---|---|
| `TableScan` | rowid tree, forward/reverse, optional rowid range, projection pushdown, pushdown of `col op const` predicates evaluated column-wise before any vector is exposed |
| `IndexRangeScan` | index tree, key range from the planner, covering or feeding `RowidLookup` |
| `RowidLookup` | batch of rowids → rows from the table tree; sorts the batch by rowid first to keep descents sequential |
| `Filter` | compiled predicate → selection vector |
| `Project` | compiled expressions → new vectors |
| `SimpleAggregate` | no `GROUP BY`; typed accumulators (`count`, `sum` with i64 overflow to f64 per SQLite, `avg`, `min`, `max`, `total`, `group_concat`), generic accumulator for user functions |
| `HashAggregate` | grouped; group keys interned to `u32` ids when the key column is Int64 or low-cardinality Text (dictionary built per statement); generic path otherwise |
| `Sort` | in-memory sort of materialised rows on memcmp-encoded keys; external merge through the VFS above 256 MiB |
| `TopN` | bounded heap for `ORDER BY ... LIMIT n` with `n <= 65536` |
| `Limit` / `Offset` | |
| `HashJoin` | build the smaller side into a hash table keyed on memcmp-encoded join keys; probe in batches; inner, left, semi, anti |
| `IndexNestedLoopJoin` | per outer row, probe an index; the right choice for selective joins (`join.selective` is already 1.54x on this shape, measured) |
| `NestedLoopJoin` | correlated subqueries and the no-index cross product fallback |
| `Distinct` | hash set on encoded rows |
| `SetOp` | `UNION`/`UNION ALL`/`EXCEPT`/`INTERSECT` |
| `Window` | partition + sort + frame evaluation; the frame logic ports from `inillucent-vm/window.rs` |
| `Materialize` | CTEs, subquery results, `IN (SELECT ...)` sets |
| `ValuesScan` | literal rows |
| `VtabScan` | virtual table protocol (FTS5, R-Tree, hybrid search) with the batch interface; a vtab may return rows one at a time and the operator batches them |
| `Insert` / `Update` / `Delete` sinks | constraint checks, `ON CONFLICT`, index maintenance using the row's already-encoded keys, trigger invocation, `RETURNING` |
| `ResultSink` | buffers rows for the pull-style statement API (`step()` pulls one row; the pipeline is driven until the sink has one) |

### Closure compilation

An expression tree is compiled once per statement into a tree of closures
`Box<dyn Fn(&Batch, &mut VectorOut, &mut Ctx) -> Result<()>>`. Compilation is specialised by the
static type the binder assigns from column affinity where the input vector is typed
(`Int64 + Int64`, `Text = Const`, `Int64 < Const` and so on) and falls back to the generic tagged
path for `Any` vectors and for a batch whose leaf had exceptions. This is "closure compilation" as in
the Cloudera/Neumann surveys: 2-4x over a switch-dispatched interpreter (*estimate*, from published
comparisons; measured against the old VM in Phase 1), with no code generator, no executable memory,
and nothing to compile at prepare time beyond allocating closures in the arena.

### The point-probe path

A plan of the shape "one table, equality on the rowid or on a unique index prefix, optional simple
predicate, projection, no aggregate, at most one row" is compiled into a `PointProbe` object instead
of a pipeline: it descends the tree with swizzled pointers, finds the row (binary search in the
sorted region, then the delta), evaluates the predicate on the row's values in place, and writes the
projected values into the statement's result slots. No batch, no vector, no selection. Target: under
500 ns per probe for a warm three-level tree (*estimate*; SQLite measures roughly 1 us on this
machine for `point.rowid`). The same object serves `IndexNestedLoopJoin` as its inner probe and the
`UNIQUE` check in the insert sink.

### Physical planning

The logical planner (`inillucent-sql/plan.rs` and `cost.rs`) keeps its algebra and rewrites (predicate
pushdown, subquery flattening, `IN` to semi-join, constant folding). A new physical pass chooses:

- `PointProbe` when the shape above matches.
- `IndexRangeScan` when a predicate restricts a prefix of an index; covering when the projection is
  inside the index key; else `RowidLookup`.
- `HashJoin` when the estimated inner cardinality is at least 1,000 rows and either no index covers
  the join key or the estimated outer cardinality times the probe cost exceeds the build cost;
  `IndexNestedLoopJoin` otherwise.
- `TopN` when `ORDER BY` and `LIMIT` co-occur with `n <= 65536`; `Sort` otherwise.
- `HashAggregate` with interning when the group key is Int64 or Text; generic otherwise.

Statistics: per table, row count; per column, an HLL distinct-count sketch and min/max maintained
incrementally by the write sinks, plus a 64-bucket equi-depth histogram built by `ANALYZE` and
rebuilt automatically when the row count moves by more than 20% since the last build. Join ordering
is a DP over up to 8 relations, greedy beyond.

`EXPLAIN` prints the physical operator tree; `EXPLAIN QUERY PLAN` keeps the SQLite-style text that the
SLT corpus does not assert on. `PRAGMA inillucent.force_plan = '<operator list>'` forces a physical
choice for the metamorphic tests.

## Catalog and DDL

The catalog is a rowid tree with the row shape `(id, kind, name, table name, root page id, sql text,
column directory blob, stats blob)`; `sqlite_schema` is a built-in view over it so that dialect users
and the SLT corpus keep working. DDL runs inside the writer transaction like any write: the catalog
rows change, a `CatalogChange` record bumps the catalog generation, and every plan cache is
invalidated. `DROP TABLE` frees the tree's pages through the free map in the same transaction.
`ALTER TABLE ADD COLUMN` appends a column directory entry with a default; existing leaves gain the
column lazily on their next compaction, and a scan of a leaf that predates the column synthesises the
default. `ALTER TABLE DROP COLUMN` marks the directory entry dropped and compaction reclaims it.

## Public API and consumer story

`inillucent::Database::open`, `open_with`, `open_with_vfs`, `connect`, `Connection::prepare`,
`execute_batch`, `query`, `interrupt`, `set_progress_handler`, `Statement::{bind, step, column_*,
reset, finalize}` keep their signatures (`crates/inillucent/src/lib.rs`). `deserialize`/`serialize`,
`backup_*` and `blob_open` are removed from the Rust API in Phase 2 and return, if at all, in a later
ticket with inillucent semantics.

The hybrid-search engine (`inillucent-search`, over `inillucent-core`) is a virtual-table module and an index
method; its contract is the `RetrievalIndex` trait (`crates/inillucent-search/src/adapter.rs`) and the
shadow-table store (`store.rs`). Under this design its shadow tables become ordinary rowid trees; the
vector column stores fixed-width `f32` vectors as a `Blob` mini-column, which is a contiguous array
per leaf and the layout its scan wants. The retrieval scorecard (`inillucent-bench grade`) is re-run on
the new engine in Phase 5 and must reach at least 1.50x its configured baseline with no quality
regression before the old storage is deleted. Until then `inillucent-search` keeps compiling against the
old `inillucent-storage` behind a cargo feature (`legacy-storage`).

`inillucent-migrate` keeps its resumable, verified copy shape (`copy.rs`, `verify.rs`, `manifest.rs`) and
gains two sources: the legacy generation format it already reads, and SQLite 3 files through
`inillucent-sqlite-reader`. Verification stays: row counts, per-table content digests, the fixed query
pack, and the source is never modified or deleted.

## Correctness architecture

The performance design only earns its place if the correctness bar stays where task-1781 put it.
Four oracles replace the one shared-file oracle.

### 1. Differential digests against SQLite 3.53.4 over an imported fixture

The scorecard, the `oracle` protocol and the differential SQL harness in `inillucent-compat` already
drive both engines from one plan file and compare digests (`scorecard.rs` lines 380-410 prepare and
step both engines identically). The only change: the inillucent side loads its database by importing the
SQLite fixture file through `inillucent-sqlite-reader` into a fresh `.rdb` (a `fixtures.rs` step), instead
of opening the same file. From then on every workload, every SLT case, and every differential corpus
query is digest-compared row for row: values, types, column names, row counts, error classes. A
timing counts only when the digests agree, exactly as today. This catches everything the current gate
catches except file-level interop, which is no longer a goal.

### 2. The model reference

New test-only crate `inillucent-model`: tables as `BTreeMap<Key, Row>`, indexes derived on demand,
transactions as a copy-on-write map with a commit log, snapshots as map versions. It implements the
same `Engine` trait the real engine exposes for traces: `begin`, `insert`, `update`, `delete`,
`point`, `range`, `scan`, `commit`, `rollback`, `savepoint`, `crash`, `recover`. A trace driver
(`tests/traces/`, TSV like the existing `tests/crash/*.tsv`) runs each operation against both, compares
every read result, and after every `crash` restores the engine from the `inillucent-sim` snapshot,
recovers, and compares the full content with the model's state at the last acknowledged commit
(invariant 12). Traces come from three sources: hand-written cases for each invariant, the
SLT corpus's DML replayed as a trace, and a seeded random generator (`inillucent-sim/schedule.rs` already
has the scheduler and RNG) that runs nightly with shrinking on failure.

### 3. `inillucent-sim` on the new storage

The new crates do all file I/O through `inillucent-vfs::VfsFile` (`read_exact_at`, `write_all_at`,
`file_size`, `truncate`, `sync`), so `SimVfs`, its `Failpoints` (`fail_nth_call`, per-site policies),
`MediaModel` (torn sectors, lost unsynced writes) and `CrashSnapshot` apply without change. Required
campaigns, each a test that runs to green in CI:

- **fail-the-Nth-call**: for each of commit, rollback, checkpoint, recovery, bulk build, run once to
  count sites, then fail each call in turn with each `Failure`; after each, recover and compare with
  the model.
- **crash-at-every-sync** and **crash-at-every-write**: `SimVfs::crash()` after the k-th operation for
  every k, with the media model dropping unsynced sectors and tearing the last one; recover; compare.
- **small pool**: the same traces with a 64-frame pool, so eviction, unswizzling and the write-ahead
  rule are exercised.
- **corrupt page / corrupt WAL**: flip each byte of a page and of a record; the reader must return a
  corruption error, never panic; the recovery must stop at the torn tail and not before.

`inillucent-sim` becomes a dev-dependency of `inillucent-pool`, `inillucent-tree`, `inillucent-wal` and `inillucent-txn`
in Phase 2; it currently is a dependency of nothing but `inillucent-compat`.

### 4. SLT, fuzzing, metamorphic planner tests

- The SLT runner (`inillucent-compat/src/bin/slt.rs`) and corpus run unchanged against the new engine
  from Phase 2 (read-only subset) and Phase 3 (all).
- The parser fuzz targets in `fuzz/` survive; new targets: leaf decoder, interior decoder, WAL record
  decoder, memcmp key decoder. Corpus coverage is merged into the coverage report.
- Metamorphic tests: every SLT `SELECT` and every scorecard query is run under each applicable
  `PRAGMA inillucent.force_plan` alternative (hash join vs index nested loop, `TopN` vs `Sort`, scan vs
  index) and must produce the same digest; predicate-pushdown and join-order permutations likewise.
- Property tests: the tree (random insert/delete/lookup against a `BTreeMap`, with compaction and
  split boundaries forced by small pages), the memcmp key encoding (order preserved for random typed
  tuples under every collation), the class-array codec, the WAL codec.

### Coverage and mutation

The same tiers as task-1781, applied to the new modules: 100% branch on the leaf codec, interior
codec, memcmp key codec, WAL record codec, class-array codec and the latch state machine, with
documented unreachable branches through one named helper that panics in test builds and is the only
thing the coverage config excludes; at least 95% branch on tree, pool, txn and recovery; mutation
score at least 85% per crate, and any surviving mutant in durability order (invariants 10-12), bounds
checks, checksum validation or latch transitions blocks release regardless of aggregate. The
by-construction techniques are the ones that worked in task-1791: table-driven "corrupt every field"
tests for codecs, exhaustive state-by-event products for state machines, and the campaigns above for
error paths. Coverage is measured before mutation, per crate, with a fixed seed and no shuffling.

## Performance contract

Ratio is SQLite time over inillucent time; higher is faster. Weights are the checked-in ones and are not
changed by this document. Targets are **estimates** unless marked measured.

| family | weight | today (measured, medium) | target | low estimate | why |
|---|---|---|---|---|---|
| open.prepare | 0.08 | 0.150 | 6x | 5x | arena prepare ~1.5 us; plan-cache hit ~0.2 us; SQLite 0.5-3 us. The harness re-prepares per iteration for this family (`prepare_each: true`), so the cache is measured |
| read.point | 0.16 | 0.558 | 2.5x | 2x | compiled probe over swizzled pointers, ~300-500 ns vs ~1 us; `point.miss` is ~0.07x today and is a bug, not a cost |
| read.range | 0.12 | 0.223 | 4x | 3x | covering ranges are memcpy off PAX leaves |
| read.analytical | 0.10 | 0.046 | 10x | 8x | PAX + vectorised aggregate; DuckDB is 10-100x SQLite here with a full columnar store; a row-store leaf and no parallelism costs some of that |
| read.join | 0.08 | 0.154 | 4x | 3x | hash join over batches; SQLite only nests loops; `join.range` is ~0.015x today and is a planner bug |
| write | 0.20 | 0.156 | 2x | 1.5x | logical WAL writes ~100 bytes per row change vs a 4 KiB+ frame per dirty page; delta-area inserts; index maintenance reuses encoded keys. Load-bearing: this is the heaviest weight |
| transaction | 0.10 | 0.309 | 1.2x | 1.0x | `txn.autocommit` is one write plus one sync on both sides; batched/large follow the write path |
| schema | 0.04 | 0.070 | 4x | 3x | bottom-up bulk build |
| extension | 0.08 | 0.106 | 2x | 1.5x | JSON parsed once to a binary form; FTS5 and R-Tree rewritten over the new trees. Least certain |
| large.values | 0.04 | 0.697 | 1.5x | 1.5x | extent store; both sides near memory bandwidth |

Arithmetic with the checked-in weights (the reader can re-run it):

- Targets: weighted sum of natural logs = 0.08·1.792 + 0.16·0.916 + 0.12·1.386 + 0.10·2.303 +
  0.08·1.386 + 0.20·0.693 + 0.10·0.182 + 0.04·1.386 + 0.08·0.693 + 0.04·0.405 = 1.081;
  e^1.081 = **2.95x**.
- Low estimates: 0.08·1.609 + 0.16·0.693 + 0.12·1.099 + 0.10·2.079 + 0.08·1.099 + 0.20·0.405 +
  0.10·0 + 0.04·1.099 + 0.08·0.405 + 0.04·0.405 = 0.841; e^0.841 = **2.32x**.

So the **3.0x** headline requires every target column to land, and the `write` family at 2x carries
0.14 of the 1.08; the low column gives 2.3x. Both clear the old 1.50x by a wide margin. The contract
this document sets:

- **Design target**: weighted geomean lower 95% bound at least **3.0x**.
- **Floor**: no family below **1.0x** (lower bound), replacing 0.90x.
- **Expected-at-parity note**: `transaction.autocommit`, `write.insert.autocommit` and `large.write`
  are bounded by one `sync` per commit under `synchronous=FULL`, which both engines pay identically.
  A ratio between 0.95x and 1.3x on those three workloads is the expected outcome and is not a
  failure; their families' floors are still enforced at the family level.
- **Per-phase gates** are stated in the phases below and are the operative go/no-go bars; the
  headline is measured at the end of Phase 5.

Comparison points, for calibration (published numbers, not measured here): DuckDB beats SQLite by
10-100x on aggregates over 10^5-10^6 rows and loses to it by 2-10x on single-row OLTP; RocksDB-backed
SQL engines lose to SQLite by 2-5x on warm point reads and scans; LeanStore and Umbra beat
B-tree-on-slotted-pages engines by 5-20x on in-memory point and scan workloads on one thread. The
contract above sits between LeanStore and DuckDB, which is where an embedded engine that must do both
belongs.

Fairness: the existing contract stands. Same SQL, same data, same `synchronous` policy, same
transaction boundaries, same warm state, interleaved A/B, 30 paired rounds, correctness-gated timings.
The plan cache is declared in the fairness section of the scorecard report as a inillucent design
feature; it does the same logical work (the harness prepares identical text each iteration) and SQLite
has no equivalent inside the library.

## Component triage

| crate / area | verdict | reason |
|---|---|---|
| `inillucent-base` | survives as-is | bytes, varint, checksum, ids, limits; engine-agnostic |
| `inillucent-vfs` | survives, trimmed | the file trait and OS backends stay and are the sim seam; `locks.rs`/`shm_locks.rs` shrink to "refuse a second process" |
| `inillucent-sim` | survives as-is | failpoints, media model, crash snapshots, scheduler; becomes a dev-dependency of the new storage crates |
| `inillucent-value` | survives with change | affinity, collation, comparison, tagged encoding stay; SQLite record decoding paths go |
| `inillucent-sql` | survives with change | lexer, parser, AST, binder, planner algebra stay; arena allocation added; physical planning pass added; `vtab.rs` adapts to batches |
| `inillucent-catalog` | rewritten | same object model, stored in the engine's own catalog tree instead of `sqlite_schema` pages |
| `inillucent-storage` | deleted, except a read-only extract | b-tree page/cell/overflow/freelist/ptrmap/vacuum/pager die; the read-only b-tree + record decoder becomes `inillucent-sqlite-reader` (test and migrate only) |
| `inillucent-transaction` | deleted | rollback journal, super-journal, hot-journal recovery, SQLite WAL format and WAL-index, lock state machines all die; replaced by `inillucent-txn` + `inillucent-wal` |
| `inillucent-vm` | deleted, with ports | the VM, verifier, compiler, sorter and ephemeral tables die; `builtin.rs`, `datetime.rs`, `mathfn.rs`, `pattern.rs`, `printf.rs`, `aggregate.rs` function bodies and `window.rs` frame logic port into `inillucent-exec` |
| `inillucent-session` | rewritten | connections, statements, pragmas and result access over the new engine; `backup.rs`, `blob.rs`, `serialize.rs` deleted |
| `inillucent` | survives with change | public API shapes kept; serialize/backup/blob methods removed |
| `inillucent-capi` | deleted | the C ABI is a non-goal |
| `inillucent-cli` | survives with change | shell over the Rust API; `.dump`/`.import` keep working; `.backup` goes |
| `inillucent-ext` (JSON, vtab, shadow) | rewritten storage adapters | JSON functions get a binary form; shadow tables become ordinary trees; vtab contract moves to batches |
| FTS5 / R-Tree (in `inillucent-ext`) | rewritten storage adapters | tokenizers, ranking, segment logic and R-Tree node logic survive; the page-level storage code is replaced |
| `inillucent-search` + `inillucent-core` | survive with change | the retrieval algorithms are untouched; the shadow-table store and adapter are rewritten over the new trees behind the `legacy-storage` feature until Phase 5 |
| `inillucent-migrate` | survives with change | copy/verify/manifest/resume shape kept; new sources (SQLite files via the reader) and new target format |
| `inillucent-bench` | survives as-is | the retrieval scorecard; re-run in Phase 5 |
| `inillucent-compat` | survives with change | scorecard, oracle, SLT, differential harness, perf contract stay; the file-interop, cross-open and SQLite-format bins die; fixtures gain an import step |
| `compat/sqlite-3.53.4.toml` manifest | survives, re-profiled | rows for file format, C API, locking, backup, serialize, VACUUM, ATTACH-across-files move to a new `not-a-goal` state; SQL and function rows keep their status |
| `tests/crash/*` | survives with rewrite | the campaigns are re-expressed as traces over the new engine and the model |
| `fuzz/` | survives, extended | parser targets stay; codec targets added |
| `docs/invariants/layering.toml` | rewritten | new crate graph |

Tests: of the 1,484 tests, the estimate is that **40-50% die with their modules**: everything that
asserts SQLite page bytes, cell layouts, opcode sequences, program verification, journal/WAL frame
bytes, lock transitions, cross-open behaviour or the C API. What survives: parser and binder tests,
SQL semantics and function tests, value/collation/affinity tests, the SLT and differential corpora,
the retrieval engine's tests, the migration tool's verification tests, `inillucent-sim`'s own tests, and
the crash campaigns once re-expressed as traces. Phase 1 counts them precisely by tagging each test
module in the triage before anything is deleted.

## Implementation sequence

### Work-package rule

The phases run in order. A phase is complete when its artifacts, invariants, tests and acceptance
evidence exist and its gate is measured, not when its code compiles. **If a phase gate misses, the
implementer stops, writes the measured result and the analysis of why into the ticket, and does not
start the next phase.** Length, risk and the size of the remaining work are never reasons to stop
inside a phase; a missed gate is the only one. The old engine stays buildable and runnable
(`cargo build --workspace` green, the old scorecard reproducible) until Phase 5 deletes it, so that a
number can be produced at any time.

Each work item records: files delivered, invariants added, tests added by oracle (differential,
model, sim, SLT, property, metamorphic), the scorecard rows moved with before/after numbers, and the
manifest rows moved.

### Phase 0: the one-hour pre-check (before any new code)

Deliver:

- a bin in `inillucent-compat` shaped like `readperf.rs` (which already drives the pager and b-tree
  directly) that computes `count(*), sum(key), max(category)` over `main_table` at medium scale by
  iterating the **existing** leaf cursor with no VM and no `Value` allocation, timed over 30 rounds;
- the same query timed in SQLite 3.53.4 end to end on the same fixture;
- the per-workload ratios from the last scorecard run, read off the report, for `point.miss`,
  `join.range`, `range.covering`, `range.lookaside`.

Acceptance (a measurement, not a gate; it decides emphasis, not whether to proceed):

- if the raw existing cursor is already at or above 3x SQLite, the executor is the whole
  `read.analytical` problem and the PAX leaf is a Phase 2 item; Phase 1 builds the executor over the
  existing leaves first;
- if it is below 1x, the storage cursor is the floor and Phase 1 must include the PAX leaf;
- the four per-workload ratios are recorded as measured; the two that are bugs (`point.miss`,
  `join.range`) are fixed in the old engine only if the fix is under a day, because they also
  validate the planner rules the new physical pass inherits.

### Phase 1: the go/no-go (weeks 1-4)

Deliver:

- `inillucent-tree` leaf codec (PAX layout above), the class array, the tagged-value heap, compaction,
  and an in-memory tree (no pool, no disk) with insert, point, range and full scan;
- `inillucent-exec` batches, `TableScan`, `Filter`, `Project`, `SimpleAggregate`, `HashAggregate`, `Sort`,
  `TopN`, `Distinct`, `ResultSink`, the closure compiler for arithmetic, comparison, `IS NULL`,
  `AND/OR/NOT`, and the SQLite aggregate semantics for `count/sum/min/max/avg/total`;
- a load path from the scorecard fixture through `inillucent-sqlite-reader` (extracted from
  `inillucent-storage`'s read-only path) into the in-memory tree;
- the existing parser/binder/planner feeding the new physical pass for the four `read.analytical`
  shapes;
- the scorecard able to run `read.analytical` against the new engine (`--engine new`) with digests
  compared to SQLite;
- in parallel and independent: the prepare arena and plan cache on the existing front end
  (`inillucent-sql`, `inillucent-session`), measured on the old engine's `open.prepare`.

Acceptance:

- all four `read.analytical` workloads digest-equal to SQLite at all three scales;
- `read.analytical` lower 95% bound **at least 5.0x** at medium scale over 30 paired rounds,
  interleaved, idle machine; reported at small and large too;
- page size 16/32/64 KiB measured and the default fixed;
- closure compilation measured against the old VM's expression evaluator on the same predicates;
- `open.prepare` on the old engine with the arena and cache at or above 1.0x (this proves the cache
  independently of the new storage);
- leaf codec at 100% branch coverage with the corrupt-every-field test; property test against
  `BTreeMap` with forced compaction and splits.

If the 5.0x bar misses: stop, write up the measured ratio and the profile, and report. A miss here
means the analytics thesis is wrong at this leaf size and row shape, and the plan's arithmetic fails
without it.

### Phase 2: a real read-only engine (weeks 5-10)

Deliver:

- `inillucent-pool`: the frame reservation, version latches, swizzling, cooling FIFO, writeback with swip
  translation, the free map, blob extents, all I/O through `inillucent-vfs`;
- `inillucent-tree` on the pool: interior pages, memcmp key encoding for every collation, secondary index
  trees, optimistic descent with restart, `RowidLookup`, `IndexRangeScan`, reverse scans;
- `PointProbe`, `HashJoin`, `IndexNestedLoopJoin`, `NestedLoopJoin`, `Materialize`, `SetOp`,
  `Window`, `ValuesScan`, `Limit/Offset`, the full built-in function set ported from `inillucent-vm`;
- the physical planner rules and `PRAGMA inillucent.force_plan`;
- `inillucent-catalog` over the catalog tree (read side), `sqlite_schema` view;
- the on-disk format written by a bulk loader from the fixture (no WAL yet: load, checkpoint, close);
- `inillucent-sim` as a dev-dependency; small-pool campaigns for the read path;
- the SLT corpus's read-only subset through the new engine; the metamorphic plan tests.

Acceptance:

- read-only SLT subset 100%; differential corpus zero unexplained differences;
- `read.point` at least 2.0x, `read.range` at least 3.0x, `read.join` at least 3.0x,
  `read.analytical` still at least 5.0x, all lower bounds at medium;
- `PointProbe` measured under 500 ns warm on the medium fixture;
- corrupt-page fuzz targets never panic; interior/key codecs at 100% branch;
- eviction campaign with a 64-frame pool green.

If a family bar misses: stop and report.

### Phase 3: writes, durability, MVCC (weeks 11-18)

Deliver:

- `inillucent-wal`: segments, record codec, the log writer thread, group commit, checkpointer, recovery;
- `inillucent-txn`: snapshots, writer slot, undo buffers, version log and GC, savepoints, commit gate,
  `synchronous` policies, `busy_timeout`;
- tree mutation on the pool: delta inserts, in-place updates, tombstones, compaction, split, merge,
  free-map allocation, all WAL-logged;
- `Insert`/`Update`/`Delete` sinks with constraints, `ON CONFLICT`, `RETURNING`, triggers, index
  maintenance; `UNIQUE` via `PointProbe`;
- `inillucent-model` and the trace driver; the fail-the-Nth-call, crash-at-every-sync,
  crash-at-every-write and corrupt-WAL campaigns; the seeded random trace generator;
- the full SLT corpus and the full differential corpus through the new engine.

Acceptance:

- SLT 100%; differential zero unexplained; model traces zero divergences including after every crash
  point; fault matrix zero ACID violations;
- `write` at least 1.5x, `transaction` at least 1.0x with the fsync-bound note applied per workload;
  read families do not regress below their Phase 2 bars;
- WAL codec 100% branch; txn and recovery at least 95% branch; durability-order mutants all killed;
- recovery idempotence test (recover twice, compare files) green.

If `write` misses 1.5x: stop and report; it is the heaviest weight and the plan does not reach 3.0x
without it.

### Phase 4: DDL, extensions, large values (weeks 19-24)

Deliver:

- DDL on the catalog tree: `CREATE/DROP/ALTER TABLE`, `CREATE/DROP INDEX` via the bulk builder,
  `CREATE VIEW/TRIGGER`, `ANALYZE` and the statistics, plan-cache invalidation;
- JSON functions over a binary form; FTS5 and R-Tree storage adapters over the new trees; the
  batch-aware vtab contract;
- large-value read/write through extents; `large.values` workloads;
- `PRAGMA` set re-profiled: keep those with meaning (`cache_size`, `synchronous`, `busy_timeout`,
  `foreign_keys`, `journal_mode` returns `wal` and accepts nothing else, `integrity_check`,
  `wal_checkpoint`, `table_info` and friends); the rest return the dialect's "no such pragma" or a
  no-op per SQLite's own convention, documented in the manifest.

Acceptance:

- `schema` at least 3.0x, `extension` at least 1.5x, `large.values` at least 1.5x;
- the full scorecard runs with every workload digest-equal;
- integrity checker validates invariants 1-4 on every fixture after every campaign.

### Phase 5: consumer, migration, deletion, release (weeks 25-28)

Deliver:

- `inillucent-search` shadow store over the new trees; the `legacy-storage` feature removed only after the
  retrieval scorecard passes;
- `inillucent-migrate` with the SQLite-file source and the new target; migration of every legacy generation
  and every task-1781 fixture, verified;
- deletion of `inillucent-storage` (except the reader), `inillucent-transaction`, `inillucent-vm`, `inillucent-capi`,
  the file-interop bins, and their tests; the manifest re-profiled; `layering.toml` updated;
- the full performance qualification: 30 rounds, three scales, both OSes, the report with weighted
  and unweighted tables, the fairness section naming the plan cache.

Acceptance:

- retrieval scorecard at least 1.50x its configured baseline, no quality regression;
- weighted geomean lower 95% bound at least 3.0x at medium, reported at all scales; no family below
  1.0x;
- every gate in "Measurable success criteria" green;
- `cargo build --workspace` has no reference to a deleted crate; the test count and the surviving
  categories are recorded against the Phase 1 tally.

### After this ticket (not in scope)

Async I/O through the VFS trait (io_uring / IOCP) for checkpoint and cold reads; a steal policy with
undo records for larger-than-memory transactions; optimistic multi-writer; a C shim; parallel scans
across leaves for `read.analytical` at large scale.

## Risks and mitigations

| risk | mitigation |
|---|---|
| The PAX leaf does not deliver 5x on `read.analytical` at 100k rows | Phase 0 measures the storage floor first; Phase 1 is the go/no-go; page size is measured, not assumed; a miss stops the program before any storage is deleted |
| `write` lands near parity because delta-area inserts plus compaction cost more than SQLite's memmove | the delta limit is measured at 16/32/64; in-place fixed-width updates and key reuse in index maintenance are cheap wins independent of the leaf; Phase 3 stops on a miss |
| Dynamic typing exceptions turn out common in real data and defeat the fast path | the class array makes the slow path per-leaf, not per-table; `Any` columns exist for columns with no affinity; the design degrades to row-at-a-time speed, not to wrong answers |
| Swizzling/latch bugs (use-after-evict, torn optimistic reads) | invariants 5-6 asserted in debug; small-pool campaigns under `inillucent-sim`; readers copy then validate; four restarts then shared latches |
| Durability-order regressions in a new WAL | invariants 10-12; crash-at-every-sync/write campaigns; mutation gate that blocks on any surviving durability-order mutant; recovery idempotence test |
| No-steal limits transaction size | documented limit with a clear error; pool grows on demand; steal is the named follow-up |
| Losing the SQLite oracle silently lowers the bar | the differential gate is kept over the imported fixture and stays the timing precondition; the model reference is stricter than SQLite ever was on durability; the SLT corpus is unchanged |
| The plan cache is contested as "less work" on `open.prepare` | declared in the fairness section; the arena alone is measured separately and must reach 1.0x without the cache in Phase 1 |
| The retrieval consumer regresses | `legacy-storage` feature keeps the old path alive until the retrieval scorecard is green on the new one; migration never deletes a source |
| Deleting tests deletes coverage nobody noticed | Phase 1 tags every test module before deletion and the Phase 5 tally must account for each category |
| Two-engine period doubles build and CI time | the old engine's crates are built only under the `legacy-storage` feature after Phase 2; CI runs the new-engine suites on every change and the legacy suite nightly |

## What would make this wrong

The single assumption most likely to be false is that **a batched scan over a PAX leaf beats SQLite by
8-10x on a 100k-row table on one thread**. Everything else in the arithmetic can slip by a third and
the plan still clears 2x; if `read.analytical` lands at 3x instead of 8x the headline drops to about
2.5x and the "5x" go/no-go has already fired. The Phase 0 raw-cursor measurement tests the storage
half of that assumption in an hour, and the Phase 1 gate tests the whole of it in four weeks, before
any of the existing engine is removed.

The second assumption is that the **`write` family reaches 2x at equal durability**. It carries the
heaviest weight, and the mechanisms behind it (smaller log records, delta inserts, key reuse) are
each modest. If Phase 3 measures it at 1.2x, the headline is about 2.6x; that is the number to
report, with the per-workload breakdown, and the decision is not the implementer's to make.

The third is that **the fixture import and the model reference together are as good an oracle as the
shared file was**. They are better on durability and equal on SQL semantics, but they cannot catch a
class of bug the old gate caught for free: an on-disk structure SQLite would have rejected. The
integrity checker, the codec fuzzers and the corrupt-page campaigns exist to cover that class; if a
storage bug ever reaches a scorecard run without one of them catching it, that campaign is the first
thing to extend.

## Definition of done

- Phases 0-5 complete with their acceptance evidence in `_agent_output/task-1816-*/` and the
  measured numbers written into the ticket;
- every row of "Measurable success criteria" green on Windows x64 and Linux x64;
- the manifest re-profiled with no `unknown` rows, SQL rows unchanged in status, and the abandoned
  areas in `not-a-goal`;
- the old engine deleted, the reader retained, the layering invariant updated and enforced;
- the retrieval consumer migrated, its scorecard green, every legacy source intact;
- the performance report published with weighted and unweighted tables, raw rounds, the fairness
  section, and every estimate in this document replaced by a measurement or an explanation of the
  difference.

## Primary sources

- LeanStore: Leis, Haubenschild, Kemper, Neumann, "LeanStore: In-Memory Data Management Beyond Main
  Memory", ICDE 2018 (pointer swizzling, cooling FIFO, optimistic latches).
- Umbra: Neumann, Freitag, "Umbra: A Disk-Based System with In-Memory Performance", CIDR 2020
  (variable-size pages, buffer management, adaptive compilation).
- PAX: Ailamaki, DeWitt, Hill, Skounakis, "Weaving Relations for Cache Performance", VLDB 2001.
- Vectorised execution: Boncz, Zukowski, Nes, "MonetDB/X100: Hyper-Pipelining Query Execution",
  CIDR 2005; DuckDB's push-based pipeline model (Raasveldt, Mühleisen).
- Closure compilation vs interpretation: Kersten, Leis, Kemper, Neumann, Pavlo, Boncz, "Everything You
  Always Wanted to Know About Compiled and Vectorized Queries But Were Afraid to Ask", VLDB 2018.
- ARIES: Mohan et al., "ARIES: A Transaction Recovery Method Supporting Fine-Granularity Locking and
  Partial Rollbacks Using Write-Ahead Logging", TODS 1992 (page-LSN idempotent redo, fuzzy checkpoints).
- Crotty, Leis, Pavlo, "Are You Sure You Want to Use MMAP in Your Database Management System?",
  CIDR 2022 (why not mmap).
- Deterministic simulation testing: FoundationDB (Zhou et al., SIGMOD 2021) and TigerBeetle's VOPR,
  for the model-reference-plus-crash-trace approach.
- SQLite 3.53.4 documentation for the SQL dialect, affinity, collation, aggregate and `synchronous`
  semantics that this engine keeps.
- This repository: `tasks/task-1781-sqlite-feature-parity-tdd.md` (the superseded design and the
  assurance architecture this document inherits), `compat/perf/contract.toml` (weights),
  `crates/inillucent-compat/src/bin/scorecard.rs` (the harness), `crates/inillucent-sim` (the simulator).
