//! Moving pages: incremental vacuum, auto-vacuum at commit, and the copy a
//! full VACUUM is built out of.
//!
//! Invariant: a page is moved by finding the one reference to it, not by
//! searching for references to it. That is what the pointer map is for, and it
//! is why every move goes through [`relocate_page`] - a second routine that
//! moved a page its own way would be a second place for a reverse pointer to go
//! stale, and a stale reverse pointer has no symptom until the next move
//! rewrites four bytes of a page that never pointed there.
//!
//! # The one page kind that cannot move
//!
//! A B-tree root is named by the catalog above, in `sqlite_schema`'s `rootpage`
//! column. Storage cannot rewrite that column, because storage does not know
//! what a column is. So a root page is never relocated, and SQLite does the
//! same - `incrVacuumStep` returns corruption if the page it is about to move
//! turns out to be a root. What keeps that from happening in practice is where
//! roots are *allocated*: under a vacuum mode a new root is placed at the
//! lowest page number that is not already a root, so the trailing pages a
//! vacuum works on are never roots.
//!
//! # Full VACUUM is a copy, not a rearrangement
//!
//! `VACUUM` builds a new database and copies everything into it in order, which
//! is why it defragments what incremental vacuum only compacts. The copy here
//! uses the append primitives rather than the inserting ones on purpose: an
//! index's order is decided by the collation its declaration names, storage
//! does not know collations, and appending in source order reproduces whatever
//! order the source was in without ever comparing two keys.

use std::sync::Arc;

use inillucent_base::error::corrupt;
use inillucent_base::ids::PageId;
use inillucent_base::limits::Limits;
use inillucent_base::{bytes, DbResult};

use crate::alloc;
use crate::btree::{BTreePage, PageLayout};
use crate::cursor::{BTreeCursor, TreeKind};
use crate::header::VacuumMode;
use crate::mutate;
use crate::pager::{FailSite, Pager};
use crate::ptrmap;

// What `auto_vacuum_commit` and `copy_database` need. They are `#[cfg(test)]`
// now, so these are too - a release build carries neither the functions nor
// their imports (task-1946, M1).
#[cfg(test)]
use crate::schema;
#[cfg(test)]
use inillucent_value::record::{encode_record, RecordRef};
#[cfg(test)]
use inillucent_value::Value;

/// Moves a page's contents to another page and repoints everything at it.
///
/// `to` must already have been taken out of the freelist, and `from` is left
/// behind for the caller to truncate away. The order is deliberate: the page's
/// contents are copied first, then what it points *at* is told where it now
/// lives, and only then is the reference *to* it moved. A failure in the middle
/// leaves reverse pointers that name a page whose contents are still correct,
/// which the integrity check reports; the opposite order would leave a live
/// reference to a page that had already been overwritten.
// **What this module's seven functions are actually reached by**, because the
// task-1946 review's M1 listed six of them as callerless and a grep says
// otherwise (checked function by function):
//
// - `relocate_page` - `mutate.rs`, in this crate. Public.
// - `incremental_vacuum` - `inillucent-compat`'s `writeperf` benchmark and
//   `btree_model.rs`. Public, and its numbers are recorded in
//   `compat/baseline/phase4-mutation-baselines.json` as `incremental-vacuum`.
// - `copy_tree` - the same benchmark, recorded as `vacuum-copy-tree`. Public.
// - `final_size` and `incremental_step` - called from inside this module by the
//   two above. Private now; they were never named from outside it.
// - `auto_vacuum_commit` and `copy_database` - this module's own tests, and
//   nothing else anywhere. Private now.
//
// So nothing here is dead, and nothing here is reached by the shipping engine
// either: `VACUUM` is `inillucent-engine/src/rebuild.rs`, and what this file
// measures is what the retired pager did. Three of the seven stop being part of
// the crate's public surface, which is the part of M1 that was true.
pub fn relocate_page(pager: &mut Pager, from: PageId, to: PageId) -> DbResult<()> {
    pager.reach_failpoint(FailSite::Relocate)?;
    if from.get() < 3 || to.get() < 3 {
        return Err(corrupt("neither of the first two pages can be relocated"));
    }
    let Some(entry) = ptrmap::get(pager, from)? else {
        return Err(corrupt(format!(
            "page {} has no pointer-map entry and cannot be moved",
            from.get()
        )));
    };
    if entry.kind == ptrmap::ROOT_PAGE {
        return Err(corrupt(format!(
            "page {} is a B-tree root, whose page number is recorded in the schema",
            from.get()
        )));
    }

    let pin = pager.get_page(from)?;
    let image = pin.bytes().to_vec();
    drop(pin);
    pager.edit_page(to, |raw| {
        let target = bytes::window_mut(raw, 0, image.len())?;
        target.copy_from_slice(&image);
        Ok(())
    })?;

    match entry.kind {
        ptrmap::BTREE_CHILD | ptrmap::ROOT_PAGE => {
            // The page's own children and overflow chains now hang from `to`.
            ptrmap::refresh_btree_page(pager, to)?;
        }
        _ => {
            let pin = pager.get_page(to)?;
            let next = bytes::read_u32(pin.bytes(), 0)?;
            drop(pin);
            if next != 0 {
                let next = PageId::from_persisted(next)?;
                ptrmap::put(pager, next, ptrmap::Entry::overflow_next(to))?;
            }
        }
    }

    let parent = PageId::from_persisted(entry.parent)
        .map_err(|_| corrupt(format!("page {} says its parent is page zero", from.get())))?;
    modify_page_pointer(pager, parent, from, to, entry.kind)?;
    ptrmap::put(pager, to, entry)?;
    Ok(())
}

