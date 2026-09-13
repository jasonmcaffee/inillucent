//! Reading and writing a module's own shadow tables.
//!
//! Invariant: a module reaches exactly the tables it was told about, by the
//! roots it was handed, and no others. There is no name resolution here and no
//! catalog: `ShadowTables` is a list of root pages that the host looked up once
//! and gave to the module, so a module that wanted to read somebody else's
//! table would have to be handed it.
//!
//! The rows are ordinary rows in ordinary b-trees, which is what makes a
//! module's storage visible to `PRAGMA integrity_check`, to `VACUUM`, and to
//! the other engine. FTS5 and R-Tree both keep their whole state this way, and
//! it is why a database either of them writes can be opened by SQLite.
//!
//! ## One arm, not two
//!
//! Every method here used to carry a second arm that reached a `Pager`
//! directly, which is what made this crate - a crate the *new* engine links -
//! depend on `inillucent-storage`, the storage model the rearchitecture
//! retired. That arm is now `inillucent_vm::shadow_pager::PagerShadowStore`,
//! an implementation of the same `ShadowStore` trait, so both engines reach
//! their rows the same way and only the retired crates name the retired
//! storage.
//!
//! What is left is a refusal for a caller that supplied no store at all. It
//! cannot happen from either engine and it is a refusal rather than a panic,
//! because a `Context` built for no engine is a caller's mistake and this
//! crate answers a caller's mistake with an error.

use std::collections::BTreeMap;

use inillucent_base::ids::PageId;
use inillucent_base::{error, DbResult};
use inillucent_sql::vtab::ShadowStore;
use inillucent_value::Value;

use crate::vtab::{Context, ModuleArguments};

/// Returns the store a module's rows live in.
///
/// @param context - the call's context
fn store_of<'c>(context: &'c mut Context<'_>) -> DbResult<&'c mut dyn ShadowStore> {
    match context.host.shadow_store() {
        Some(store) => Ok(store),
        None => Err(error::misuse(
            "this host has no shadow store, so a module has nowhere to keep its rows",
        )),
    }
}

/// The root pages of one module's shadow tables, by the suffix that names them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShadowTables {
    roots: BTreeMap<Vec<u8>, u32>,
}

impl ShadowTables {
    /// Returns the roots of the tables a module needs, refusing a missing one.
    ///
    /// A missing shadow table is a corrupt schema rather than a state to cope
    /// with: the module's whole storage is those tables, and one that is not
    /// there means the rows are not there either.
    pub fn of(arguments: &ModuleArguments, needed: &[&[u8]]) -> DbResult<ShadowTables> {
        let mut roots = BTreeMap::new();
        for suffix in needed {
            let Some(root) = arguments.shadow(suffix) else {
                return Err(error::corrupt(format!(
                    "the shadow table {}_{} is missing",
                    String::from_utf8_lossy(&arguments.table),
                    String::from_utf8_lossy(suffix)
                )));
            };
            roots.insert(suffix.to_vec(), root);
        }
        Ok(ShadowTables { roots })
    }

    /// Returns one shadow table's root as the store names it.
    ///
    /// @param suffix - which shadow table
    pub fn root_id(&self, suffix: &[u8]) -> DbResult<u32> {
        self.roots.get(suffix).copied().ok_or_else(|| {
            error::misuse(format!(
                "no shadow table {}",
                String::from_utf8_lossy(suffix)
            ))
        })
    }

    /// Returns one shadow table's root page.
    pub fn root(&self, suffix: &[u8]) -> DbResult<PageId> {
        let Some(root) = self.roots.get(suffix).copied() else {
            return Err(error::misuse(format!(
                "no shadow table {}",
                String::from_utf8_lossy(suffix)
            )));
        };
        PageId::from_persisted(root)
    }

