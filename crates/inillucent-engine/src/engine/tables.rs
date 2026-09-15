//! Questions asked about one table's declaration.
//!
//! Invariant: **every answer comes from the declaration handed in.** Nothing
//! here reads the file, the catalog or the connection, so the answers are the
//! same whether the declaration came from the catalog tree or from a statement
//! that has not been committed yet.

use crate::*;

/// Returns the value of one `name=value` module argument, when it is there.
///
/// **Only the arguments the engine wrote itself.** Deriving a module's columns
/// from its `CREATE` text would be a second implementation of its argument
/// grammar; reading back a marker this engine put there is not, and there is
/// nowhere else durable to keep the link between an index and the table it
/// indexes.
///
/// @param arguments - the module's arguments, as written
/// @param name - the argument to find
pub(crate) fn argument_of(arguments: &[Vec<u8>], name: &[u8]) -> Option<Vec<u8>> {
    for argument in arguments {
        let text = String::from_utf8_lossy(argument);
        let Some((key, value)) = text.split_once('=') else {
            continue;
        };
        if key.trim().as_bytes().eq_ignore_ascii_case(name) {
            let value = value.trim();
            if value.is_empty() {
                return None;
            }
            return Some(value.as_bytes().to_vec());
        }
    }
    None
}

/// Returns `sqlite_schema` under the name almost every tool actually types.
///
/// **`sqlite_master` is the same table, and a database that could not answer it
/// would be one no existing tool could inspect.** SQLite accepts both names;
/// the old engine synthesised the alias in `inillucent-catalog`, through a
/// helper that reaches into `inillucent-storage` and so cannot outlive it. This
/// is the same idea with the new engine's own schema table, registered beside
/// it rather than instead of it.
///
/// The alias is a name that resolves, not a row: `sqlite_schema` has never
/// listed itself, and it does not list this either.
///
/// @param schema - the schema table's own declaration
pub(crate) fn schema_alias_of(schema: &TableInfo) -> TableInfo {
    schema_named(schema, b"sqlite_master")
}

/// Returns one schema's catalog declaration under another name.
///
/// A temporary database's catalog is `sqlite_temp_schema` and
/// `sqlite_temp_master`; an attached one's is `sqlite_schema` and
/// `sqlite_master` under its own qualifier. Same tree, same five columns, same
/// handle - only the name a statement writes differs.
///
/// @param schema - the catalog declaration to rename
/// @param name - the name it will answer to
pub(crate) fn schema_named(schema: &TableInfo, name: &[u8]) -> TableInfo {
    let mut alias = schema.clone();
    alias.name = name.to_vec();
    alias.folded = name.to_ascii_lowercase();
    alias
}

/// Returns whether a table is one a virtual table owns.
///
/// FTS5 keeps `<name>_data`, `<name>_idx`, `<name>_docsize`, `<name>_content`
/// and `<name>_config`; R*Tree keeps `<name>_node`, `<name>_rowid` and
/// `<name>_parent`. Recognised by the prefix rather than by a list of suffixes,
/// because the suffixes are the module's to choose and a list would be right
/// until a module added one.
///
/// @param owners - the folded names of the file's virtual tables
/// @param folded - the folded name of the table being considered
pub(crate) fn is_shadow_of(owners: &[Vec<u8>], folded: &[u8]) -> bool {
    owners.iter().any(|owner| {
        folded
            .strip_prefix(owner.as_slice())
            .and_then(|rest| rest.strip_prefix(b"_".as_slice()))
            .is_some_and(|suffix| !suffix.is_empty())
    })
}

/// Returns whether a declared column is a `VIRTUAL` generated one.
///
/// The one predicate behind the whole `VIRTUAL` shift. A `VIRTUAL` generated
/// column is computed on read and never written, so it occupies no field in a
/// SQLite record and no column in one of this engine's trees; a `STORED` one is
/// an ordinary column that happens to have been filled in by an expression.
///
/// @param info - the table's declaration
/// @param declared - the column's declared position
pub(crate) fn is_virtual_column(info: &TableInfo, declared: usize) -> bool {
    info.columns
        .get(declared)
        .is_some_and(|column| column.generated && !column.stored)
}

/// Returns the declared positions a table's record holds, in record order.
///
/// **One derivation of "which columns are actually stored", used by the shape,
/// the import and the row comparison alike.** Everything that walks a record -
/// the tree builder, the fixture importer, `logical_row` - used to walk
/// `0..info.columns.len()` and so silently assumed that a declared position and
/// a record field are the same number. They are, until a table declares a
/// `VIRTUAL` generated column, after which every later column reads one field
/// early and returns its neighbour's value: data rather than a refusal, which
/// is the one failure this engine is not allowed to have.
///
/// The rowid-alias column is included, because SQLite's record does carry a
/// (NULL) field for it and the callers drop it themselves.
///
/// @param info - the table's declaration
pub(crate) fn stored_positions(info: &TableInfo) -> Vec<usize> {
    (0..info.columns.len())
        .filter(|declared| !is_virtual_column(info, *declared))
        .collect()
}