/// Rewrites the one reference on `parent` that names `from` so it names `to`.
fn modify_page_pointer(
    pager: &mut Pager,
    parent: PageId,
    from: PageId,
    to: PageId,
    kind: u8,
) -> DbResult<()> {
    if kind == ptrmap::OVERFLOW2 {
        let seen = {
            let pin = pager.get_page(parent)?;
            bytes::read_u32(pin.bytes(), 0)?
        };
        if seen != from.get() {
            return Err(corrupt(format!(
                "overflow page {} says page {} precedes it, but that page points at {seen}",
                from.get(),
                parent.get()
            )));
        }
        return pager.edit_page(parent, |raw| bytes::write_u32(raw, 0, to.get()));
    }

    let layout = read_layout(pager, parent)?;
    let pin = pager.get_page(parent)?;
    let view = BTreePage::new(pin.bytes(), &layout);
    let mut at: Option<usize> = None;
    for index in 0..layout.cell_count {
        let cell = view.cell(index)?;
        if kind == ptrmap::OVERFLOW1 {
            if cell.overflow == Some(from) {
                // The overflow pointer is the last four bytes of the cell.
                at = Some(cell.offset.saturating_add(cell.len).saturating_sub(4));
                break;
            }
        } else if cell.left_child == Some(from) {
            at = Some(cell.offset);
            break;
        }
    }
    if at.is_none() && kind != ptrmap::OVERFLOW1 && layout.right_child == Some(from) {
        at = Some(
            layout
                .base
                .saturating_add(crate::btree::offsets::RIGHT_CHILD),
        );
    }
    drop(pin);
    let Some(offset) = at else {
        return Err(corrupt(format!(
            "page {} claims page {} points at it, and it does not",
            from.get(),
            parent.get()
        )));
    };
    pager.edit_page(parent, |raw| bytes::write_u32(raw, offset, to.get()))
}

/// Reads a page's validated layout.
fn read_layout(pager: &mut Pager, page: PageId) -> DbResult<Arc<PageLayout>> {
    let usable = pager.usable_size()?;
    let pin = pager.get_page(page)?;
    pin.layout(usable)
}

/// Returns the page number after which nothing but pointer maps and the lock
/// byte would be left, given how many pages are free.
///
/// This is SQLite's `finalDbSize`, and the correction it makes is worth
/// stating: freeing pages also frees the pointer-map pages that covered them,
/// so the file shrinks by more than the free count. Getting it wrong does not
/// corrupt anything - it makes a vacuum stop one page early or try one page too
/// many - but it is what decides when the work is done.
fn final_size(pager: &Pager) -> DbResult<u32> {
    let original = pager.page_count();
    let free = pager.header().freelist_count;
    let usable = pager.usable_size()?;
    let entries = u64::from(usable / 5);
    if entries == 0 {
        return Err(corrupt("a page too small to hold a pointer-map entry"));
    }
    let original64 = u64::from(original);
    let free64 = u64::from(free);
    let last_map = u64::from(map_page_number(pager, original)?);
    let maps = free64
        .saturating_sub(original64)
        .wrapping_add(last_map)
        .wrapping_add(entries)
        / entries;
    let mut result = original64.saturating_sub(free64).saturating_sub(maps);
    let lock_byte = u64::from(alloc::lock_byte_page(pager.page_size()));
    if original64 > lock_byte && result < lock_byte {
        result = result.saturating_sub(1);
    }
    let mut result = u32::try_from(result).unwrap_or(1).max(1);
    while result > 1 {
        let page = PageId::from_persisted(result)?;
        if ptrmap::is_map_page(pager, page)? || result == alloc::lock_byte_page(pager.page_size()) {
            result = result.saturating_sub(1);
            continue;
        }
        break;
    }
    Ok(result)
}

