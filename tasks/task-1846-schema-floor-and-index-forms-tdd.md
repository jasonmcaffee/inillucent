# task-1846 — the schema floor, and the three `CREATE INDEX` forms

The remainder of Phase 3. Four pieces, in the order they will be built:

- **A** — `schema` is the last family under the 1.00x floor. `CREATE INDEX` at medium costs 48.1 ms
  against SQLite's 31.6, and closing that gap is the whole of H5.
- **B** — partial indexes, `CREATE INDEX ix ON t(a) WHERE b > 5`.
- **C** — indexes on expressions, `CREATE INDEX ix ON t(lower(a))`.
- **D** — `CREATE INDEX` on a `WITHOUT ROWID` table.

B, C and D are the three `Differs` rows in `crates/inillucent-compat/tests/semantics.rs`; A is the
one family the four-run gate reports as `UNDER THE FLOOR`.

---

## The measurement this starts from

Taken on this box, this checkout, `9ef6aee`, with a fresh copy of the medium fixture
(100,000 `main_table` rows), `target/release/inillucent-indexprofile --iterations 12`:

```
## CREATE INDEX main_label ON main_table(label)
  median total: 48.1 ms
  last round  : scan 13.3 ms, sort 5.3 ms, unique 0.0 ms, pack 15.1 ms (of which borrow 4.2 ms), catalog 7.0 ms
```

SQLite builds the same index in **31.6 ms** (task-1845's four-run medium gate, `schema.index`
31,643,800 ns). So the target is a **16.5 ms** reduction, and the stages account for it like this:

| stage | ms | what it is | addressable |
|---|---:|---|---|
| prologue | 7.4 | parse, bind, `canonical_sql`, `allocate_root`, `index_from_create_sql` — the part of the statement before `scan` starts, arrived at by subtraction | to be measured, then judged |
| scan | 13.3 | `index_entries` — one pass over the table tree, projecting `(label, rowid)` | **yes** |
| sort | 5.3 | `in_key_order` — a stable comparison sort that decodes and compares values | **yes** |
| unique | 0.0 | not a `UNIQUE` index | — |
| pack | 15.1 | `build_tree_from` → `bulk_build_logged`, of which 4.2 ms is the `Vec<Vec<Datum>>` borrow | **yes** |
| catalog | 7.0 | `record`, `rebuild_tables`, `refresh_catalog`, `seal` — and `seal` is a log sync both engines pay under `synchronous = FULL` | **no** |

`scan + sort + pack = 33.7 ms` has to become roughly **17 ms** for the total to land at SQLite's, or
about **21 ms** if the prologue also gives up a few milliseconds. Both are in scope.

### Where the time actually goes

Three allocations per row, on a hundred thousand rows:

1. `index_entries` builds a `Vec<OwnedDatum>` per row — one allocation for the vector, one more for
   `OwnedDatum::Text`'s copy of the 45-byte label.
2. `in_key_order` sorts those vectors by `compare_under`, which dispatches on collation per column
   per comparison and chases two pointers per value.
3. `build_tree_from` then materialises a *second* `Vec<Vec<Datum>>` of the whole input — one more
   allocation per row — before `bulk_build_logged` walks it. That is the 4.2 ms named `borrow`.

The narrow fix — making `pack_with` take `&[&[Datum]]` — was tried in task-1845 and backed out: it is
worth about 4 ms of the 17 needed and it reaches five call sites including the hot compaction path in
`write.rs`. It is only worth doing as part of the whole change, which is what this ticket is.

---

## Part A — the bulk builder

### A1. One arena instead of three hundred thousand allocations

A new `EntrySet` in `inillucent-engine`, built by `index_entries` and consumed by the packer. It
holds the entries columnar-ish, in four vectors and no per-row allocation at all:

```rust
/// One cell of one entry, pointing into the arena rather than owning bytes.
#[derive(Clone, Copy)]
enum Cell {
    Null,
    Int(i64),
    Real(f64),
    Text { at: u32, len: u32 },
    Blob { at: u32, len: u32 },
}

struct EntrySet {
    /// How many columns an entry has: the key columns, then the row identity.
    width: usize,
    /// `rows * width` cells, in entry order.
    cells: Vec<Cell>,
    /// Every text and blob payload, appended once.
    bytes: Vec<u8>,
    /// Every entry's memcmp key, appended once, under the tree's own encoding.
    keys: Vec<u8>,
    /// `rows + 1` offsets into `keys`.
    key_at: Vec<u32>,
}
```

