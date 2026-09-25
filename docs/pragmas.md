# Pragmas

A pragma is a statement that reads or changes a setting of the database, such
as `PRAGMA page_size` or `PRAGMA busy_timeout = 2000`. This page lists every
pragma the engine recognises.

This page is generated. `cargo run -p inillucent-compat --bin inillucent-obligations`
writes it from `inillucent_sql::pragma_register::REGISTER`, and
`cargo test -p inillucent-compat --test tooling harness::` fails when the page and the
register differ. Do not edit it by hand.

There are **68 pragmas**. 62 of them take an argument in parentheses.

A pragma that is not in this table is not recognised. It returns no rows and no
error, which is also what SQLite does. So running a pragma does not tell you
whether the engine knows it. Check this table instead.

The table gives each pragma's name, whether it takes an argument, and the
columns of the rows it returns. [SQL support](sql.md) describes what the
pragmas do.

| pragma | takes an argument | columns of its answer |
|---|---|---|
| `analysis_limit` | yes | one unnamed column |
| `application_id` | yes | one unnamed column |
| `auto_vacuum` | yes | one unnamed column |
| `automatic_index` | yes | one unnamed column |
| `busy_timeout` | yes | `timeout` |
| `cache_size` | yes | one unnamed column |
| `cache_spill` | yes | one unnamed column |
| `case_sensitive_like` | yes | one unnamed column |
| `cell_size_check` | yes | one unnamed column |
| `checkpoint_fullfsync` | yes | one unnamed column |
| `collation_list` | no | `seq`, `name` |
| `compile_options` | no | `compile_options` |
| `count_changes` | yes | one unnamed column |
| `data_store_directory` | yes | one unnamed column |
| `data_version` | yes | one unnamed column |
| `database_list` | no | `seq`, `name`, `file` |
| `default_cache_size` | yes | one unnamed column |
| `defensive` | yes | one unnamed column |
| `defer_foreign_keys` | yes | one unnamed column |
| `empty_result_callbacks` | yes | one unnamed column |
| `encoding` | yes | `encoding` |
| `foreign_key_check` | yes | `table`, `rowid`, `parent`, `fkid` |
| `foreign_key_list` | yes | `id`, `seq`, `table`, `from`, `to`, `on_update`, `on_delete`, `match` |
| `foreign_keys` | yes | one unnamed column |
| `freelist_count` | yes | one unnamed column |
| `full_column_names` | yes | one unnamed column |
| `fullfsync` | yes | one unnamed column |
| `function_list` | no | `name`, `builtin`, `type`, `enc`, `narg`, `flags` |
| `hard_heap_limit` | yes | one unnamed column |
| `ignore_check_constraints` | yes | one unnamed column |
| `incremental_vacuum` | yes | one unnamed column |
| `index_info` | yes | `seqno`, `cid`, `name` |
| `index_list` | yes | `seq`, `name`, `unique`, `origin`, `partial` |
| `index_xinfo` | yes | `seqno`, `cid`, `name`, `desc`, `coll`, `key` |
| `integrity_check` | yes | `integrity_check` |
| `journal_mode` | yes | `journal_mode` |
| `journal_size_limit` | yes | one unnamed column |
| `legacy_alter_table` | yes | one unnamed column |
| `locking_mode` | yes | `locking_mode` |
| `max_page_count` | yes | one unnamed column |
| `mmap_size` | yes | one unnamed column |
| `module_list` | no | `name` |
| `optimize` | yes | one unnamed column |
| `page_count` | yes | one unnamed column |
| `page_size` | yes | one unnamed column |
| `pragma_list` | no | `name` |
| `query_only` | yes | one unnamed column |
| `quick_check` | yes | `quick_check` |
| `read_uncommitted` | yes | one unnamed column |
| `recursive_triggers` | yes | one unnamed column |
| `reverse_unordered_selects` | yes | one unnamed column |
| `schema_version` | yes | one unnamed column |
| `secure_delete` | yes | one unnamed column |
| `short_column_names` | yes | one unnamed column |
| `shrink_memory` | yes | one unnamed column |
| `soft_heap_limit` | yes | one unnamed column |
| `synchronous` | yes | one unnamed column |
| `table_info` | yes | `cid`, `name`, `type`, `notnull`, `dflt_value`, `pk` |
| `table_list` | yes | `schema`, `name`, `type`, `ncol`, `wr`, `strict` |
| `table_xinfo` | yes | `cid`, `name`, `type`, `notnull`, `dflt_value`, `pk`, `hidden` |
| `temp_store` | yes | one unnamed column |
| `temp_store_directory` | yes | one unnamed column |
| `threads` | yes | one unnamed column |
| `trusted_schema` | yes | one unnamed column |
| `user_version` | yes | one unnamed column |
| `wal_autocheckpoint` | yes | `wal_autocheckpoint` |
| `wal_checkpoint` | yes | `busy`, `log`, `checkpointed` |
| `writable_schema` | yes | one unnamed column |

## Two defaults that matter when several processes share a file

`busy_timeout` starts at **5000** milliseconds. When another process holds the
file, a statement waits up to this long and then fails with `busy`. Set
`busy_timeout` to 0 to make the statement fail at once. `busy_timeout` applies to
waits between processes and to waits inside one process.

`locking_mode` starts at **normal**, which is also SQLite's default. In normal
mode the file lock is released between statements, so a second process can open
the database. In `exclusive` mode the connection keeps the lock until it closes.
`exclusive` is faster for a program that only ever opens one connection. While
it holds the lock, a second process waits for `busy_timeout` and then fails. Any
value other than `normal` or `exclusive` is an error.

`compat/api/pragmas.toml` holds the same register in a form a program can read.