/// Returns the pointer-map page that covers a page, or zero when none does.
fn map_page_number(pager: &Pager, page: u32) -> DbResult<u32> {
    let Ok(page_id) = PageId::from_persisted(page) else {
        return Ok(0);
    };
    Ok(pager
        .header()
        .pointer_map_page(page_id)?
        .map(PageId::get)
        .unwrap_or(page))
}

/// Performs one step of an incremental vacuum, reporting whether it did work.
///
/// A step takes the last page of the file: if it is free it comes off the
/// freelist, and if it is in use it is moved into a free page lower down. Then
/// the file loses its trailing page, together with any pointer maps that have
/// nothing left to cover.
fn incremental_step(pager: &mut Pager) -> DbResult<bool> {
    if pager.header().vacuum_mode == VacuumMode::None {
        return Ok(false);
    }
    let free = pager.header().freelist_count;
    if free == 0 {
        return Ok(false);
    }
    let original = pager.page_count();
    let finish = final_size(pager)?;
    if original < finish || free >= original {
        return Err(corrupt(format!(
            "a {original}-page database with {free} free pages cannot shrink to {finish}"
        )));
    }
    if original <= finish {
        return Ok(false);
    }

    let last = PageId::from_persisted(original)?;
    let lock_byte = alloc::lock_byte_page(pager.page_size());
    if !ptrmap::is_map_page(pager, last)? && original != lock_byte {
        let Some(entry) = ptrmap::get(pager, last)? else {
            return Err(corrupt(format!("page {original} has no pointer-map entry")));
        };
        match entry.kind {
            ptrmap::ROOT_PAGE => {
                return Err(corrupt(format!(
                    "page {original} is a B-tree root and cannot be moved"
                )))
            }
            ptrmap::FREE_PAGE => alloc::allocate_exact(pager, last)?,
            _ => {
                let target = alloc::allocate_page(pager)?;
                if target.get() >= last.get() {
                    return Err(corrupt(format!(
                        "a vacuum step was given page {} to move page {original} into",
                        target.get()
                    )));
                }
                relocate_page(pager, last, target)?;
            }
        }
    }

    let mut shrunk = original.saturating_sub(1);
    while shrunk > 1 {
        let page = PageId::from_persisted(shrunk)?;
        if ptrmap::is_map_page(pager, page)? || shrunk == lock_byte {
            shrunk = shrunk.saturating_sub(1);
            continue;
        }
        break;
    }
    pager.set_page_count(shrunk)?;
    let mut header = *pager.header();
    header.database_size = shrunk;
    pager.set_header(header)?;
    Ok(true)
}

/// Runs up to `pages` incremental vacuum steps, returning how many ran.
pub fn incremental_vacuum(pager: &mut Pager, pages: u32) -> DbResult<u32> {
    let mut done = 0u32;
    while done < pages {
        if !incremental_step(pager)? {
            break;
        }
        done = done.saturating_add(1);
    }
    Ok(done)
}

// Only this module's own tests reach it, so a release build does not carry
// it (task-1946, M1). Nothing in the shipping engine ever did: `VACUUM` is
// `inillucent-engine/src/rebuild.rs`.
#[cfg(test)]
/// Shrinks the file as far as it will go, which is what a FULL auto-vacuum
/// database does at commit.
///
/// SQLite computes the final size once and moves each page directly to where it
/// belongs. This runs the incremental step until there is nothing left to do,
/// which reaches the same file with more page moves. The end state is what the
/// format specifies; the route to it is not.
fn auto_vacuum_commit(pager: &mut Pager) -> DbResult<u32> {
    if pager.header().vacuum_mode != VacuumMode::Auto {
        return Ok(0);
    }
    let mut moved = 0u32;
    // The bound is the file's own length: every step removes at least one page,
    // so a run longer than that is a loop rather than a vacuum.
    let limit = pager.page_count().saturating_add(1);
    while moved < limit {
        if !incremental_step(pager)? {
            break;
        }
        moved = moved.saturating_add(1);
    }
    Ok(moved)
}