`push` takes the row's `Datum`s straight out of the leaf, appends the cells, copies the text bytes
into `bytes`, and encodes the key into `keys` with the tree's own `KeyEncoding` — so the sort order
this produces *is* the order the tree will be read by, which is the invariant `in_key_order`'s
comment defends. Four growing vectors, reserved up front from `tree.row_count()`, is a handful of
allocations for the whole build.

`inillucent-tree` grows one method to make the key encoding appendable rather than returning a
`Vec` per row:

```rust
impl KeyEncoding {
    /// Appends a key tuple's comparable bytes to a buffer.
    pub fn encode_into(self, values: &[Datum<'_>], collations: &[Collation], out: &mut Vec<u8>);
}
```

and `encode_under` becomes a two-line wrapper over it, so there is one encoding and not two.

### A2. A radix pass instead of comparisons

`EntrySet::order()` returns the entry indices in key order:

1. Pack the first **eight bytes** of each entry's key into a `u64`, big-endian, zero-padded, and
   pair it with the entry's index.
2. **LSD radix sort** those pairs: eight 256-way counting passes over a `Vec<(u64, u32)>` and one
   scratch buffer. No comparisons, no allocation per element.
3. Walk the result and, wherever a run of entries shares a prefix, sort just that run by `memcmp`
   over the full keys.

That is exact — a `memcmp` order over whole keys — and it is the order the tree defines, because the
keys were produced by the tree's own encoder. Ties are broken by the entry index so the result is
deterministic and the stability `in_key_order` documents is preserved.

For `main_label` the keys are `[TEXT] 'row N lorem ipsum…'`, so the eight-byte prefix separates most
entries outright and the fix-up runs are short.

### A3. The packer stops copying

`LeafBuilder::pack_with`, `LeafBuilder::encode_with` and `PagedTree::bulk_build_logged` become
generic over the row's container:

```rust
pub fn pack_with<'d, R: AsRef<[Datum<'d>]>>(&self, rows: &[R], fill: f64, spill: Option<&mut dyn Spill>) -> DbResult<Packed>
```

`Vec<Datum>` already implements `AsRef<[Datum]>`, so **every existing call site compiles unchanged** —
including the compaction path in `write.rs` that made the narrow version not worth doing. The index
build passes `&[&[Datum]]`, whose slices point into the arena, and the 4.2 ms borrow disappears with
nothing else moving.

`ImportedDatabase::build_tree_from` becomes generic the same way; the two callers that still hold
`Vec<Vec<OwnedDatum>>` (the `ALTER TABLE` rebuild and the fixture import) do their own borrow, which
is where it belongs — it is their cost, not the builder's.

### A4. One log record per packed leaf

Already true and re-stated here so it is not re-attacked: `log_built_page` writes one `AllocPage` and
one `WritePage` per leaf image, and the whole-image record is the right record for a page that did
not exist before the build. Verified, not changed.

### A5. The prologue

7.4 ms of the 48.1 is spent before `scan` begins. `create_index` is instrumented with a sixth stage
so the number stops being a subtraction, and whatever it turns out to be — a redundant re-parse of
the statement in `index_from_create_sql`, a catalog snapshot, `rebuild_tables` — is then either
removed or reported as irreducible. It is not guessed at in advance.

### What guards this

The bulk builder's output is compared **page for page against SQLite's own index** by the existing
fixture round-trip: `ImportedDatabase::import_with` reads the catalog back and refuses if it differs
from what it wrote. That is the guard rail this change wants, and it is why the change earns its own
ticket rather than the tail of another one. Alongside it: `cargo test -p inillucent-tree`,
`-p inillucent-engine`, and the compat qualification suites.

---

## Part B — partial indexes

Half of it is already built: `index_from_create_sql` parses the predicate onto
`IndexInfo::partial_sql`, and the planner already declines to *use* a partial index
(`plan.rs` `index_candidate`, and `covering_slots`). What is missing is acceptance and maintenance.

1. **Accept it.** `directive.rs::bind_create_index` drops
   `if filter.is_some() { return Err(unsupported("partial indexes")) }`. The predicate needs no new
   directive field — the engine re-parses the canonical SQL — but it *is* bound against the table
   here, so `CREATE INDEX ix ON t(a) WHERE nosuchcolumn > 5` is refused at `CREATE` time as SQLite
   refuses it, rather than at the first write.

