# Pragmas

Every `PRAGMA` this engine recognises. **Generated** from
`inillucent_sql::pragma_register::REGISTER` by
`cargo run -p inillucent-compat --bin inillucent-obligations`, and checked by
`cargo test -p inillucent-compat --test harness`, which fails when this page and
the register disagree. Do not edit it by hand.

**68 pragmas**, 62 of which take an argument in parentheses.

A pragma this table does not list is not recognised, and answers no rows rather
than an error - which is SQLite's own behaviour, and is why asking for one is not
a way to find out whether it exists. What each one *does* is
[SQL support](sql.md); what is below is what a caller has to know before writing
one: its name, the columns its answer has, and whether it takes an argument.

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

## The two defaults that decide what a second process sees

`busy_timeout` starts at **5000** milliseconds. It is how long a statement waits
for a file another process holds before it is refused with `busy`, and setting it
to 0 makes a contended statement fail at once. It governs the wait between
processes as well as the one inside a process; before this the cross-process
wait was a constant this pragma could not reach.

`locking_mode` starts at **normal**, which is SQLite's default too: the file lock
is released between statements, so a second process can open the database.
`exclusive` keeps the lock for the connection's whole life, which is faster for a
program that never opens a second connection and means a second process waits out
that connection or is refused. A value that is neither is an error rather than a
silently kept setting.

`compat/api/pragmas.toml` is the same register in the form a program reads,
and `docs/README.md` lists this page in its reading order.