/// Copies one B-tree into a fresh tree in another database, in source order.
///
/// The destination tree is created here and its root returned, because a copy
/// that reused the source's page numbers would not be a copy - the whole point
/// of a VACUUM is that the destination is laid out again from nothing.
pub fn copy_tree(source: &mut Pager, destination: &mut Pager, root: PageId) -> DbResult<PageId> {
    let kind = mutate::tree_kind(source, root)?;
    let limits = Limits::default();
    match kind {
        TreeKind::Table => {
            let new_root = mutate::create_table(destination)?;
            let mut cursor = BTreeCursor::table(root);
            let mut more = cursor.first(source)?;
            while more {
                let rowid = cursor.rowid()?;
                let payload = cursor.payload(source, &limits)?;
                mutate::append_row(destination, new_root, rowid, &payload)?;
                more = cursor.next(source)?;
            }
            Ok(new_root)
        }
        TreeKind::Index => {
            let new_root = mutate::create_index(destination)?;
            let mut cursor = BTreeCursor::index(root, Default::default());
            let mut more = cursor.first(source)?;
            while more {
                let payload = cursor.payload(source, &limits)?;
                mutate::append_entry(destination, new_root, &payload)?;
                more = cursor.next(source)?;
            }
            Ok(new_root)
        }
    }
}