2. **Maintain it.** An entry exists only for a row the predicate accepts.
   - **Build path:** `index_entries` evaluates the predicate per row and skips the rows it rejects.
   - **Write path:** the binder already binds a table's `CHECK` constraints onto every `BoundInsert`,
     `BoundUpdate` and `BoundDelete` as `Vec<BoundCheck>`, and `WriteDeclarations::compile` turns
     those into compiled expressions the row space evaluates. A partial predicate is the same shape,
     so it travels the same road:

     ```rust
     /// The expressions one index needs evaluated per row to be maintained.
     pub struct BoundIndexExprs {
         /// Its position in `table.indexes`.
         pub position: usize,
         /// The partial predicate, when it has one.
         pub predicate: Option<BoundExpr>,
         /// The key expressions, one per key column, `None` for a bare column.
         pub keys: Vec<Option<BoundExpr>>,
     }
     ```

     Empty for every table with no partial or expression index, which is every table the gate
     measures — so the write families' numbers do not move.

     `index_entry` gains the compiled expressions and the row space; `add_index_entries`,
     `update_index_entries` and `remove_index_entries` skip an index whose predicate is false of the
     row image they are given. An `UPDATE` evaluates the predicate on the **old** image to decide
     whether to remove and on the **new** image to decide whether to add, which is what moves a row
     into and out of a partial index correctly.

3. **Use it.** An index may only answer a query whose `WHERE` *implies* the predicate. The
   conservative rule, which is SQLite's, is that the predicate appears as a conjunct of the
   statement's `WHERE`. The bound predicate is compared structurally against each conjunct; a match
   lifts the `continue` in `index_candidate`, and `covering_slots` follows the same rule.

Steps 1 and 2 alone are a correct engine — the index exists, is maintained, and is simply never
chosen — so they land first and are verified on their own before step 3 touches the planner.

---

## Part C — indexes on expressions

The same shape. `IndexColumnInfo::expr_sql` is already filled by the loader, and `index_shape`
already handles a key column with no table column behind it: it pushes `ColumnSpec::key(Any)` and
leaves the slot unmapped, which is exactly what makes such an index non-covering.

1. **Accept it.** `directive.rs::bind_create_index` stops refusing a key that is not a bare column.
   `IndexKeyColumn::column` becomes `Option<u16>` and the struct gains `expr_sql: Option<Vec<u8>>` —
   two fields, one other reader (`create_vector_index`, which requires a bare column and now says
   so).
2. **Maintain it.** The key value is `evaluate(expr, row)` rather than `row[slot]`, on both the build
   path and the write path, through the same `BoundIndexExprs` Part B introduces.
3. **Use it.** The planner matches a bound `lower(a)` in the `WHERE` against the index's bound key
   expression, structurally, the same comparison Part B's conjunct test uses.

---

## Part D — `CREATE INDEX` on a `WITHOUT ROWID` table

Refused today in `ddl.rs::create_index`, with a comment saying exactly why: an index on such a table
carries the table's **primary key** as its trailing entry rather than a rowid, and every tree here
appends exactly one rowid column.

- **`index_shape`** appends the primary key's columns — in `keyed_table_shape`'s key order, which is
  the order the table tree is keyed by — instead of one `Int64` rowid, and reports `rowid: None`.
- **`index_entries`** and **`index_entry`** project those columns instead of the rowid.
- **`SourceLayout` gains `identity: Vec<usize>`** — the tree columns that identify the *table* row.
  For a rowid table's layout it is the rowid's slot; for a `WITHOUT ROWID` table's it is the primary
  key's slots; for an index layout it is the trailing columns of the entry. `key_columns` cannot
  answer this: on an index layout it names the whole entry, and it is deliberately left **empty**
  when the tree is not "already sorted" (a `DESC` column, a non-binary collation), which would make
  index maintenance silently wrong on exactly the tables that need it most.
- **`physical.rs`'s non-covering lookup** (around line 2586) builds its probe key from
  `previous_layout.identity` rather than from `vec![Expr::Column(offset + rowid)]`.

That last one is on the read path the gate's read families measure, so it gets a **read-gate run
either side of the change**, and a difference larger than the run-to-run spread is a refusal.

---

## Acceptance

1. `inillucent-fullgate` medium, 30 rounds, **four consecutive runs**, each on its own fixture copy:
   weighted lower bound at least **3.00x** and **no family under 1.00x**. That is H5, and A is the
   only thing standing in the way.
2. `crates/inillucent-compat/tests/semantics.rs` at **92 of 92**, with `index.partial`, `index.expr`
   and `without.rowid.index` moved from `Differs` to `Agrees`. The test fails when one of them starts
   agreeing until its row is moved, so it reports the change rather than having to be remembered.
3. `cargo test --workspace --no-fail-fast` no worse than the 17 accounted-for red binaries the README
   records.
4. `inillucent-readgate` either side of Part D, with no read family moving beyond its spread.

## Non-goals

Unchanged from Phase 3: no bar, weight or fixture in `compat/perf/contract.toml` is touched; the old
engine is not deleted (blocked on task-1837's driver); multi-process and multi-thread access stay out
of scope.
