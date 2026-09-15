# Glossary

Every word this documentation uses that a general programmer would not already know, in one table,
with one sentence each. Both halves of the engine: the relational one and the retrieval one.

It is here because half the documentation defined its words and half did not.
[Architecture](architecture.md) opens with a 26-term table that explains HNSW, quantisation and
cosine distance before it uses them; [Relational architecture](relational-architecture.md),
[SQL support](sql.md) and [Feature comparison](feature-comparison.md) used B-tree, WAL, page,
pragma, rowid and collation with no definition anywhere in the repository.

The retrieval terms below are short entries pointing at
[Architecture's own table](architecture.md#2-words-to-know), which explains each of them properly
and with diagrams. Nothing is duplicated.

---

## Storage: what is on disk

| Term | What it means |
|---|---|
| **Page** | The fixed-size block a database file is divided into, and the unit every read and write moves. 32 KiB here by default; SQLite's default is 4 KiB. A file is a whole number of pages and nothing is ever read or written in smaller pieces. |
| **Page size** | How many bytes one page is. Chosen when the file is created and unchangeable afterwards, because every offset in the file is computed from it. |
| **Buffer pool** (also **page cache**) | The pages held in memory so a read does not go to the disk. One per open file. In this repository "the pool" always means this and never a connection pool, of which there is none. |
| **Frame** | One slot in the buffer pool, holding one page. `--frames 4096` at a 32 KiB page size is a 128 MiB pool. |
| **Pin** | Holding a frame in memory while something reads it, so the pool cannot evict the page out from under the reader. A value read out of a pinned page borrows the page's own bytes rather than copying them. |
| **Eviction** | Removing a page from the pool to make room for another. A page that was written has to be written out first. |
| **B-tree** | The structure an index or a table is stored in: a shallow tree whose leaves hold the rows in key order, so a lookup costs a few page reads whatever the size of the table. |
| **B+tree** | A B-tree whose rows live only in the leaves, the interior pages holding nothing but keys and pointers. This engine's trees are B+trees; SQLite's tables are too. |
| **Leaf** | A B-tree page holding rows. |
| **Interior page** | A B-tree page holding separator keys and pointers to pages below it. Its contents are addresses, which is why a damaged one is worse than a damaged leaf: a descent through it lands somewhere else in the file. |
| **Descent** | Walking from the root of a B-tree down to the leaf that could hold a key. Three page reads on a large table, which is the whole point of the structure. |
| **PAX leaf** | This engine's leaf layout: the rows' values are grouped by *column* inside the page rather than laid out row by row. A scan that reads one column of a wide table then touches one region of the page instead of stepping over every other column. |
| **Delta area** | A small unsorted region at the end of a leaf where a new row is written without rewriting the sorted region. Compaction folds it in later. |
| **Cell** | One row's bytes inside a page, in SQLite's own layout. |
| **Overflow page** | Where the tail of a value too long for one page is kept, chained page by page. This engine calls the same idea a **blob extent**. |
| **Free map** | The record of which pages in the file are not in use, so a new page can be taken from the file rather than added to the end of it. SQLite calls its version the **freelist**. |
| **Meta page** | The first page of the file, holding the page size, the catalog's root page and the free map's head. It says how to read every other page, so a file whose meta page is damaged is refused rather than believed. Written in two copies. |
| **Rowid** | The 64-bit integer that identifies a row in a table that has one. A table declared `WITHOUT ROWID` is keyed by its primary key instead. |
| **Catalog** | The engine's record of what tables, indexes, views and triggers exist and what their columns are. SQLite keeps the same thing in the `sqlite_schema` table. |
| **Schema cookie** | A counter that changes whenever the catalog does, so a statement compiled against an older schema recompiles itself rather than running against a plan that no longer matches. |

## Transactions: what happens when something is written

| Term | What it means |
|---|---|
| **Transaction** | A group of statements that all happen or none do. `BEGIN` opens one; `COMMIT` keeps its work and `ROLLBACK` discards it. In the Rust API a [`Transaction`](../drivers/README.md) is a value, and dropping it rolls back. |
| **Write-ahead log** (**WAL**) | A file the engine appends a record to *before* it changes the database file, so a crash part way through a write can be recovered from the log. The rule that makes it work - the log record reaches the disk before the page does - is the write-ahead rule. |
| **Journal** | The older shape of the same idea: the *original* copy of a page is written aside before the page is changed, so a crash can undo. This engine writes a log; `journal_mode` selects between them and `journal_mode = off` writes neither. |
| **Log record** | One entry in the log: a page image, a row change, a commit, a checkpoint. Recovery reads them in order. |
| **LSN** (log sequence number) | The position of a record in the log, and the number stamped on a page to say which record last changed it. Recovery applies a record to a page only when the page is older than the record. |
| **Checkpoint** | Writing the log's changes into the database file and then retiring that part of the log, so the log does not grow forever and the next open has less to replay. |
| **Recovery** | What an open does to a file a crash left behind: read the log from the last checkpoint and apply what the database file has not got yet. |
| **Redo** | Applying a log record's change to a page during recovery. **Physical redo** writes a whole page image and needs no catalog; **logical redo** applies a row change and needs to know the table's shape. |
| **Undo** | Putting back what an abandoned transaction wrote. |
| **Savepoint** | A named point inside a transaction that `ROLLBACK TO` returns to, without ending the transaction. |
| **Torn page** | A page a crash left half written: some of its bytes are the old page and some are the new one. The checksum in the page header is what detects it. |
| **MVCC** (multi-version concurrency control) | Keeping more than one version of a row so a reader can read the version that was current when it started while a writer writes a newer one, and neither waits for the other. |
| **Commit timestamp** (**cts**) | The number a transaction is given when it commits, which is what decides which version of a row a reader sees. |
| **Durability** | The property that what a commit reported as written survives the process, the operating system or the machine stopping immediately afterwards. |
| **fsync** | The system call that makes a write reach the disk rather than sitting in the operating system's own cache. The expensive part of a commit, and the reason `synchronous` is a setting. |

## SQL: what happens to a statement

| Term | What it means |
|---|---|
| **Parser** | Turns SQL text into a syntax tree. It decides what the statement *says*. |
| **Binder** | Resolves the names in that tree against the catalog: which table, which column, which type. It decides what the statement *means*, and it is where a missing table is reported. |
| **Planner** | Decides *how* to answer: which index to use, which order to join in, whether a sort is needed. |
| **Executor** | Runs the plan and produces the rows. This engine's executor is batch-at-a-time and has no bytecode. |
| **Plan** | The planner's choice, as a tree of operators. `EXPLAIN QUERY PLAN` prints it. |
| **Operator** | One step of a plan: a scan, a filter, a projection, a join, a sort. |
| **Scan** | Reading every row of a table or index in order. |
| **Seek** | Descending an index to one key rather than scanning. |
| **Covering index** | An index that holds every column a query needs, so the query is answered from the index and never touches the table. |
| **Cardinality** | How many rows something produces. The planner's estimates of it decide the plan. |
| **Selectivity** | What fraction of rows a condition lets through. A highly selective condition is worth an index; an unselective one is not. |
| **Affinity** | SQLite's rule for what a column's declared type does to a value written into it: a `TEXT` column stores `1` as `'1'`, an `INTEGER` column stores `'1'` as `1`. It is a preference rather than a constraint, which is the thing that surprises people arriving from PostgreSQL. |
| **Collation** | The rule for comparing two text values: `BINARY` compares bytes, `NOCASE` folds ASCII case. It decides both sort order and whether two values are equal. |
| **Storage class** | What a value actually is on disk, independent of the column's declared type: NULL, INTEGER, REAL, TEXT or BLOB. |
| **Pragma** | A statement that reads or changes a setting rather than data: `PRAGMA journal_mode`, `PRAGMA integrity_check`. [The pragma table](pragmas.md) lists every one this engine recognises. |
| **Virtual table** | A table whose rows come from a module rather than from a B-tree - an FTS5 index, an R-tree, `generate_series`. It is read and written with ordinary SQL. |
| **Shadow table** | An ordinary table a virtual table's module keeps its own storage in. |
| **Trigger** | A statement the engine runs by itself when a row is inserted, updated or deleted. |
| **Prepared statement** | A statement compiled once and run many times with different bound values, so the parse, the bind and the plan are paid once. |
| **Bound parameter** | A `?1` in a statement, filled in with a value at run time rather than pasted into the text. The thing that makes SQL injection impossible. |
| **VFS** (virtual file system) | The layer between the engine and the operating system's files: open, read, write, sync, lock. Swapping it is how the fault-injecting test file system and the in-memory one work. |
| **Lever** | A named optimisation this engine can be told to switch off, so its effect can be measured rather than asserted. [`Levers`](repository.md) lists them. |

## Retrieval: searching by meaning

Short entries. [Architecture's own table](architecture.md#2-words-to-know) explains each of these
properly, with the diagrams.

| Term | What it means |
|---|---|
| **Embedding** (also **vector**) | A list of 768 numbers that stands for the meaning of a piece of text, produced by a trained model. Two texts about similar things get similar lists even when they share no words. |
| **Chunk** | A document cut into a searchable piece, roughly a paragraph. Chunks are what a search returns. |
| **Semantic search** | Finding chunks whose embedding is near the query's embedding: matching meaning. |
| **Lexical search** | Finding chunks that contain the query's actual words: matching words. |
| **Hybrid search** | Both at once, with the two ranked lists combined. |
| **HNSW** | Hierarchical Navigable Small World, the structure that makes semantic search fast by comparing the query against a clever subset instead of every chunk. |
| **Recall** | The fraction of the genuinely nearest chunks an approximate search actually returned. How approximation quality is measured. |
| **BM25** | The formula that scores how well a chunk matches a set of query words. |
| **Inverted index** | A lookup table from each word to the chunks containing it. What makes lexical search fast. |
| **Quantisation** | Storing each number of an embedding less precisely, to use less memory. |
| **Generation** | One published, immutable version of a retrieval index. A new one is built and swapped in rather than the live one being edited. |

---

## Where to read more

| you want | page |
|---|---|
| how the retrieval half works | [Architecture](architecture.md) |
| how the SQL half works | [Relational architecture](relational-architecture.md) |
| both at once, in one diagram | [Architecture overview](architecture-overview.md) |
| which SQL runs | [SQL support](sql.md) |
| every pragma | [Pragmas](pragmas.md) |
| the crates and the contracts | [Repository](repository.md) |