// Only this module's own tests reach it, so a release build does not carry
// it (task-1946, M1). Nothing in the shipping engine ever did: `VACUUM` is
// `inillucent-engine/src/rebuild.rs`.
#[cfg(test)]
/// Copies a whole database into another, rewriting each schema row's root page.
///
/// This is the mechanism a full `VACUUM` runs on: build the new file, copy every
/// object into it in order, and replace the original with it. The replacement
/// itself belongs to the transaction layer - it is a file operation with a
/// durability contract - so what is here is the part that is about pages.
///
/// `sqlite_schema`'s fourth column is the root page, and it is the only column
/// this touches. Reading a row, replacing one field and encoding it again is
/// record work rather than SQL work: nothing here knows what the column is
/// called, only which position the format puts it in.
fn copy_database(source: &mut Pager, destination: &mut Pager) -> DbResult<usize> {
    let objects = schema::load_schema(source)?;
    let mut moved = Vec::new();
    for object in &objects {
        let Some(root) = object.root_page else {
            continue;
        };
        let new_root = copy_tree(source, destination, root)?;
        moved.push((root.get(), new_root.get()));
    }

    let limits = Limits::default();
    let encoding = source.text_encoding();
    let schema_root = PageId::from_persisted(schema::SCHEMA_ROOT)?;
    let destination_root = PageId::from_persisted(schema::SCHEMA_ROOT)?;
    let mut cursor = BTreeCursor::table(schema_root);
    let mut copied = 0usize;
    let mut more = cursor.first(source)?;
    while more {
        let rowid = cursor.rowid()?;
        let payload = cursor.payload(source, &limits)?;
        let record = RecordRef::parse_with_limits(&payload, encoding, &limits)?;
        let mut values: Vec<Value<'static>> = record
            .values()?
            .into_iter()
            .map(|value| value.into_owned())
            .collect::<DbResult<Vec<_>>>()?;
        if let Some(field) = values.get_mut(3) {
            if let Value::Integer(old) = field {
                let old = u32::try_from(*old).unwrap_or(0);
                if let Some((_, new_root)) = moved.iter().find(|(from, _)| *from == old) {
                    *field = Value::Integer(i64::from(*new_root));
                }
            }
        }
        let rewritten = encode_record(&values, destination.text_encoding(), 4)?;
        mutate::append_row(destination, destination_root, rowid, &rewritten)?;
        copied = copied.saturating_add(1);
        more = cursor.next(source)?;
    }
    Ok(copied)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::check::{self, CheckOptions};
    use crate::header::VacuumMode;
    use crate::pager::{NewDatabase, PagerOptions};
    use inillucent_base::page::PageSize;
    use inillucent_value::{BlobValue, TextEncoding};
    use inillucent_vfs::memory::MemoryVfs;
    use inillucent_vfs::DbPath;

    /// Creates an empty database in memory and opens it for writing.
    fn create(vfs: &MemoryVfs, name: &str, page_size: u32, vacuum: VacuumMode) -> Pager {
        Pager::create(
            vfs,
            &DbPath::new(name),
            PagerOptions::default(),
            NewDatabase {
                page_size: PageSize::new(page_size).unwrap(),
                reserved_bytes: 0,
                text_encoding: TextEncoding::Utf8,
                vacuum_mode: vacuum,
            },
        )
        .unwrap()
    }

    /// Encodes a record holding one blob of `len` bytes.
    fn blob_row(marker: u8, len: usize) -> Vec<u8> {
        let blob = vec![marker; len];
        encode_record(
            &[Value::Blob(BlobValue::borrowed(&blob))],
            TextEncoding::Utf8,
            4,
        )
        .unwrap()
    }

    /// Reads every row of a table B-tree in cursor order.
    fn scan(pager: &mut Pager, root: PageId) -> Vec<(i64, Vec<u8>)> {
        let limits = Limits::default();
        let mut cursor = BTreeCursor::table(root);
        let mut rows = Vec::new();
        let mut more = cursor.first(pager).unwrap();
        while more {
            rows.push((
                cursor.rowid().unwrap(),
                cursor.payload(pager, &limits).unwrap(),
            ));
            more = cursor.next(pager).unwrap();
        }
        rows
    }

    /// Runs the integrity check over named roots and fails with what it found.
    fn check_roots(pager: &mut Pager, roots: &[PageId]) {
        let report =
            check::check_database_with_options(pager, &CheckOptions::roots(roots.to_vec()))
                .unwrap();
        assert!(report.is_ok(), "{:#?}", report.as_pragma_output());
    }

    /// An incremental vacuum gives pages back to the file system and leaves
    /// every row where it was.
    #[test]
    fn an_incremental_vacuum_shrinks_the_file_and_keeps_every_row() {
        let vfs = MemoryVfs::new();
        let mut pager = create(&vfs, "incr.db", 512, VacuumMode::Incremental);
        pager.begin_write().unwrap();
        let root = mutate::create_table(&mut pager).unwrap();
        for rowid in 1..=300i64 {
            mutate::insert_row(&mut pager, root, rowid, &blob_row(1, 400)).unwrap();
        }
        pager.commit().unwrap();
        let grown = pager.page_count();

        pager.begin_write().unwrap();
        for rowid in 1..=200i64 {
            assert!(mutate::delete_row(&mut pager, root, rowid).unwrap());
        }
        pager.commit().unwrap();
        let freed = pager.header().freelist_count;
        assert!(freed > 10, "expected a real freelist, found {freed}");

        pager.begin_write().unwrap();
        let steps = incremental_vacuum(&mut pager, 10_000).unwrap();
        pager.commit().unwrap();
        assert!(steps > 0);
        assert!(
            pager.page_count() < grown,
            "the file did not shrink: {} pages",
            pager.page_count()
        );
        assert_eq!(pager.header().freelist_count, 0);
        check_roots(&mut pager, &[root]);

        let rows = scan(&mut pager, root);
        assert_eq!(rows.len(), 100);
        for (index, (rowid, payload)) in rows.iter().enumerate() {
            assert_eq!(*rowid, index as i64 + 201);
            assert_eq!(payload, &blob_row(1, 400));
        }
    }

    /// A FULL auto-vacuum database reclaims everything at commit.
    #[test]
    fn a_full_auto_vacuum_leaves_no_free_pages() {
        let vfs = MemoryVfs::new();
        let mut pager = create(&vfs, "auto.db", 1024, VacuumMode::Auto);
        pager.begin_write().unwrap();
        let root = mutate::create_table(&mut pager).unwrap();
        for rowid in 1..=400i64 {
            mutate::insert_row(&mut pager, root, rowid, &blob_row(2, 300)).unwrap();
        }
        pager.commit().unwrap();
        let grown = pager.page_count();

        pager.begin_write().unwrap();
        for rowid in (1..=400i64).step_by(2) {
            assert!(mutate::delete_row(&mut pager, root, rowid).unwrap());
        }
        auto_vacuum_commit(&mut pager).unwrap();
        pager.commit().unwrap();

        assert_eq!(pager.header().freelist_count, 0);
        assert!(pager.page_count() < grown);
        check_roots(&mut pager, &[root]);
        let rows: Vec<i64> = scan(&mut pager, root)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(
            rows,
            (1..=400).filter(|id| id % 2 == 0).collect::<Vec<i64>>()
        );
    }

    /// Relocating a page that holds an overflow chain moves every reference to
    /// it: the cell that owns the chain, the next page in the chain, and the
    /// pointer-map entries on both sides.
    #[test]
    fn relocation_moves_every_reference_to_a_page() {
        let vfs = MemoryVfs::new();
        let mut pager = create(&vfs, "reloc.db", 512, VacuumMode::Incremental);
        pager.begin_write().unwrap();
        let root = mutate::create_table(&mut pager).unwrap();
        for rowid in 1..=40i64 {
            mutate::insert_row(&mut pager, root, rowid, &blob_row(3, 5_000)).unwrap();
        }
        pager.commit().unwrap();
        let before = scan(&mut pager, root);

        // Free some pages so there is somewhere to relocate into, then vacuum
        // the whole file, which moves nearly every page in it.
        pager.begin_write().unwrap();
        for rowid in 1..=20i64 {
            assert!(mutate::delete_row(&mut pager, root, rowid).unwrap());
        }
        pager.commit().unwrap();
        pager.begin_write().unwrap();
        incremental_vacuum(&mut pager, 10_000).unwrap();
        pager.commit().unwrap();

        check_roots(&mut pager, &[root]);
        let after = scan(&mut pager, root);
        assert_eq!(after, before.get(20..).unwrap().to_vec());
    }

    /// Copying a database reproduces every schema row and every row of every
    /// table, with each object's root page rewritten to where it now lives.
    #[test]
    fn copying_a_database_reproduces_it() {
        let vfs = MemoryVfs::new();
        let mut source = create(&vfs, "source.db", 1024, VacuumMode::None);
        let schema_root = PageId::from_persisted(schema::SCHEMA_ROOT).unwrap();
        source.begin_write().unwrap();
        let table = mutate::create_table(&mut source).unwrap();
        let index = mutate::create_index(&mut source).unwrap();
        for rowid in 1..=250i64 {
            mutate::insert_row(&mut source, table, rowid, &blob_row(4, 60)).unwrap();
        }
        for value in 0..250i64 {
            let entry = encode_record(
                &[Value::Integer(value), Value::Integer(value)],
                TextEncoding::Utf8,
                4,
            )
            .unwrap();
            mutate::insert_entry(
                &mut source,
                index,
                &inillucent_value::record::KeyInfo::binary(2),
                &entry,
            )
            .unwrap();
        }
        // Two schema rows, in the five columns the format defines.
        for (rowid, kind, name, root, sql) in [
            (1i64, "table", "t", table.get(), "CREATE TABLE t(a)"),
            (2i64, "index", "i", index.get(), "CREATE INDEX i ON t(a)"),
        ] {
            let record = encode_record(
                &[
                    Value::text_utf8(kind.as_bytes()),
                    Value::text_utf8(name.as_bytes()),
                    Value::text_utf8(b"t"),
                    Value::Integer(i64::from(root)),
                    Value::text_utf8(sql.as_bytes()),
                ],
                TextEncoding::Utf8,
                4,
            )
            .unwrap();
            mutate::insert_row(&mut source, schema_root, rowid, &record).unwrap();
        }
        source.commit().unwrap();

        let mut destination = create(&vfs, "destination.db", 1024, VacuumMode::None);
        source.begin_read().unwrap();
        destination.begin_write().unwrap();
        let rows = copy_database(&mut source, &mut destination).unwrap();
        destination.commit().unwrap();
        assert_eq!(rows, 2);

        destination.begin_read().unwrap();
        let report = check::integrity_check(&mut destination).unwrap();
        assert!(report.is_ok(), "{:#?}", report.as_pragma_output());

        let objects = schema::load_schema(&mut destination).unwrap();
        assert_eq!(objects.len(), 2);
        let table_root = objects
            .iter()
            .find(|object| object.name == "t")
            .and_then(|object| object.root_page)
            .unwrap();
        let copied = scan(&mut destination, table_root);
        let original = scan(&mut source, table);
        assert_eq!(copied, original);
    }
}
