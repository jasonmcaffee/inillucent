# Glossary

This page explains the specialist words the inillucent documentation uses. Each entry is one or two
plain sentences. The terms are grouped by topic, and each group is in alphabetical order.

If a word on another page is not here and should be, add it.

| Group | What it covers |
|---|---|
| [Storage](#storage) | how a database file is laid out and read |
| [Transactions and the log](#transactions-and-the-log) | what happens when data is written, and after a crash |
| [SQL](#sql) | what happens to a statement, and the SQLite rules the engine follows |
| [Search](#search) | searching by meaning and by keyword |
| [Programs, testing and measurement](#programs-testing-and-measurement) | the words the project pages use |

## Storage

| Term | What it means |
|---|---|
| **B-tree** | The structure a table or an index is stored in. It is a shallow tree of pages with the rows in key order, so finding one key takes a few page reads however large the table is. |
| **B+tree** | A B-tree that keeps rows only in its leaves. The pages above the leaves hold only keys and page numbers. inillucent's tables and indexes are B+trees, and so are SQLite's tables. |
| **Blob extent** | Where the end of a value too long for one page is stored, as a chain of pages. SQLite calls the same idea an **overflow page**. |
| **Buffer pool** | The pages the engine holds in memory so a read does not have to go to the disk. Each open database file has one. It is also called the **page cache**. The default is 128 MiB, set by `PRAGMA cache_size`. |
| **Catalog** | The engine's record of which tables, indexes, views and triggers exist and what their columns are. SQLite keeps the same record in the `sqlite_schema` table. |
| **Cell** | The bytes of one row inside a page. |
| **Delta area** | A small unsorted region at the end of a leaf. A new row is written there first, so the sorted part of the leaf does not have to be rewritten on every insert. A later compaction sorts the delta area into the leaf. |
| **Descent** | Walking a B-tree from its root page down to the leaf that holds a key. On a large table this is about three page reads. |
| **Eviction** | Removing a page from the buffer pool to make room for another page. A page that was changed is written out before it is evicted. |
| **Frame** | One slot in the buffer pool. A frame holds one page. |
| **Free map** | The record of which pages in the file are unused. A new page is taken from the free map before the file is made longer. SQLite calls its version the **freelist**. |
| **Interior page** | A B-tree page above the leaves. It holds keys and the page numbers of the pages below it. |
| **Leaf** | A B-tree page at the bottom of the tree. Leaves hold the rows. |
| **Meta page** | The first page of an inillucent file. It holds the page size, the page number of the catalog and the start of the free map, and it is written in two copies. The engine refuses to open a file whose meta page cannot be read. |
| **Page** | The fixed size block a database file is divided into. Every read and every write moves whole pages. inillucent's default page size is 32 KiB. SQLite's default is 4 KiB. |
| **Page size** | The number of bytes in one page. It is chosen when the file is created and cannot change afterwards. |
| **PAX leaf** | inillucent's leaf layout. The values in a leaf are grouped by column, so a query that reads one column of a wide table reads one region of each page. |
| **Pin** | Holding a page in its frame while code reads it, so the buffer pool cannot evict the page during the read. Reading from a pinned page uses the page's bytes where they are, without copying them. |
| **Rowid** | The 64 bit integer that identifies a row in an ordinary table. A table declared `WITHOUT ROWID` has no rowid and is keyed by its primary key. |
| **Schema cookie** | A number that changes every time the catalog changes. A prepared statement compiled against an older catalog sees the new number and compiles itself again. |
| **Shadow table** | An ordinary table that a virtual table stores its data in. An `inillucent_search` table keeps its index in five shadow tables, such as `docs_content` and `docs_gen`. |
| **Torn page** | A page that a crash left half written, with some old bytes and some new bytes. The checksum in each page header detects a torn page. |

## Transactions and the log

| Term | What it means |
|---|---|
| **Busy** | The error a statement gets when another connection holds the lock it needs and `PRAGMA busy_timeout` has run out. Its status name is `busy`. |
| **`busy_timeout`** | The pragma that sets how long a connection waits for a lock before it fails with `busy`. The default is 5000 milliseconds. |
| **Checkpoint** | Copying the changes recorded in the write ahead log into the database file, and then deleting the part of the log that is no longer needed. After a checkpoint, recovery has less log to read. The `inillucent checkpoint` command runs one. |
| **Commit timestamp** | The number a transaction receives when it commits. It decides which version of a row a snapshot sees. |
| **Durability** | The promise that a committed transaction survives a crash of the process, the operating system or the machine. |
| **fsync** | The operating system call that makes written bytes reach the disk. It is the slow part of a commit. `PRAGMA synchronous` controls how often the engine calls it. |
| **Group commit** | Writing the log records of several commits with one fsync, so each commit pays less for the disk write. |
| **Journal** | A file that holds a copy of a page from before a change, so a crash can put the old page back. inillucent accepts SQLite's six `PRAGMA journal_mode` values, and `delete` is the default. In inillucent the journal mode decides how a checkpoint is protected, and the write ahead log records every change in every mode. |
| **`locking_mode`** | The pragma that decides whether a connection keeps its file lock. The default, `normal`, releases the lock between statements, so other processes can use the file. `exclusive` keeps the lock until the connection closes. |
| **Log record** | One entry in the write ahead log: a page image, a row change, a commit or a checkpoint. |
| **LSN** | Log sequence number: the position of a record in the write ahead log. Every page stores the LSN of the last record that changed it, so recovery can skip records the page already has. |
| **MVCC** | Multiversion concurrency control: keeping several versions of a row so a reader can read an old version while a writer writes a new one. |
| **Recovery** | What the engine does when it opens a file after a crash. It reads the write ahead log from the last checkpoint and applies the records the database file does not have yet. |
| **Redo** | Applying a log record to a page during recovery. |
| **Savepoint** | A named point inside a transaction. `ROLLBACK TO` a savepoint undoes the work done after it without ending the transaction. |
| **Snapshot** | The state of the database at one moment. A reader that uses a snapshot sees the same data for the whole of its transaction. |
| **`synchronous`** | The pragma that decides how often a commit calls fsync. `FULL` waits for the disk on every commit, and `OFF` never waits. |
| **Transaction** | A group of statements that all take effect or none do. `BEGIN` starts one, `COMMIT` keeps its changes and `ROLLBACK` discards them. In the Rust driver a [`Transaction`](../drivers/README.md) is a value, and dropping it rolls the transaction back. |
| **Undo** | Putting back the old contents of what a transaction changed, when the transaction rolls back. |
| **WAL** | Write ahead log. A file the engine appends a record to before it changes the database file. The record reaches the disk before the page does, so a crash can always be repaired from the log. inillucent's log files sit beside the database and are named `<database>-wal.NNNNNNNNNN`. |

## SQL

| Term | What it means |
|---|---|
| **Affinity** | SQLite's rule for how a column's declared type changes a value written into it. A `TEXT` column stores `1` as `'1'`, and an `INTEGER` column stores `'1'` as `1`. Affinity converts a value when it can and stores the value unchanged when it cannot. |
| **`ATTACH`** | The statement that opens a second database file on the same connection, so one query can read tables from both files. |
| **Binder** | The step that looks up every name in a statement in the catalog: which table, which column, which type. A missing table is reported by the binder. |
| **Bound parameter** | A placeholder such as `?1` in a statement, filled with a value when the statement runs. Binding values keeps them out of the SQL text, which prevents SQL injection. |
| **Collation** | The rule for comparing two text values. `BINARY` compares bytes and `NOCASE` ignores the case of ASCII letters. The collation decides sort order and whether two values are equal. |
| **Covering index** | An index that holds every column a query needs, so the query never reads the table. |
| **Executor** | The part of the engine that runs a plan and produces rows. inillucent's executor works on batches of rows. |
| **FTS5** | SQLite's full text search module. It is a virtual table that indexes words, and inillucent answers the same SQL for it. |
| **Parser** | The step that turns SQL text into a syntax tree. A statement that is not valid SQL fails in the parser. |
| **Plan** | The steps the planner chose to answer a query. `EXPLAIN QUERY PLAN` prints it. |
| **Planner** | The step that decides how to answer a query: which index to use, which order to join tables in, and whether a sort is needed. |
| **Pragma** | A statement that reads or changes a setting of the engine, such as `PRAGMA busy_timeout` or `PRAGMA integrity_check`. [Pragmas](pragmas.md) lists every pragma inillucent recognises. |
| **Prepared statement** | A statement compiled once and run many times with different bound values. |
| **R-Tree** | SQLite's module for indexing rectangles, used to find shapes that overlap an area. It is a virtual table. |
| **Scan** | Reading every row of a table or an index in order. |
| **Seek** | Going straight to one key in an index with a descent, without a scan. |
| **Storage class** | The type a value actually has when it is stored: NULL, INTEGER, REAL, TEXT or BLOB. It can differ from the column's declared type. |
| **`STRICT`** | A table option that makes the engine refuse a value whose storage class does not match the column's declared type. |
| **Trigger** | A statement the engine runs by itself when a row is inserted, updated or deleted. |
| **Virtual table** | A table whose rows come from a module instead of from a B-tree. FTS5, the R-Tree, `generate_series` and `inillucent_search` are virtual tables. They are read and written with ordinary SQL. |
| **VFS** | Virtual file system: the layer between the engine and the operating system's files. It opens, reads, writes, syncs and locks. The tests swap in a VFS that injects faults. |
| **`WITHOUT ROWID`** | A table option that stores rows keyed by the primary key, with no rowid. |

## Search

| Term | What it means |
|---|---|
| **Abstention** | Returning no result when nothing in the data answers the query. inillucent decides to abstain by comparing each hit's confidence with a threshold. |
| **Approximate search** | Finding nearest neighbors by checking only part of the data, through an index such as HNSW. It is much faster than exact search and can miss a correct answer. |
| **BM25** | The standard formula for scoring how well a piece of text matches a set of query words. It rewards rare words and words that appear often in a short text. |
| **Chunk** | A piece of a document, about a paragraph long, that is indexed and returned as one search result. |
| **Confidence** | A number that each search hit carries beside its score. The score decides the order of the hits. The confidence says how likely the hit is to answer the query, and it is the number a search compares with its abstention threshold. |
| **Cosine distance** | A measure of how far apart two vectors point, ignoring their length. 0 means the same direction. `vector_distance_cos` computes it. |
| **Embedding** | A vector that a trained model produces from a piece of text. Texts with similar meaning get vectors that are close together, even when they share no words. inillucent's default model produces 768 numbers per text. |
| **Exact search** | Finding nearest neighbors by comparing the query with every vector. It is always correct, and it is slow on large tables. |
| **Facet** | A column of an `inillucent_search` table declared `FACET`. A search can filter on a facet while it runs, and the facet's value is not indexed as text. |
| **Generation** | One published version of a built search index, stored in the `%_gen` shadow table. A new generation is written beside the old one, and the old one is never edited. |
| **HNSW** | Hierarchical navigable small world: a graph index for vectors. Each vector is linked to a few near neighbors, and a search walks the links toward the query instead of comparing it with every vector. |
| **Hybrid search** | A search that runs a keyword search and a vector search and combines the two ranked lists into one. |
| **Inverted index** | A lookup from each word to the chunks that contain it. It is what makes keyword search fast. |
| **Keyword search** | Finding the chunks that contain the query's words. Also called **lexical search**. |
| **MCP** | Model Context Protocol: a standard way for an AI agent to call tools. `inillucent-mcp` serves inillucent's commands as MCP tools over standard input and output. |
| **Nearest neighbor** | The stored vector closest to a query vector. A vector search returns the k nearest neighbors. |
| **Posting** | One entry in an inverted index: a chunk that contains a word, and where the word appears in it. |
| **Quantization** | Storing each number of a vector with fewer bits to save memory. inillucent stores one byte per number (int8) and checks the top candidates again against the full vectors. |
| **Recall** | The share of the true nearest neighbors that an approximate search returned. A recall of 1.0 means it found all of them. |
| **Reciprocal rank fusion** | A way to combine two ranked lists by adding one divided by each hit's rank in each list. inillucent offers it as an option. The default fusion adds the two scores after scaling each list to the same range. |
| **Semantic search** | Finding the chunks whose embeddings are close to the query's embedding, which matches meaning instead of words. Also called **vector search**. |
| **Vector** | A list of numbers. In a `VECTOR(N)` column every value is a list of N 32 bit floats. |

## Programs, testing and measurement

| Term | What it means |
|---|---|
| **C ABI** | The plain C interface of `inillucent-driver-capi`. Every language binding (Python, Node, Go, PHP) calls the engine through it. |
| **Capability** | One row of what `inillucent capabilities` prints: a feature name and `yes`, `partial` or `no`. A test runs each row against the engine. |
| **Crate** | A Rust package. The inillucent repository is a workspace of 29 crates, such as `inillucent-sql` and `inillucent-cli`. |
| **Differential probe** | A test that runs the same SQL through inillucent and through SQLite and compares the output byte for byte. [Feature comparison](feature-comparison.md) reports its 416 cases. |
| **Fixture** | A prepared database file that a test or a benchmark reads, such as the `medium` fixture the performance gate uses. |
| **Geometric mean** | The average of several ratios found by multiplying them and taking the root. It is used to combine the speed ratios of many workloads into one number. |
| **Lower bound** | The low end of a 95% confidence interval. A performance gate passes a result only when its lower bound is past the target. |
| **Oracle** | The program whose answers a test treats as correct. inillucent's tests use a pinned build of SQLite 3.53.4 as the oracle for SQL results. |
| **p50, p95** | The median and the 95th percentile of a set of timings. |
| **Resident set** | The memory a process actually holds in RAM. [Performance](performance.md) reports the peak. |
| **Unsupported** | The status an inillucent command returns for SQL the engine has not built yet. The command line exits with code 3. |

## Where to read more

| You want | Page |
|---|---|
| both engines in one diagram | [Architecture in one page](architecture-overview.md) |
| how the SQL engine works | [Relational architecture](relational-architecture.md) |
| how the search engine works | [Architecture](architecture.md) |
| which SQL runs | [SQL support](sql.md) |
| every pragma | [Pragmas](pragmas.md) |
| the crates and the rules they follow | [Repository](repository.md) |
