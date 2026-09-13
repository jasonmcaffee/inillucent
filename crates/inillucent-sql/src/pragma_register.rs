//! `PRAGMA`: the register of names, the columns each answers, and whether it
//! takes an argument.
//!
//! Invariant: this is the structural half of a pragma - what a caller has to
//! know before it writes one - and nothing here decides what any pragma does.
//! That is why it lives here rather than beside an engine: `PRAGMA
//! busy_timeout` names the same column and takes an argument the same way
//! whichever engine answers it, so the register is shared and each engine's
//! own `pragma` module supplies the behaviour.
//!
//! It lived in `inillucent-session`, the old engine's connection, until the old
//! engine was retired. `compat/api/pragmas.toml` is generated from
//! [`REGISTER`] by `inillucent-compat`'s `inillucent-obligations` binary, and
//! that register has to survive whichever engine ships to keep meaning
//! anything.

/// What a pragma is, in the register.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PragmaSpec {
    /// The name, folded.
    pub name: &'static str,
    /// The column names of its answer, in order.
    pub columns: &'static [&'static str],
    /// Whether it takes an argument in parentheses, as `table_info(t)` does.
    pub takes_argument: bool,
}

/// Every pragma this build knows, in the order `pragma_list` reports them.
///
/// The order is alphabetical because that is what `pragma_list` produces and
/// what a differential test comparing the two engines' lists will see.
pub const REGISTER: &[PragmaSpec] = &[
    boolean("analysis_limit"),
    boolean("application_id"),
    boolean("auto_vacuum"),
    boolean("automatic_index"),
    PragmaSpec {
        name: "busy_timeout",
        columns: &["timeout"],
        takes_argument: true,
    },
    boolean("cache_size"),
    boolean("cache_spill"),
    boolean("case_sensitive_like"),
    boolean("cell_size_check"),
    boolean("checkpoint_fullfsync"),
    PragmaSpec {
        name: "collation_list",
        columns: &["seq", "name"],
        takes_argument: false,
    },
    PragmaSpec {
        name: "compile_options",
        columns: &["compile_options"],
        takes_argument: false,
    },
    boolean("count_changes"),
    boolean("data_store_directory"),
    boolean("data_version"),
    PragmaSpec {
        name: "database_list",
        columns: &["seq", "name", "file"],
        takes_argument: false,
    },
    boolean("default_cache_size"),
    boolean("defensive"),
    boolean("defer_foreign_keys"),
    boolean("empty_result_callbacks"),
    PragmaSpec {
        name: "encoding",
        columns: &["encoding"],
        takes_argument: true,
    },
    PragmaSpec {
        name: "foreign_key_check",
        columns: &["table", "rowid", "parent", "fkid"],
        takes_argument: true,
    },
    PragmaSpec {
        name: "foreign_key_list",
        columns: &[
            "id",
            "seq",
            "table",
            "from",
            "to",
            "on_update",
            "on_delete",
            "match",
        ],
        takes_argument: true,
    },
    boolean("foreign_keys"),
    boolean("freelist_count"),
    boolean("full_column_names"),
    boolean("fullfsync"),
    PragmaSpec {
        name: "function_list",
        columns: &["name", "builtin", "type", "enc", "narg", "flags"],
        takes_argument: false,
    },
    boolean("hard_heap_limit"),
    boolean("ignore_check_constraints"),
    boolean("incremental_vacuum"),
    PragmaSpec {
        name: "index_info",
        columns: &["seqno", "cid", "name"],
        takes_argument: true,
    },
    PragmaSpec {
        name: "index_list",
        columns: &["seq", "name", "unique", "origin", "partial"],
        takes_argument: true,
    },
    PragmaSpec {
        name: "index_xinfo",
        columns: &["seqno", "cid", "name", "desc", "coll", "key"],
        takes_argument: true,
    },
    PragmaSpec {
        name: "integrity_check",
        columns: &["integrity_check"],
        takes_argument: true,
    },
    PragmaSpec {
        name: "journal_mode",
        columns: &["journal_mode"],
        takes_argument: true,
    },
    boolean("journal_size_limit"),
    boolean("legacy_alter_table"),
    PragmaSpec {
        name: "locking_mode",
        columns: &["locking_mode"],
        takes_argument: true,
    },
    boolean("max_page_count"),
    boolean("mmap_size"),
    PragmaSpec {
        name: "module_list",
        columns: &["name"],
        takes_argument: false,
    },
    boolean("optimize"),
    boolean("page_count"),
    boolean("page_size"),
    PragmaSpec {
        name: "pragma_list",
        columns: &["name"],
        takes_argument: false,
    },
    boolean("query_only"),
    PragmaSpec {
        name: "quick_check",
        columns: &["quick_check"],
        takes_argument: true,
    },
    boolean("read_uncommitted"),
    boolean("recursive_triggers"),
    boolean("reverse_unordered_selects"),
    boolean("schema_version"),
    boolean("secure_delete"),
    boolean("short_column_names"),
    boolean("shrink_memory"),
    boolean("soft_heap_limit"),
    boolean("synchronous"),
    PragmaSpec {
        name: "table_info",
        columns: &["cid", "name", "type", "notnull", "dflt_value", "pk"],
        takes_argument: true,
    },
    PragmaSpec {
        name: "table_list",
        columns: &["schema", "name", "type", "ncol", "wr", "strict"],
        takes_argument: true,
    },
    PragmaSpec {
        name: "table_xinfo",
        columns: &[
            "cid",
            "name",
            "type",
            "notnull",
            "dflt_value",
            "pk",
            "hidden",
        ],
        takes_argument: true,
    },
    boolean("temp_store"),
    boolean("temp_store_directory"),
    boolean("threads"),
    boolean("trusted_schema"),
    boolean("user_version"),
    PragmaSpec {
        name: "wal_autocheckpoint",
        columns: &["wal_autocheckpoint"],
        takes_argument: true,
    },
    PragmaSpec {
        name: "wal_checkpoint",
        columns: &["busy", "log", "checkpointed"],
        takes_argument: true,
    },
    boolean("writable_schema"),
];

/// Returns the register row for a pragma that answers with its own name.
///
/// Most of them do: `PRAGMA cache_size` answers one column called `cache_size`,
/// and so do the two dozen others whose whole answer is a setting's value.
const fn boolean(name: &'static str) -> PragmaSpec {
    PragmaSpec {
        name,
        columns: &[],
        takes_argument: true,
    }
}

/// Returns the register row a name spells.
pub fn spec(name: &[u8]) -> Option<&'static PragmaSpec> {
    let folded = name.to_ascii_lowercase();
    REGISTER
        .iter()
        .find(|entry| entry.name.as_bytes() == folded.as_slice())
}

/// Returns the column names a pragma's answer carries.
pub fn columns(name: &[u8]) -> Vec<Vec<u8>> {
    let Some(spec) = spec(name) else {
        return Vec::new();
    };
    if spec.columns.is_empty() {
        return vec![spec.name.as_bytes().to_vec()];
    }
    spec.columns
        .iter()
        .map(|column| column.as_bytes().to_vec())
        .collect()
}