    /// Reads one row by rowid, or nothing when there is not one.
    ///
    /// The row comes back with its rowid in place of the first column, because
    /// a rowid table's first column *is* the rowid when it is declared
    /// `INTEGER PRIMARY KEY` - and every shadow table here is.
    pub fn read_row(
        &self,
        context: &mut Context<'_>,
        suffix: &[u8],
        rowid: i64,
    ) -> DbResult<Option<Vec<Value<'static>>>> {
        let root = self.root_id(suffix)?;
        store_of(context)?.read_row(root, rowid)
    }

    /// Writes one row by rowid, replacing whatever was there.
    pub fn write_row(
        &self,
        context: &mut Context<'_>,
        suffix: &[u8],
        rowid: i64,
        values: &[Value<'static>],
    ) -> DbResult<()> {
        let root = self.root_id(suffix)?;
        store_of(context)?.write_row(root, rowid, values)
    }

    /// Removes one row by rowid, reporting nothing when there was not one.
    pub fn delete_row(&self, context: &mut Context<'_>, suffix: &[u8], rowid: i64) -> DbResult<()> {
        let root = self.root_id(suffix)?;
        store_of(context)?.delete_row(root, rowid)
    }

    /// Returns the largest rowid one shadow table holds.
    pub fn max_rowid(&self, context: &mut Context<'_>, suffix: &[u8]) -> DbResult<i64> {
        let root = self.root_id(suffix)?;
        store_of(context)?.max_rowid(root)
    }

    /// Runs a body over every row of one shadow table, in rowid order.
    pub fn scan(
        &self,
        context: &mut Context<'_>,
        suffix: &[u8],
        mut body: impl FnMut(i64, &[Value<'static>]) -> DbResult<bool>,
    ) -> DbResult<()> {
        let root = self.root_id(suffix)?;
        store_of(context)?.scan(root, &mut body)
    }

    /// Runs a body over every row of one shadow table whose rowid is at
    /// least `from`, in rowid order.
    ///
    /// A rowid table's rows are already in key order, so a caller holding a
    /// watermark - "everything above sequence N" is the shape every caller of
    /// this has - seeks to it once rather than reading and discarding
    /// everything below it on every call.
    /// @param from - the smallest rowid to visit
    pub fn scan_from(
        &self,
        context: &mut Context<'_>,
        suffix: &[u8],
        from: i64,
        mut body: impl FnMut(i64, &[Value<'static>]) -> DbResult<bool>,
    ) -> DbResult<()> {
        let root = self.root_id(suffix)?;
        store_of(context)?.scan_from(root, from, &mut body)
    }
}

impl ShadowTables {
    /// Reads one row of a keyed shadow table, or nothing when there is not one.
    ///
    /// A `WITHOUT ROWID` table's b-tree holds the whole row as its key, ordered
    /// by the primary key's columns - so a lookup is a seek on a record and
    /// what comes back is the record itself.
    pub fn read_keyed(
        &self,
        context: &mut Context<'_>,
        suffix: &[u8],
        key: &[Value<'static>],
        columns: usize,
    ) -> DbResult<Option<Vec<Value<'static>>>> {
        let root = self.root_id(suffix)?;
        store_of(context)?.read_keyed(root, key, columns)
    }

    /// Writes one row of a keyed shadow table, replacing whatever was there.
    pub fn write_keyed(
        &self,
        context: &mut Context<'_>,
        suffix: &[u8],
        key_columns: usize,
        values: &[Value<'static>],
    ) -> DbResult<()> {
        let root = self.root_id(suffix)?;
        store_of(context)?.write_keyed(root, key_columns, values)
    }

    /// Removes one row of a keyed shadow table.
    pub fn delete_keyed(
        &self,
        context: &mut Context<'_>,
        suffix: &[u8],
        key: &[Value<'static>],
    ) -> DbResult<()> {
        let root = self.root_id(suffix)?;
        store_of(context)?.delete_keyed(root, key)
    }

    /// Runs a body over every row of a keyed shadow table, in key order.
    pub fn scan_keyed(
        &self,
        context: &mut Context<'_>,
        suffix: &[u8],
        key_columns: usize,
        mut body: impl FnMut(&[Value<'static>]) -> DbResult<bool>,
    ) -> DbResult<()> {
        let root = self.root_id(suffix)?;
        store_of(context)?.scan_keyed(root, key_columns, &mut body)
    }

    /// Runs a body over every row of a keyed shadow table whose key sorts at
    /// or after `from`, in key order.
    ///
    /// The keyed twin of [`Self::scan_from`]: a caller holding a starting key
    /// of more than one column - `terms_with_prefix`'s `(segid, prefix)`, one
    /// live segment at a time - seeks to it once rather than reading and
    /// discarding every row that sorts below it.
    ///
    /// @param from - the key to start at
    pub fn scan_keyed_from(
        &self,
        context: &mut Context<'_>,
        suffix: &[u8],
        key_columns: usize,
        from: &[Value<'static>],
        mut body: impl FnMut(&[Value<'static>]) -> DbResult<bool>,
    ) -> DbResult<()> {
        let root = self.root_id(suffix)?;
        store_of(context)?.scan_keyed_from(root, key_columns, from, &mut body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use inillucent_sql::vtab::ShadowRoot;

    /// Builds the arguments a module would be connected with.
    fn arguments(shadows: &[(&[u8], u32)]) -> ModuleArguments {
        ModuleArguments {
            database: 0,
            schema: b"main".to_vec(),
            table: b"t".to_vec(),
            module: b"rtree".to_vec(),
            arguments: Vec::new(),
            shadows: shadows
                .iter()
                .map(|(suffix, root)| ShadowRoot {
                    suffix: suffix.to_vec(),
                    root: *root,
                })
                .collect(),
        }
    }

    /// Every table a module names has to be there.
    #[test]
    fn a_missing_shadow_table_is_refused() {
        let arguments = arguments(&[(b"node", 4)]);
        assert!(ShadowTables::of(&arguments, &[b"node"]).is_ok());
        let missing = ShadowTables::of(&arguments, &[b"node", b"rowid"]);
        assert!(missing.is_err());
    }

    /// A root a module was not given is one it cannot reach.
    #[test]
    fn an_unlisted_table_cannot_be_reached() {
        let tables = ShadowTables::of(&arguments(&[(b"node", 4)]), &[b"node"]).expect("built");
        assert!(tables.root(b"node").is_ok());
        assert!(tables.root(b"secret").is_err());
    }
}
